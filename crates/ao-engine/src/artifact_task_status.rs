//! In-memory status tracking for background subagent runs spawned by
//! [`crate::artifact_regen::spawn_artifact_agent`], plus the completion sink
//! that populates it and durably appends the run's reply to the artifact's
//! chat transcript.
//!
//! Both jobs fire off the same terminal event on the same throwaway
//! [`ao_engine_tools_core::context::RunnerContext`], so they share one
//! [`DelegateCompletionSink`] implementation rather than two independently
//! wired sinks — the context only has room for one
//! (`RunnerContext::delegate_completion_sink`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::Utc;

use ao_engine_tools_core::background_agents::handle::{TaskFinalReport, TaskFinalStatus};
use ao_engine_tools_core::delegate_completion_sink::DelegateCompletionSink;
use ao_persistence::PersistenceLayer;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

/// Coarse state of a spawned artifact subagent run, as observed by the HTTP
/// status endpoint. Collapses [`TaskFinalStatus::Cancelled`] into `Failed`
/// (with an `error` note) since the status endpoint's response contract only
/// distinguishes `running` / `completed` / `failed` / `unknown` — see
/// `ao_server::routes::artifacts::get_artifact_task_status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactTaskState {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub struct ArtifactTaskStatus {
    pub state: ArtifactTaskState,
    pub error: Option<String>,
}

/// Process-wide, in-memory status tracking for background subagent runs.
/// Matches the ephemeral lifetime of `BackgroundAgentRegistry` — nothing here
/// survives a server restart, and nothing needs to.
///
/// Holds two views of the same runs:
/// - `inner`: `task_id` (a [`BackgroundAgentId`]'s string form) to its latest
///   observed [`ArtifactTaskStatus`]. Backs the per-task status endpoint.
/// - `by_artifact`: `artifact_id` to the most recent `task_id` marked running
///   for it. Backs [`running_task_id_for_artifact`], which the getArtifact
///   read path uses so a client that navigated away and back can resume a
///   progress spinner it would otherwise have torn down on unmount. Only the
///   latest run per artifact is retained; a newer run overwrites the entry.
///   Terminal outcomes are NOT reflected here — the getter re-checks the
///   indexed task's status in `inner` instead (see its doc comment).
///
/// [`BackgroundAgentId`]: ao_engine_tools_core::background_agents::BackgroundAgentId
/// [`running_task_id_for_artifact`]: ArtifactTaskStatusStore::running_task_id_for_artifact
#[derive(Default)]
pub struct ArtifactTaskStatusStore {
    inner: RwLock<HashMap<String, ArtifactTaskStatus>>,
    by_artifact: RwLock<HashMap<String, String>>,
}

