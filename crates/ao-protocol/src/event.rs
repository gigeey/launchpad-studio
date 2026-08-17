use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;
use crate::memory::MemoryScope;
use crate::tasklist::{Task, TaskAssignment, TasklistOwner};
use crate::thread::Thread;
use crate::transcript::TranscriptEntry;

/// Wire-safe representation of a selectable option in a form's checkbox or radio field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormOptionEventPayload {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Wire-safe representation of a single form field emitted via SSE.
///
/// Flat struct with optional extras avoids enum-in-enum serde complexity.
/// The `kind` string is one of `"checkbox"` | `"radio"` | `"text"` | `"textarea"` | `"file"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormFieldEventPayload {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<FormOptionEventPayload>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_files: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept: Option<String>,
}

/// Wire-safe representation of the full async form spec carried by
/// [`AgentEventPayload::FormPosted`] — same title/intro/fields shape as
/// [`AgentEventPayload::FormRequest`] itself (down to reusing
/// [`FormFieldEventPayload`] for `fields`), so a form posted asynchronously
/// serializes identically, field for field, to one requested synchronously.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormSpecEventPayload {
    pub form_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intro: Option<String>,
    pub fields: Vec<FormFieldEventPayload>,
}

/// A single item in a [`AgentEventPayload::TodoListCreated`] event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoListCreatedItem {
    pub task_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment: Option<TaskAssignment>,
}

/// Terminal outcome counts for a [`AgentEventPayload::TodoListComplete`] event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoListTerminalCounts {
    pub succeeded: usize,
    pub failed: usize,
    pub skipped: usize,
    pub cancelled: usize,
}

