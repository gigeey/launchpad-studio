use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;
use crate::attachment::Attachment;
use crate::team::TeamId;

/// Owner of a tasklist: either a Team or an Agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TasklistOwner {
    Team { team_id: TeamId },
    Agent { agent_id: AgentId },
}

/// Scope discriminator carried in RunScope::Tasklist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TasklistScope {
    Team(TeamId),
    Agent(AgentId),
}

pub type TasklistId = String;
pub type TaskGroupId = String;
pub type TaskId = String;
pub type TaskCommentId = String;

/// How a task was assigned to its owner agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentMode {
    /// Explicitly pinned by the caller — never overwritten by the classifier.
    Pinned,
    /// Resolved automatically by the task classifier.
    Classified,
}

/// Ownership record stored on each task row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAssignment {
    pub owner_agent_id: String,
    pub mode: AssignmentMode,
}

/// Lifecycle state of a single task within a tasklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Blocked,
    /// User-decided terminal state: the task is skipped past without success
    /// or failure. Used by Skip-failed-task and similar recovery actions.
    Skipped,
    /// Per-task halt: the task was explicitly stopped by the orchestrating
    /// agent via TodoStopTask. The task is neither terminal nor in-progress —
    /// it waits until TodoResumeTask transitions it back to Pending for
    /// re-dispatch. In SEQ groups a stopped task blocks all subsequent tasks,
    /// preserving ordering. In PAR groups siblings continue unaffected.
    Stopped,
}

impl TaskStatus {
    /// Terminal states stop the feeder from advancing the same task.
    /// `Stopped` is NOT terminal — it can be resumed via TodoResumeTask.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Skipped
        )
    }
}

/// Dispatch mode for a task group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TaskGroupMode {
    Par,
    Seq,
}

/// Lifecycle state of an entire tasklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TasklistStatus {
    Active,
    Paused,
    Completed,
    Cancelled,
    Failed,
}

/// Source of a `TaskComment` — either the user (typed in the inline panel) or
/// an agent (posted via the API during a run).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCommentAuthorKind {
    User,
    Agent,
}

/// A comment attached to a single task. Stored inline on the `Task` so the
/// existing tasklist read-path returns comments without an extra join. Comments
/// augment the task prompt at dispatch time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskComment {
    pub id: TaskCommentId,
    pub author_id: String,
    pub author_kind: TaskCommentAuthorKind,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub owner_agent_id: AgentId,
    pub prompt: String,
    #[serde(default)]
    pub expected_outputs: Vec<String>,
    pub status: TaskStatus,
    pub group_id: TaskGroupId,
    #[serde(default)]
    pub attempt_count: u32,
    #[serde(default)]
    pub error_log: Vec<String>,
    #[serde(default)]
    pub comments: Vec<TaskComment>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Agent that should be notified via `<task-item-notification>` when this
    /// task reaches a terminal state. `None` means no notification routing.
    /// Existing tasklists deserialize with `None` (serde default) so on-disk
    /// state from before this field was introduced loads cleanly.
    #[serde(default)]
    pub remind_me: Option<AgentId>,
    /// Set true when the auto-reprompt retry budget for the
    /// `<task-item-notification>` block is exhausted and the system synthesized
    /// a changelog entry on the producing agent's behalf. Defaults to `false`.
    #[serde(default)]
    pub parse_failed: bool,
    /// How many times the auto-reprompt path has fired for this task because
    /// the producing agent emitted a missing or malformed
    /// `<task-item-notification>` block alongside a terminal `<task action="…">`.
    /// Persisted so a server restart doesn't reset the budget. Bumped by
    /// `handle_task_item_notification_parse_failure` in agent_runner.
    #[serde(default)]
    pub notification_parse_retry_count: u32,
    /// Which agent owns this task and how the assignment was determined.
    /// `None` signals the task is awaiting classifier resolution (in-flight or
    /// pre-boot-sweep). Missing field on legacy rows deserializes as `None`.
    #[serde(default)]
    pub assignment: Option<TaskAssignment>,
    /// Monotonic token bumped on every assignment mutation. Used by the
    /// classifier write-back CAS to detect stale results from concurrent
    /// classification spawns. Defaults to 0 for legacy rows.
    #[serde(default)]
    pub classifier_token: u64,
    /// Monotonic token bumped every time a task is (re)claimed for dispatch
    /// while it stays `InProgress` — i.e. the reprompt/recovery path, not the
    /// initial `Pending -> InProgress` dispatch (that path already has an
    /// atomic claim via the status transition itself). Reprompt/recovery has
    /// no status transition to serialize on, so this token is the CAS anchor
    /// instead: a caller captures the value it read, and the claim only
    /// succeeds if that value still matches under the tasklist's write lock.
    /// A deliberately separate field from `classifier_token` — the two guard
    /// unrelated concurrency concerns and conflating them would make either
    /// CAS spuriously reject the other's writer. Defaults to 0 for legacy rows.
    #[serde(default)]
    pub dispatch_token: u64,
}

