use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use ao_persistence::artifact_store::ArtifactStore;
use ao_persistence::assignment_store::AssignmentStore;
use ao_persistence::memory::MemoryStore;
use ao_persistence::outcome::OutcomeStore;
use ao_persistence::preferences::UserPreferencesStore;
use ao_persistence::profiles::AgentProfileStore;
use ao_persistence::projects::ProjectStore;
use ao_persistence::reflection_staging::ReflectionStagingStore;
use ao_persistence::snapshot::SnapshotStore;
use ao_persistence::thread_store::ThreadStore;
use ao_persistence::transcript::TranscriptStore;
use ao_protocol::agent::{AgentId, WorkflowBinding};
use ao_protocol::artifact::IntentSource;
use ao_protocol::error::AoError;
use ao_protocol::event::TodoListCreatedItem;
use ao_protocol::outcome::ArtifactRef;
use chrono::{DateTime, Utc};

use crate::assignment_fire_handle::AssignmentFireHandle;
use crate::classifier_handle::{ClassifierHandle, ClassifierInFlight};

use crate::background_agents::{BackgroundAgentRegistry, RunnerEvent};
use crate::background_commands::BackgroundCommandRegistry;
use crate::background_processes::BackgroundProcessRegistry;
use crate::memory_loader::{MemoryLoader, NoopMemoryLoader};
use crate::permissions::{PermissionMode, SessionKind};
use crate::read_file_state::ReadFileState;
use crate::registry::Registry;
use crate::skill_registry::SkillRegistry;
use crate::tasklist_service_handle::TasklistServiceHandle;
use crate::telemetry::{NoopTelemetryWriter, TelemetryWriter};
use crate::workflow_runner_handle::WorkflowRunnerHandle;

/// Default concurrency cap for the per-parent [`BackgroundAgentRegistry`].
pub const DEFAULT_BACKGROUND_AGENT_CAP: usize = 8;

/// Default capacity for the per-context [`BackgroundProcessRegistry`].
pub const DEFAULT_BACKGROUND_PROCESS_CAP: usize = 8;

/// Default capacity for the per-context [`BackgroundCommandRegistry`].
pub const DEFAULT_BACKGROUND_COMMAND_CAP: usize = 16;

/// Interior-mutable handle for the session-wide [`PermissionMode`].
///
/// Methods take and immediately drop the write/read guard so the guard is
/// never returned to a caller and never held across an `await`. Tools that
/// need to read the mode call `mode()`, which returns an owned
/// `PermissionMode` copy. Tools that need to flip the mode (e.g.
/// `EnterPlanMode`) call `set_mode()`, `enter_plan_mode()`, or `exit_plan_mode()`.
///
/// Parent and child contexts share the same `Arc<PermissionStore>`, so a
/// parent flipping plan mode propagates to all children mid-run.
/// (See [`RunnerContext::child`] rustdoc.)
///
/// Lock ordering: always acquire `prior` (Mutex) before `inner` (RwLock)
/// to prevent deadlocks.
pub struct PermissionStore {
    inner: RwLock<PermissionMode>,
    /// The mode to restore on `exit_plan_mode`. `None` means no transition
    /// is in progress (either never entered plan mode, or already exited).
    prior: Mutex<Option<PermissionMode>>,
}

impl PermissionStore {
    /// Read the current mode.
    pub fn mode(&self) -> PermissionMode {
        *self.inner.read().unwrap()
    }

    /// Replace the current mode with `mode`.
    pub fn set_mode(&self, mode: PermissionMode) {
        *self.inner.write().unwrap() = mode;
    }

    /// Transition to `Plan` mode, saving the current mode so `exit_plan_mode`
    /// can restore it.
    ///
    /// Idempotent: a second consecutive call while already in `Plan` is a
    /// no-op and does NOT clobber the saved prior mode.
    pub fn enter_plan_mode(&self) {
        let mut prior_guard = self.prior.lock().unwrap();
        let mut inner_guard = self.inner.write().unwrap();
        if *inner_guard == PermissionMode::Plan {
            return;
        }
        let prev = *inner_guard;
        *inner_guard = PermissionMode::Plan;
        if prior_guard.is_none() {
            *prior_guard = Some(prev);
        }
    }

    /// Restore the mode saved by the most recent `enter_plan_mode` call.
    ///
    /// Idempotent: if the current mode is not `Plan` (either never entered,
    /// or already exited), this is a no-op.
    pub fn exit_plan_mode(&self) {
        let mut prior_guard = self.prior.lock().unwrap();
        let mut inner_guard = self.inner.write().unwrap();
        if *inner_guard != PermissionMode::Plan {
            return;
        }
        if let Some(prev) = prior_guard.take() {
            *inner_guard = prev;
        }
    }
}

impl Default for PermissionStore {
    fn default() -> Self {
        Self {
            inner: RwLock::new(PermissionMode::default()),
            prior: Mutex::new(None),
        }
    }
}

/// Completion state for a single todo item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// A single todo item associated with an agent.
#[derive(Debug, Clone, PartialEq)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
    pub active_form: String,
}

/// Interior-mutable in-memory store for todo items, keyed by `agent_id`.
///
/// Subagent isolation is by `agent_id` key — all contexts (parent and child)
/// share the same underlying `Arc<TodoStore>`. Each agent writes and reads
/// under its own key, so different agents never observe each other's lists
/// through this store. See [`RunnerContext::child`].
///
/// This is the in-memory store, NOT the persistent `tasklist_*`
/// backend (reserved for the `todoV2` flag path).
///
/// Lock discipline: the `Mutex` guard is held only for the duration of the
/// insert/replace/clone — never returned from a method and never held across
/// an `await`.
pub struct TodoStore {
    inner: Mutex<HashMap<String, Vec<TodoItem>>>,
}

impl TodoStore {
    /// Replace the full item list for `agent_id` with `items`.
    ///
    /// Replace-all semantics: any previous list for this key is discarded.
    pub fn replace(&self, agent_id: &str, items: Vec<TodoItem>) {
        self.inner
            .lock()
            .unwrap()
            .insert(agent_id.to_string(), items);
    }

    /// Return a clone of the item list for `agent_id`.
    ///
    /// Returns an empty `Vec` if no items have been written for this key.
    pub fn get(&self, agent_id: &str) -> Vec<TodoItem> {
        self.inner
            .lock()
            .unwrap()
            .get(agent_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Remove all items for `agent_id`.
    pub fn clear(&self, agent_id: &str) {
        self.inner.lock().unwrap().remove(agent_id);
    }
}

impl Default for TodoStore {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

/// Events that a tool may emit during its invocation.
///
/// The consumer — typically a WebSocket transport or a test spy — receives
/// these events through an [`EventSink`] implementation. The real transport
/// lives downstream in the runner consumer; this crate ships only the trait
/// and a no-op default.
///
/// The variant set is non-exhaustive from the tools' perspective: downstream
/// phases may add variants without breaking the core trait.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// A brief status message to display to the user.
    Brief { content: String },
    /// Path to a written plan artifact, forwarded here
    /// so the sink can record or transmit it).
    PlanArtifact { plan_path: PathBuf },
    /// A question directed at the user, with a set of selectable choices.
    Question {
        id: String,
        prompt: String,
        choices: Vec<String>,
    },
    /// Emitted after `TodoWrite` replaces the agent's todo list.
    TodosUpdated {
        count: usize,
        in_progress: usize,
        pending: usize,
        completed: usize,
    },
    /// Emitted when `EnterPlanMode` or `ExitPlanMode` transitions the session
    /// between permission modes. Only fires on an actual transition; idempotent
    /// (no-op) calls emit nothing.
    PermissionModeChanged {
        from: PermissionMode,
        to: PermissionMode,
    },
    /// Emitted when `EnterWorktree` or `ExitWorktree` changes the session cwd.
    /// Only fires on actual cwd changes; the idempotent no-op `ExitWorktree`
    /// (empty stack) emits nothing.
    CwdChanged { from: PathBuf, to: PathBuf },
    /// Emitted by the sync TodoCreate heartbeat while awaiting the terminal
    /// watcher. Carries the minimum data the frontend pill needs. Cadence is
    /// controlled by `HEARTBEAT_CADENCE_SECS` in the TodoCreate tool.
    ToolProgress {
        tasklist_id: String,
        items_done: usize,
        items_total: usize,
        last_terminal_task_title: Option<String>,
    },
    /// Emitted once by TodoCreate after creating an agent-scoped tasklist.
    /// Snapshot of assignment state at emit time — classified items may still
    /// show `None` while the background classifier is in-flight.
    TodoListCreated {
        tasklist_id: String,
        item_count: usize,
        items: Vec<TodoListCreatedItem>,
    },
    /// A form prompt directed at the operator through the [`FormBridge`].
    FormRequest {
        id: String,
        agent_id: String,
        session_id: String,
        title: String,
        intro: Option<String>,
        fields: Vec<FormFieldPayload>,
    },
    /// Emitted right after an async `AskUserQuestionWithForm` call posts a form:
    /// the `form_request` transcript entry has been written and a `pending_forms`
    /// entry recorded on the agent snapshot, scoped to the run's own thread. Lets
    /// operator clients surface the waiting form live, without a manual reload.
    ///
    /// `spec` is the complete form the async tool call produced — same flat
    /// wire shape [`UserEvent::FormRequest`] carries for a sync form — so a
    /// live client can render the card directly from this event.
    FormPosted {
        form_id: String,
        spec: FormSpecPayload,
    },
    /// Emitted by the project agent tools (`ProjectUpdate`, `ProjectComplete`)
    /// after any mutation to the project record — status transitions, spec
    /// writes, name/emoji changes. Carries the post-mutation values so the UI
    /// can refresh the project panel without a full refetch.
    ProjectStateChanged {
        project_id: String,
        status: String,
        name: String,
    },
    /// Emitted by the `RenameThread` tool right after it persists an
    /// explicit title. Lets a subscribed client patch its in-memory thread
    /// row and update the tab strip immediately, instead of waiting for the
    /// next full thread-list refetch.
    ThreadRenamed { thread_id: String, title: String },
}

/// Wire-safe representation of a form field emitted through [`UserEvent::FormRequest`].
///
/// Flat structure with optional extras avoids enum-in-enum serde complexity when
/// crossing the event bus boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormFieldPayload {
    pub id: String,
    /// Lowercase discriminant: `"checkbox"` | `"radio"` | `"text"` | `"textarea"` | `"file"`.
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<FormOptionPayload>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_files: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept: Option<String>,
}

/// A selectable option within a checkbox or radio [`FormFieldPayload`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormOptionPayload {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Full async form spec carried by [`UserEvent::FormPosted`] — same flat
/// wire shape as [`UserEvent::FormRequest`]'s own title/intro/fields (see
/// [`FormFieldPayload`]). Sourced from the async `AskUserQuestionWithForm`
/// tool's own `spec` JSON, which is built from this exact shape (see
/// `ao-engine-tools-engine`'s `ask_user_question_form` tool) — so a posted
/// form serializes identically, field for field, to a requested one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormSpecPayload {
    pub form_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intro: Option<String>,
    pub fields: Vec<FormFieldPayload>,
}