/// A single task's terminal outcome within a [`AgentEventPayload::TodoListComplete`] event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoListCompleteTask {
    pub task_id: String,
    pub title: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub event_id: String,
    pub run_id: String,
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub agent_id: AgentId,
    pub thread_id: Option<String>,
    pub payload: AgentEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum AgentEventPayload {
    RunStarted,
    RunEnded {
        reason: RunEndReason,
    },
    TextDelta {
        text: String,
    },
    TextComplete {
        text: String,
    },
    /// The provider opened a dedicated reasoning channel. Emitted before any
    /// `ThinkingDelta` arrives — and, importantly, also emitted in the
    /// `display = "omitted"` case where no deltas will arrive at all. UIs
    /// should use this as the cue to mount a "Thinking…" indicator on the
    /// in-flight bubble; the absence of subsequent deltas is itself a valid
    /// signal that the reasoning text was suppressed at the provider level.
    ThinkingStarted,
    /// A chunk of reasoning text from an in-progress thinking block. Multiple
    /// deltas concatenate to form the full reasoning trace. Anthropic chunks
    /// these at multi-character boundaries — they are not character-by-character
    /// like `TextDelta`.
    ThinkingDelta {
        text: String,
    },
    /// The provider closed the current reasoning channel. `elapsed_ms` is the
    /// wall-clock duration from the matching `ThinkingStarted`, used by the UI
    /// to render a "Thought for Ns" footer when the bubble collapses.
    ThinkingEnded {
        elapsed_ms: u64,
    },
    ToolCallStarted {
        tool_name: String,
        tool_input: Option<serde_json::Value>,
        /// Native-mode override for the streaming chip label. When `Some`, the
        /// frontend renders it verbatim and skips its own label derivation. CLI
        /// normalizers leave this `None` so CLI chips stay frontend-computed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// The provider's real correlation id for this call (e.g. Claude's
        /// `toolu_...` block id), when the normalizer/runner captured one.
        /// Lets a CLI-mode runner persist a `tool_use`/`tool_result` transcript
        /// entry pair keyed the same way the native path already does, instead
        /// of only broadcasting the event live — see `CliAgentRunner`'s use of
        /// `TimelineAdapter::record_xml_tool_use`/`record_xml_tool_result`.
        /// `None` for normalizers that don't expose a stable id (e.g. Codex).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
    },
    ToolCallCompleted {
        tool_name: String,
        output: Option<String>,
        /// See `ToolCallStarted::tool_use_id` — the same id, so a CLI-mode
        /// runner can pair this completion with its originating call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        /// Whether the call failed. Carried alongside `tool_use_id` so a
        /// CLI-mode runner's persisted `tool_result` entry reflects the same
        /// error state the native path already records — `history::to_messages`
        /// reads it back out to mark `<tool_result is_error="true">` when
        /// replaying transcript into context, so a wrong default here would
        /// misrepresent a failed call as successful on the next turn.
        #[serde(default)]
        is_error: bool,
    },
    MessageReceived {
        message_id: String,
    },
    MessageProcessingStarted {
        message_id: String,
    },
    AgentBusy {
        run_id: String,
        started_at: DateTime<Utc>,
    },
    Error {
        message: String,
        recoverable: bool,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        /// Tokens served from the provider's prompt cache (Anthropic's
        /// `cache_read_input_tokens`). A non-zero value on turn N≥2 means the
        /// cache-anchor work is paying off — the stable prefix produced a
        /// cache hit. A persistent zero across turns is the smoke-test signal
        /// for an anchor drift bug.
        cache_read_tokens: u64,
        /// Tokens newly written to the provider's prompt cache on this turn
        /// (Anthropic's `cache_creation_input_tokens`). Expected to be non-zero
        /// on the first turn of a session (the cache write) and zero on
        /// subsequent turns. Forwarded so the chat-side observability path can
        /// see both halves of the cache picture without falling back to log
        /// scraping.
        cache_creation_tokens: u64,
        total_tokens: u64,
    },
    DelegationStarted {
        delegation_id: String,
        target_agent_id: String,
        task_summary: String,
    },
    DelegationCompleted {
        delegation_id: String,
        source_agent_id: String,
        status: String,
    },
    TeamRoundStarted {
        round: u32,
    },
    TeamRoundCompleted {
        round: u32,
        has_more_delegations: bool,
    },
    WorkflowTaskCreated {
        task_id: String,
        workflow_id: String,
        project_name: String,
    },
    PhaseStarted {
        task_id: String,
        phase_id: String,
        phase_name: String,
    },
    PhaseCompleted {
        task_id: String,
        phase_id: String,
    },
    PhaseSkipped {
        task_id: String,
        phase_id: String,
        reason: String,
    },
    PhaseFailed {
        task_id: String,
        phase_id: String,
        error: String,
    },
    PhasePaused {
        task_id: String,
        phase_id: String,
        reason: String,
    },
    WorkflowPhaseProgress {
        task_id: String,
        phase_id: String,
        status: String,
        message: Option<String>,
        percent: Option<u8>,
    },
    WorkflowCompleted {
        task_id: String,
    },
    WorkflowTaskStarted {
        task_id: String,
    },
    WorkflowTaskFailed {
        task_id: String,
        error: String,
    },
    WorkflowTaskStopped {
        task_id: String,
    },
    WorkflowTaskReopened {
        task_id: String,
        phase_id: String,
    },
    /// A system-level message to display in the chat as a centered bubble.
    /// Persisted in the transcript and rendered distinctly from agent/user messages.
    SystemMessage {
        text: String,
        /// Coarse tone hint for a handful of emitters that have an opinion
        /// about it (presently: agent-watch contract authoring). `None` —
        /// every other emitter — renders exactly as before this field
        /// existed: a neutral bubble, no implied tone either way.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        severity: Option<SystemMessageSeverity>,
    },
    /// Emitted periodically while a sync TodoCreate tool call is in-flight so
    /// the frontend can render a live "Using TodoList" progress pill. Carries
    /// the minimum data the pill needs without requiring a full tasklist fetch.
    ToolProgress {
        tasklist_id: String,
        items_done: usize,
        items_total: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_terminal_task_title: Option<String>,
    },
    /// Emitted once by TodoCreate after creating an agent-scoped tasklist.
    /// Fires on the parent agent's chat channel so the UI knows the list exists
    /// immediately while classifier-routed assignments resolve in the background.
    TodoListCreated {
        tasklist_id: String,
        item_count: usize,
        items: Vec<TodoListCreatedItem>,
    },
    /// Emitted once by the TaskFeeder when an agent-owned tasklist reaches a
    /// terminal state (completed, failed, or cancelled). Fires on the parent
    /// agent's chat channel as a single batched summary chip — replaces the
    /// per-task post_completion_summary pings previously used for agent scope.
    /// Team-owned tasklists are unaffected; this event is agent-scope only.
    ///
    /// The suppression is keyed on TasklistScope::Agent, not on whether a sync
    /// watcher fired — both sync and async agent-owned runs emit this event.
    TodoListComplete {
        tasklist_id: String,
        status: String,
        counts: TodoListTerminalCounts,
        tasks: Vec<TodoListCompleteTask>,
    },
    /// Emitted on the parent agent's chat channel the instant an async
    /// delegate's background run is registered — before the child actually
    /// starts producing output. Brackets with [`AgentEventPayload::DelegateComplete`]
    /// so the frontend can light a "running" indicator for the delegate's
    /// whole background duration instead of only its (near-instant) spawn
    /// tool call. Fired only for `mode: "async"` delegates — a sync delegate
    /// keeps the parent's own turn in-flight, which the existing typing
    /// indicator already covers.
    ///
    /// Also replayed synthetically at `/system/stream` connect time for any
    /// delegation still live in an `McpAgentSession`'s `BackgroundAgentRegistry`,
    /// so a client that reconnects mid-run reconfirms it rather than relying on
    /// a `tool_call_started` event it may have missed. Not persisted to the
    /// transcript — unlike `DelegateComplete`, there is nothing useful to show
    /// after a reload once the run has already finished.
    DelegateStarted {
        delegate_name: String,
        delegation_id: String,
        /// When the background delegate was actually spawned. Carried through
        /// (rather than stamped at emit time) so a replayed event on
        /// reconnect reports the run's real start rather than the moment the
        /// client happened to reconnect — letting a UI elapsed-time indicator
        /// pick up where it would have been had the connection never dropped.
        spawned_at: DateTime<Utc>,
    },
    /// Emitted on the parent agent's chat channel when an async delegate
    /// finishes (completed, failed, or cancelled). Fires from the
    /// `QueueDelegateCompletionSink` after the model-facing queue message is
    /// submitted, so the UI shows a completion pill at the instant the result
    /// lands rather than waiting for the parent's next reply.
    ///
    /// Also persisted as a `delegate_complete` transcript marker so the pill
    /// survives page reloads.
    DelegateComplete {
        delegate_name: String,
        delegation_id: String,
        /// Terminal status: "completed" | "failed" | "cancelled"
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        transcript_path: String,
    },
    AgentActionStarted {
        action_id: String,
        kind: String,
        summary: String,
    },
    AgentActionCompleted {
        action_id: String,
    },
    /// Emitted the instant a `<tool_use>` opening tag is recognized in CLI
    /// binary stdout. Lets the UI render an in-flight "Using X…" chip.
    ToolUseStarted {
        tool_use_id: String,
        tool_name: String,
        agent_id: String,
        run_id: String,
        timestamp: DateTime<Utc>,
    },
    /// Emitted when the closing `</tool_use>` tag lands with the
    /// fully-parsed, schema-coerced parameter block. The runner dispatches
    /// the actual tool invocation after receiving this event.
    ToolUseCompleted {
        tool_use_id: String,
        tool_name: String,
        input: serde_json::Value,
        agent_id: String,
        run_id: String,
        timestamp: DateTime<Utc>,
    },
    /// A transcript entry persisted mid-run that the client would otherwise
    /// miss until a page refresh (e.g. the hidden skill-body injection written
    /// between two agent turns). Delivered so the in-memory transcript stays
    /// in sync with what a fresh fetch would return.
    HiddenTranscriptEntry {
        entry: TranscriptEntry,
    },
    /// Emitted after each <save_memory> or <save_global_memory> tag is processed.
    MemorySaved {
        content: String,
        scope: MemoryScope,
    },
    /// A new tasklist was created for this team.
    TasklistCreated {
        team_id: String,
        tasklist_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<TasklistOwner>,
        /// Set when the tasklist belongs to a project. Lets the per-agent
        /// chat SSE handlers skip project-scoped events so they don't bleed
        /// into the personal chat view.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
    },
    /// A single task within a tasklist changed in some way (status, owner,
    /// expected_outputs, comments, error_log, attempt_count, …). The full
    /// post-mutation `Task` is included so the client can replace its cached
    /// row in one shot without needing a tailored payload per field; new
    /// fields added to `Task` flow through automatically.
    TasklistTaskUpdated {
        team_id: String,
        tasklist_id: String,
        task: Task,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<TasklistOwner>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
    },
    /// The tasklist reached the `completed` terminal state.
    TasklistCompleted {
        team_id: String,
        tasklist_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<TasklistOwner>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
    },
    /// The tasklist reached the `failed` terminal state.
    TasklistFailed {
        team_id: String,
        tasklist_id: String,
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<TasklistOwner>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
    },
    /// The tasklist's overall lifecycle status changed (currently used for
    /// pause/resume — Active <-> Paused). `status` is the snake_case
    /// `TasklistStatus` value.
    TasklistStatusChanged {
        team_id: String,
        tasklist_id: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<TasklistOwner>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
    },
    /// Emitted when a task cannot be dispatched yet because its assignment is
    /// awaiting classification. The Tasks panel renders a 'Classifying…' badge
    /// until assignment resolves and the task enters dispatch.
    TaskDeferred {
        team_id: String,
        tasklist_id: String,
        task_id: String,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<TasklistOwner>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
    },
    /// A new task was appended to a tasklist via the inline composer.
    /// Subscribers refetch the tasklist (or apply optimistic state) and
    /// dedupe by `task_id`.
    TasklistTaskAdded {
        team_id: String,
        tasklist_id: String,
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<TasklistOwner>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
    },
    /// The tasklist transitioned from dormant to active. Emitted by the
    /// lifecycle state machine on task-added, task-revived (a previously
    /// terminal task transitioning back to non-terminal), and overlay-open
    /// (the FE pinging `GET /tasklists/{id}/copilot`). Consumed by the mailbox
    /// poller to enroll the bound co-pilot agent in active polling.
    /// `reason` is one of `"task_added" | "task_revived" | "overlay_opened"`.
    TasklistWoke {
        team_id: String,
        tasklist_id: String,
        reason: String,
    },
    /// The tasklist transitioned to dormant — every task is in a terminal
    /// state, the overlay is not currently open, and the grace window has
    /// elapsed since the last activity. Emitted by the lifecycle state
    /// machine after task-terminal transitions and on each poller tick that
    /// observes a sleep-eligible tasklist. Consumed by the mailbox poller
    /// to drop the bound co-pilot from the enrolled set.
    TasklistSlept {
        team_id: String,
        tasklist_id: String,
    },
    /// Emitted when a project agent tool mutates the project's state —
    /// status transitions, spec updates, name or emoji changes. The frontend
    /// can use this to refresh the project panel without a full page reload.
    /// `status` is the snake_case `ProjectStatus` value at the time of
    /// emission; `name` reflects the post-mutation display name.
    ProjectStateChanged {
        project_id: String,
        status: String,
        name: String,
    },
    /// Sent to the frontend when an agent invokes `AskUserQuestionWithForm`.
    /// The frontend replaces `ChatInput` with a form component until the
    /// operator submits answers or the run ends.
    FormRequest {
        form_id: String,
        agent_id: String,
        session_id: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        intro: Option<String>,
        fields: Vec<FormFieldEventPayload>,
    },
    /// Emitted after an async `AskUserQuestionWithForm` call has written the
    /// `form_request` transcript entry and recorded a `pending_forms` entry on
    /// the agent snapshot, scoped to the run's own thread (this event's own
    /// envelope `thread_id` carries the same scope). Clients should refetch
    /// agent state and transcript to surface the waiting form without
    /// requiring a manual reload.
    ///
    /// `spec` carries the complete form — same field-level shape as
    /// `FormRequest` above (down to reusing [`FormFieldEventPayload`] for
    /// `fields`) — so a live client can render the card straight from this
    /// event instead of waiting on the transcript refetch or the next
    /// `pending_forms` poll to fill it in.
    FormPosted {
        form_id: String,
        spec: FormSpecEventPayload,
    },
    /// Emitted after an async form has been answered — its `form_answer`
    /// transcript entry appended and its `pending_forms` pointer cleared (see
    /// `ao-server`'s `async_form_answer`/`async_form_answer_project`).
    /// Symmetric counterpart to `FormPosted`, same transport — this event's
    /// envelope `thread_id` carries the form's own thread, same as
    /// `FormPosted`'s. Lets the client clear its pending-form indicator live,
    /// without polling or refetching agent state.
    FormResolved {
        form_id: String,
    },
    /// Emitted after `active_tasklist_title` is refreshed and persisted for an
    /// agent snapshot. Clients receiving this event are guaranteed to read the
    /// updated value on a subsequent agent list fetch — eliminates the race
    /// between the async snapshot recompute and any client re-fetch.
    AgentSnapshotUpdated {
        agent_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_tasklist_title: Option<String>,
    },
    /// Emitted whenever a thread's display label changes server-side:
    /// either the `RenameThread` tool explicitly named it (`title` set), or
    /// the first-user-message auto-title hook labeled it (`auto_title` set).
    /// Exactly one of the two fields is `Some` per emission — carries
    /// whichever one just changed so the frontend can patch its in-memory
    /// thread row without a full refetch and update the tab strip live.
    ThreadRenamed {
        thread_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_title: Option<String>,
    },
    /// Emitted the instant a `Fresh`/`Dedicated` thread row is created by a
    /// server-initiated automation (a scheduled task fire or a recurring
    /// assignment run) — there is no interactive HTTP request in the loop to
    /// hand the new row back to a client directly, unlike `POST /threads`
    /// (whose response IS the thread). Without this, an already-open chat's
    /// thread tab strip only learns about the new thread on its next full
    /// `loadThreads` refetch (e.g. navigating away and back), even though the
    /// automation's reply is already landing there live via SSE. Carries the
    /// full `Thread` row so the client can append it in place, mirroring how
    /// `TasklistTaskUpdated` carries the full `Task`.
    ThreadCreated {
        thread: Thread,
    },
}

