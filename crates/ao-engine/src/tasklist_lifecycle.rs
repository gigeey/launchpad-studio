//! Tasklist lifecycle state machine.
//!
//! Single source of truth for "is this tasklist currently active?" and for
//! the wake/sleep transition events that downstream consumers (the mailbox
//! poller, the wake-on-deliver path) react to.
//!
//! The machine is intentionally pure-data: every predicate takes a `&Tasklist`
//! plus an explicit `now: DateTime<Utc>` so tests can drive transitions
//! deterministically without monkey-patching the clock. Event emission is a
//! thin wrapper over `EventBus` so call sites don't have to know the synthetic
//! `agent_id` / `run_id` convention every tasklist event uses.
//!
//! Active definition:
//!   - any task in a non-terminal state (Pending, InProgress, Blocked), OR
//!   - `last_opened_at` within `ACTIVE_HEARTBEAT_WINDOW_HOURS` of `now`.
//!
//! Sleep definition:
//!   - every task is in a terminal state (Completed, Failed, Skipped), AND
//!   - the overlay is not currently open (no `last_opened_at` ping within
//!     `OVERLAY_OPEN_KEEPALIVE_SECS`), AND
//!   - the most recent activity timestamp (max of `last_active_at` and
//!     `last_opened_at`) is at least `SLEEP_GRACE_WINDOW_MINUTES` old.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use ao_persistence::PersistenceLayer;
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::tasklist::Tasklist;

use crate::event_bus::EventBus;

/// Window during which a tasklist counts as "active" purely on the strength of
/// a recent overlay open, even when every task is terminal. PRD-default 24h —
/// long enough that a user who closes a finished tasklist and comes back the
/// next day still sees an active co-pilot, short enough that abandoned
/// tasklists eventually fall out of the enrolled set.
pub const ACTIVE_HEARTBEAT_WINDOW_HOURS: i64 = 24;

/// Window during which `last_opened_at` is treated as "overlay still open".
/// The FE keepalive cadence is expected to be well below this — a missed ping
/// trips sleep eligibility. Picked at 60s so a user who closes the overlay
/// stops counting as "open" within one minute.
pub const OVERLAY_OPEN_KEEPALIVE_SECS: i64 = 60;

/// Grace window between the last activity (`last_active_at` or
/// `last_opened_at`, whichever is more recent) and the sleep transition.
/// PRD-default 5 minutes — keeps a freshly-completed tasklist enrolled long
/// enough to receive any in-flight `<task-item-notification>` reminders before
/// the poller drops it.
pub const SLEEP_GRACE_WINDOW_MINUTES: i64 = 5;

/// Why the tasklist is being woken. Carried verbatim into the
/// `TasklistWoke.reason` field on the event bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeReason {
    /// A new task was appended to the tasklist (via inline composer or agent).
    TaskAdded,
    /// A previously-terminal task transitioned back to a non-terminal state
    /// (Continue / Skip-failed recovery paths).
    TaskRevived,
    /// The FE pinged `GET /tasklists/{id}/copilot`, recording an overlay open.
    OverlayOpened,
}

impl WakeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            WakeReason::TaskAdded => "task_added",
            WakeReason::TaskRevived => "task_revived",
            WakeReason::OverlayOpened => "overlay_opened",
        }
    }
}

/// Are any tasks in a non-terminal state?
pub fn has_non_terminal_task(tasklist: &Tasklist) -> bool {
    tasklist
        .groups
        .iter()
        .flat_map(|g| g.tasks.iter())
        .any(|t| !t.status.is_terminal())
}

/// A tasklist is active iff (a) at least one task is non-terminal OR
/// (b) `last_opened_at` is within the heartbeat window of `now`.
pub fn is_tasklist_active(tasklist: &Tasklist, now: DateTime<Utc>) -> bool {
    if has_non_terminal_task(tasklist) {
        return true;
    }
    if let Some(opened) = tasklist.last_opened_at {
        if now.signed_duration_since(opened) < Duration::hours(ACTIVE_HEARTBEAT_WINDOW_HOURS) {
            return true;
        }
    }
    false
}

/// Is the overlay currently open? Proxied by a recent `last_opened_at`. Used
/// only by `should_sleep` — `is_tasklist_active` deliberately uses a much
/// wider 24h window because "active" includes "user looked at this recently".
pub fn is_overlay_open(tasklist: &Tasklist, now: DateTime<Utc>) -> bool {
    match tasklist.last_opened_at {
        Some(t) => now.signed_duration_since(t) < Duration::seconds(OVERLAY_OPEN_KEEPALIVE_SECS),
        None => false,
    }
}