impl From<FormFieldPayload> for ao_protocol::event::FormFieldEventPayload {
    fn from(f: FormFieldPayload) -> Self {
        ao_protocol::event::FormFieldEventPayload {
            id: f.id,
            kind: f.kind,
            label: f.label,
            description: f.description,
            required: f.required,
            options: f.options.map(|opts| {
                opts.into_iter()
                    .map(|o| ao_protocol::event::FormOptionEventPayload {
                        id: o.id,
                        label: o.label,
                        description: o.description,
                    })
                    .collect()
            }),
            placeholder: f.placeholder,
            max_files: f.max_files,
            accept: f.accept,
        }
    }
}

impl From<FormSpecPayload> for ao_protocol::event::FormSpecEventPayload {
    fn from(s: FormSpecPayload) -> Self {
        ao_protocol::event::FormSpecEventPayload {
            form_id: s.form_id,
            title: s.title,
            intro: s.intro,
            fields: s.fields.into_iter().map(Into::into).collect(),
        }
    }
}

/// A structured question to present to the operator through the [`QuestionBridge`].
#[derive(Debug, Clone)]
pub struct QuestionRequest {
    pub question: String,
    pub choices: Vec<Choice>,
    pub agent_id: String,
    pub session_id: String,
}

/// A selectable answer in a [`QuestionRequest`].
#[derive(Debug, Clone)]
pub struct Choice {
    pub id: ChoiceId,
    pub label: String,
    pub description: Option<String>,
}

/// Opaque identifier for a [`Choice`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChoiceId(pub String);

/// Error returned by [`QuestionBridge::ask_question`].
#[derive(Debug)]
pub enum AskQuestionError {
    Cancelled,
    NoOperator,
}

impl std::fmt::Display for AskQuestionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "user cancelled question"),
            Self::NoOperator => write!(f, "bridge has no operator available"),
        }
    }
}

impl std::error::Error for AskQuestionError {}

/// Trait for presenting a structured multiple-choice question to the operator.
///
/// Implement this to route questions to a transport (WebSocket, CLI, test spy).
/// The default is [`NoopQuestionBridge`], which always returns `NoOperator`.
///
/// Override `cancel_pending` in live bridges to drain pending channel senders
/// so awaiting `ask_question` futures resolve with `Err(Cancelled)` on session
/// cancellation.
///
/// NOTE: the `AskUserQuestion` engine tool — formerly the sole production
/// consumer of `ask_question` — was retired in favor of
/// `AskUserQuestionWithForm` (which suspends through [`FormBridge`] instead).
/// This trait survives as the supertrait of the permission-prompt bridge and
/// for its `cancel_pending` cleanup hook; the `ask_question` surface itself is
/// scheduled for teardown.
#[async_trait]
pub trait QuestionBridge: Send + Sync {
    async fn ask_question(&self, request: QuestionRequest) -> Result<ChoiceId, AskQuestionError>;

    fn cancel_pending(&self) {}
}

/// No-op [`QuestionBridge`] that always returns `Err(NoOperator)`.
pub struct NoopQuestionBridge;

#[async_trait]
impl QuestionBridge for NoopQuestionBridge {
    async fn ask_question(&self, _request: QuestionRequest) -> Result<ChoiceId, AskQuestionError> {
        Err(AskQuestionError::NoOperator)
    }
}

// ─── Form types ───────────────────────────────────────────────────────────────

/// A complete form to present to the operator through [`FormBridge`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormRequest {
    /// Routing identifier minted by [`FormBridge::ask_form`].
    pub id: String,
    pub agent_id: String,
    pub session_id: String,
    pub title: String,
    pub intro: Option<String>,
    pub fields: Vec<FormField>,
}

/// A single field within a [`FormRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormField {
    pub id: String,
    pub kind: FormFieldKind,
    pub label: String,
    pub description: Option<String>,
    pub required: bool,
}

/// Type and type-specific metadata for a [`FormField`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FormFieldKind {
    Checkbox {
        options: Vec<FormOption>,
    },
    Radio {
        options: Vec<FormOption>,
    },
    Text {
        placeholder: Option<String>,
    },
    Textarea {
        placeholder: Option<String>,
    },
    File {
        max_files: u8,
        accept: Option<String>,
    },
}

/// A selectable option within a checkbox or radio [`FormField`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

impl From<&FormOption> for FormOptionPayload {
    fn from(opt: &FormOption) -> Self {
        FormOptionPayload {
            id: opt.id.clone(),
            label: opt.label.clone(),
            description: opt.description.clone(),
        }
    }
}

/// Flatten a typed [`FormField`] into the wire-safe [`FormFieldPayload`] the UI
/// consumes — a string `kind` discriminant with the type-specific extras hoisted
/// to top-level fields. Both the synchronous (`UserEvent::FormRequest`) and async
/// (posted-form `spec`) paths must funnel through this single conversion; if they
/// don't, the two representations drift and the renderer silently drops controls
/// it can't match against `field.kind`.
impl From<&FormField> for FormFieldPayload {
    fn from(field: &FormField) -> Self {
        let (kind, options, placeholder, max_files, accept) = match &field.kind {
            FormFieldKind::Checkbox { options } => (
                "checkbox",
                Some(options.iter().map(FormOptionPayload::from).collect()),
                None,
                None,
                None,
            ),
            FormFieldKind::Radio { options } => (
                "radio",
                Some(options.iter().map(FormOptionPayload::from).collect()),
                None,
                None,
                None,
            ),
            FormFieldKind::Text { placeholder } => ("text", None, placeholder.clone(), None, None),
            FormFieldKind::Textarea { placeholder } => {
                ("textarea", None, placeholder.clone(), None, None)
            }
            FormFieldKind::File { max_files, accept } => {
                ("file", None, None, Some(*max_files), accept.clone())
            }
        };
        FormFieldPayload {
            id: field.id.clone(),
            kind: kind.to_string(),
            label: field.label.clone(),
            description: field.description.clone(),
            required: field.required,
            options,
            placeholder,
            max_files,
            accept,
        }
    }
}

/// The operator's complete response to a [`FormRequest`].
///
/// Normally this carries `answers` from a real submission. It can instead
/// carry an `action` — the operator clicked Cancel / Regenerate / Something
/// else on the form UI instead of filling it in — in which case `answers` is
/// empty and the tool surfaces the action to the agent so it can react
/// in-turn rather than blindly retrying the same form.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FormResponse {
    pub form_id: String,
    /// Keyed by field `id`. Fields absent from the map were left unanswered
    /// (only valid when `required: false` for that field). Empty when
    /// `action` is `Some`.
    #[serde(default)]
    pub answers: HashMap<String, FormAnswer>,
    /// Set when the operator used one of the form's action buttons instead of
    /// submitting. `None` for a normal answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<FormAction>,
    /// Optional free-text note accompanying `action` (currently unpopulated by
    /// the UI — reserved for a future inline note field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A non-submit action the operator can take on a live [`FormRequest`] from
/// the form UI's action row, instead of filling in and submitting the form.
///
/// IMPORTANT: `Cancel` here means "the operator declined to answer, but the
/// agent should keep going and react" — it is delivered through the normal
/// `Ok(FormResponse { action: Some(Cancel), .. })` path. This is NOT the same
/// thing as [`AskQuestionError::Cancelled`], which tears the whole session
/// down (session-level abort, e.g. the run was stopped). Never map a Cancel
/// button click to `Err(AskQuestionError::Cancelled)` — that would kill the
/// turn instead of letting the agent respond to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormAction {
    /// The operator doesn't want to answer this form right now.
    Cancel,
    /// The operator wants a different form — the questions asked weren't the
    /// right ones.
    Regenerate,
    /// The operator wants something not covered by the offered fields; expect
    /// them to explain in chat right after this.
    Other,
}

/// One field's answer within a [`FormResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FormAnswer {
    /// Answer for `text` and `textarea` fields.
    Text(String),
    /// Selected option ids for `checkbox` and `radio` fields.
    Selections(Vec<String>),
    /// Attachment ids for `file` fields.
    Files(Vec<String>),
}

/// Trait the `AskUserQuestionWithForm` engine tool uses to present structured forms.
///
/// A separate trait from [`QuestionBridge`] keeps form and question paths fully
/// independent — adding `ask_form` to the existing trait would break every impl
/// and mix unrelated return types.
///
/// Override `cancel_pending` in live bridges to drain pending oneshot senders so
/// awaiting `ask_form` futures resolve with `Err(Cancelled)` on session end.
#[async_trait]
pub trait FormBridge: Send + Sync {
    async fn ask_form(&self, request: FormRequest) -> Result<FormResponse, AskQuestionError>;

    /// Drain all pending senders. Parked `ask_form` futures resolve with
    /// `Err(Cancelled)`. Default is a no-op.
    fn cancel_pending(&self) {}
}

/// No-op [`FormBridge`] that always returns `Err(NoOperator)`.
pub struct NoopFormBridge;

#[async_trait]
impl FormBridge for NoopFormBridge {
    async fn ask_form(&self, _: FormRequest) -> Result<FormResponse, AskQuestionError> {
        Err(AskQuestionError::NoOperator)
    }
}