/// Number of leading task-id characters used to namespace per-task output
/// filenames inside a tasklist's shared workspace. Picked so the on-disk name
/// stays readable in agent prompts (`abc12345__service-design.md` rather than
/// the full 36-char UUID) while keeping per-tasklist collision odds negligible
/// for any realistic task count.
pub const OUTPUT_FILENAME_PREFIX_LEN: usize = 8;

/// Apply the per-task filename prefix to a single declared output. Idempotent:
/// already-prefixed filenames pass through unchanged so re-runs and follow-up
/// mutations don't double-prefix.
///
/// The shared workspace is a single flat directory so every task's outputs
/// remain readable to siblings; the prefix is what prevents two parallel
/// tasks from clobbering each other when their classifiers happen to pick
/// the same base filename.
pub fn prefix_expected_output(task_id: &TaskId, filename: &str) -> String {
    let prefix = task_id_filename_prefix(task_id);
    let expected_marker = format!("{}__", prefix);
    if filename.starts_with(&expected_marker) {
        return filename.to_string();
    }
    format!("{}{}", expected_marker, filename)
}

/// Apply the prefix in place to a list of expected_outputs. See
/// [`prefix_expected_output`] for the invariant.
pub fn prefix_expected_outputs(task_id: &TaskId, outputs: &mut [String]) {
    for out in outputs.iter_mut() {
        *out = prefix_expected_output(task_id, out);
    }
}