/// Tone hint carried by [`AgentEventPayload::SystemMessage`] — see that
/// field's own doc.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SystemMessageSeverity {
    Success,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunEndReason {
    Completed,
    TimedOut,
    NoOutputTimeout,
    Cancelled,
    /// A native (in-process API) run was force-stopped after reaching its
    /// configured `AgentProfile::max_turns` cap. Distinct from `Cancelled`
    /// (which means the user, or something acting on their behalf, actually
    /// fired the run's cancellation token) — this reason fires when nothing
    /// cancelled the run at all, the turn budget just ran out.
    TurnLimitReached,
    Error,
    Signal,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(payload: AgentEventPayload) -> AgentEventPayload {
        let json = serde_json::to_string(&payload).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn test_workflow_task_created_roundtrip() {
        let payload = AgentEventPayload::WorkflowTaskCreated {
            task_id: "task-1".into(),
            workflow_id: "wf-1".into(),
            project_name: "My Project".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"type\":\"WorkflowTaskCreated\""));
        assert!(json.contains("\"task_id\":\"task-1\""));
        let _: AgentEventPayload = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn test_phase_started_roundtrip() {
        let payload = AgentEventPayload::PhaseStarted {
            task_id: "task-1".into(),
            phase_id: "phase-1".into(),
            phase_name: "Planning".into(),
        };
        let rt = roundtrip(payload);
        match rt {
            AgentEventPayload::PhaseStarted { task_id, phase_id, phase_name } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(phase_id, "phase-1");
                assert_eq!(phase_name, "Planning");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_phase_completed_roundtrip() {
        let payload = AgentEventPayload::PhaseCompleted {
            task_id: "task-1".into(),
            phase_id: "phase-1".into(),
        };
        let rt = roundtrip(payload);
        match rt {
            AgentEventPayload::PhaseCompleted { task_id, phase_id } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(phase_id, "phase-1");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_phase_skipped_roundtrip() {
        let payload = AgentEventPayload::PhaseSkipped {
            task_id: "task-1".into(),
            phase_id: "phase-1".into(),
            reason: "Not needed".into(),
        };
        let rt = roundtrip(payload);
        match rt {
            AgentEventPayload::PhaseSkipped { task_id, phase_id, reason } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(phase_id, "phase-1");
                assert_eq!(reason, "Not needed");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_phase_failed_roundtrip() {
        let payload = AgentEventPayload::PhaseFailed {
            task_id: "task-1".into(),
            phase_id: "phase-1".into(),
            error: "Script failed with exit code 1".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"type\":\"PhaseFailed\""));
        assert!(json.contains("\"error\":\"Script failed with exit code 1\""));
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::PhaseFailed { task_id, phase_id, error } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(phase_id, "phase-1");
                assert_eq!(error, "Script failed with exit code 1");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_phase_paused_roundtrip() {
        let payload = AgentEventPayload::PhasePaused {
            task_id: "task-1".into(),
            phase_id: "phase-2".into(),
            reason: "Missing required inputs: prev_analysis".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"type\":\"PhasePaused\""));
        assert!(json.contains("\"reason\":\"Missing required inputs: prev_analysis\""));
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::PhasePaused { task_id, phase_id, reason } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(phase_id, "phase-2");
                assert!(reason.contains("prev_analysis"));
            }
            _ => panic!("Wrong variant"),
        }
    }

    /// A `SystemMessage` serialized without `severity` (old wire format, or
    /// any of the many emitters that never set it) must deserialize with
    /// `severity: None` and omit the field on the way back out.
    #[test]
    fn system_message_backward_compat_missing_severity() {
        let json = r#"{"type":"SystemMessage","data":{"text":"hello"}}"#;
        let payload: AgentEventPayload = serde_json::from_str(json).unwrap();
        match payload {
            AgentEventPayload::SystemMessage { text, severity } => {
                assert_eq!(text, "hello");
                assert_eq!(severity, None);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn system_message_severity_roundtrip() {
        let payload =
            AgentEventPayload::SystemMessage { text: "converged".into(), severity: Some(SystemMessageSeverity::Success) };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"severity\":\"success\""));
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::SystemMessage { severity, .. } => {
                assert_eq!(severity, Some(SystemMessageSeverity::Success));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_workflow_completed_roundtrip() {
        let payload = AgentEventPayload::WorkflowCompleted {
            task_id: "task-1".into(),
        };
        let rt = roundtrip(payload);
        match rt {
            AgentEventPayload::WorkflowCompleted { task_id } => {
                assert_eq!(task_id, "task-1");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_workflow_task_started_roundtrip() {
        let payload = AgentEventPayload::WorkflowTaskStarted {
            task_id: "task-1".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"type\":\"WorkflowTaskStarted\""));
        assert!(json.contains("\"task_id\":\"task-1\""));
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::WorkflowTaskStarted { task_id } => {
                assert_eq!(task_id, "task-1");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_workflow_task_failed_roundtrip() {
        let payload = AgentEventPayload::WorkflowTaskFailed {
            task_id: "task-1".into(),
            error: "Workflow failed: phases [phase-2] failed".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"type\":\"WorkflowTaskFailed\""));
        assert!(json.contains("\"error\":\"Workflow failed: phases [phase-2] failed\""));
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::WorkflowTaskFailed { task_id, error } => {
                assert_eq!(task_id, "task-1");
                assert!(error.contains("phase-2"));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_agent_action_started_roundtrip() {
        let payload = AgentEventPayload::AgentActionStarted {
            action_id: "action-1".into(),
            kind: "memory_save".into(),
            summary: "Saving memory…".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"type\":\"AgentActionStarted\""));
        assert!(json.contains("\"action_id\":\"action-1\""));
        assert!(json.contains("\"kind\":\"memory_save\""));
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::AgentActionStarted { action_id, kind, summary } => {
                assert_eq!(action_id, "action-1");
                assert_eq!(kind, "memory_save");
                assert_eq!(summary, "Saving memory…");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_agent_action_completed_roundtrip() {
        let payload = AgentEventPayload::AgentActionCompleted {
            action_id: "action-1".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"type\":\"AgentActionCompleted\""));
        assert!(json.contains("\"action_id\":\"action-1\""));
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::AgentActionCompleted { action_id } => {
                assert_eq!(action_id, "action-1");
            }
            _ => panic!("Wrong variant"),
        }
    }

    /// An event serialized without `label` (old wire format) must deserialize
    /// with `label: None` so existing clients aren't broken.
    #[test]
    fn tool_call_started_backward_compat_missing_label() {
        let json = r#"{"type":"ToolCallStarted","data":{"tool_name":"Read","tool_input":{"path":"/tmp/x"}}}"#;
        let payload: AgentEventPayload = serde_json::from_str(json).unwrap();
        match payload {
            AgentEventPayload::ToolCallStarted { label, .. } => {
                assert_eq!(label, None);
            }
            _ => panic!("Wrong variant"),
        }
    }

    /// A `Some` label survives a serialize → deserialize round-trip.
    #[test]
    fn tool_call_started_label_roundtrip() {
        let payload = AgentEventPayload::ToolCallStarted {
            tool_name: "RunSkill".into(),
            tool_input: Some(serde_json::json!({ "skill": "systematic-debugging" })),
            label: Some("Loading skill: Systematic Debugging".into()),
            tool_use_id: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"label\":\"Loading skill: Systematic Debugging\""));
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::ToolCallStarted { label, tool_name, .. } => {
                assert_eq!(tool_name, "RunSkill");
                assert_eq!(label, Some("Loading skill: Systematic Debugging".into()));
            }
            _ => panic!("Wrong variant"),
        }
    }

    /// When `label` is `None`, `skip_serializing_if` must omit the field from the wire.
    #[test]
    fn tool_call_started_none_label_omitted_from_wire() {
        let payload = AgentEventPayload::ToolCallStarted {
            tool_name: "Read".into(),
            tool_input: None,
            label: None,
            tool_use_id: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.contains("\"label\""), "label field must be absent when None");
    }

    /// `project_id` round-trips when set, is absent from the wire when None.
    #[test]
    fn tasklist_created_project_id_roundtrip() {
        let with_pid = AgentEventPayload::TasklistCreated {
            team_id: String::new(),
            tasklist_id: "tl-1".into(),
            owner: None,
            project_id: Some("proj-abc".into()),
        };
        let json = serde_json::to_string(&with_pid).unwrap();
        assert!(json.contains("\"project_id\":\"proj-abc\""));
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::TasklistCreated { project_id, .. } => {
                assert_eq!(project_id, Some("proj-abc".into()));
            }
            _ => panic!("Wrong variant"),
        }
    }

    /// Old wire payloads without `project_id` must deserialize cleanly (default = None).
    #[test]
    fn tasklist_created_backward_compat_missing_project_id() {
        let json = r#"{"type":"TasklistCreated","data":{"team_id":"","tasklist_id":"tl-1"}}"#;
        let payload: AgentEventPayload = serde_json::from_str(json).unwrap();
        match payload {
            AgentEventPayload::TasklistCreated { project_id, .. } => {
                assert_eq!(project_id, None);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn agent_snapshot_updated_roundtrip_with_title() {
        let payload = AgentEventPayload::AgentSnapshotUpdated {
            agent_id: "agent-1".into(),
            active_tasklist_title: Some("Sprint 3 tasks".into()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"type\":\"AgentSnapshotUpdated\""));
        assert!(json.contains("\"agent_id\":\"agent-1\""));
        assert!(json.contains("\"active_tasklist_title\":\"Sprint 3 tasks\""));
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::AgentSnapshotUpdated { agent_id, active_tasklist_title } => {
                assert_eq!(agent_id, "agent-1");
                assert_eq!(active_tasklist_title, Some("Sprint 3 tasks".into()));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn agent_snapshot_updated_none_title_omitted_from_wire() {
        let payload = AgentEventPayload::AgentSnapshotUpdated {
            agent_id: "agent-2".into(),
            active_tasklist_title: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.contains("\"active_tasklist_title\""), "title must be absent when None");
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::AgentSnapshotUpdated { active_tasklist_title, .. } => {
                assert_eq!(active_tasklist_title, None);
            }
            _ => panic!("Wrong variant"),
        }
    }

    /// When `project_id` is `None`, the field must be absent from the wire.
    #[test]
    fn tasklist_status_changed_none_project_id_omitted() {
        let payload = AgentEventPayload::TasklistStatusChanged {
            team_id: "t".into(),
            tasklist_id: "tl-1".into(),
            status: "active".into(),
            owner: None,
            project_id: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.contains("\"project_id\""), "project_id must be absent when None");
    }

    #[test]
    fn thread_created_roundtrip_carries_the_full_thread() {
        use crate::thread::{ThreadKind, ThreadScope};

        let now = Utc::now();
        let thread = Thread {
            id: "thread-1".into(),
            title: None,
            auto_title: None,
            scope: ThreadScope::AgentChat { agent_id: "agent-1".into() },
            transcript_path: "/tmp/thread-1.jsonl".into(),
            kind: ThreadKind::Fresh,
            history_floor_ts: None,
            distilled_through_ts: None,
            promotion_swept_at: None,
            branch_source: None,
            archived_at: None,
            channel_origin: None,
            assignment_origin: None,
            created_at: now,
            updated_at: now,
        };
        let payload = AgentEventPayload::ThreadCreated { thread };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"type\":\"ThreadCreated\""));
        assert!(json.contains("\"id\":\"thread-1\""));
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::ThreadCreated { thread } => {
                assert_eq!(thread.id, "thread-1");
                assert_eq!(thread.kind, ThreadKind::Fresh);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_agent_event_full_roundtrip() {
        let event = AgentEvent {
            event_id: "evt-1".into(),
            run_id: "run-1".into(),
            seq: 42,
            ts: Utc::now(),
            agent_id: "workflow:test-wf".into(),
            thread_id: None,
            payload: AgentEventPayload::PhaseFailed {
                task_id: "task-1".into(),
                phase_id: "phase-2".into(),
                error: "Missing output file".into(),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        let rt: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.event_id, "evt-1");
        assert_eq!(rt.run_id, "run-1");
        assert_eq!(rt.seq, 42);
        match rt.payload {
            AgentEventPayload::PhaseFailed { task_id, phase_id, error } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(phase_id, "phase-2");
                assert_eq!(error, "Missing output file");
            }
            _ => panic!("Wrong variant"),
        }
    }

    /// `FormPosted`'s `spec` must carry the full form — not a placeholder —
    /// so a live client can render the card straight from this event
    /// instead of waiting on a transcript refetch. Mirrors `FormRequest`'s
    /// own field-level shape (`FormFieldEventPayload`/`FormOptionEventPayload`)
    /// so the two variants serialize identically down to the field level.
    #[test]
    fn test_form_posted_roundtrip_carries_spec() {
        let payload = AgentEventPayload::FormPosted {
            form_id: "form-1".into(),
            spec: FormSpecEventPayload {
                form_id: "form-1".into(),
                title: "Rate this".into(),
                intro: Some("Quick check-in".into()),
                fields: vec![FormFieldEventPayload {
                    id: "satisfaction".into(),
                    kind: "radio".into(),
                    label: "How did it go?".into(),
                    description: None,
                    required: true,
                    options: Some(vec![FormOptionEventPayload {
                        id: "good".into(),
                        label: "Good".into(),
                        description: None,
                    }]),
                    placeholder: None,
                    max_files: None,
                    accept: None,
                }],
            },
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"type\":\"FormPosted\""));
        // The spec must be present and non-null on the wire.
        assert!(json.contains("\"spec\":{"));
        assert!(!json.contains("\"spec\":null"));
        let rt: AgentEventPayload = serde_json::from_str(&json).unwrap();
        match rt {
            AgentEventPayload::FormPosted { form_id, spec } => {
                assert_eq!(form_id, "form-1");
                assert_eq!(spec.form_id, "form-1");
                assert_eq!(spec.title, "Rate this");
                assert_eq!(spec.intro.as_deref(), Some("Quick check-in"));
                assert_eq!(spec.fields.len(), 1);
                assert_eq!(spec.fields[0].id, "satisfaction");
                assert_eq!(spec.fields[0].kind, "radio");
            }
            _ => panic!("Wrong variant"),
        }
    }
}