/// Sink for tool-emitted [`UserEvent`]s.
///
/// Implement this trait to route events to a transport (WebSocket, test spy,
/// etc.). The runtime default is [`NoopEventSink`], which discards all events.
///
/// # Error handling
///
/// When `emit` returns `Err`, the consuming tool is expected to propagate the
/// error as `ToolOutput::Error { recoverable: true }` rather than terminating
/// the run. The trait only documents the expectation; enforcement is the
/// tool's responsibility.
///
/// # Object safety
///
/// The trait is object-safe; callers hold it as `Arc<dyn EventSink + Send + Sync>`.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Emit a single event. The implementation must not hold any lock across
    /// the `await` point.
    async fn emit(&self, event: UserEvent) -> Result<(), AoError>;
}

/// No-op [`EventSink`] implementation that discards all events.
///
/// Used as the default sink in [`RunnerContext`] so that tools work without
/// a real transport wired up. Test setups that need to verify emitted events
/// should supply a custom spy implementation via
/// [`RunnerContext::with_event_sink`].
pub struct NoopEventSink;

#[async_trait]
impl EventSink for NoopEventSink {
    async fn emit(&self, _event: UserEvent) -> Result<(), AoError> {
        Ok(())
    }
}

/// Two-tier queue of user-role messages pending injection after the current turn.
///
/// Normal-priority messages (from inline skill dispatch etc.) drain on every turn
/// boundary. Low-priority messages (background delegate completion notices etc.)
/// also drain on every turn boundary in Interactive sessions; in Autonomous sessions
/// they are held until the first turn where the Sleep tool did NOT run — this lets a
/// sleeping agent batch several completion notices without issuing a new provider
/// request mid-sleep.
pub struct PendingMessageQueue {
    normal: VecDeque<String>,
    low: VecDeque<String>,
}

impl PendingMessageQueue {
    pub fn new() -> Self {
        Self {
            normal: VecDeque::new(),
            low: VecDeque::new(),
        }
    }

    /// Enqueue a normal-priority message. Always drained at the next turn boundary.
    pub fn enqueue(&mut self, msg: String) {
        self.normal.push_back(msg);
    }

    /// Enqueue a low-priority message.
    ///
    /// In Interactive sessions this drains at the same turn boundary as normal
    /// messages. In Autonomous sessions it is held until the first turn boundary
    /// where [`SessionKind::Autonomous`] saw no `Sleep` call (i.e. `sleep_ran` is
    /// `false`).
    pub fn enqueue_low(&mut self, msg: String) {
        self.low.push_back(msg);
    }

    /// Drain messages appropriate for the current turn boundary.
    ///
    /// - Normal-priority items always drain.
    /// - Low-priority items drain when: `kind == Interactive` OR `!sleep_ran`.
    pub fn drain_for(&mut self, kind: SessionKind, sleep_ran: bool) -> Vec<String> {
        let mut out: Vec<String> = self.normal.drain(..).collect();
        if kind == SessionKind::Interactive || !sleep_ran {
            out.extend(self.low.drain(..));
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.normal.is_empty() && self.low.is_empty()
    }

    pub fn len(&self) -> usize {
        self.normal.len() + self.low.len()
    }

    pub fn front(&self) -> Option<&String> {
        self.normal.front().or_else(|| self.low.front())
    }

    /// Push a normal-priority message. Alias for [`enqueue`] — used by tests
    /// that work with the queue directly.
    pub fn push_back(&mut self, msg: String) {
        self.normal.push_back(msg);
    }

    /// Iterate over all messages: normal-priority first, then low-priority.
    pub fn iter(&self) -> impl Iterator<Item = &String> {
        self.normal.iter().chain(self.low.iter())
    }
}

impl std::ops::Index<usize> for PendingMessageQueue {
    type Output = String;
    fn index(&self, idx: usize) -> &String {
        if idx < self.normal.len() {
            &self.normal[idx]
        } else {
            &self.low[idx - self.normal.len()]
        }
    }
}

impl Default for PendingMessageQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-invocation context handed to every tool. Owns the things tools need
/// access to but should not reach into the runner for directly:
///
/// - Identity of the caller (session + agent).
/// - The cancellation token for the current turn.
/// - A handle to the registry so meta-tools (`ToolSearch`, `Agent`) can
///   enumerate or look up other tools.
/// - Recursion depth, used by the `Agent` tool to enforce the spawn cap.
/// - The current working directory, interior-mutable so tools holding
///   `&RunnerContext` in parallel can read or observe cwd changes without
///   contending on ownership.
/// - The session-wide permission mode handle, shared between parent and
///   child contexts so a plan-mode flip propagates to all live children.
///
/// Handles for subsystems a tool may or may not have available (tasklist,
/// classifier, staging queue, …) are all `Option`: a registry built for a
/// minimal session simply leaves them unset rather than requiring every
/// caller to construct the whole world.
/// Agent-level admission gate for tools, derived from the agent profile's
/// `ToolsConfig` before session startup.
///
/// This is orthogonal to deferred loading: it decides which tools the agent is
/// *permitted* to see at all, while `always_load_tools` decides which permitted
/// tools are presented eagerly versus flagged for lazy `ToolSearch` activation.
/// The gate is applied once when the per-turn tool array is built, so a denied
/// tool never reaches the model — neither eagerly nor as a deferred entry — and
/// cannot be smuggled in via `ToolSearch` activation.
///
/// `allow` and `deny` are deliberately distinct world models:
/// - [`ToolAdmission::Allow`] is closed-world — only the named tools are
///   admitted; everything else is excluded.
/// - [`ToolAdmission::Deny`] is open-world — every registered tool is admitted
///   except the named ones. Modeling deny as an exclusion set (rather than a
///   pre-computed allow set) keeps it correct for tools that are registered
///   after the gate is computed, such as the autonomous-only tools added at
///   session init.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolAdmission {
    /// Only these tool names are admitted. An empty set admits nothing.
    Allow(HashSet<String>),
    /// Every registered tool is admitted except these names.
    Deny(HashSet<String>),
}

impl ToolAdmission {
    /// Whether a tool of the given name is permitted under this gate.
    pub fn permits(&self, name: &str) -> bool {
        match self {
            ToolAdmission::Allow(set) => set.contains(name),
            ToolAdmission::Deny(set) => !set.contains(name),
        }
    }
}

/// Metadata captured when `EnterWorktree` switches into a git worktree.
///
/// Stored on the `worktree_stack` so `ExitWorktree` can restore the prior
/// working directory and, when requested, clean up the branch and directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// The working directory to restore when exiting this worktree.
    pub restore_cwd: PathBuf,
    /// Absolute path to the worktree directory on disk.
    pub worktree_path: PathBuf,
    /// Name of the git branch created for this worktree (e.g. `worktree/slug`).
    pub branch: String,
    /// The commit SHA that `HEAD` resolved to when the worktree was created.
    pub base_commit: String,
}