/// Short prefix derived from the task id. Truncated to
/// [`OUTPUT_FILENAME_PREFIX_LEN`] codepoints and falls back to the full id
/// when the id is shorter than the truncation window (test fixtures).
pub fn task_id_filename_prefix(task_id: &TaskId) -> String {
    task_id
        .chars()
        .take(OUTPUT_FILENAME_PREFIX_LEN)
        .collect::<String>()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskGroup {
    pub id: TaskGroupId,
    pub mode: TaskGroupMode,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tasklist {
    pub id: TasklistId,
    pub owner: TasklistOwner,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<TeamId>,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: TasklistStatus,
    #[serde(default)]
    pub groups: Vec<TaskGroup>,
    /// Absolute path to the per-tasklist shared workspace directory.
    pub workspace_dir: String,
    /// Absolute path to the per-tasklist transcripts directory.
    pub transcripts_dir: String,
    pub created_at: DateTime<Utc>,
    /// Timestamp of the most recent transition out of `Active`. Stamped by the
    /// `set_status` chokepoint whenever the tasklist leaves `Active` (to
    /// Completed, Failed, Cancelled, or Paused). Used by the append-task
    /// auto-resume window so a freshly-Completed tasklist can revive directly
    /// to `Active` instead of `Paused` when the user adds a new task soon
    /// after. `None` for legacy tasklists or ones that have never been Active.
    #[serde(default)]
    pub last_active_at: Option<DateTime<Utc>>,
    /// Co-pilot agent bound to this tasklist. Set on first call to the
    /// `GET /tasklists/{id}/copilot` endpoint and never reassigned for the
    /// life of the tasklist (the binding is idempotent — see
    /// `TasklistStore::bind_copilot_agent_id`). `None` for legacy tasklists
    /// or ones that have not yet had their overlay opened.
    #[serde(default)]
    pub copilot_agent_id: Option<AgentId>,
    /// Timestamp of the most recent overlay-open ping (the FE hitting
    /// `GET /tasklists/{id}/copilot`). Drives the lifecycle state machine: an
    /// `is_tasklist_active` check considers a tasklist active for a 24h heartbeat
    /// window after an overlay open even when every task is terminal, and the
    /// sleep check uses a short keepalive window to detect "overlay still open".
    /// `None` for legacy tasklists or ones that have not been opened yet.
    #[serde(default)]
    pub last_opened_at: Option<DateTime<Utc>>,
    /// Project this tasklist belongs to, when created by a project agent via
    /// TodoCreate inside a project-scoped run. `None` for standalone agent
    /// tasklists and all team-owned tasklists. Defaults to `None` on legacy
    /// persisted tasklists so old disk state loads cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Thread that was active when this tasklist was created via `TodoCreate`.
    /// `None` for tasklists created outside a thread-scoped run. Read back at
    /// completion time to route the `todo_list.complete` SSE event and
    /// persisted transcript marker to the originating thread instead of
    /// always falling back to the agent's default-thread transcript. Defaults
    /// to `None` on legacy persisted tasklists so old disk state loads cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

#[derive(Deserialize)]
struct TasklistRaw {
    pub id: TasklistId,
    #[serde(default)]
    pub owner: Option<TasklistOwner>,
    #[serde(default)]
    pub team_id: Option<TeamId>,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: TasklistStatus,
    #[serde(default)]
    pub groups: Vec<TaskGroup>,
    pub workspace_dir: String,
    pub transcripts_dir: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub last_active_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub copilot_agent_id: Option<AgentId>,
    #[serde(default)]
    pub last_opened_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
}

impl<'de> serde::Deserialize<'de> for Tasklist {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = TasklistRaw::deserialize(d)?;
        let owner = match (raw.owner, raw.team_id.as_deref()) {
            (Some(o), _) => o,
            (None, Some(tid)) => TasklistOwner::Team { team_id: tid.to_string() },
            (None, None) => return Err(serde::de::Error::missing_field("owner")),
        };
        Ok(Tasklist {
            id: raw.id,
            owner,
            team_id: raw.team_id,
            title: raw.title,
            description: raw.description,
            status: raw.status,
            groups: raw.groups,
            workspace_dir: raw.workspace_dir,
            transcripts_dir: raw.transcripts_dir,
            created_at: raw.created_at,
            last_active_at: raw.last_active_at,
            copilot_agent_id: raw.copilot_agent_id,
            last_opened_at: raw.last_opened_at,
            project_id: raw.project_id,
            thread_id: raw.thread_id,
        })
    }
}

impl Tasklist {
    /// The inner scope id string for routing and SSE use.
    /// Returns the team_id for team-owned tasklists, agent_id for agent-owned ones.
    pub fn scope_id(&self) -> &str {
        match &self.owner {
            TasklistOwner::Team { team_id } => team_id,
            TasklistOwner::Agent { agent_id } => agent_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_task() -> Task {
        Task {
            id: "t-1".to_string(),
            owner_agent_id: "agent-a".to_string(),
            prompt: "do work".to_string(),
            expected_outputs: vec!["out.md".to_string()],
            status: TaskStatus::Pending,
            group_id: "g-1".to_string(),
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

    #[test]
    fn task_round_trip_carries_remind_me_and_parse_failed() {
        let mut t = sample_task();
        t.remind_me = Some("agent-b".to_string());
        t.parse_failed = true;
        t.notification_parse_retry_count = 2;

        let json = serde_json::to_string(&t).unwrap();
        let parsed: Task = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.remind_me.as_deref(), Some("agent-b"));
        assert!(parsed.parse_failed);
        assert_eq!(parsed.notification_parse_retry_count, 2);
        assert_eq!(parsed, t);
    }

    #[test]
    fn task_legacy_json_deserializes_with_defaults() {
        // JSON shape from before remind_me / parse_failed were introduced.
        let legacy = r#"{
            "id": "t-1",
            "owner_agent_id": "agent-a",
            "prompt": "do work",
            "expected_outputs": ["out.md"],
            "status": "pending",
            "group_id": "g-1",
            "attempt_count": 0,
            "error_log": [],
            "comments": [],
            "attachments": []
        }"#;

        let parsed: Task = serde_json::from_str(legacy).unwrap();
        assert!(parsed.remind_me.is_none());
        assert!(!parsed.parse_failed);
        assert_eq!(parsed.notification_parse_retry_count, 0);
    }

    #[test]
    fn tasklist_legacy_json_deserializes_with_last_opened_at_none() {
        // JSON shape from before last_opened_at was introduced.
        let legacy = r#"{
            "id": "tl-1",
            "team_id": "team-1",
            "title": "Legacy",
            "description": "",
            "status": "active",
            "groups": [],
            "workspace_dir": "/tmp/ws",
            "transcripts_dir": "/tmp/tr",
            "created_at": "2025-01-01T00:00:00Z"
        }"#;

        let parsed: Tasklist = serde_json::from_str(legacy).unwrap();
        assert!(parsed.last_opened_at.is_none());
        assert!(parsed.last_active_at.is_none());
        assert!(parsed.copilot_agent_id.is_none());
        assert!(matches!(parsed.owner, TasklistOwner::Team { .. }));
    }

    #[test]
    fn tasklist_owner_team_round_trips() {
        let tl = Tasklist {
            id: "tl-1".to_string(),
            owner: TasklistOwner::Team { team_id: "team-1".to_string() },
            team_id: Some("team-1".to_string()),
            title: "Test".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![],
            workspace_dir: "/tmp/ws".to_string(),
            transcripts_dir: "/tmp/tr".to_string(),
            created_at: chrono::Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
        };
        let json = serde_json::to_string(&tl).unwrap();
        let parsed: Tasklist = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.owner, tl.owner);
        assert_eq!(parsed.team_id, tl.team_id);
    }

    #[test]
    fn tasklist_project_id_round_trips() {
        let legacy = r#"{
            "id": "tl-proj",
            "owner": {"kind": "agent", "agent_id": "a1"},
            "title": "proj tasklist",
            "description": "",
            "status": "active",
            "groups": [],
            "workspace_dir": "/tmp/ws",
            "transcripts_dir": "/tmp/tr",
            "created_at": "2025-01-01T00:00:00Z",
            "project_id": "proj-1"
        }"#;
        let parsed: Tasklist = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.project_id.as_deref(), Some("proj-1"));
        let json = serde_json::to_string(&parsed).unwrap();
        let re: Tasklist = serde_json::from_str(&json).unwrap();
        assert_eq!(re.project_id.as_deref(), Some("proj-1"));
    }

    #[test]
    fn tasklist_project_id_absent_defaults_to_none() {
        let legacy = r#"{
            "id": "tl-no-proj",
            "team_id": "team-1",
            "title": "no-project",
            "description": "",
            "status": "active",
            "groups": [],
            "workspace_dir": "/tmp/ws",
            "transcripts_dir": "/tmp/tr",
            "created_at": "2025-01-01T00:00:00Z"
        }"#;
        let parsed: Tasklist = serde_json::from_str(legacy).unwrap();
        assert!(parsed.project_id.is_none());
    }

    #[test]
    fn tasklist_legacy_team_id_deserializes_to_owner() {
        let legacy = r#"{
            "id": "tl-1",
            "team_id": "team-1",
            "title": "Legacy",
            "description": "",
            "status": "active",
            "groups": [],
            "workspace_dir": "/tmp/ws",
            "transcripts_dir": "/tmp/tr",
            "created_at": "2025-01-01T00:00:00Z"
        }"#;
        let parsed: Tasklist = serde_json::from_str(legacy).unwrap();
        assert!(matches!(parsed.owner, TasklistOwner::Team { ref team_id } if team_id == "team-1"));
        assert_eq!(parsed.team_id.as_deref(), Some("team-1"));
    }

    // --- task_assignment tests ---

    #[test]
    fn task_assignment_roundtrip_pinned() {
        let assignment = TaskAssignment {
            owner_agent_id: "backend".to_string(),
            mode: AssignmentMode::Pinned,
        };
        let json = serde_json::to_string(&assignment).unwrap();
        assert!(json.contains("\"pinned\""), "pinned must serialize as snake_case: {json}");
        let parsed: TaskAssignment = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.owner_agent_id, "backend");
        assert_eq!(parsed.mode, AssignmentMode::Pinned);
    }

    #[test]
    fn task_assignment_roundtrip_classified() {
        let assignment = TaskAssignment {
            owner_agent_id: "frontend".to_string(),
            mode: AssignmentMode::Classified,
        };
        let json = serde_json::to_string(&assignment).unwrap();
        assert!(json.contains("\"classified\""), "classified must serialize as snake_case: {json}");
        let parsed: TaskAssignment = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.mode, AssignmentMode::Classified);
    }

    #[test]
    fn task_assignment_null_serializes_as_null() {
        // assignment: None → field may be absent or null; when explicitly set to null it must round-trip
        let json_null = r#"{"assignment": null}"#;
        let partial: serde_json::Value = serde_json::from_str(json_null).unwrap();
        assert!(partial["assignment"].is_null());
    }

    #[test]
    fn task_assignment_unknown_mode_rejected() {
        let bad = r#"{"owner_agent_id": "x", "mode": "unknown_future_mode"}"#;
        let result: Result<TaskAssignment, _> = serde_json::from_str(bad);
        assert!(result.is_err(), "unknown mode must be rejected with a typed serde error");
    }

    #[test]
    fn task_loop_j_fixture_no_assignment_deserializes_as_none() {
        // Loop-J shaped task JSON — no assignment field at all
        let legacy = r#"{
            "id": "t-loop-j",
            "owner_agent_id": "agent-a",
            "prompt": "do work",
            "expected_outputs": [],
            "status": "pending",
            "group_id": "g-1",
            "attempt_count": 0,
            "error_log": [],
            "comments": [],
            "attachments": []
        }"#;
        let parsed: Task = serde_json::from_str(legacy).unwrap();
        assert!(parsed.assignment.is_none(), "missing assignment field must deserialize as None");
        assert_eq!(parsed.classifier_token, 0, "missing classifier_token must default to 0");
    }

    #[test]
    fn task_assignment_null_field_deserializes_as_none() {
        // Fixture with explicit assignment: null
        let fixture = r#"{
            "id": "t-1",
            "owner_agent_id": "agent-a",
            "prompt": "do work",
            "expected_outputs": [],
            "status": "pending",
            "group_id": "g-1",
            "attempt_count": 0,
            "error_log": [],
            "comments": [],
            "attachments": [],
            "assignment": null
        }"#;
        let parsed: Task = serde_json::from_str(fixture).unwrap();
        assert!(parsed.assignment.is_none(), "assignment: null must deserialize as None");
    }

    #[test]
    fn task_classifier_token_defaults_to_zero_on_legacy_rows() {
        let legacy = r#"{
            "id": "t-legacy",
            "owner_agent_id": "a",
            "prompt": "p",
            "expected_outputs": [],
            "status": "pending",
            "group_id": "g",
            "attempt_count": 0,
            "error_log": [],
            "comments": [],
            "attachments": []
        }"#;
        let parsed: Task = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.classifier_token, 0);
    }
}
