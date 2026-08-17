//! Engine tool implementations (TodoWrite, AskUserQuestionWithForm, Brief,
//! EnterPlanMode/ExitPlanMode, EnterWorktree/ExitWorktree, Config, the
//! `Agent` spawner, `RunSkill` loader, ToolSearch, Cron suite, etc.). Tools
//! land incrementally across Phases 3–8; this crate is the deployment crate
//! that bundles them for the runner.
//!
//! # Registering with a `Registry`
//!
//! ```no_run
//! use ao_engine_tools_core::Registry;
//! use ao_engine_tools_engine::register_all;
//!
//! let mut registry = Registry::new();
//! register_all(&mut registry);
//! assert!(registry.lookup_engine("Brief").is_some());
//! ```

use std::sync::Arc;

use ao_engine_tools_core::Registry;

pub mod agent_author;
pub mod artifact;
pub mod ask_user_question_form;
pub mod assignment;
pub mod brief;
pub mod config;
pub mod datetime;
pub mod delegate;
pub mod list_threads;
pub mod load_memory;
pub mod memory;
pub mod plan_mode;
pub mod project;
pub mod recall_history;
pub mod rename_thread;
pub mod send_email;
pub mod skill;
pub mod sleep;
pub mod summarize_thread;
pub mod todo;
pub mod tool_search;
pub mod delegate_output;
pub mod delegate_stop;
pub mod todo_write;
pub mod workflow_action;
pub mod worktree;

pub use agent_author::AgentAuthor;
pub use artifact::ArtifactWrite;
pub use ask_user_question_form::AskUserQuestionWithForm;
pub use brief::Brief;
pub use config::Config;
pub use datetime::DateTime;
pub use delegate::Delegate;
pub use plan_mode::{EnterPlanMode, ExitPlanMode};
pub use skill::RunSkill;
pub use skill::SkillRegister;
pub use delegate_output::DelegateOutput;
pub use delegate_stop::DelegateStop;
pub use todo::add::TodoAdd;
pub use todo::cancel::TodoCancel;
pub use todo::check_zombies::TodoCheckZombies;
pub use todo::classify_with_retry;
pub use todo::comment::TodoComment;
pub use todo::complete::TodoComplete;
pub use todo::create::TodoCreate;
pub use todo::delete::TodoDelete;
pub use todo::list::TodoList;
pub use todo::requeue::TodoRequeue;
pub use todo::resume::TodoResume;
pub use todo::resume_task::TodoResumeTask;
pub use todo::start::TodoStart;
pub use todo::stop_task::TodoStopTask;
pub use todo::update::TodoUpdate;
pub use todo_write::TodoWrite;
pub use tool_search::ToolSearch;
pub use assignment::{
    AssignmentCreate, AssignmentDelete, AssignmentList, AssignmentTrigger, AssignmentUpdate,
};
pub use workflow_action::{
    WorkflowActionCompletePhase, WorkflowActionCreate, WorkflowActionDelete,
    WorkflowActionReadState, WorkflowActionSkipPhase, WorkflowActionStart,
    WorkflowActionWriteOutput,
};
pub use project::{ProjectComplete, ProjectGet, ProjectUpdate, ProjectVerify};
pub use project::register_project_tools;
pub use list_threads::ListThreads;
pub use load_memory::LoadMemory;
pub use recall_history::RecallHistory;
pub use rename_thread::RenameThread;
pub use send_email::SendEmail;
pub use sleep::Sleep;
pub use summarize_thread::SummarizeThread;
pub use worktree::{EnterWorktree, ExitWorktree};

/// Register every engine tool into the supplied [`Registry`].
///
/// External callers (e.g. the session bootstrap) use this to install the full
/// engine catalog with one call instead of knowing each struct name.
pub fn register_all(registry: &mut Registry) {
    registry.register_engine(Arc::new(AgentAuthor::new()));
    // Same stub-then-overwrite pattern as AgentAuthor above: `SendEmail::new()`
    // has no agent store or secret store wired in, so every call fails with a
    // clear error until `AppState` construction replaces this entry with
    // `SendEmail::with_deps(...)`. Kept always-registered (rather than only
    // admitted when an agent has an Email binding) because no cheap
    // conditional-admission mechanism exists in this registry yet.
    registry.register_engine(Arc::new(SendEmail::new()));
    registry.register_engine(Arc::new(AskUserQuestionWithForm));
    registry.register_engine(Arc::new(Brief));
    registry.register_engine(Arc::new(Config));
    registry.register_engine(Arc::new(DateTime));
    registry.register_engine(Arc::new(EnterPlanMode));
    registry.register_engine(Arc::new(ExitPlanMode));
    registry.register_engine(Arc::new(EnterWorktree));
    registry.register_engine(Arc::new(ExitWorktree));
    registry.register_engine(Arc::new(RunSkill::new()));
    registry.register_engine(Arc::new(SkillRegister));
    registry.register_engine(Arc::new(TodoAdd));
    registry.register_engine(Arc::new(TodoCancel));
    registry.register_engine(Arc::new(TodoCheckZombies));
    registry.register_engine(Arc::new(TodoComment));
    registry.register_engine(Arc::new(TodoComplete));
    registry.register_engine(Arc::new(TodoCreate));
    registry.register_engine(Arc::new(TodoDelete));
    registry.register_engine(Arc::new(TodoList));
    registry.register_engine(Arc::new(TodoRequeue));
    registry.register_engine(Arc::new(TodoResume));
    registry.register_engine(Arc::new(TodoResumeTask));
    registry.register_engine(Arc::new(TodoStart));
    registry.register_engine(Arc::new(TodoStopTask));
    registry.register_engine(Arc::new(TodoUpdate));
    registry.register_engine(Arc::new(TodoWrite));
    registry.register_engine(Arc::new(ToolSearch));
    recall_history::register_recall_history_tool(registry);
    assignment::register_assignment_tools(registry);
    workflow_action::register_workflow_action_tools(registry);
    delegate::register(registry);
    memory::register_memory_tools(registry);
    artifact::register_artifact_tools(registry);
    load_memory::register(registry);
    project::register_project_tools(registry);
    registry.build_deferred_index();
}