#[derive(Clone)]
pub struct RunnerContext {
    pub session_id: String,
    pub agent_id: String,
    pub depth: usize,
    pub cancel: CancellationToken,
    pub registry: Arc<Registry>,
    /// The current working directory for this context.
    ///
    /// Lock discipline: lock, clone the `PathBuf`, drop the guard, then do
    /// IO. Never hold the read guard across an `await` — doing so blocks
    /// the runtime thread.
    pub cwd: Arc<RwLock<PathBuf>>,
    /// The session-wide permission mode, shared between parent and child
    /// contexts. When the `EnterPlanMode` tool flips this to
    /// `PermissionMode::Plan`, all children sharing the same `Arc` see the
    /// change immediately.
    ///
    /// Read via `ctx.permissions.mode()`; write via `ctx.permissions.set_mode(m)`.
    pub permissions: Arc<PermissionStore>,
    /// In-memory todo list store, keyed by `agent_id`.
    ///
    /// Parent and child contexts share the same `Arc<TodoStore>`. Subagent
    /// isolation is by `agent_id` key — each agent reads and writes only its
    /// own slice of the store. See [`RunnerContext::child`].
    pub todos: Arc<TodoStore>,
    /// Sink for tool-emitted [`UserEvent`]s.
    ///
    /// Defaults to [`NoopEventSink`], which discards all events. Supply a
    /// custom implementation via [`RunnerContext::with_event_sink`] to route
    /// events to a transport or test spy.
    ///
    /// Parent and child contexts share the same `Arc<dyn EventSink + Send + Sync>`.
    /// See [`RunnerContext::child`].
    pub event_sink: Arc<dyn EventSink + Send + Sync>,
    /// Stack of active worktree entries for `EnterWorktree` / `ExitWorktree`.
    ///
    /// Push-prior / pop-restore semantic: `EnterWorktree` pushes a
    /// [`WorktreeEntry`] carrying the old cwd and worktree metadata;
    /// `ExitWorktree` pops the top entry and restores `cwd` to
    /// `entry.restore_cwd`. Index 0 holds the oldest (session-start) entry.
    ///
    /// Parent and child contexts share the same `Arc`, matching the `cwd`
    /// Arc-sharing semantic so that a worktree switch in the parent is visible
    /// to children and vice versa.
    ///
    /// The stack is **in-memory only**. If the session crashes, the stack is
    /// lost and the host is responsible for any session-restart cwd reset.
    pub worktree_stack: Arc<Mutex<Vec<WorktreeEntry>>>,
    /// Bridge for presenting structured questions to the operator.
    ///
    /// Parent and child contexts share the same `Arc<dyn QuestionBridge>`.
    /// Defaults to [`NoopQuestionBridge`], which always returns `NoOperator`.
    /// Supply a `LiveBridge` (or any `QuestionBridge` impl) via
    /// [`RunnerContext::with_prompt_bridge`] to wire up real interactivity.
    pub prompt_bridge: Arc<dyn QuestionBridge + Send + Sync>,
    /// Bridge for presenting structured forms to the operator.
    ///
    /// Parent and child contexts share the same `Arc<dyn FormBridge>`.
    /// Defaults to [`NoopFormBridge`], which always returns `NoOperator`.
    /// Supply a `LiveFormBridge` via [`RunnerContext::with_form_bridge`] when a
    /// live operator can answer forms (desktop app native-runner path).
    pub form_bridge: Arc<dyn FormBridge + Send + Sync>,
    /// Chain of subagent type names from the root down to this context.
    ///
    /// Empty for a top-level runner. Each time a subagent is spawned, the
    /// child's spawn_chain is extended by the spawner with the subagent type
    /// name. Used by the recursion guard to detect name-recursion cycles.
    ///
    /// Full construction logic (with chain extension) lives in
    /// `SubagentSpawner::build_child_context`.
    pub spawn_chain: Vec<String>,
    /// Chain of agent profile IDs from the root down to this context.
    ///
    /// Empty for a top-level runner. Each time a Delegate tool spawns a child,
    /// `SubagentSpawner::build_child_context` clones the parent's chain and
    /// pushes the parent's `agent_id` before constructing the child context.
    /// Mirrors `spawn_chain` but keyed on `AgentProfile.id` rather than
    /// `subagent_type` — the two namespaces do not overlap.
    ///
    /// Used by the Delegate tool for depth-cap enforcement (cap = 8).
    pub delegate_chain: Vec<AgentId>,
    /// Per-parent registry of live background agent handles.
    ///
    /// Holds all children spawned by this context. Enforces the concurrency
    /// cap, supports snapshot-based lookup, and cascades cancellation on
    /// parent teardown.
    ///
    /// Each context gets its own `Arc<BackgroundAgentRegistry>` (not shared
    /// with the parent) so grandchildren are tracked under their direct parent.
    pub background_agents: Arc<BackgroundAgentRegistry>,
    /// Per-context registry of live background subprocess handles (legacy).
    ///
    /// Holds processes spawned by the Bash tool's `run_in_background` mode.
    /// Each context gets its own `Arc<BackgroundProcessRegistry>` — not shared
    /// with the parent.
    ///
    /// New code should use `background_commands` instead. This field is
    /// retained for backward compatibility with existing tests.
    pub background_processes: Arc<BackgroundProcessRegistry>,
    /// Per-session registry of live background command handles.
    ///
    /// Holds commands spawned by the Bash tool's `run_in_background` path.
    /// Each context gets its own `Arc<BackgroundCommandRegistry>` — not shared
    /// with child contexts (mirrors the `background_agents` pattern).
    ///
    /// The future BashStatus and BashKill tools read and mutate this registry
    /// through the same `Arc`, so every tool invocation in the session sees
    /// the current set of registered commands without extra plumbing.
    pub background_commands: Arc<BackgroundCommandRegistry>,
    /// Shared memory loader for all five memory categories (user, feedback,
    /// project, reference, global).
    ///
    /// Parent and child share the same `Arc<dyn MemoryLoader>` so the child
    /// reads memory through the same loader instance. `build_child_context`
    /// uses this to assemble the child's resolved system prompt.
    /// Defaults to [`NoopMemoryLoader`].
    pub memory_loader: Arc<dyn MemoryLoader>,
    /// The resolved system prompt for this runner context.
    ///
    /// For top-level runners this is set by the bootstrap path
    /// (e.g. from `RunnerConfig`). For child runners it is assembled by
    /// `SubagentSpawner::build_child_context` as:
    ///   parent_system_prompt + memory_blob + definition.system_prompt_fragment
    ///
    /// `None` means no system prompt is in effect (the provider default applies).
    pub system_prompt: Option<String>,
    /// The parent agent's own event stream.
    ///
    /// Observers subscribe via `runner_events.subscribe()` to receive events
    /// emitted by this agent (e.g., [`RunnerEvent::AsyncLaunched`] when a
    /// background child is launched). Each context gets a fresh independent
    /// sender; child contexts do not share the parent's sender.
    pub runner_events: Arc<broadcast::Sender<RunnerEvent>>,
    /// The loaded skill registry for this agent, populated from user and plugin pools.
    ///
    /// Parent and child share the same `Arc<SkillRegistry>` so skills are
    /// visible to all contexts without redundant re-loading. The RunSkill
    /// tool replaces this Arc when `SkillRegister` creates a new skill.
    pub skill_registry: Arc<RwLock<Arc<SkillRegistry>>>,
    /// Whether this is an attended (Interactive) or unattended (Autonomous) session.
    ///
    /// Tools read this to opt into autonomous-only behaviour. The runner reads it
    /// to gate tool registration, system-prompt pacing injection, drain priority,
    /// and permission-ask resolution. Set once at session start; never changed
    /// mid-session. Default is `Interactive`.
    pub kind: SessionKind,
    /// Per-turn flag recording whether the `Sleep` tool executed during the current
    /// batch of tool calls.
    ///
    /// The Sleep tool sets this to `true` via [`RunnerContext::set_sleep_ran`] when
    /// it completes. The runner resets it to `false` at the start of every new turn
    /// via [`RunnerContext::reset_sleep_ran`]. The drain logic reads it once per turn
    /// boundary to decide whether to release low-priority pending messages.
    ///
    /// Parent and child contexts do NOT share this flag — only the owning session's
    /// drain loop reads it, and child sessions have their own Sleep calls.
    pub sleep_ran: Arc<AtomicBool>,
    /// Two-tier queue of user-role messages to inject after the current assistant turn.
    ///
    /// Normal-priority messages (inline skill bodies, direct enqueues) always drain at
    /// the next turn boundary. Low-priority messages (background delegate completion
    /// notices) drain immediately in Interactive sessions but are held across Sleep
    /// calls in Autonomous sessions until the first Sleep-free turn boundary.
    ///
    /// Parent and child share the same Arc so inline skill dispatch from a subagent
    /// context is always visible to the owning runner.
    pub pending_user_messages: Arc<Mutex<PendingMessageQueue>>,
    /// Per-turn tool filter set by inline skill dispatch.
    ///
    /// `Some(set)` means only the named tools (plus `RunSkill`/`SkillRegister`)
    /// may execute during the current assistant turn. `None` means no filter
    /// is active. The runner executor clears this to `None` at each turn
    /// boundary.
    pub skill_tool_filter: Arc<RwLock<Option<HashSet<String>>>>,
    /// Whether inline-skill bodies must be returned via the tool result rather
    /// than enqueued onto [`Self::pending_user_messages`].
    ///
    /// `false` (default) is the in-process runner contract: the query loop owns
    /// the turn sequence and drains the pending-message queue at each turn
    /// boundary, so an inline skill body enqueued mid-turn surfaces as a hidden
    /// user-role follow-up (and a "Loaded skill" chip).
    ///
    /// `true` is the single-call dispatch contract used by the MCP HTTP route:
    /// the context lives for exactly one tool call and is dropped on return, so
    /// nothing ever drains the queue. An externally-driven agent (a CLI binary
    /// reaching us over MCP) only observes the tool result, so the inline skill
    /// body must be delivered there or it is silently lost. Set per-request by
    /// the route handler; never inherited by child contexts (a spawned subagent
    /// runs its own draining loop).
    pub inline_skill_via_tool_result: bool,
    /// Agent-level admission gate, pre-computed from the agent profile's
    /// `ToolsConfig` before session startup.
    ///
    /// `None` — no agent-level filter; every registered tool is admitted.
    /// `Some(gate)` — only tools the gate permits are presented to the model.
    /// See [`ToolAdmission`] for the closed-world (`Allow`) vs open-world
    /// (`Deny`) semantics. An empty `Allow(∅)` is legal and means the model
    /// receives an empty tools array (a warn fires at computation time).
    ///
    /// Not inherited by child contexts — each subagent session starts with `None`
    /// and is filtered by its own profile's ToolsConfig if applicable.
    pub tool_admission: Option<ToolAdmission>,
    /// The set of tool names that are resolved to always-loaded for this session.
    ///
    /// Computed once at session startup from `Registry::resolved_loaded_set` with
    /// the user's `tool_load_overrides`. Shared between parent and child via
    /// Arc-clone; child contexts inherit the same resolved set without re-computing.
    pub always_load_tools: Arc<HashSet<String>>,
    // FUTURE: activated_tools telemetry will feed agent-scoped auto-promotion policy
    /// Runtime-activated deferred tools for this session.
    ///
    /// `ToolSearch` inserts tool names here when the model selects them. The
    /// runner includes activated tools in the next turn's tool array alongside
    /// the always-loaded set. Parent and child share the same `Arc<Mutex<…>>`
    /// so a subagent activation is immediately visible to the parent.
    pub activated_tools: Arc<Mutex<HashSet<String>>>,
    /// Deferred tools resolved by ToolSearch in this session.
    ///
    /// ToolSearch inserts a tool name here when the model selects it. The
    /// runner then emits that tool WITHOUT `defer_loading` (Anthropic) or
    /// includes it in the `tools[]` array (OpenAI/Gemini) on the next turn.
    ///
    /// Parent and child share the same `Arc<RwLock<…>>` so a subagent
    /// resolution is visible to the parent runner on its next turn.
    pub loaded_deferred_tools: Arc<RwLock<HashSet<String>>>,
    /// Sink for tool usage telemetry (selections and invocations).
    ///
    /// Defaults to [`NoopTelemetryWriter`], which discards all events. Supply
    /// a concrete writer (e.g. `JsonlTelemetryWriter`) via
    /// `.with_telemetry()`. Parent and child share the same `Arc` so events
    /// from subagents flow to the same writer.
    pub telemetry: Arc<dyn TelemetryWriter + Send + Sync>,
    /// Per-session map of the most-recent read snapshot for each file.
    ///
    /// Populated by the `Read` tool on every successful read. Consumed by
    /// `Edit` and `Write` to enforce read-before-write and detect on-disk
    /// staleness. Parent and child contexts share the same `Arc<ReadFileState>`
    /// so a parent's read allows a child's edit without an extra `Read`
    /// round-trip — the same Arc-share pattern used for `cwd`, `permissions`,
    /// and `todos`. A future subagent-isolation refactor must NOT split this
    /// map per-agent; the shared view is intentional.
    pub read_file_state: Arc<ReadFileState>,
    /// Handle to the workflow runner for WorkflowAction* tools.
    ///
    /// `None` in test fixtures, CLI-runner contexts, and subagent contexts that
    /// are not workflow-capable. Tools that require this field return
    /// `ToolOutput::Error { recoverable: false }` when it is `None`.
    ///
    /// Defined as `dyn WorkflowRunnerHandle` (trait object) rather than the
    /// concrete `ao_engine::WorkflowRunner` to avoid a circular crate dependency:
    /// `ao-engine` already depends on `ao-engine-tools-core` for `RunnerContext`,
    /// so `ao-engine-tools-core` cannot in turn depend on `ao-engine`. The trait
    /// is defined here; `ao-engine` implements it on its concrete runner.
    pub workflow_runner: Option<Arc<dyn WorkflowRunnerHandle + Send + Sync>>,
    /// Handle to the user-preferences store for timezone resolution in
    /// Assignment* tools.
    ///
    /// `None` in test fixtures and contexts where preferences are not available.
    pub preferences: Option<Arc<UserPreferencesStore>>,
    /// Handle to the assignment store for Assignment* tools.
    ///
    /// `None` in test fixtures and contexts where assignments are not available.
    pub assignment_store: Option<Arc<AssignmentStore>>,
    /// Handle to fire an assignment immediately (`AssignmentTrigger`'s
    /// fire-now capability), abstracted to avoid a circular dependency on
    /// `ao-engine`. See [`AssignmentFireHandle`].
    ///
    /// `None` in test fixtures and contexts where assignment firing is not wired.
    pub assignment_fire: Option<Arc<dyn AssignmentFireHandle + Send + Sync>>,
    /// The bound workflows for the running agent, taken from `AgentProfile.workflows`.
    ///
    /// Used by `WorkflowActionCreate` to enforce the binding gate: the agent may
    /// only create tasks for workflows it is explicitly bound to (or `All`).
    /// `None` means the agent has no workflow binding (guest / unbound agent).
    pub agent_workflows: Option<WorkflowBinding>,
    /// Handle to the memory store for the Memory tool family.
    ///
    /// `None` in test fixtures and contexts where memory persistence is not available.
    pub memory_store: Option<Arc<MemoryStore>>,
    /// Handle to the artifact store for the `ArtifactWrite` tool.
    ///
    /// `None` in test fixtures and contexts where artifact persistence is
    /// not available — `ArtifactWrite` returns a recoverable error in that
    /// case, mirroring `memory_store`'s fail-open convention.
    pub artifact_store: Option<Arc<ArtifactStore>>,
    /// Id of the message this context's turn is producing, when the caller
    /// has pre-allocated one. Read (not written) by `ArtifactWrite` to stamp
    /// `ArtifactRecord.source_message_id` so a thread bubble can resolve its
    /// own artifact inline — the agent never supplies this itself.
    ///
    /// `None` in test fixtures and any caller that hasn't wired per-turn
    /// message-id pre-allocation yet; an artifact written under a `None`
    /// context simply has no inline thread linkage, which is a supported
    /// (not an error) state — see `ArtifactRecord::source_message_id`.
    pub current_message_id: Option<String>,
    /// Which [`IntentSource`] an in-place `ArtifactWrite(id=...)` call made
    /// under this context should be tagged with, when the spawn context
    /// already knows. Read (not written) by `ArtifactWrite`; stamped from
    /// context the same way `current_message_id` is, never taken from model
    /// input.
    ///
    /// `None` means "let `ArtifactWrite` fall back to its normal-conversation
    /// default" — this covers the main agent thread and any spawn path that
    /// doesn't set it explicitly. `spawn_artifact_agent` sets this to
    /// `Some(IntentSource::Regenerate)` for a whole-artifact regenerate run so
    /// its `ArtifactWrite` call is labeled correctly instead of defaulting.
    pub artifact_intent_source: Option<IntentSource>,
    /// Handle to the transcript store for the RecallHistory tool.
    ///
    /// `None` in test fixtures and contexts that don't need backward history extension.
    pub transcript_store: Option<Arc<TranscriptStore>>,
    /// Handle to the per-turn outcome-record store (self-improvement
    /// The query loop persists one [`ao_protocol::outcome::OutcomeRecord`]
    /// through this handle when a turn completes naturally.
    ///
    /// `None` in test fixtures and contexts that don't wire outcome
    /// persistence — in that case the query loop skips the write silently.
    pub outcome_store: Option<Arc<OutcomeStore>>,
    /// Handle to the staging queue (durable "not live yet"
    /// candidate record — see [`ReflectionStagingStore`]). The
    /// reflection pass was this store's first writer; the `MemoryWrite` tool
    /// (`ao_engine_tools_engine::memory::write`) is the second — every
    /// `StagingTier::StageForReview` verdict it produces is staged here too,
    /// so a candidate the trust gate marks for human review is never
    /// silently dropped from the tool result alone.
    ///
    /// `None` in test fixtures and contexts that don't wire staging — in
    /// that case a `StageForReview` write's tool result still reports
    /// `"staged": true`, but nothing durable is recorded (matches this
    /// field's sibling stores' fail-open convention).
    pub reflection_staging: Option<Arc<ReflectionStagingStore>>,
    /// Artifacts (memory entries, skills) this context has drawn on so far
    /// during the current turn.
    ///
    /// Populated two ways: the memory-surfacing call site records one
    /// [`ArtifactRef::memory`] per entry injected into the system prompt
    /// before `run_session` starts, and the query loop records one
    /// [`ArtifactRef::skill`] per successful `RunSkill` invocation as the
    /// turn runs. Read once at turn end to build that turn's
    /// [`ao_protocol::outcome::OutcomeRecord`].
    ///
    /// Fresh (empty) on every context, including children created via
    /// [`Self::child`] — each `run_session` call is its own turn with its
    /// own outcome record, so a child's artifact usage belongs to the
    /// child's own record, not the parent's.
    pub artifacts_used: Arc<Mutex<Vec<ArtifactRef>>>,
    /// Timestamp of the oldest message in the current loaded history window.
    ///
    /// Set by NativeAgentRunner after `history::select()`; used by RecallHistory to
    /// return messages immediately before this floor. `None` means no window has been
    /// loaded (e.g. test fixtures), in which case RecallHistory operates on all history.
    pub window_floor_ts: Option<DateTime<Utc>>,
    /// Optional transcript path that RecallHistory should read from when the
    /// agent is not running on its default chat history (e.g. a thread that
    /// owns a dedicated JSONL file, or a branch thread whose pre-floor history
    /// lives in the source thread's transcript). When `None`, RecallHistory
    /// falls back to the agent-keyed default path so single-thread agents stay
    /// byte-equivalent.
    pub recall_transcript_path: Option<PathBuf>,
    /// Handle to the tasklist service for Todo* tools.
    ///
    /// `None` in test fixtures and contexts where tasklist management is not available.
    /// Defined as a trait object to avoid a circular crate dependency with `ao-engine`.
    pub tasklist_service: Option<Arc<dyn TasklistServiceHandle + Send + Sync>>,
    /// Handle to the task classifier for address-book-routed classification.
    ///
    /// `None` in test fixtures and contexts that don't perform task classification.
    /// Defined as a trait object to avoid a circular crate dependency with `ao-engine`.
    pub classifier: Option<Arc<dyn ClassifierHandle + Send + Sync>>,
    /// Process-wide dedup registry for in-flight classifier attempts.
    ///
    /// Shared with the periodic classifier reconciler so concurrent ticks can
    /// not re-spawn a task that an event-driven spawn (Todo* tool, HTTP route)
    /// is already classifying. `None` in test fixtures that don't care about
    /// dedup — `classify_with_retry` then proceeds without a claim.
    pub classifier_in_flight: Option<Arc<ClassifierInFlight>>,
    /// Handle to the agent profile store, used by the Todo* tools to resolve
    /// an `owner` value (agent_id or address-book display name) to a
    /// canonical agent_id at task-creation/update time.
    ///
    /// `None` in test fixtures and contexts that don't wire it — owner
    /// resolution then falls back to treating the raw `owner` string as
    /// already-canonical, matching the pre-resolution behavior for those
    /// contexts.
    pub agent_profile_store: Option<Arc<AgentProfileStore>>,
    /// The session_id of the parent session that spawned this context.
    ///
    /// `None` for top-level sessions. Set by `SubagentSpawner::build_child_context`
    /// and `build_delegate_context` so `NativeChildRunner` can register the child
    /// session entry with correct parent linkage.
    pub parent_session_id: Option<String>,
    /// The agent_id of the parent that spawned this context.
    ///
    /// `None` for top-level sessions.
    pub parent_agent_id: Option<String>,
    /// Snapshot of the parent's current_cwd at delegation time.
    ///
    /// Used by `NativeChildRunner` when registering the child session so the
    /// memory scope resolver can default project-scope writes to the
    /// parent's project context. `None` for top-level sessions.
    pub parent_current_cwd: Option<PathBuf>,
    /// Handle to the agent snapshot store for recording a `pending_forms`
    /// entry when the runner posts an async form.
    ///
    /// `None` in test fixtures and non-native runner contexts.
    pub snapshot_store: Option<Arc<SnapshotStore>>,
    /// Optional durable-queue notification sink for delegate completion over
    /// the MCP route.
    ///
    /// When present, `spawn_named_async` dispatches a `QueuedMessage` to the
    /// parent agent's queue once the background delegate finishes — the same
    /// mechanism as tasklist-completion notifications. This lets an MCP-driven
    /// parent agent learn about delegate completion without polling.
    ///
    /// `None` in native-runner contexts (which observe completion through
    /// `pending_user_messages`) and in test fixtures that do not exercise the
    /// MCP notification path. Not inherited by child contexts — each subagent
    /// manages its own delegate notifications if it spawns further delegates.
    pub delegate_completion_sink:
        Option<Arc<dyn crate::delegate_completion_sink::DelegateCompletionSink>>,
    /// ID of the project this run is serving, when the agent is operating as a
    /// project's main agent. `None` for all other run types (personal, team, tasklist).
    pub project_id: Option<String>,
    /// ID of the thread that was active when this run started, when the run
    /// was launched from a specific (possibly non-default) thread. `None` for
    /// run types that are not thread-scoped. Read by tools (e.g. `TodoCreate`,
    /// `Delegate`) that need to tag completion events and persisted transcript
    /// markers with the thread the tool call actually happened on, rather than
    /// always falling back to the agent's default-thread transcript.
    pub thread_id: Option<String>,
    /// Handle to the thread store. Used by `RenameThread` to look up and
    /// mutate the acting thread's row, and by session-init logic (native
    /// runner) / per-request context building (MCP route) to decide whether
    /// `RenameThread` should even be registered for this run (see
    /// `Thread::offers_rename_tool`). `None` in contexts that don't wire it
    /// (tests, non-thread-scoped runs) — `RenameThread` returns a
    /// recoverable error rather than panicking when this is absent.
    pub thread_store: Option<Arc<ThreadStore>>,
    /// Handle to the project store for project agent tools (`ProjectGet`,
    /// `ProjectUpdate`, `ProjectComplete`, `ProjectVerify`). `None` in all
    /// contexts that are not serving a project-scoped channel run.
    pub project_store: Option<Arc<ProjectStore>>,
    /// Pluggable verification back-end injected at session-wiring time. Used by
    /// `ProjectVerify` (mode=`"quick"`) and the `ProjectComplete` gate to run a
    /// single-model-call verdict against the project goal. `None` when no
    /// provider is configured or the context is not project-scoped; the tool
    /// returns a recoverable error in that case.
    pub verification_engine: Option<Arc<dyn crate::verification_engine::VerificationEngine>>,
    /// Full (inspection) verification back-end injected at session-wiring time.
    /// Used by `ProjectVerify` (mode=`"full"`) and required by the
    /// `ProjectComplete` gate for final sign-off. The inspection engine spawns
    /// an isolated read-only child agent that examines the working directory,
    /// reads diffs, and runs the test suite. `None` when the inspection engine
    /// is not available or the context is not project-scoped; the tool falls
    /// back to a descriptive recoverable error.
    pub full_verification_engine: Option<Arc<dyn crate::verification_engine::VerificationEngine>>,
    /// Pluggable one-shot summarization back-end used by the `SummarizeThread`
    /// engine tool to condense another thread's transcript into prose. `None`
    /// when no provider is configured for this session; the tool falls back
    /// to a descriptive recoverable error rather than panicking.
    pub thread_summarization_engine:
        Option<Arc<dyn crate::thread_summarization_engine::ThreadSummarizationEngine>>,
}