/// Should the tasklist transition to sleep right now? Three conditions:
///   1. every task is terminal (no work outstanding),
///   2. the overlay is not currently open,
///   3. the grace window has elapsed since the last activity.
pub fn should_sleep(tasklist: &Tasklist, now: DateTime<Utc>) -> bool {
    if has_non_terminal_task(tasklist) {
        return false;
    }
    if is_overlay_open(tasklist, now) {
        return false;
    }
    let last_activity = match (tasklist.last_active_at, tasklist.last_opened_at) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    match last_activity {
        Some(t) => now.signed_duration_since(t) >= Duration::minutes(SLEEP_GRACE_WINDOW_MINUTES),
        // Never had any activity — there's nothing to grace; safe to sleep.
        None => true,
    }
}

fn synthetic_run_id(tasklist_id: &str) -> String {
    format!("tasklist:{}", tasklist_id)
}

fn synthetic_agent_id(team_id: &str) -> String {
    format!("team:{}", team_id)
}

/// Emit a `TasklistWoke` event with the supplied reason. Idempotent at the
/// caller layer — the lifecycle module does not deduplicate; consumers that
/// care about uniqueness should track their own enrolled set.
pub async fn emit_wake(
    event_bus: &EventBus,
    team_id: &str,
    tasklist_id: &str,
    reason: WakeReason,
) {
    let agent_id = synthetic_agent_id(team_id);
    let run_id = synthetic_run_id(tasklist_id);
    event_bus
        .emit(
            &run_id,
            &agent_id,
            None,
            AgentEventPayload::TasklistWoke {
                team_id: team_id.to_string(),
                tasklist_id: tasklist_id.to_string(),
                reason: reason.as_str().to_string(),
            },
        )
        .await;
}

/// Emit a `TasklistSlept` event. See [`maybe_emit_sleep`] for the gated form.
pub async fn emit_sleep(event_bus: &EventBus, team_id: &str, tasklist_id: &str) {
    let agent_id = synthetic_agent_id(team_id);
    let run_id = synthetic_run_id(tasklist_id);
    event_bus
        .emit(
            &run_id,
            &agent_id,
            None,
            AgentEventPayload::TasklistSlept {
                team_id: team_id.to_string(),
                tasklist_id: tasklist_id.to_string(),
            },
        )
        .await;
}

/// Check `should_sleep` against `now` and emit `TasklistSlept` only if the
/// predicate holds. Returns `true` if an event was emitted, `false` otherwise
/// — useful for diagnostic logging at call sites without re-running the check.
pub async fn maybe_emit_sleep(
    event_bus: &EventBus,
    tasklist: &Tasklist,
    now: DateTime<Utc>,
) -> bool {
    if should_sleep(tasklist, now) {
        emit_sleep(event_bus, tasklist.team_id.as_deref().unwrap_or_default(), &tasklist.id).await;
        true
    } else {
        false
    }
}

