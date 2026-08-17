//! Co-pilot mailbox poller.
//!
//! Maintains an in-memory **enrolled set** of co-pilot agent ids that are
//! currently considered "active" — i.e. whose tasklist still has work or has
//! been opened recently. The set is driven by `TasklistWoke` / `TasklistSlept`
//! lifecycle events and is read by the wake-on-deliver path.
//!
//! Three responsibilities:
//!   1. **Event reactor** — subscribe to the existing [`EventBus`] and
//!      add/remove the bound co-pilot on every wake/sleep event. Add is
//!      idempotent — repeat wakes are no-ops.
//!   2. **Periodic sleep sweep** — every [`POLL_INTERVAL`] tick, walk a
//!      snapshot of the enrolled set and evict any whose tasklist's
//!      `should_sleep` predicate fires. Team-owned tasklists are evicted by
//!      emitting `TasklistSlept`, which loops back through this same
//!      subscriber, unifying the path with externally-driven sleeps.
//!      Agent-owned tasklists are evicted inline, because `TasklistSlept`
//!      carries only a team id and could never resolve back to them.
//!   3. **Startup rebuild** — on spawn, walk every tasklist across teams
//!      (via [`TasklistStore::list_all_across_teams`]) and enroll any whose
//!      `is_tasklist_active` predicate is true and that has a bound co-pilot.
//!      Stale enrollments from a previous process do not survive a restart
//!      because the set lives in memory.
//!
//! KNOWN GAP: the startup rebuild and the `TasklistWoke` reactor are both
//! team-only — the rebuild enumerates `teams/` and the wake handler resolves
//! its team id with a team-keyed `get`. Agent-owned (project) co-pilots are
//! therefore never enrolled by either path; they enrol on demand through
//! wake-on-deliver in [`crate::queue_manager::QueueManagerRegistry::submit_message`],
//! which keys off the agent's profile template rather than tasklist ownership.
//! Closing the gap properly means carrying the owner on the lifecycle events
//! rather than a bare team id.
//!
//! Non-co-pilot agents are unaffected: the enrolled set is purely
//! advisory state read by co-pilot-specific code paths. The existing per-agent
//! `AgentQueueManager` continues to dispatch any agent's queued messages
//! regardless of enrollment.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{watch, RwLock};
use tracing::{debug, info, warn};

use ao_persistence::PersistenceLayer;
use ao_protocol::agent::AgentId;
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;

use crate::event_bus::EventBus;
use crate::tasklist_lifecycle::{is_tasklist_active, maybe_emit_sleep, should_sleep};

/// How often the poller sweeps the enrolled set looking for sleep-eligible
/// tasklists. Each tick reads one tasklist meta per enrolled co-pilot — cheap.
/// Picked at 60s to align with [`crate::tasklist_lifecycle::OVERLAY_OPEN_KEEPALIVE_SECS`]
/// so a missed FE keepalive can be evicted within roughly one tick of the
/// keepalive expiring.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Concurrent in-memory set of enrolled co-pilot agent ids. Held inside an
/// `Arc` and shared between the poller's spawned task and any external
/// readers (e.g. the wake-on-deliver path).
#[derive(Default, Debug)]
pub struct EnrolledCopilots {
    inner: RwLock<HashSet<AgentId>>,
}

impl EnrolledCopilots {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashSet::new()),
        }
    }

    /// Insert `agent_id`. Returns `true` if the agent was newly added; `false`
    /// if it was already enrolled (idempotent).
    pub async fn enroll(&self, agent_id: &str) -> bool {
        let mut set = self.inner.write().await;
        set.insert(agent_id.to_string())
    }

    /// Remove `agent_id`. Returns `true` if the agent was present; `false`
    /// if it was already unenrolled (idempotent — duplicate sleep events
    /// from the periodic sweep + an external emitter cannot loop forever).
    pub async fn unenroll(&self, agent_id: &str) -> bool {
        let mut set = self.inner.write().await;
        set.remove(agent_id)
    }

    pub async fn is_enrolled(&self, agent_id: &str) -> bool {
        self.inner.read().await.contains(agent_id)
    }

    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    pub async fn snapshot(&self) -> Vec<AgentId> {
        self.inner.read().await.iter().cloned().collect()
    }
}

/// Co-pilot mailbox poller. Construct with [`Self::new`], then call
/// [`Self::run`] to spawn the background task.
pub struct CopilotMailboxPoller {
    enrolled: Arc<EnrolledCopilots>,
    persistence: Arc<PersistenceLayer>,
    event_bus: Arc<EventBus>,
    poll_interval: Duration,
}

