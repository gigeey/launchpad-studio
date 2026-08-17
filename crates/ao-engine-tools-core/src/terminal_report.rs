use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Outcome returned by `TasklistServiceHandle::cancel_for_agent`.
#[derive(Debug, Clone)]
pub struct CancelOutcome {
    pub tasklist_id: String,
    pub skipped_count: usize,
    pub in_flight_count: usize,
}

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use ao_protocol::error::AoError;

/// Aggregate outcome of a tasklist run, sent to the sync TodoCreate caller
/// when the tasklist reaches a terminal state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalReport {
    pub status: String,
    pub counts: TerminalCounts,
    pub tasks: Vec<TerminalTaskEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalCounts {
    pub succeeded: usize,
    pub failed: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalTaskEntry {
    pub id: String,
    pub title: String,
    pub status: String,
    /// One-line summary the producing subagent emitted in its
    /// `<task-item-notification>` block, sourced from the tasklist changelog.
    /// `None` when the task produced no parsed notification (e.g. skipped, or
    /// a parse failure that exhausted its retries before synthesizing one).
    pub summary: Option<String>,
    /// Optional longer detail from the same notification block. Carries the
    /// subagent's fuller explanation when it provided one; `None` otherwise.
    pub details: Option<String>,
    pub output_path: PathBuf,
    pub attempt_count: u32,
}

/// Shared registry keyed by tasklist_id. TaskFeeder inserts senders here;
/// TerminalWatcherGuard holds the receiver and removes the entry on drop.
pub type TerminalWatcherRegistry =
    Arc<Mutex<HashMap<String, oneshot::Sender<TerminalReport>>>>;

/// Returned by `TasklistServiceHandle::terminal_watcher`. Holds the receiving
/// end of a oneshot channel that fires exactly once when the tasklist reaches
/// Completed, Failed, or Cancelled. Dropping the guard before `wait()` returns
/// removes the sender from the registry so the feeder skips the send cleanly.
pub struct TerminalWatcherGuard {
    receiver: Option<oneshot::Receiver<TerminalReport>>,
    registry: TerminalWatcherRegistry,
    tasklist_id: String,
}

impl TerminalWatcherGuard {
    pub fn new(
        receiver: oneshot::Receiver<TerminalReport>,
        registry: TerminalWatcherRegistry,
        tasklist_id: String,
    ) -> Self {
        Self {
            receiver: Some(receiver),
            registry,
            tasklist_id,
        }
    }

    /// Await the terminal report. Returns `Err` if the sender was dropped
    /// before firing (e.g., tasklist deleted out from under the run).
    pub async fn wait(mut self) -> Result<TerminalReport, AoError> {
        let rx = self.receiver.take().expect("TerminalWatcherGuard::wait called twice");
        rx.await.map_err(|_| {
            AoError::Internal(
                "terminal watcher: tasklist sender dropped before completion".into(),
            )
        })
    }
}

impl Drop for TerminalWatcherGuard {
    fn drop(&mut self) {
        // Non-blocking removal. If try_lock fails under rare contention the
        // stale entry is cleaned up lazily when fire_terminal_watcher sees
        // the sender is closed.
        if let Ok(mut map) = self.registry.try_lock() {
            map.remove(&self.tasklist_id);
        }
    }
}