/// Record an overlay-open ping: stamps `last_opened_at = now()` on the
/// persisted tasklist and emits a `TasklistWoke { reason: overlay_opened }`
/// event. Called by the `GET /tasklists/{id}/copilot` route handler
/// every time the FE binds the overlay's chat thread.
pub async fn record_overlay_open(
    persistence: &Arc<PersistenceLayer>,
    event_bus: &EventBus,
    team_id: &str,
    tasklist_id: &str,
) -> Result<(), AoError> {
    persistence
        .tasklists
        .mutate(team_id, tasklist_id, |tl| {
            tl.last_opened_at = Some(Utc::now());
            Ok(())
        })
        .await?;
    emit_wake(event_bus, team_id, tasklist_id, WakeReason::OverlayOpened).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::tasklist::{
        Task, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistStatus,
    };

    fn task_with_status(id: &str, status: TaskStatus) -> Task {
        Task {
            id: id.to_string(),
            owner_agent_id: "agent-a".to_string(),
            prompt: "do work".to_string(),
            expected_outputs: vec![],
            status,
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
        }
    }

    fn make_tasklist(task_statuses: &[TaskStatus]) -> Tasklist {
        use ao_protocol::tasklist::TasklistOwner;
        let tasks: Vec<Task> = task_statuses
            .iter()
            .enumerate()
            .map(|(i, s)| task_with_status(&format!("t{}", i), *s))
            .collect();
        Tasklist {
            id: "tl-1".to_string(),
            owner: TasklistOwner::Team { team_id: "team-a".to_string() },
            team_id: Some("team-a".to_string()),
            title: "Sample".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: "g1".to_string(),
                mode: TaskGroupMode::Seq,
                tasks,
            }],
            workspace_dir: "/tmp/ws".to_string(),
            transcripts_dir: "/tmp/tx".to_string(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            }
    }

    // ---- is_tasklist_active ----------------------------------------------

    #[test]
    fn active_when_any_task_is_non_terminal() {
        let now = Utc::now();
        let tl = make_tasklist(&[TaskStatus::Completed, TaskStatus::Pending]);
        assert!(is_tasklist_active(&tl, now));
    }

    #[test]
    fn active_when_in_progress_task_present() {
        let now = Utc::now();
        let tl = make_tasklist(&[TaskStatus::InProgress, TaskStatus::Skipped]);
        assert!(is_tasklist_active(&tl, now));
    }

    #[test]
    fn active_when_blocked_task_present() {
        let now = Utc::now();
        let tl = make_tasklist(&[TaskStatus::Blocked]);
        assert!(is_tasklist_active(&tl, now));
    }

    #[test]
    fn active_when_recently_opened_within_heartbeat_window() {
        let now = Utc::now();
        let mut tl = make_tasklist(&[TaskStatus::Completed]);
        tl.last_opened_at = Some(now - Duration::hours(ACTIVE_HEARTBEAT_WINDOW_HOURS - 1));
        assert!(is_tasklist_active(&tl, now));
    }

    #[test]
    fn inactive_when_all_terminal_and_no_recent_open() {
        let now = Utc::now();
        let tl = make_tasklist(&[TaskStatus::Completed, TaskStatus::Failed]);
        assert!(!is_tasklist_active(&tl, now));
    }

    #[test]
    fn inactive_when_all_terminal_and_open_outside_heartbeat_window() {
        let now = Utc::now();
        let mut tl = make_tasklist(&[TaskStatus::Completed]);
        tl.last_opened_at = Some(now - Duration::hours(ACTIVE_HEARTBEAT_WINDOW_HOURS + 1));
        assert!(!is_tasklist_active(&tl, now));
    }

    // ---- is_overlay_open --------------------------------------------------

    #[test]
    fn overlay_open_within_keepalive_window() {
        let now = Utc::now();
        let mut tl = make_tasklist(&[TaskStatus::Completed]);
        tl.last_opened_at = Some(now - Duration::seconds(OVERLAY_OPEN_KEEPALIVE_SECS - 1));
        assert!(is_overlay_open(&tl, now));
    }

    #[test]
    fn overlay_closed_outside_keepalive_window() {
        let now = Utc::now();
        let mut tl = make_tasklist(&[TaskStatus::Completed]);
        tl.last_opened_at = Some(now - Duration::seconds(OVERLAY_OPEN_KEEPALIVE_SECS + 1));
        assert!(!is_overlay_open(&tl, now));
    }

    #[test]
    fn overlay_closed_when_never_opened() {
        let now = Utc::now();
        let tl = make_tasklist(&[TaskStatus::Completed]);
        assert!(!is_overlay_open(&tl, now));
    }

    // ---- should_sleep -----------------------------------------------------

    #[test]
    fn should_not_sleep_when_any_task_non_terminal() {
        let now = Utc::now();
        let mut tl = make_tasklist(&[TaskStatus::Completed, TaskStatus::Pending]);
        tl.last_active_at = Some(now - Duration::hours(1));
        assert!(!should_sleep(&tl, now));
    }

    #[test]
    fn should_not_sleep_when_overlay_open() {
        let now = Utc::now();
        let mut tl = make_tasklist(&[TaskStatus::Completed]);
        // Overlay just pinged — within keepalive.
        tl.last_opened_at = Some(now - Duration::seconds(5));
        // And activity is well past the grace window so the only thing
        // blocking sleep is the open-overlay condition.
        tl.last_active_at = Some(now - Duration::hours(1));
        assert!(!should_sleep(&tl, now));
    }

    #[test]
    fn should_not_sleep_when_grace_window_not_elapsed() {
        let now = Utc::now();
        let mut tl = make_tasklist(&[TaskStatus::Completed]);
        // Overlay closed but activity is fresh.
        tl.last_active_at = Some(now - Duration::minutes(SLEEP_GRACE_WINDOW_MINUTES - 1));
        assert!(!should_sleep(&tl, now));
    }

    #[test]
    fn should_sleep_when_all_terminal_overlay_closed_and_grace_elapsed() {
        let now = Utc::now();
        let mut tl = make_tasklist(&[TaskStatus::Completed, TaskStatus::Failed]);
        tl.last_active_at = Some(now - Duration::minutes(SLEEP_GRACE_WINDOW_MINUTES + 1));
        // No overlay open at all.
        assert!(should_sleep(&tl, now));
    }

    #[test]
    fn should_sleep_uses_max_of_active_and_opened_for_grace_check() {
        let now = Utc::now();
        let mut tl = make_tasklist(&[TaskStatus::Completed]);
        // last_active_at is past the grace window…
        tl.last_active_at = Some(now - Duration::hours(1));
        // …but last_opened_at is recent (overlay closed but well within grace).
        tl.last_opened_at = Some(now - Duration::minutes(SLEEP_GRACE_WINDOW_MINUTES - 1));
        assert!(!should_sleep(&tl, now));
    }

    #[test]
    fn should_sleep_when_no_activity_recorded() {
        let now = Utc::now();
        let tl = make_tasklist(&[TaskStatus::Completed]);
        // Neither timestamp set. A fully-terminal tasklist with no activity
        // is sleep-eligible by construction — there's no in-flight work to
        // grace, and no overlay session to wait on.
        assert!(should_sleep(&tl, now));
    }

    // ---- WakeReason::as_str -----------------------------------------------

    #[test]
    fn wake_reason_strings_are_stable() {
        assert_eq!(WakeReason::TaskAdded.as_str(), "task_added");
        assert_eq!(WakeReason::TaskRevived.as_str(), "task_revived");
        assert_eq!(WakeReason::OverlayOpened.as_str(), "overlay_opened");
    }

    // ---- emit_wake / emit_sleep / maybe_emit_sleep ------------------------

    #[tokio::test]
    async fn emit_wake_publishes_payload_with_reason_string() {
        let bus = EventBus::new(64);
        let mut rx = bus.subscribe();

        emit_wake(&bus, "team-a", "tl-1", WakeReason::TaskAdded).await;

        let event = rx.recv().await.expect("receive wake");
        match event.payload {
            AgentEventPayload::TasklistWoke {
                team_id,
                tasklist_id,
                reason,
            } => {
                assert_eq!(team_id, "team-a");
                assert_eq!(tasklist_id, "tl-1");
                assert_eq!(reason, "task_added");
            }
            other => panic!("expected TasklistWoke, got {:?}", other),
        }
        assert_eq!(event.agent_id, "team:team-a");
        assert_eq!(event.run_id, "tasklist:tl-1");
    }

    #[tokio::test]
    async fn emit_wake_carries_each_reason_variant() {
        let bus = EventBus::new(64);
        let mut rx = bus.subscribe();

        emit_wake(&bus, "team-a", "tl-1", WakeReason::TaskRevived).await;
        emit_wake(&bus, "team-a", "tl-1", WakeReason::OverlayOpened).await;

        for expected in ["task_revived", "overlay_opened"] {
            let event = rx.recv().await.expect("receive wake");
            match event.payload {
                AgentEventPayload::TasklistWoke { reason, .. } => assert_eq!(reason, expected),
                other => panic!("expected TasklistWoke, got {:?}", other),
            }
        }
    }

    #[tokio::test]
    async fn emit_sleep_publishes_payload() {
        let bus = EventBus::new(64);
        let mut rx = bus.subscribe();

        emit_sleep(&bus, "team-a", "tl-1").await;

        let event = rx.recv().await.expect("receive sleep");
        match event.payload {
            AgentEventPayload::TasklistSlept {
                team_id,
                tasklist_id,
            } => {
                assert_eq!(team_id, "team-a");
                assert_eq!(tasklist_id, "tl-1");
            }
            other => panic!("expected TasklistSlept, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn maybe_emit_sleep_emits_when_eligible() {
        let bus = EventBus::new(64);
        let mut rx = bus.subscribe();

        let now = Utc::now();
        let mut tl = make_tasklist(&[TaskStatus::Completed]);
        tl.last_active_at = Some(now - Duration::minutes(SLEEP_GRACE_WINDOW_MINUTES + 1));

        let emitted = maybe_emit_sleep(&bus, &tl, now).await;
        assert!(emitted);

        let event = rx.recv().await.expect("receive sleep");
        assert!(matches!(event.payload, AgentEventPayload::TasklistSlept { .. }));
    }

    #[tokio::test]
    async fn maybe_emit_sleep_no_op_when_not_eligible() {
        let bus = EventBus::new(64);
        let mut rx = bus.subscribe();

        let now = Utc::now();
        // Has a non-terminal task → not eligible.
        let tl = make_tasklist(&[TaskStatus::Pending]);

        let emitted = maybe_emit_sleep(&bus, &tl, now).await;
        assert!(!emitted);

        // No event waiting on the channel.
        assert!(rx.try_recv().is_err());
    }

    // ---- record_overlay_open ----------------------------------------------

    async fn make_persistence_with_tasklist() -> (Arc<PersistenceLayer>, tempfile::TempDir, Tasklist) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let persistence = Arc::new(
            PersistenceLayer::init_with_root(ao_persistence::paths::DataRoot::new(tmp.path()))
                .await
                .expect("persistence init"),
        );

        use ao_protocol::tasklist::TasklistOwner;
        let tasklist = Tasklist {
            id: "tl-1".to_string(),
            owner: TasklistOwner::Team { team_id: "team-a".to_string() },
            team_id: Some("team-a".to_string()),
            title: "Sample".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![],
            workspace_dir: persistence
                .data_root
                .tasklist_workspace_dir("team-a", "tl-1")
                .to_string_lossy()
                .to_string(),
            transcripts_dir: persistence
                .data_root
                .tasklist_transcripts_dir("team-a", "tl-1")
                .to_string_lossy()
                .to_string(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        persistence
            .tasklists
            .create(&tasklist)
            .await
            .expect("create tasklist");
        (persistence, tmp, tasklist)
    }

    #[tokio::test]
    async fn record_overlay_open_stamps_last_opened_at_and_emits_wake() {
        let (persistence, _tmp, tasklist) = make_persistence_with_tasklist().await;
        let bus = EventBus::new(64);
        let mut rx = bus.subscribe();

        let before = Utc::now();
        let tl_team_id = tasklist.team_id.as_deref().unwrap_or_default();
        record_overlay_open(&persistence, &bus, tl_team_id, &tasklist.id)
            .await
            .expect("record overlay open");
        let after = Utc::now();

        // last_opened_at persisted within the call window.
        let reloaded = persistence
            .tasklists
            .get(tl_team_id, &tasklist.id)
            .await
            .expect("reload")
            .expect("tasklist still exists");
        let stamped = reloaded.last_opened_at.expect("last_opened_at set");
        assert!(stamped >= before && stamped <= after);

        // Wake event with reason = overlay_opened was emitted.
        let event = rx.recv().await.expect("receive wake");
        match event.payload {
            AgentEventPayload::TasklistWoke {
                team_id,
                tasklist_id,
                reason,
            } => {
                assert_eq!(team_id, tasklist.team_id.as_deref().unwrap_or_default());
                assert_eq!(tasklist_id, tasklist.id);
                assert_eq!(reason, "overlay_opened");
            }
            other => panic!("expected TasklistWoke, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn record_overlay_open_returns_error_for_missing_tasklist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let persistence = Arc::new(
            PersistenceLayer::init_with_root(ao_persistence::paths::DataRoot::new(tmp.path()))
                .await
                .expect("persistence init"),
        );
        let bus = EventBus::new(64);

        let err = record_overlay_open(&persistence, &bus, "team-x", "missing")
            .await
            .expect_err("missing tasklist should error");
        match err {
            AoError::TasklistNotFound(id) => assert_eq!(id, "missing"),
            other => panic!("expected TasklistNotFound, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn recently_opened_tasklist_with_all_terminal_tasks_is_active() {
        // Integration of is_tasklist_active + the heartbeat path: a tasklist
        // whose only open signal is `last_opened_at` is still active for the
        // duration of the heartbeat window.
        let now = Utc::now();
        let mut tl = make_tasklist(&[TaskStatus::Completed, TaskStatus::Failed]);
        tl.last_opened_at = Some(now - Duration::minutes(30));
        assert!(is_tasklist_active(&tl, now));
    }
}
