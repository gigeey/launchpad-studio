//! Foundation crate for the native engine tool surface.
//!
//! This crate is the *frozen* contract between the runner and tool
//! implementations. It defines the load-bearing trait surfaces
//! (`IoTool`, `EngineTool`), the dispatch context (`RunnerContext`), the
//! per-session catalog (`Registry`), the success/error return shape
//! (`ToolOutput`), and the permission primitives (`PermissionDecision`,
//! `PermissionContext`, `PermissionMode`, `DenialTracker`) consumed by
//! the runner's permission gate.
//!
//! Tools live in the sibling `ao-engine-tools-io` and
//! `ao-engine-tools-engine` crates; the runner consumes a `Registry` and
//! never imports tool implementations directly.

pub mod agent_profile_cache;
pub mod assignment_fire_handle;
pub mod background_agents;
pub mod background_commands;
pub mod background_processes;
pub mod classifier_handle;
pub mod context;
pub mod delegate_completion_sink;
pub mod delegation_usage;
pub mod form_events;
pub mod memory_loader;
pub mod memory_usage;
pub mod output;
pub mod permissions;
pub mod policy;
pub mod read_file_state;
pub mod registry;
pub mod skill_registry;
pub mod tasklist_service_handle;
pub mod telemetry;
pub mod terminal_report;
pub mod thread_summarization_engine;
pub mod tool;
pub mod trust_gate;
pub mod verification_engine;
pub mod workflow_runner_handle;

pub use agent_profile_cache::{AgentProfileCacheInvalidator, NoopAgentProfileCacheInvalidator};
pub use assignment_fire_handle::AssignmentFireHandle;
pub use background_commands::{
    BackgroundCommandHandle, BackgroundCommandId, BackgroundCommandRegistry,
    BackgroundCommandRegistryError, BackgroundCommandStatus, BoundedOutputBuffer,
    OUTPUT_BUFFER_CAP,
};
pub use background_processes::{
    BackgroundProcessHandle, BackgroundProcessId, BackgroundProcessRegistry,
    RegistryError as BackgroundProcessRegistryError,
};
pub use classifier_handle::{
    ClassifierClaim, ClassifierHandle, ClassifierInFlight, ClassifyOutcome,
};
pub use context::{
    AskQuestionError, Choice, ChoiceId, EventSink, FormAction, FormAnswer, FormBridge, FormField,
    FormFieldKind, FormFieldPayload, FormOption, FormOptionPayload, FormRequest, FormResponse,
    FormSpecPayload, NoopEventSink, NoopFormBridge, NoopQuestionBridge, PendingMessageQueue,
    PermissionStore, QuestionBridge, QuestionRequest, RunnerContext, TodoItem, TodoStatus,
    TodoStore, ToolAdmission, UserEvent, WorktreeEntry,
};
pub use delegate_completion_sink::{DelegateCompletionSink, DELEGATE_EXCERPT_CAP};
pub use form_events::{
    form_answer_content, form_answer_spec_snapshot, form_request_entry, form_withdrawn_content,
    form_withdrawn_entry, parse_form_spec_payload, wire_posted_async_form, FormAnswerMeta,
    FormDismissedMeta, FormRequestMeta, FormWithdrawnMeta, FORM_ANSWER, FORM_DISMISSED,
    FORM_REQUEST, FORM_WITHDRAWN,
};
pub use memory_loader::{MemoryLoader, NoopMemoryLoader, StaticMemoryLoader};
pub use output::{ToolBlock, ToolOutput};
pub use permissions::{
    DenialTracker, NoopDenialTracker, PermissionContext, PermissionDecision, PermissionMode,
    SessionKind,
};
pub use policy::{LoadPolicy, LoadPolicyOverride};
pub use read_file_state::{ReadEntry, ReadFileState};
pub use registry::{DeferredEntry, DeferredIndex, Registry, ToolCategory, ToolRef};
pub use tasklist_service_handle::{
    ResumeOutcome, StartOutcome, StartOutcomeKind, TasklistServiceHandle, ZombieReport,
};
pub use telemetry::{EventKind, NoopTelemetryWriter, TelemetryWriter, ToolUsageEvent};
pub use terminal_report::{
    CancelOutcome, TerminalCounts, TerminalReport, TerminalTaskEntry, TerminalWatcherGuard,
    TerminalWatcherRegistry,
};
pub use thread_summarization_engine::{ThreadSummarizationEngine, ThreadSummarizationInput};
pub use tool::{EngineTool, IoTool};
pub use verification_engine::{
    PriorVerdict, TasklistEvidence, VerificationEngine, VerificationInput, VerificationVerdict,
};
pub use workflow_runner_handle::WorkflowRunnerHandle;