impl RunnerContext {
    /// Construct a context resolving the cwd from the process environment.
    ///
    /// Returns `Err` if `std::env::current_dir()` fails (e.g. the process
    /// working directory has been deleted).
    pub fn new(
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Result<Self, AoError> {
        let cwd = std::env::current_dir()
            .map_err(|e| AoError::Internal(format!("failed to resolve cwd: {e}")))?;
        Ok(Self::new_with_cwd(session_id, agent_id, cwd))
    }

    /// Infallible constructor that accepts a caller-supplied `cwd`.
    ///
    /// Useful in tests and in callers that already know the working directory
    /// without needing to interrogate the process environment.
    pub fn new_with_cwd(
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            agent_id: agent_id.into(),
            depth: 0,
            cancel: CancellationToken::new(),
            registry: Arc::new(Registry::default()),
            cwd: Arc::new(RwLock::new(cwd)),
            permissions: Arc::new(PermissionStore::default()),
            todos: Arc::new(TodoStore::default()),
            event_sink: Arc::new(NoopEventSink),
            worktree_stack: Arc::new(Mutex::new(Vec::new())),
            prompt_bridge: Arc::new(NoopQuestionBridge),
            form_bridge: Arc::new(NoopFormBridge),
            spawn_chain: Vec::new(),
            delegate_chain: Vec::new(),
            background_agents: Arc::new(BackgroundAgentRegistry::new(DEFAULT_BACKGROUND_AGENT_CAP)),
            background_processes: Arc::new(BackgroundProcessRegistry::new(
                DEFAULT_BACKGROUND_PROCESS_CAP,
            )),
            background_commands: Arc::new(BackgroundCommandRegistry::new(
                DEFAULT_BACKGROUND_COMMAND_CAP,
            )),
            memory_loader: Arc::new(NoopMemoryLoader),
            system_prompt: None,
            runner_events: Arc::new(broadcast::channel::<RunnerEvent>(256).0),
            skill_registry: Arc::new(RwLock::new(Arc::new(SkillRegistry::empty()))),
            kind: SessionKind::Interactive,
            sleep_ran: Arc::new(AtomicBool::new(false)),
            pending_user_messages: Arc::new(Mutex::new(PendingMessageQueue::new())),
            skill_tool_filter: Arc::new(RwLock::new(None)),
            inline_skill_via_tool_result: false,
            tool_admission: None,
            always_load_tools: Arc::new(HashSet::new()),
            activated_tools: Arc::new(Mutex::new(HashSet::new())),
            loaded_deferred_tools: Arc::new(RwLock::new(HashSet::new())),
            telemetry: Arc::new(NoopTelemetryWriter),
            read_file_state: Arc::new(ReadFileState::default()),
            workflow_runner: None,
            preferences: None,
            assignment_store: None,
            assignment_fire: None,
            agent_workflows: None,
            memory_store: None,
            artifact_store: None,
            current_message_id: None,
            artifact_intent_source: None,
            transcript_store: None,
            outcome_store: None,
            reflection_staging: None,
            artifacts_used: Arc::new(Mutex::new(Vec::new())),
            window_floor_ts: None,
            recall_transcript_path: None,
            tasklist_service: None,
            classifier: None,
            classifier_in_flight: None,
            agent_profile_store: None,
            parent_session_id: None,
            parent_agent_id: None,
            parent_current_cwd: None,
            snapshot_store: None,
            delegate_completion_sink: None,
            project_id: None,
            thread_id: None,
            thread_store: None,
            project_store: None,
            verification_engine: None,
            full_verification_engine: None,
            thread_summarization_engine: None,
        }
    }