/// Register engine tools that are only appropriate for autonomous / scheduled-agent
/// sessions — tools that an interactive user would not expect to see in a normal
/// chat session. Call this in addition to [`register_all`] when building a registry
/// for a background or scheduled agent profile.
///
/// Currently registers: [`Sleep`].
///
/// TODO: replace this manual call with a first-class autonomous-mode session
/// profile once the runner gains that concept.
///
/// Sleep is kept out of interactive sessions deliberately: a chat user who
/// asked a question is waiting on the answer, so an agent that can decide to
/// pause mid-turn reads as a hang with no way to tell the difference. An
/// autonomous or scheduled run has no one waiting, so the same capability is
/// useful there (backing off a poll, spacing retries) rather than alarming.
pub fn register_autonomous_tools(registry: &mut Registry) {
    sleep::register(registry);
}

/// Crate-wide mutex that serialises every test that mutates the process-global
/// `LAUNCHPAD_STUDIO_DATA_DIR` env var.  Any test in this crate that reads or
/// writes that env var (`config::tests`, `skill::tests`, `delegate::tests`)
/// must hold this lock for its duration so they do not race with each other.
/// Acquire it via [`lock_env_var`] rather than locking directly.
#[cfg(test)]
pub(crate) static ENV_VAR_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) mod test_env;

/// Acquire [`ENV_VAR_MUTEX`], recovering from a poisoned lock.
///
/// If a test panics while holding the guard (e.g. an assertion fails after it
/// has set the env var), the mutex becomes poisoned. Recovering the inner guard
/// instead of unwrapping stops a single real failure from cascading into
/// spurious `PoisonError` panics across every other env-mutating test in the
/// suite — so failures stay legible (1 failure, not N).
#[cfg(test)]
pub(crate) fn lock_env_var() -> std::sync::MutexGuard<'static, ()> {
    ENV_VAR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_engine_tools_core::Registry;

    #[test]
    fn register_all_installs_ask_user_question_with_form() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_engine("AskUserQuestionWithForm").is_some());
        // The single-question AskUserQuestion tool was retired in favor of
        // AskUserQuestionWithForm (a one-radio-field form covers its use case);
        // it must no longer appear in the catalog.
        assert!(r.lookup_engine("AskUserQuestion").is_none());
    }

    #[test]
    fn register_all_installs_brief() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_engine("Brief").is_some());
    }

    #[test]
    fn register_all_installs_todo_write() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_engine("TodoWrite").is_some());
    }

    #[test]
    fn register_all_installs_enter_plan_mode() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_engine("EnterPlanMode").is_some());
    }

    #[test]
    fn register_all_installs_datetime() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_engine("DateTime").is_some());
    }

    #[test]
    fn register_all_installs_workflow_action_delete() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_io("WorkflowActionDelete").is_some());
    }

    #[test]
    fn register_all_installs_exit_plan_mode() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_engine("ExitPlanMode").is_some());
    }

    #[test]
    fn register_all_installs_enter_worktree() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_engine("EnterWorktree").is_some());
    }

    #[test]
    fn register_all_installs_exit_worktree() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_engine("ExitWorktree").is_some());
    }

    #[test]
    fn register_all_installs_assignment_tools() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_io("AssignmentCreate").is_some());
        assert!(r.lookup_io("AssignmentList").is_some());
        assert!(r.lookup_io("AssignmentUpdate").is_some());
        assert!(r.lookup_io("AssignmentDelete").is_some());
        assert!(r.lookup_io("AssignmentTrigger").is_some());
    }

    #[test]
    fn register_all_installs_load_memory() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_io("LoadMemory").is_some());
    }

    #[test]
    fn register_all_installs_artifact_write() {
        // ArtifactWrite defaults to `LoadPolicy::AlwaysLoad` (like
        // `MemoryWrite` — neither overrides `load_policy()`), so it is
        // registered and immediately visible via `lookup_io`, not surfaced
        // through the *deferred* index (which only lists `Deferred` tools).
        // `register_all` already calls `build_deferred_index()` internally;
        // this just confirms that pass completes without dropping the tool
        // from the registry.
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_io("ArtifactWrite").is_some());
    }
}