impl CopilotMailboxPoller {
    pub fn new(persistence: Arc<PersistenceLayer>, event_bus: Arc<EventBus>) -> Self {
        Self {
            enrolled: Arc::new(EnrolledCopilots::new()),
            persistence,
            event_bus,
            poll_interval: POLL_INTERVAL,
        }
    }

    /// Test-only override of the periodic sweep interval. Production callers
    /// should use [`Self::new`] which defaults to [`POLL_INTERVAL`].
    #[cfg(test)]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Handle to the shared enrolled set. Clone freely; consumers (e.g. the
    /// wake-on-deliver path) take a long-lived `Arc` reference.
    pub fn enrolled(&self) -> Arc<EnrolledCopilots> {
        Arc::clone(&self.enrolled)
    }

    /// Walk every tasklist across teams and enroll the bound co-pilot of any
    /// tasklist whose `is_tasklist_active(now)` is true. Idempotent — if a
    /// tasklist's co-pilot is already in the set, it is left there. Returns
    /// the number of newly-added agents.
    pub async fn rebuild_from_active(&self) -> Result<usize, AoError> {
        let now = Utc::now();
        let tasklists = self.persistence.tasklists.list_all_across_teams().await?;
        let mut count = 0;
        for tl in tasklists {
            let Some(agent_id) = tl.copilot_agent_id.as_deref() else {
                continue;
            };
            if !is_tasklist_active(&tl, now) {
                continue;
            }
            if self.enrolled.enroll(agent_id).await {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Spawn the poller as a background tokio task. Returns a `watch::Sender`
    /// — drop it (or `send(())`) to stop both the event-reactor and the
    /// periodic sweep.
    pub fn run(self) -> watch::Sender<()> {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(());

        info!(
            poll_interval = ?self.poll_interval,
            "CopilotMailboxPoller starting",
        );

        tokio::spawn(async move {
            // Subscribe BEFORE the startup rebuild so any wake/sleep events
            // emitted during the initial scan aren't lost.
            let mut rx = self.event_bus.subscribe();

            match self.rebuild_from_active().await {
                Ok(n) => info!(
                    enrolled = n,
                    "CopilotMailboxPoller startup rebuild complete",
                ),
                Err(e) => warn!("CopilotMailboxPoller startup rebuild failed: {e}"),
            }

            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        info!("CopilotMailboxPoller shutting down");
                        break;
                    }
                    _ = tokio::time::sleep(self.poll_interval) => {
                        if let Err(e) = self.sweep_for_sleep().await {
                            warn!("CopilotMailboxPoller sweep errored: {e}");
                        }
                    }
                    evt = rx.recv() => {
                        match evt {
                            Ok(event) => self.handle_event(&event.payload).await,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                warn!(
                                    skipped,
                                    "CopilotMailboxPoller lagged on broadcast bus",
                                );
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        });

        shutdown_tx
    }

    /// React to a single wake/sleep event. Public for tests.
    pub async fn handle_event(&self, payload: &AgentEventPayload) {
        match payload {
            AgentEventPayload::TasklistWoke {
                team_id,
                tasklist_id,
                ..
            } => {
                if let Err(e) = self.enroll_for_tasklist(team_id, tasklist_id).await {
                    warn!(
                        team_id = %team_id,
                        tasklist_id = %tasklist_id,
                        "CopilotMailboxPoller enroll failed: {e}",
                    );
                }
            }
            AgentEventPayload::TasklistSlept {
                team_id,
                tasklist_id,
            } => {
                if let Err(e) = self.unenroll_for_tasklist(team_id, tasklist_id).await {
                    warn!(
                        team_id = %team_id,
                        tasklist_id = %tasklist_id,
                        "CopilotMailboxPoller unenroll failed: {e}",
                    );
                }
            }
            _ => {}
        }
    }

    async fn enroll_for_tasklist(
        &self,
        team_id: &str,
        tasklist_id: &str,
    ) -> Result<(), AoError> {
        let Some(tl) = self.persistence.tasklists.get(team_id, tasklist_id).await? else {
            // Tasklist deleted between event emission and now — nothing to do.
            return Ok(());
        };
        let Some(agent_id) = tl.copilot_agent_id else {
            // Wake fired before the co-pilot was bound (e.g. task added before
            // the user ever opened the overlay). Nothing to enroll yet.
            return Ok(());
        };
        let added = self.enrolled.enroll(&agent_id).await;
        debug!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            agent_id = %agent_id,
            newly_added = added,
            "CopilotMailboxPoller enrolled co-pilot",
        );
        Ok(())
    }

    async fn unenroll_for_tasklist(
        &self,
        team_id: &str,
        tasklist_id: &str,
    ) -> Result<(), AoError> {
        let Some(tl) = self.persistence.tasklists.get(team_id, tasklist_id).await? else {
            return Ok(());
        };
        let Some(agent_id) = tl.copilot_agent_id else {
            return Ok(());
        };
        let removed = self.enrolled.unenroll(&agent_id).await;
        debug!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            agent_id = %agent_id,
            was_present = removed,
            "CopilotMailboxPoller unenrolled co-pilot",
        );
        Ok(())
    }

    /// Walk the enrolled set, reload each tasklist, and emit `TasklistSlept`
    /// for any that have transitioned to sleep-eligible. The emitted event
    /// loops back through `handle_event` to actually remove the agent — keeps
    /// eviction logic in one place.
    async fn sweep_for_sleep(&self) -> Result<(), AoError> {
        let now = Utc::now();
        let snapshot = self.enrolled.snapshot().await;
        for agent_id in snapshot {
            let Some(tl) = self
                .persistence
                .tasklists
                .find_by_copilot_agent_id(&agent_id)
                .await?
            else {
                // Tasklist gone (deleted) — drop the orphan enrollment so
                // we don't carry a phantom forever.
                self.enrolled.unenroll(&agent_id).await;
                continue;
            };
            match &tl.owner {
                ao_protocol::tasklist::TasklistOwner::Team { .. } => {
                    maybe_emit_sleep(&self.event_bus, &tl, now).await;
                }
                ao_protocol::tasklist::TasklistOwner::Agent { .. } => {
                    // `TasklistSlept` carries only a team id, and
                    // `unenroll_for_tasklist` resolves it with a team-keyed
                    // `get`, so an agent-owned tasklist routed through the
                    // event round-trip would never resolve and the co-pilot
                    // would stay enrolled forever. Evict directly instead —
                    // this loop already holds both the tasklist and the bound
                    // agent id, which is exactly what the round-trip exists to
                    // rediscover.
                    if should_sleep(&tl, now) {
                        let removed = self.enrolled.unenroll(&agent_id).await;
                        debug!(
                            tasklist_id = %tl.id,
                            agent_id = %agent_id,
                            was_present = removed,
                            "CopilotMailboxPoller unenrolled agent-owned co-pilot",
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ao_persistence::paths::DataRoot;
    use ao_protocol::tasklist::{
        Task, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistStatus,
    };
    use chrono::Duration as ChronoDuration;
    use tempfile::TempDir;

    use crate::tasklist_lifecycle::{
        emit_sleep, emit_wake, SLEEP_GRACE_WINDOW_MINUTES, WakeReason,
    };

    async fn make_persistence() -> (Arc<PersistenceLayer>, TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let persistence = Arc::new(
            PersistenceLayer::init_with_root(DataRoot::new(tmp.path()))
                .await
                .expect("persistence init"),
        );
        (persistence, tmp)
    }

    fn make_tasklist(team_id: &str, tasklist_id: &str, statuses: &[TaskStatus]) -> Tasklist {
        let tasks: Vec<Task> = statuses
            .iter()
            .enumerate()
            .map(|(i, s)| Task {
                id: format!("t{i}"),
                owner_agent_id: "agent-a".to_string(),
                prompt: "do work".to_string(),
                expected_outputs: vec![],
                status: *s,
                group_id: "g1".to_string(),
                attempt_count: 0,
                error_log: vec![],
                comments: vec![],
                attachments: vec![],
                remind_me: None,
                parse_failed: false,
                notification_parse_retry_count: 0,
                assignment: None,
                classifier_token: 0,
                dispatch_token: 0,
            })
            .collect();
        Tasklist {
            id: tasklist_id.to_string(),
            owner: ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() },
            team_id: Some(team_id.to_string()),
            title: "Sample".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: "g1".to_string(),
                mode: TaskGroupMode::Seq,
                tasks,
            }],
            workspace_dir: format!("/tmp/ws-{tasklist_id}"),
            transcripts_dir: format!("/tmp/tx-{tasklist_id}"),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            }
    }

    async fn seed_bound_tasklist(
        persistence: &Arc<PersistenceLayer>,
        team_id: &str,
        tasklist_id: &str,
        agent_id: &str,
        statuses: &[TaskStatus],
    ) -> Tasklist {
        let tl = make_tasklist(team_id, tasklist_id, statuses);
        persistence.tasklists.create(&tl).await.expect("create tl");
        persistence
            .tasklists
            .bind_copilot_agent_id(team_id, tasklist_id, agent_id)
            .await
            .expect("bind copilot");
        persistence
            .tasklists
            .get(team_id, tasklist_id)
            .await
            .expect("reload")
            .expect("exists")
    }

    // ---- EnrolledCopilots --------------------------------------------------

    #[tokio::test]
    async fn enroll_is_idempotent() {
        let set = EnrolledCopilots::new();
        assert!(set.enroll("a").await, "first enroll is a real add");
        assert!(!set.enroll("a").await, "second enroll is a no-op");
        assert_eq!(set.len().await, 1);
    }

    #[tokio::test]
    async fn unenroll_returns_present_then_absent() {
        let set = EnrolledCopilots::new();
        set.enroll("a").await;
        assert!(set.unenroll("a").await);
        assert!(!set.unenroll("a").await);
        assert!(!set.is_enrolled("a").await);
    }

    // ---- handle_event ------------------------------------------------------

    #[tokio::test]
    async fn wake_event_enrolls_bound_copilot() {
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));
        seed_bound_tasklist(&persistence, "team-a", "tl-1", "copilot-A", &[TaskStatus::Pending])
            .await;

        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus));
        poller
            .handle_event(&AgentEventPayload::TasklistWoke {
                team_id: "team-a".to_string(),
                tasklist_id: "tl-1".to_string(),
                reason: WakeReason::TaskAdded.as_str().to_string(),
            })
            .await;

        assert!(poller.enrolled.is_enrolled("copilot-A").await);
        assert_eq!(poller.enrolled.len().await, 1);
    }

    #[tokio::test]
    async fn repeat_wake_does_not_duplicate() {
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));
        seed_bound_tasklist(&persistence, "team-a", "tl-1", "copilot-A", &[TaskStatus::Pending])
            .await;

        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus));
        let wake = AgentEventPayload::TasklistWoke {
            team_id: "team-a".to_string(),
            tasklist_id: "tl-1".to_string(),
            reason: "task_added".to_string(),
        };
        poller.handle_event(&wake).await;
        poller.handle_event(&wake).await;
        poller.handle_event(&wake).await;

        assert_eq!(poller.enrolled.len().await, 1);
    }

    #[tokio::test]
    async fn sleep_event_unenrolls_bound_copilot() {
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));
        seed_bound_tasklist(&persistence, "team-a", "tl-1", "copilot-A", &[TaskStatus::Completed])
            .await;

        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus));
        // Pre-enroll so the sleep handler has something to remove.
        poller.enrolled.enroll("copilot-A").await;

        poller
            .handle_event(&AgentEventPayload::TasklistSlept {
                team_id: "team-a".to_string(),
                tasklist_id: "tl-1".to_string(),
            })
            .await;

        assert!(!poller.enrolled.is_enrolled("copilot-A").await);
    }

    #[tokio::test]
    async fn wake_event_with_unbound_tasklist_is_a_noop() {
        // Wake fires before the co-pilot is bound (e.g. user adds a task
        // before ever opening the overlay). Nothing to enroll.
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));
        let tl = make_tasklist("team-a", "tl-1", &[TaskStatus::Pending]);
        persistence.tasklists.create(&tl).await.expect("create");

        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus));
        poller
            .handle_event(&AgentEventPayload::TasklistWoke {
                team_id: "team-a".to_string(),
                tasklist_id: "tl-1".to_string(),
                reason: "task_added".to_string(),
            })
            .await;

        assert_eq!(poller.enrolled.len().await, 0);
    }

    #[tokio::test]
    async fn unrelated_event_is_a_noop() {
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));
        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus));

        poller
            .handle_event(&AgentEventPayload::RunStarted)
            .await;

        assert_eq!(poller.enrolled.len().await, 0);
    }

    // ---- rebuild_from_active ----------------------------------------------

    #[tokio::test]
    async fn rebuild_seeds_only_active_bound_tasklists() {
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));

        // team-a / tl-1: bound + has Pending task → active.
        seed_bound_tasklist(&persistence, "team-a", "tl-1", "copilot-A", &[TaskStatus::Pending])
            .await;
        // team-b / tl-2: bound but all tasks Completed and never opened → dormant.
        seed_bound_tasklist(
            &persistence,
            "team-b",
            "tl-2",
            "copilot-B",
            &[TaskStatus::Completed],
        )
        .await;
        // team-c / tl-3: NOT bound, but active. Should not enroll anyone.
        let tl_c = make_tasklist("team-c", "tl-3", &[TaskStatus::Pending]);
        persistence.tasklists.create(&tl_c).await.expect("create tl-c");

        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus));
        let added = poller.rebuild_from_active().await.expect("rebuild");
        assert_eq!(added, 1, "exactly one bound + active tasklist");
        assert!(poller.enrolled.is_enrolled("copilot-A").await);
        assert!(!poller.enrolled.is_enrolled("copilot-B").await);
    }

    #[tokio::test]
    async fn rebuild_includes_recently_opened_terminal_tasklist() {
        // A tasklist whose tasks are all terminal but whose overlay was
        // pinged within the heartbeat window is still active — and
        // therefore must be enrolled on startup. This pins the requirement
        // that "is_tasklist_active" (not "TasklistStatus::Active") drives the
        // rebuild.
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));

        seed_bound_tasklist(
            &persistence,
            "team-a",
            "tl-1",
            "copilot-A",
            &[TaskStatus::Completed],
        )
        .await;
        // Stamp last_opened_at within the 24h window so the heartbeat path
        // keeps the tasklist active despite all tasks being terminal.
        persistence
            .tasklists
            .mutate("team-a", "tl-1", |tl| {
                tl.last_opened_at = Some(Utc::now() - ChronoDuration::hours(2));
                Ok(())
            })
            .await
            .expect("mutate");

        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus));
        poller.rebuild_from_active().await.expect("rebuild");
        assert!(poller.enrolled.is_enrolled("copilot-A").await);
    }

    #[tokio::test]
    async fn rebuild_on_empty_root_returns_zero() {
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));
        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus));
        let added = poller.rebuild_from_active().await.expect("rebuild");
        assert_eq!(added, 0);
        assert_eq!(poller.enrolled.len().await, 0);
    }

    // ---- end-to-end: spawn + bus events ----------------------------------

    #[tokio::test]
    async fn live_wake_then_sleep_via_event_bus() {
        // Wake / sleep events emitted on the live event bus drive the
        // background loop's enrollment exactly the same way as the synchronous
        // handle_event path.
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));
        seed_bound_tasklist(&persistence, "team-a", "tl-1", "copilot-A", &[TaskStatus::Pending])
            .await;

        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus))
            .with_poll_interval(Duration::from_secs(3600));
        let enrolled = poller.enrolled();
        let _shutdown = poller.run();

        // Wait for the spawned task to subscribe + run startup rebuild. The
        // tasklist has a Pending task so the rebuild should enroll copilot-A.
        wait_for(|| async { enrolled.is_enrolled("copilot-A").await }).await;

        // Externally-emitted sleep event should evict the agent.
        emit_sleep(&bus, "team-a", "tl-1").await;
        wait_for(|| async { !enrolled.is_enrolled("copilot-A").await }).await;

        // Externally-emitted wake event should re-enroll.
        emit_wake(&bus, "team-a", "tl-1", WakeReason::TaskAdded).await;
        wait_for(|| async { enrolled.is_enrolled("copilot-A").await }).await;
    }

    #[tokio::test]
    async fn dormant_tasklist_is_not_enrolled_on_startup() {
        // "Dormant tasklist → co-pilot not polled" — concretely, a
        // tasklist whose tasks are all terminal and which has never been
        // opened must not appear in the enrolled set after the startup
        // rebuild.
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));
        seed_bound_tasklist(
            &persistence,
            "team-a",
            "tl-1",
            "copilot-A",
            &[TaskStatus::Completed],
        )
        .await;

        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus))
            .with_poll_interval(Duration::from_secs(3600));
        let enrolled = poller.enrolled();
        let _shutdown = poller.run();

        // Give the spawned task a chance to run startup rebuild.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!enrolled.is_enrolled("copilot-A").await);
    }

    #[tokio::test]
    async fn periodic_sweep_evicts_sleep_eligible_tasklist() {
        // A tasklist that was active at enrollment time but has
        // since transitioned to sleep-eligible (all terminal + grace elapsed
        // + overlay closed) is dropped by the periodic sweep without any
        // external wake/sleep emitter.
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));

        // Seed an "active" tasklist so the startup rebuild enrolls the copilot.
        seed_bound_tasklist(&persistence, "team-a", "tl-1", "copilot-A", &[TaskStatus::Pending])
            .await;

        // Use a very short sweep interval so the test doesn't have to wait
        // 60s; pin the production value via the dedicated POLL_INTERVAL test.
        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus))
            .with_poll_interval(Duration::from_millis(50));
        let enrolled = poller.enrolled();
        let _shutdown = poller.run();

        wait_for(|| async { enrolled.is_enrolled("copilot-A").await }).await;

        // Mutate the tasklist into a sleep-eligible state: all tasks terminal,
        // last_active_at well past the grace window, no recent overlay open.
        persistence
            .tasklists
            .mutate("team-a", "tl-1", |tl| {
                for g in &mut tl.groups {
                    for t in &mut g.tasks {
                        t.status = TaskStatus::Completed;
                    }
                }
                tl.last_active_at = Some(
                    Utc::now() - ChronoDuration::minutes(SLEEP_GRACE_WINDOW_MINUTES + 1),
                );
                tl.last_opened_at = None;
                Ok(())
            })
            .await
            .expect("mutate");

        // Periodic sweep should fire within a few ticks and evict copilot-A.
        wait_for(|| async { !enrolled.is_enrolled("copilot-A").await }).await;
    }

    /// An agent-owned (project) co-pilot must still be evicted when its
    /// tasklist goes to sleep.
    ///
    /// This path only became reachable once `find_by_copilot_agent_id` learned
    /// to walk the agent tree — before that the sweep never resolved such a
    /// tasklist and unenrolled the co-pilot as an orphan instead. It cannot go
    /// through `TasklistSlept`, which carries only a team id, so the sweep
    /// evicts agent-owned bindings inline; this test pins that.
    #[tokio::test]
    async fn sweep_evicts_a_sleeping_agent_owned_copilot() {
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));

        // All tasks terminal and no activity timestamps => should_sleep fires.
        let mut tl = make_tasklist("unused", "tl-agent", &[TaskStatus::Completed]);
        tl.owner = ao_protocol::tasklist::TasklistOwner::Agent {
            agent_id: "proj-owner".to_string(),
        };
        tl.team_id = None;
        tl.copilot_agent_id = Some("copilot-P".to_string());
        persistence
            .tasklists
            .create_for_agent(&tl)
            .await
            .expect("create agent-owned tasklist");

        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus));
        let enrolled = poller.enrolled();
        enrolled.enroll("copilot-P").await;

        poller.sweep_for_sleep().await.expect("sweep");

        assert!(
            !enrolled.is_enrolled("copilot-P").await,
            "a sleeping agent-owned co-pilot must be evicted by the sweep",
        );
    }

    /// The same sweep must NOT evict an agent-owned co-pilot whose tasklist
    /// still has outstanding work — otherwise the inline eviction above would
    /// just be the old orphan-drop bug wearing a different hat.
    #[tokio::test]
    async fn sweep_keeps_a_busy_agent_owned_copilot_enrolled() {
        let (persistence, _tmp) = make_persistence().await;
        let bus = Arc::new(EventBus::new(64));

        let mut tl = make_tasklist("unused", "tl-agent-busy", &[TaskStatus::Pending]);
        tl.owner = ao_protocol::tasklist::TasklistOwner::Agent {
            agent_id: "proj-owner".to_string(),
        };
        tl.team_id = None;
        tl.copilot_agent_id = Some("copilot-Q".to_string());
        persistence
            .tasklists
            .create_for_agent(&tl)
            .await
            .expect("create agent-owned tasklist");

        let poller = CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&bus));
        let enrolled = poller.enrolled();
        enrolled.enroll("copilot-Q").await;

        poller.sweep_for_sleep().await.expect("sweep");

        assert!(
            enrolled.is_enrolled("copilot-Q").await,
            "a co-pilot with a Pending task must stay enrolled",
        );
    }

    /// Spin briefly until `cond()` is true (or panic after 2s). Used to wait
    /// for the spawned task to observe a state change without polluting tests
    /// with hardcoded sleep durations that have to chase tokio scheduler jitter.
    async fn wait_for<F, Fut>(mut cond: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if cond().await {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("wait_for timed out");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