    pub fn with_registry(mut self, registry: Arc<Registry>) -> Self {
        self.registry = registry;
        self
    }

    /// Mark this as a single-call dispatch context (the MCP HTTP route): inline
    /// skill bodies are returned via the tool result instead of enqueued, since
    /// no turn loop will ever drain [`Self::pending_user_messages`]. See the
    /// field docs on [`Self::inline_skill_via_tool_result`].
    pub fn with_inline_skill_via_tool_result(mut self) -> Self {
        self.inline_skill_via_tool_result = true;
        self
    }

    pub fn with_depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    /// Replace the cancellation token on this context.
    ///
    /// Defaults to a fresh, private `CancellationToken` that nobody else
    /// holds (see [`Self::new`]) — nothing can ever cancel it, so any
    /// `tokio::select!` racing `ctx.cancel.cancelled()` (e.g.
    /// `AskUserQuestionWithForm`'s sync-form wait) has a permanently dead
    /// cancel arm unless the caller supplies a real token here, sourced from
    /// the run or session that owns this context's lifetime. The native
    /// runner does this via `RunHandle::cancel`; the MCP HTTP route does it
    /// via the owning `McpAgentSession::cancel`.
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Replace the cwd on this context with `cwd`.
    ///
    /// Lock discipline: lock, clone the `PathBuf`, drop the guard, then do
    /// IO. Never hold the read guard across an `await`.
    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Arc::new(RwLock::new(cwd));
        self
    }

    /// Share an existing `Arc<RwLock<PathBuf>>` as this context's cwd.
    ///
    /// Used by the native runner to bind `ctx.cwd` to the same Arc as the
    /// `McpAgentSession.cwd` entry so Bash-cd writes and
    /// `EnterWorktree`/`ExitWorktree` writes automatically propagate to the
    /// session store without any extra bookkeeping.
    pub fn with_cwd_arc(mut self, cwd: Arc<RwLock<PathBuf>>) -> Self {
        self.cwd = cwd;
        self
    }

    /// Share an existing `Arc<ReadFileState>` as this context's read-snapshot map.
    ///
    /// Used by the MCP HTTP route to bind each per-request context to the
    /// session-scoped store on the session entry, so a `Read` performed in one
    /// JSON-RPC call is visible to an `Edit`/`Write` in a later call within the
    /// same session. The native runner does not need this — it keeps one
    /// long-lived context whose default `ReadFileState` already persists for the
    /// whole run. Mirrors [`Self::with_cwd_arc`].
    pub fn with_read_file_state_arc(mut self, read_file_state: Arc<ReadFileState>) -> Self {
        self.read_file_state = read_file_state;
        self
    }

    /// Replace the permissions handle on this context.
    ///
    /// Typically used in test setups or the runner's bootstrap path to supply
    /// a pre-configured `PermissionStore`. Production contexts use the default
    /// (`PermissionMode::Default`).
    pub fn with_permissions(mut self, handle: Arc<PermissionStore>) -> Self {
        self.permissions = handle;
        self
    }

    /// Replace the todo store handle on this context.
    pub fn with_todos(mut self, handle: Arc<TodoStore>) -> Self {
        self.todos = handle;
        self
    }

    /// Replace the event sink on this context.
    pub fn with_event_sink(mut self, sink: Arc<dyn EventSink + Send + Sync>) -> Self {
        self.event_sink = sink;
        self
    }

    /// Replace the worktree stack on this context.
    ///
    /// Typically used in test setups to inject a pre-populated stack, or in
    /// the runner bootstrap path to share an existing stack with a new context.
    pub fn with_worktree_stack(mut self, stack: Arc<Mutex<Vec<WorktreeEntry>>>) -> Self {
        self.worktree_stack = stack;
        self
    }

    /// Replace the question bridge on this context.
    ///
    /// Supply a `LiveBridge` (or any [`QuestionBridge`] impl) here when a live
    /// operator can answer questions. The default is [`NoopQuestionBridge`],
    /// which returns `Err(NoOperator)` immediately.
    pub fn with_prompt_bridge(mut self, bridge: Arc<dyn QuestionBridge + Send + Sync>) -> Self {
        self.prompt_bridge = bridge;
        self
    }

    /// Replace the form bridge on this context.
    ///
    /// Supply a `LiveFormBridge` (or any [`FormBridge`] impl) here when a live
    /// operator can submit form answers. The default is [`NoopFormBridge`],
    /// which returns `Err(NoOperator)` immediately.
    pub fn with_form_bridge(mut self, bridge: Arc<dyn FormBridge + Send + Sync>) -> Self {
        self.form_bridge = bridge;
        self
    }

    /// Replace the spawn chain on this context.
    ///
    /// Typically used by `SubagentSpawner::build_child_context` to
    /// inject the extended chain when constructing a child context.
    pub fn with_spawn_chain(mut self, chain: Vec<String>) -> Self {
        self.spawn_chain = chain;
        self
    }

    /// Replace the delegate chain on this context.
    ///
    /// Typically used in test setups or by `SubagentSpawner::build_child_context`
    /// to inject the chain of delegating agent IDs.
    pub fn with_delegate_chain(mut self, chain: Vec<AgentId>) -> Self {
        self.delegate_chain = chain;
        self
    }

    /// Set parent session linkage for a delegated/spawned child context.
    ///
    /// Called by `SubagentSpawner::build_child_context` and `build_delegate_context`
    /// so `NativeChildRunner` can register the child session with correct parent info.
    pub fn with_parent_session_info(
        mut self,
        parent_session_id: String,
        parent_agent_id: String,
        parent_current_cwd: PathBuf,
    ) -> Self {
        self.parent_session_id = Some(parent_session_id);
        self.parent_agent_id = Some(parent_agent_id);
        self.parent_current_cwd = Some(parent_current_cwd);
        self
    }

    /// Replace the background agent registry on this context.
    ///
    /// Allows injecting a pre-configured registry (e.g., with a custom cap
    /// or pre-populated handles) for testing or specialised runner setups.
    pub fn with_background_agents(mut self, registry: Arc<BackgroundAgentRegistry>) -> Self {
        self.background_agents = registry;
        self
    }

    /// Replace the background process registry on this context.
    ///
    /// Allows injecting a pre-configured registry (e.g., with a custom cap)
    /// for testing or specialised runner setups.
    pub fn with_background_processes(mut self, registry: Arc<BackgroundProcessRegistry>) -> Self {
        self.background_processes = registry;
        self
    }

    /// Replace the background command registry on this context.
    pub fn with_background_commands(mut self, registry: Arc<BackgroundCommandRegistry>) -> Self {
        self.background_commands = registry;
        self
    }

    /// Replace the memory loader on this context.
    ///
    /// Supply a concrete implementation (e.g. backed by the persistence layer)
    /// to enable memory injection into child system prompts. The default is
    /// [`NoopMemoryLoader`], which produces an empty blob.
    pub fn with_memory_loader(mut self, loader: Arc<dyn MemoryLoader>) -> Self {
        self.memory_loader = loader;
        self
    }

    /// Set the resolved system prompt for this context.
    ///
    /// Used by the runner bootstrap path to inject the session-level system
    /// prompt. `SubagentSpawner::build_child_context` assembles the child's
    /// system prompt from the parent's and writes it here.
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Replace the skill registry on this context (builder pattern).
    pub fn with_skill_registry(mut self, registry: Arc<SkillRegistry>) -> Self {
        self.skill_registry = Arc::new(RwLock::new(registry));
        self
    }

    /// Replace the inner skill registry in-place (for live replacement from `&self`).
    pub fn replace_skill_registry(&self, registry: Arc<SkillRegistry>) {
        *self.skill_registry.write().unwrap() = registry;
    }

    /// Push a normal-priority message onto the pending-user-messages queue.
    ///
    /// Normal-priority messages always drain at the next turn boundary regardless
    /// of session kind or Sleep activity.
    pub fn enqueue_user_message(&self, msg: String) {
        self.pending_user_messages.lock().unwrap().enqueue(msg);
    }

    /// Push a low-priority message onto the pending-user-messages queue.
    ///
    /// In Autonomous sessions, low-priority messages are held until the first turn
    /// boundary where Sleep did not run. In Interactive sessions they drain
    /// immediately alongside normal-priority messages.
    pub fn enqueue_low_priority_message(&self, msg: String) {
        self.pending_user_messages.lock().unwrap().enqueue_low(msg);
    }

    /// Record that Sleep ran during the current turn.
    ///
    /// Called by the Sleep tool on successful completion. Reset at the start of
    /// each new turn by the runner via [`Self::reset_sleep_ran`].
    pub fn set_sleep_ran(&self) {
        self.sleep_ran.store(true, Ordering::Relaxed);
    }

    /// Return whether Sleep ran during the current turn.
    pub fn sleep_ran(&self) -> bool {
        self.sleep_ran.load(Ordering::Relaxed)
    }

    /// Clear the per-turn Sleep flag. Called by the runner at the start of each
    /// new turn (before the next batch of tool calls executes).
    pub fn reset_sleep_ran(&self) {
        self.sleep_ran.store(false, Ordering::Relaxed);
    }

    /// Return a builder copy of this context with `kind` set to `new_kind`.
    pub fn with_kind(mut self, new_kind: SessionKind) -> Self {
        self.kind = new_kind;
        self
    }

    /// Activate the turn-scoped tool filter with the given set of allowed tool names.
    pub fn set_skill_tool_filter(&self, tools: HashSet<String>) {
        *self.skill_tool_filter.write().unwrap() = Some(tools);
    }

    /// Deactivate the turn-scoped tool filter (all tools allowed again).
    pub fn clear_skill_tool_filter(&self) {
        *self.skill_tool_filter.write().unwrap() = None;
    }

    /// Return `true` if `tool_name` is permitted under the current filter.
    ///
    /// `RunSkill` and `SkillRegister` are always permitted regardless of the
    /// filter (so a skill that allow-lists only Read can still chain into
    /// another skill). When no filter is active (`None`), all tools are
    /// permitted.
    pub fn check_skill_tool_filter(&self, tool_name: &str) -> bool {
        if tool_name == "RunSkill" || tool_name == "SkillRegister" {
            return true;
        }
        match &*self.skill_tool_filter.read().unwrap() {
            None => true,
            Some(allowed) => allowed.contains(tool_name),
        }
    }

    /// Set the agent-level admission gate on this context.
    ///
    /// `None` resets to "no gate" (every registered tool is admitted).
    /// `Some(gate)` restricts the per-turn tool array to the tools the gate
    /// permits, independent of load-policy resolution.
    pub fn with_tool_admission(mut self, admission: Option<ToolAdmission>) -> Self {
        self.tool_admission = admission;
        self
    }

    /// Replace the always-loaded tool set on this context.
    pub fn with_always_load_tools(mut self, tools: Arc<HashSet<String>>) -> Self {
        self.always_load_tools = tools;
        self
    }

    /// Replace the activated-tools set on this context.
    pub fn with_activated_tools(mut self, tools: Arc<Mutex<HashSet<String>>>) -> Self {
        self.activated_tools = tools;
        self
    }

    /// Replace the loaded-deferred-tools set on this context.
    pub fn with_loaded_deferred_tools(mut self, tools: Arc<RwLock<HashSet<String>>>) -> Self {
        self.loaded_deferred_tools = tools;
        self
    }

    /// Replace the telemetry writer on this context.
    pub fn with_telemetry(mut self, writer: Arc<dyn TelemetryWriter + Send + Sync>) -> Self {
        self.telemetry = writer;
        self
    }

    /// Replace the read-file-state map on this context.
    pub fn with_read_file_state(mut self, state: Arc<ReadFileState>) -> Self {
        self.read_file_state = state;
        self
    }

    /// Supply a workflow runner handle (WorkflowAction* tools).
    pub fn with_workflow_runner(
        mut self,
        runner: Arc<dyn WorkflowRunnerHandle + Send + Sync>,
    ) -> Self {
        self.workflow_runner = Some(runner);
        self
    }

    /// Supply a user-preferences store handle (Assignment* tools).
    pub fn with_preferences(mut self, prefs: Arc<UserPreferencesStore>) -> Self {
        self.preferences = Some(prefs);
        self
    }

    /// Supply an assignment store handle (Assignment* tools).
    pub fn with_assignment_store(mut self, store: Arc<AssignmentStore>) -> Self {
        self.assignment_store = Some(store);
        self
    }

    /// Supply an assignment-fire handle (`AssignmentTrigger`'s fire-now capability).
    pub fn with_assignment_fire(
        mut self,
        handle: Arc<dyn AssignmentFireHandle + Send + Sync>,
    ) -> Self {
        self.assignment_fire = Some(handle);
        self
    }

    /// Set the agent's workflow binding (WorkflowActionCreate gate).
    pub fn with_agent_workflows(mut self, binding: WorkflowBinding) -> Self {
        self.agent_workflows = Some(binding);
        self
    }

    /// Supply a memory store handle (Memory tool family).
    pub fn with_memory_store(mut self, store: Arc<MemoryStore>) -> Self {
        self.memory_store = Some(store);
        self
    }

    /// Supply an artifact store handle (`ArtifactWrite`).
    pub fn with_artifact_store(mut self, store: Arc<ArtifactStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    /// Pre-allocate the id of the message this context's turn is producing,
    /// so `ArtifactWrite` can stamp `ArtifactRecord.source_message_id`.
    pub fn with_current_message_id(mut self, id: impl Into<String>) -> Self {
        self.current_message_id = Some(id.into());
        self
    }

    /// Tag this context with the [`IntentSource`] an in-place `ArtifactWrite`
    /// call made under it should be recorded as, when the spawn context
    /// already knows (e.g. a whole-artifact regenerate run).
    pub fn with_artifact_intent_source(mut self, source: IntentSource) -> Self {
        self.artifact_intent_source = Some(source);
        self
    }

    /// Supply a transcript store handle (RecallHistory tool).
    pub fn with_transcript_store(mut self, store: Arc<TranscriptStore>) -> Self {
        self.transcript_store = Some(store);
        self
    }

    /// Supply an outcome-record store handle.
    pub fn with_outcome_store(mut self, store: Arc<OutcomeStore>) -> Self {
        self.outcome_store = Some(store);
        self
    }

    /// Supply a staging-queue store handle.
    pub fn with_reflection_staging(mut self, store: Arc<ReflectionStagingStore>) -> Self {
        self.reflection_staging = Some(store);
        self
    }

    /// Record that `artifact` was used during the current turn.
    pub fn record_artifact_used(&self, artifact: ArtifactRef) {
        self.artifacts_used.lock().unwrap().push(artifact);
    }

    /// Record every artifact in `artifacts` as used during the current turn.
    pub fn record_artifacts_used(&self, artifacts: impl IntoIterator<Item = ArtifactRef>) {
        self.artifacts_used.lock().unwrap().extend(artifacts);
    }

    /// Snapshot of every artifact recorded as used so far this turn.
    pub fn artifacts_used_snapshot(&self) -> Vec<ArtifactRef> {
        self.artifacts_used.lock().unwrap().clone()
    }

    /// Supply a snapshot store handle (async form pending state).
    pub fn with_snapshot_store(mut self, store: Arc<SnapshotStore>) -> Self {
        self.snapshot_store = Some(store);
        self
    }

    /// Attach a delegate-completion notification sink.
    ///
    /// The MCP route supplies this sink when building a per-request context so
    /// that a `Delegate mode=async` tool call can notify the parent agent via
    /// its durable queue when the background delegate finishes.  In-process
    /// (native-runner) contexts do not need this sink because they observe
    /// completion through `pending_user_messages`.
    pub fn with_delegate_completion_sink(
        mut self,
        sink: Arc<dyn crate::delegate_completion_sink::DelegateCompletionSink>,
    ) -> Self {
        self.delegate_completion_sink = Some(sink);
        self
    }

    /// Mark this context as serving a specific project channel run.
    ///
    /// Engine tools that need to know which project they are acting for read
    /// `ctx.project_id` rather than receiving the project ID as a parameter.
    pub fn with_project(mut self, project_id: String) -> Self {
        self.project_id = Some(project_id);
        self
    }

    /// Mark this context as serving a specific thread. The thread that was
    /// active when the tool call happened, threaded through so completion
    /// events and persisted transcript markers can be tagged with it instead
    /// of always falling back to the agent's default-thread transcript.
    pub fn with_thread(mut self, thread_id: String) -> Self {
        self.thread_id = Some(thread_id);
        self
    }

    /// Supply the thread store so `RenameThread` can read and mutate the
    /// acting thread's row without going through the HTTP layer.
    pub fn with_thread_store(mut self, store: Arc<ThreadStore>) -> Self {
        self.thread_store = Some(store);
        self
    }

    /// Supply the project store so project agent tools can read and mutate
    /// the project record without going through the HTTP layer.
    pub fn with_project_store(mut self, store: Arc<ProjectStore>) -> Self {
        self.project_store = Some(store);
        self
    }

    /// Inject the quick verification engine used by `ProjectVerify` (mode=`"quick"`)
    /// and as the fallback inside the `ProjectComplete` gate.
    pub fn with_verification_engine(
        mut self,
        engine: Arc<dyn crate::verification_engine::VerificationEngine>,
    ) -> Self {
        self.verification_engine = Some(engine);
        self
    }

    /// Inject the full (inspection) verification engine used by `ProjectVerify`
    /// (mode=`"full"`) and required by the `ProjectComplete` gate for final sign-off.
    pub fn with_full_verification_engine(
        mut self,
        engine: Arc<dyn crate::verification_engine::VerificationEngine>,
    ) -> Self {
        self.full_verification_engine = Some(engine);
        self
    }

    /// Inject the summarization engine used by `SummarizeThread` to condense
    /// another thread's transcript into prose.
    pub fn with_thread_summarization_engine(
        mut self,
        engine: Arc<dyn crate::thread_summarization_engine::ThreadSummarizationEngine>,
    ) -> Self {
        self.thread_summarization_engine = Some(engine);
        self
    }

    /// Supply a tasklist service handle (Todo* tools).
    pub fn with_tasklist_service(
        mut self,
        service: Arc<dyn TasklistServiceHandle + Send + Sync>,
    ) -> Self {
        self.tasklist_service = Some(service);
        self
    }

    /// Supply a classifier handle (task routing).
    pub fn with_classifier(mut self, classifier: Arc<dyn ClassifierHandle + Send + Sync>) -> Self {
        self.classifier = Some(classifier);
        self
    }

    /// Supply the process-wide classifier dedup registry. When set, any
    /// `classify_with_retry` spawn launched from this context claims its
    /// `(agent, tasklist, task)` slot before proceeding so concurrent
    /// reconciler ticks cannot double-spawn.
    pub fn with_classifier_in_flight(mut self, in_flight: Arc<ClassifierInFlight>) -> Self {
        self.classifier_in_flight = Some(in_flight);
        self
    }

    /// Supply an agent profile store handle (Todo* tools' `owner` resolution).
    pub fn with_agent_profile_store(mut self, store: Arc<AgentProfileStore>) -> Self {
        self.agent_profile_store = Some(store);
        self
    }

    /// Set the history window floor timestamp (RecallHistory tool).
    pub fn with_window_floor_ts(mut self, ts: DateTime<Utc>) -> Self {
        self.window_floor_ts = Some(ts);
        self
    }

    /// Point RecallHistory at a specific transcript file instead of the
    /// agent-keyed default. Used by the runner when the current turn is
    /// running in a non-default thread (so backward recall reads from the
    /// thread's own transcript) or in a branch thread (so backward recall
    /// reads from the source thread's transcript, surfacing pre-branch
    /// history as context).
    pub fn with_recall_transcript_path(mut self, path: PathBuf) -> Self {
        self.recall_transcript_path = Some(path);
        self
    }

    /// Produce a child context for a spawned subagent. Bumps depth by 1 and
    /// reuses the same registry + cancel token.
    ///
    /// `cwd` is Arc-cloned into the child — parent and child share the same
    /// underlying `RwLock<PathBuf>`. A future `EnterWorktree` call that
    /// mutates the parent's cwd will therefore be visible to children, which
    /// is the intended behaviour for fan-out subagents that should inherit
    /// any worktree switch.
    ///
    /// `permissions` is Arc-cloned into the child — parent and child share
    /// the same underlying `PermissionStore`. When `EnterPlanMode`
    /// flips the mode on the parent, the child observes the change immediately.
    /// This shared-state semantic is intentional: plan-mode propagation must
    /// reach all children mid-run.
    ///
    /// `todos` is Arc-cloned into the child — parent and child share the same
    /// underlying `TodoStore`. Subagent isolation is by `agent_id` key inside
    /// the store: each agent reads and writes only the slice keyed by its own
    /// `agent_id`, so different agents do not observe each other's lists even
    /// though they share a single `Arc<TodoStore>`.
    ///
    /// `event_sink` is Arc-cloned into the child — parent and child point to
    /// the same trait object, so events emitted by either flow to the same
    /// sink implementation.
    ///
    /// Caller is responsible for enforcing the depth cap before invoking this.
    pub fn child(&self, agent_id: impl Into<String>) -> Self {
        Self {
            session_id: self.session_id.clone(),
            agent_id: agent_id.into(),
            depth: self.depth + 1,
            cancel: self.cancel.clone(),
            registry: self.registry.clone(),
            cwd: self.cwd.clone(),
            permissions: self.permissions.clone(),
            todos: self.todos.clone(),
            event_sink: self.event_sink.clone(),
            worktree_stack: self.worktree_stack.clone(),
            prompt_bridge: self.prompt_bridge.clone(),
            form_bridge: self.form_bridge.clone(),
            spawn_chain: self.spawn_chain.clone(),
            delegate_chain: self.delegate_chain.clone(),
            background_agents: Arc::new(BackgroundAgentRegistry::new(DEFAULT_BACKGROUND_AGENT_CAP)),
            background_processes: Arc::new(BackgroundProcessRegistry::new(
                DEFAULT_BACKGROUND_PROCESS_CAP,
            )),
            background_commands: Arc::new(BackgroundCommandRegistry::new(
                DEFAULT_BACKGROUND_COMMAND_CAP,
            )),
            memory_loader: self.memory_loader.clone(),
            system_prompt: self.system_prompt.clone(),
            runner_events: Arc::new(broadcast::channel::<RunnerEvent>(256).0),
            skill_registry: self.skill_registry.clone(),
            kind: self.kind,
            sleep_ran: Arc::new(AtomicBool::new(false)),
            pending_user_messages: self.pending_user_messages.clone(),
            skill_tool_filter: self.skill_tool_filter.clone(),
            // Not inherited: a spawned subagent drives its own draining loop,
            // so it follows the default enqueue contract regardless of how the
            // parent was dispatched.
            inline_skill_via_tool_result: false,
            tool_admission: None,
            always_load_tools: self.always_load_tools.clone(),
            activated_tools: self.activated_tools.clone(),
            loaded_deferred_tools: self.loaded_deferred_tools.clone(),
            telemetry: self.telemetry.clone(),
            read_file_state: self.read_file_state.clone(),
            workflow_runner: self.workflow_runner.clone(),
            preferences: self.preferences.clone(),
            assignment_store: self.assignment_store.clone(),
            assignment_fire: self.assignment_fire.clone(),
            agent_workflows: self.agent_workflows.clone(),
            memory_store: self.memory_store.clone(),
            artifact_store: self.artifact_store.clone(),
            // Fresh, not inherited: a spawned subagent's turn produces its
            // own message, distinct from the parent's — matching
            // `window_floor_ts`'s "fresh on every child" convention below.
            current_message_id: None,
            artifact_intent_source: None,
            transcript_store: self.transcript_store.clone(),
            outcome_store: self.outcome_store.clone(),
            reflection_staging: self.reflection_staging.clone(),
            // Fresh, not inherited — see the field doc on `artifacts_used`.
            artifacts_used: Arc::new(Mutex::new(Vec::new())),
            window_floor_ts: None,
            recall_transcript_path: None,
            tasklist_service: self.tasklist_service.clone(),
            classifier: self.classifier.clone(),
            classifier_in_flight: self.classifier_in_flight.clone(),
            agent_profile_store: self.agent_profile_store.clone(),
            parent_session_id: None,
            parent_agent_id: None,
            parent_current_cwd: None,
            snapshot_store: self.snapshot_store.clone(),
            // Not inherited: the sink is bound per-request to the parent session.
            // Child contexts manage their own delegate notifications if they
            // spawn further delegates.
            delegate_completion_sink: None,
            project_id: self.project_id.clone(),
            thread_id: self.thread_id.clone(),
            thread_store: self.thread_store.clone(),
            project_store: self.project_store.clone(),
            verification_engine: self.verification_engine.clone(),
            full_verification_engine: self.full_verification_engine.clone(),
            thread_summarization_engine: self.thread_summarization_engine.clone(),
        }
    }
}

#[cfg(test)]
mod tests;