impl ArtifactTaskStatusStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `task_id` has been handed to the spawner and is now
    /// in-flight for `artifact_id`. Called right after a successful spawn,
    /// before the caller returns the id to its HTTP client — so a client that
    /// polls immediately never observes a false "unknown". Also updates the
    /// `artifact_id -> task_id` index so [`running_task_id_for_artifact`] can
    /// find this run on the getArtifact read path.
    ///
    /// [`running_task_id_for_artifact`]: ArtifactTaskStatusStore::running_task_id_for_artifact
    pub fn mark_running(&self, task_id: String, artifact_id: String) {
        self.write().insert(
            task_id.clone(),
            ArtifactTaskStatus {
                state: ArtifactTaskState::Running,
                error: None,
            },
        );
        self.write_by_artifact().insert(artifact_id, task_id);
    }

    /// Record a terminal outcome for `task_id`, called from
    /// [`ArtifactTaskCompletionSink::notify`] when the spawned run reaches a
    /// terminal state.
    pub fn mark_terminal(&self, task_id: String, status: TaskFinalStatus, error: Option<String>) {
        let (state, error) = match status {
            TaskFinalStatus::Completed => (ArtifactTaskState::Completed, None),
            TaskFinalStatus::Failed => (ArtifactTaskState::Failed, error),
            // The status endpoint's contract has no distinct "cancelled"
            // bucket — surface it as a failure with a note explaining why.
            TaskFinalStatus::Cancelled => (
                ArtifactTaskState::Failed,
                Some(error.unwrap_or_else(|| "cancelled".to_string())),
            ),
        };
        self.write().insert(task_id, ArtifactTaskStatus { state, error });
    }

    pub fn get(&self, task_id: &str) -> Option<ArtifactTaskStatus> {
        self.read().get(task_id).cloned()
    }

    /// The `task_id` of the currently-running background run for `artifact_id`,
    /// or `None` when there is no in-flight run OR the latest run has reached a
    /// terminal state. Verifies the indexed task's status is still
    /// [`ArtifactTaskState::Running`] rather than trusting the index alone: a
    /// finished run is therefore never reported as running, and the terminal
    /// transition (via [`mark_terminal`]) needs no bookkeeping on the index at
    /// all — flipping the task's status in `inner` is enough to stop this
    /// getter from returning it.
    ///
    /// [`mark_terminal`]: ArtifactTaskStatusStore::mark_terminal
    pub fn running_task_id_for_artifact(&self, artifact_id: &str) -> Option<String> {
        let task_id = self.read_by_artifact().get(artifact_id).cloned()?;
        match self.read().get(&task_id) {
            Some(status) if status.state == ArtifactTaskState::Running => Some(task_id),
            _ => None,
        }
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, ArtifactTaskStatus>> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, ArtifactTaskStatus>> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_by_artifact(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, String>> {
        self.by_artifact.write().unwrap_or_else(|e| e.into_inner())
    }

    fn read_by_artifact(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, String>> {
        self.by_artifact.read().unwrap_or_else(|e| e.into_inner())
    }
}

/// The combined completion sink for `spawn_artifact_agent` runs. On the
/// spawned subagent's terminal event, this:
///
/// 1. Always updates [`ArtifactTaskStatusStore`] with the outcome, so the
///    status endpoint can tell "still running" from "finished" from "dead".
/// 2. On a completed run, refetches the artifact and appends its latest
///    intent-ledger note as an assistant-role entry in the artifact's own
///    chat transcript (a purpose-built JSONL file — see
///    [`ao_persistence::paths::DataRoot::artifact_thread_path`]), so the
///    chat panel's reply survives a page reload without polling.
///
/// A failed/cancelled run intentionally appends nothing to the transcript —
/// there's no reply to show, and the status endpoint already carries the
/// error for the UI to render inline.
pub struct ArtifactTaskCompletionSink {
    pub status: Arc<ArtifactTaskStatusStore>,
    pub persistence: Arc<PersistenceLayer>,
    pub agent_id: String,
    pub artifact_id: String,
}

#[async_trait]
impl DelegateCompletionSink for ArtifactTaskCompletionSink {
    async fn notify(
        &self,
        _delegate_name: &str,
        delegation_id: &str,
        report: &TaskFinalReport,
        _transcript_path: &str,
    ) {
        self.status.mark_terminal(
            delegation_id.to_string(),
            report.status.clone(),
            report.error_message.clone(),
        );

        if report.status != TaskFinalStatus::Completed {
            return;
        }

        let record = match self
            .persistence
            .artifacts
            .get(&self.agent_id, &self.artifact_id)
            .await
        {
            Ok(record) => record,
            Err(e) => {
                tracing::warn!(
                    artifact_id = %self.artifact_id,
                    error = %e,
                    "artifact task completion: failed to refetch artifact for chat transcript append",
                );
                return;
            }
        };

        let Some(note) = record
            .intent_ledger
            .last()
            .and_then(|entry| entry.intent_note.clone())
        else {
            return;
        };

        let entry = TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::System("assistant".to_string()),
            content: note,
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        };

        let thread = match self.persistence.threads.ensure_artifact_thread(&self.artifact_id).await {
            Ok(thread) => thread,
            Err(e) => {
                tracing::warn!(
                    artifact_id = %self.artifact_id,
                    error = %e,
                    "artifact task completion: failed to resolve chat thread for assistant reply",
                );
                return;
            }
        };
        let path = PathBuf::from(&thread.transcript_path);
        if let Err(e) = self.persistence.transcripts.append_at(&path, &entry).await {
            tracing::warn!(
                artifact_id = %self.artifact_id,
                error = %e,
                "artifact task completion: failed to append assistant reply to chat transcript",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_task_id_returns_none() {
        let store = ArtifactTaskStatusStore::new();
        assert!(store.get("no-such-task").is_none());
    }

    #[test]
    fn mark_running_then_get_reports_running() {
        let store = ArtifactTaskStatusStore::new();
        store.mark_running("task-1".to_string(), "artifact-1".to_string());
        let status = store.get("task-1").expect("must be present after mark_running");
        assert_eq!(status.state, ArtifactTaskState::Running);
        assert!(status.error.is_none());
    }

    #[test]
    fn mark_terminal_completed_clears_error() {
        let store = ArtifactTaskStatusStore::new();
        store.mark_running("task-1".to_string(), "artifact-1".to_string());
        store.mark_terminal(
            "task-1".to_string(),
            TaskFinalStatus::Completed,
            Some("stale message that should be dropped".to_string()),
        );
        let status = store.get("task-1").unwrap();
        assert_eq!(status.state, ArtifactTaskState::Completed);
        assert!(status.error.is_none());
    }

    #[test]
    fn mark_terminal_failed_preserves_error_message() {
        let store = ArtifactTaskStatusStore::new();
        store.mark_terminal(
            "task-1".to_string(),
            TaskFinalStatus::Failed,
            Some("boom".to_string()),
        );
        let status = store.get("task-1").unwrap();
        assert_eq!(status.state, ArtifactTaskState::Failed);
        assert_eq!(status.error.as_deref(), Some("boom"));
    }

    #[test]
    fn mark_terminal_cancelled_maps_to_failed_with_note() {
        let store = ArtifactTaskStatusStore::new();
        store.mark_terminal("task-1".to_string(), TaskFinalStatus::Cancelled, None);
        let status = store.get("task-1").unwrap();
        assert_eq!(status.state, ArtifactTaskState::Failed);
        assert_eq!(status.error.as_deref(), Some("cancelled"));
    }

    #[test]
    fn mark_terminal_overwrites_running_state_for_same_task_id() {
        let store = ArtifactTaskStatusStore::new();
        store.mark_running("task-1".to_string(), "artifact-1".to_string());
        store.mark_terminal("task-1".to_string(), TaskFinalStatus::Failed, None);
        let status = store.get("task-1").unwrap();
        assert_eq!(status.state, ArtifactTaskState::Failed);
    }

    // --- running_task_id_for_artifact: spinner-resume lookup ----------------

    #[test]
    fn running_task_id_for_artifact_returns_task_while_running() {
        let store = ArtifactTaskStatusStore::new();
        store.mark_running("task-1".to_string(), "artifact-1".to_string());
        assert_eq!(
            store.running_task_id_for_artifact("artifact-1").as_deref(),
            Some("task-1"),
        );
    }

    #[test]
    fn running_task_id_for_artifact_returns_none_after_terminal() {
        let store = ArtifactTaskStatusStore::new();
        store.mark_running("task-1".to_string(), "artifact-1".to_string());
        // Any terminal outcome must stop the artifact from reporting a run.
        store.mark_terminal("task-1".to_string(), TaskFinalStatus::Completed, None);
        assert!(store.running_task_id_for_artifact("artifact-1").is_none());
    }

    #[test]
    fn running_task_id_for_artifact_none_for_artifact_that_never_ran() {
        let store = ArtifactTaskStatusStore::new();
        assert!(store.running_task_id_for_artifact("never-run").is_none());
    }

    #[test]
    fn running_task_id_for_artifact_tracks_latest_run_per_artifact() {
        let store = ArtifactTaskStatusStore::new();
        store.mark_running("task-1".to_string(), "artifact-1".to_string());
        store.mark_terminal("task-1".to_string(), TaskFinalStatus::Completed, None);
        // A second run for the same artifact makes it "running" again, and the
        // getter returns the newer task, not the finished one.
        store.mark_running("task-2".to_string(), "artifact-1".to_string());
        assert_eq!(
            store.running_task_id_for_artifact("artifact-1").as_deref(),
            Some("task-2"),
        );
    }
}
