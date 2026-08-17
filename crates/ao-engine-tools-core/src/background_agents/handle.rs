use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use ao_protocol::error::AoError;

/// Opaque stable identifier for a live background agent.
///
/// Wraps a UUID v4 string so the identifier is globally unique, URL-safe,
/// and survives round-trips through JSON and CLI surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BackgroundAgentId(String);

impl BackgroundAgentId {
    /// Generate a fresh random id.
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Return the underlying string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for BackgroundAgentId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for BackgroundAgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for BackgroundAgentId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        uuid::Uuid::parse_str(s)
            .map(|u| Self(u.to_string()))
            .map_err(|e| format!("invalid BackgroundAgentId '{s}': {e}"))
    }
}

impl From<String> for BackgroundAgentId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Events the runner emits as a child agent processes its turn loop.
///
/// The broadcast channel held by [`BackgroundAgentHandle`] carries these
/// events so callers (the DelegateOutput tool) can observe child progress
/// without blocking on the join handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunnerEvent {
    /// The child produced a chunk of assistant text.
    AssistantText {
        background_agent_id: BackgroundAgentId,
        text: String,
    },
    /// The child invoked a tool.
    ToolUse {
        background_agent_id: BackgroundAgentId,
        tool_name: String,
    },
    /// The child's turn loop completed normally.
    Completed {
        background_agent_id: BackgroundAgentId,
    },
    /// The child's turn loop was cancelled via its [`CancellationToken`].
    Cancelled {
        background_agent_id: BackgroundAgentId,
    },
    /// The child's turn loop ended in an error before completing — e.g. the
    /// provider was misconfigured or the first model call failed. Carries the
    /// human-readable error so callers and the sidechain transcript can show
    /// *why* the run died instead of silently reporting an empty result.
    Failed {
        background_agent_id: BackgroundAgentId,
        error: String,
    },
    /// A background child was launched in background mode (emitted on the
    /// *parent*'s event stream so the UI can render the sidechain card).
    AsyncLaunched {
        background_agent_id: BackgroundAgentId,
        subagent_type: String,
        parent_agent_id: String,
        spawned_at: DateTime<Utc>,
    },
}

/// Terminal status of a background agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFinalStatus {
    Completed,
    Cancelled,
    Failed,
}

/// The outcome the child runner's [`JoinHandle`] resolves to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskFinalReport {
    pub status: TaskFinalStatus,
    /// The last assistant text the child produced, if any.
    pub final_assistant_text: Option<String>,
    /// Human-readable error message when `status == Failed`.
    pub error_message: Option<String>,
    /// Wall-clock milliseconds from session start to terminal event.
    /// `None` when the runner did not capture timing (e.g. test mocks).
    pub duration_ms: Option<u64>,
    /// Number of provider turns the child completed. A turn is one full
    /// provider request/response cycle. `None` when not tracked.
    pub num_turns: Option<u32>,
}

impl TaskFinalReport {
    pub fn completed(final_assistant_text: Option<String>) -> Self {
        Self {
            status: TaskFinalStatus::Completed,
            final_assistant_text,
            error_message: None,
            duration_ms: None,
            num_turns: None,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            status: TaskFinalStatus::Cancelled,
            final_assistant_text: None,
            error_message: None,
            duration_ms: None,
            num_turns: None,
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            status: TaskFinalStatus::Failed,
            final_assistant_text: None,
            error_message: Some(message.into()),
            duration_ms: None,
            num_turns: None,
        }
    }

    /// Attach timing and turn-count stats collected by the runner.
    ///
    /// Returns `self` with `duration_ms` and `num_turns` set, suitable for
    /// chaining onto any of the constructor methods.
    pub fn with_stats(mut self, duration_ms: u64, num_turns: u32) -> Self {
        self.duration_ms = Some(duration_ms);
        self.num_turns = Some(num_turns);
        self
    }
}

/// A live handle to an in-flight background agent.
///
/// Held by [`BackgroundAgentRegistry`](super::registry::BackgroundAgentRegistry)
/// for the lifetime of the child's run. Dropping the handle does not cancel
/// the child — call `cancel.cancel()` first, then await `join` if a clean
/// shutdown is needed.
pub struct BackgroundAgentHandle {
    pub id: BackgroundAgentId,
    pub subagent_name: String,
    pub spawned_at: DateTime<Utc>,
    pub cancel: CancellationToken,
    pub events: broadcast::Receiver<RunnerEvent>,
    pub join: JoinHandle<Result<TaskFinalReport, AoError>>,
}

impl fmt::Debug for BackgroundAgentHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackgroundAgentHandle")
            .field("id", &self.id)
            .field("subagent_name", &self.subagent_name)
            .field("spawned_at", &self.spawned_at)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_agent_id_display_roundtrip() {
        let id = BackgroundAgentId::new();
        let s = id.to_string();
        let parsed: BackgroundAgentId = s.parse().expect("valid uuid should parse");
        assert_eq!(id, parsed);
    }

    #[test]
    fn background_agent_id_serde_roundtrip() {
        let id = BackgroundAgentId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        let decoded: BackgroundAgentId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, decoded);
    }

    #[test]
    fn background_agent_id_from_str_rejects_garbage() {
        let result = "not-a-uuid".parse::<BackgroundAgentId>();
        assert!(result.is_err());
    }

    #[test]
    fn background_agent_id_hash_eq_consistency() {
        use std::collections::HashSet;
        let id = BackgroundAgentId::new();
        let mut set = HashSet::new();
        set.insert(id.clone());
        assert!(set.contains(&id));
    }

    #[test]
    fn task_final_report_constructors() {
        let r = TaskFinalReport::completed(Some("hello".to_string()));
        assert_eq!(r.status, TaskFinalStatus::Completed);
        assert_eq!(r.final_assistant_text.as_deref(), Some("hello"));

        let r = TaskFinalReport::cancelled();
        assert_eq!(r.status, TaskFinalStatus::Cancelled);

        let r = TaskFinalReport::failed("oops");
        assert_eq!(r.status, TaskFinalStatus::Failed);
        assert_eq!(r.error_message.as_deref(), Some("oops"));
    }
}
