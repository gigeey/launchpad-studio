use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::info;

use ao_persistence::PersistenceLayer;
use ao_protocol::background_activity::background_activity_count;

use crate::instance_registry::InstanceRegistry;
use crate::sleep_guard::SleepGuard;

/// Polling interval for refreshing the agent-runner sleep guard.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Background task that holds a system sleep guard whenever any agent run is
/// active in the [`InstanceRegistry`]. Mirrors the workflow queue manager's
/// guard but for the agent runner side, so a long-running agent reply doesn't
/// get interrupted by display sleep.
pub struct AgentSleepGuardRunner {
    persistence: Arc<PersistenceLayer>,
    instance_registry: Arc<InstanceRegistry>,
    sleep_guard: SleepGuard,
}

impl AgentSleepGuardRunner {
    pub fn new(
        persistence: Arc<PersistenceLayer>,
        instance_registry: Arc<InstanceRegistry>,
    ) -> Self {
        Self {
            persistence,
            instance_registry,
            sleep_guard: SleepGuard::new(1.0),
        }
    }

    /// Spawn the runner as a background tokio task. Returns a shutdown sender;
    /// drop it (or send `()`) to stop the loop and release the guard.
    pub fn run(self) -> watch::Sender<()> {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(());
        info!("AgentSleepGuardRunner starting");

        tokio::spawn(async move {
            let mut runner = self;
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        info!("AgentSleepGuardRunner shutting down");
                        runner.sleep_guard.update_active(false);
                        break;
                    }
                    _ = tokio::time::sleep(POLL_INTERVAL) => {
                        runner.tick().await;
                    }
                }
            }
        });

        shutdown_tx
    }

    async fn tick(&mut self) {
        let prefs = self.persistence.preferences.get().await.ok().flatten();

        let enabled = prefs
            .as_ref()
            .map(|prefs| prefs.prevent_sleep_during_agent_runs)
            .unwrap_or(true);
        self.sleep_guard.set_disabled(!enabled);

        let keep_display_awake = prefs.map(|prefs| prefs.keep_display_awake).unwrap_or(false);
        self.sleep_guard.set_keep_display_awake(keep_display_awake);

        let active = self.should_hold().await;
        self.sleep_guard.update_active(active);
    }

    /// Whether the guard should currently be held. True while a run is
    /// registered, but also for the whole lifetime of a tasklist — including
    /// the gap between one dispatched task completing and the next one being
    /// registered, where `is_any_active` alone would read "inactive" and let
    /// the display sleep mid-tasklist. Also true while any background/delegate
    /// subagent is in flight — that work runs in a per-parent
    /// `BackgroundAgentRegistry` invisible to `InstanceRegistry`, so it's
    /// tracked via the process-global `background_activity_count` instead.
    async fn should_hold(&self) -> bool {
        self.instance_registry.is_any_active().await
            || self.instance_registry.has_any_active_tasklist().await
            || background_activity_count() > 0
    }
}

#[cfg(test)]
mod tests {
    use ao_persistence::paths::DataRoot;
    use ao_protocol::agent::AgentId;
    use tempfile::TempDir;

    use super::*;

    /// `background_activity_count` reads a process-global counter, and the test
    /// binary runs its tests concurrently on separate threads. Any test that
    /// asserts on `should_hold` therefore observes every other test's background
    /// activity, so all of them must serialize here — not just the ones that
    /// take a guard. Without this lock,
    /// `should_hold_stays_true_across_inter_task_tasklist_gap` fails about a
    /// quarter of runs: it never touches the counter itself, but it asserts the
    /// guard is released at a moment when
    /// `should_hold_is_true_while_background_activity_is_in_flight` may be
    /// holding one.
    ///
    /// Async-aware rather than `std::sync::Mutex` because it is held across
    /// `.await` points. This mirrors the `TEST_LOCK` in `ao-protocol`'s
    /// `background_activity` tests, which serializes the same static for the
    /// same reason.
    static COUNTER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn make_runner() -> (TempDir, AgentSleepGuardRunner) {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = Arc::new(
            PersistenceLayer::init_with_root(DataRoot::new(tmp.path()))
                .await
                .expect("persistence init"),
        );
        let instance_registry = Arc::new(InstanceRegistry::new());
        let runner = AgentSleepGuardRunner::new(persistence, instance_registry);
        (tmp, runner)
    }

    /// The inter-task gap: no run is registered (a task just completed and
    /// the next hasn't been dispatched yet), but the tasklist itself is still
    /// marked active. Without `has_any_active_tasklist`, `should_hold` would
    /// read false here and let the display sleep mid-tasklist.
    #[tokio::test]
    async fn should_hold_stays_true_across_inter_task_tasklist_gap() {
        let _serial = COUNTER_LOCK.lock().await;
        let (_tmp, runner) = make_runner().await;
        let agent: AgentId = "agent-tasklist-gap".to_string();

        assert!(!runner.instance_registry.is_any_active().await);
        assert!(!runner.should_hold().await);

        runner.instance_registry.mark_has_active_tasklist(&agent).await;

        assert!(
            !runner.instance_registry.is_any_active().await,
            "no run is registered during the inter-task gap"
        );
        assert!(
            runner.should_hold().await,
            "guard must stay held across the inter-task tasklist gap"
        );

        runner.instance_registry.clear_has_active_tasklist(&agent).await;

        assert!(
            !runner.should_hold().await,
            "guard must release once the tasklist is cleared and no run is registered"
        );
    }

    /// Background/delegate subagents live in a per-parent
    /// `BackgroundAgentRegistry` that `InstanceRegistry` can't see, so they are
    /// tracked through the process-global background-activity counter instead.
    /// Holds [`COUNTER_LOCK`] because the guard it takes is visible to every
    /// concurrently-running test in this binary.
    #[tokio::test]
    async fn should_hold_is_true_while_background_activity_is_in_flight() {
        let _serial = COUNTER_LOCK.lock().await;
        let (_tmp, runner) = make_runner().await;

        assert!(!runner.instance_registry.is_any_active().await);
        assert!(!runner.instance_registry.has_any_active_tasklist().await);
        assert!(!runner.should_hold().await);

        let guard = ao_protocol::background_activity::background_activity_guard();
        assert!(
            runner.should_hold().await,
            "guard must hold while a background/delegate agent is in flight, \
             even with no active instances or tasklists"
        );

        drop(guard);
        assert!(
            !runner.should_hold().await,
            "guard must release once background activity returns to zero"
        );
    }
}
