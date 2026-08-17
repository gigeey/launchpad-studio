use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::task_feeder::TaskFeeder;

/// How often the watchdog scans every Active tasklist for stuck `InProgress`
/// tasks. Each tick is a few small disk reads plus one in-memory map lookup
/// per InProgress task — cheap.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Background sweeper that asks the [`TaskFeeder`] to recover stuck
/// `InProgress` tasks. A "stuck" task is one whose owning agent has zero
/// active runs in the [`crate::instance_registry::InstanceRegistry`] but the
/// task itself is still marked InProgress on disk — typically because the
/// run's `RunEnded` event was lost (silent drop, pause-then-resume, server
/// restart) or the agent process crashed mid-run.
pub struct DispatchWatchdogRunner {
    task_feeder: Arc<TaskFeeder>,
}

impl DispatchWatchdogRunner {
    pub fn new(task_feeder: Arc<TaskFeeder>) -> Self {
        Self { task_feeder }
    }

    /// Spawn the runner as a background tokio task. Returns a shutdown sender;
    /// drop it (or send `()`) to stop the loop.
    pub fn run(self) -> watch::Sender<()> {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(());
        info!("DispatchWatchdogRunner starting (poll {:?})", POLL_INTERVAL);

        tokio::spawn(async move {
            let runner = self;
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        info!("DispatchWatchdogRunner shutting down");
                        break;
                    }
                    _ = tokio::time::sleep(POLL_INTERVAL) => {
                        match runner.task_feeder.watchdog_tick().await {
                            Ok(0) => {}
                            Ok(n) => info!(recovered = n, "watchdog tick recovered tasks"),
                            Err(e) => warn!("watchdog tick errored: {e}"),
                        }
                    }
                }
            }
        });

        shutdown_tx
    }
}
