use ao_protocol::error::AoError;
use ao_protocol::tasklist::{Task, TaskAssignment, TaskComment, TaskGroup, TaskGroupMode, Tasklist};
use async_trait::async_trait;

use crate::terminal_report::{CancelOutcome, TerminalWatcherGuard};

/// One InProgress task whose owning runner has no active runs.
///
/// Returned by [`TasklistServiceHandle::check_zombies_for_agent`]. The report
/// is informational — callers decide whether to requeue or escalate.
#[derive(Debug, Clone)]
pub struct ZombieReport {
    pub task_id: String,
    pub task_title: String,
    /// Seconds since the feeder registered the dispatch timestamp.
    /// `None` means no timestamp was found — common after a server restart
    /// where the in-memory timestamp map was wiped while the task remained
    /// `InProgress` on disk.
    pub secs_since_dispatch: Option<u64>,
    /// The agent that was supposed to run this task.
    pub agent_id: String,
    pub tasklist_id: String,
}

/// Outcome returned by `TasklistServiceHandle::resume_for_agent`.
#[derive(Debug, Clone)]
pub struct ResumeOutcome {
    pub tasklist_id: String,
    /// Number of `Failed` tasks reset to `Pending`.
    pub reset_count: usize,
}

/// What `TasklistServiceHandle::start_for_agent` actually accomplished.
///
/// `Task::attempt_count` is a retry counter bumped only on the
/// watchdog/failure path — a task that dispatched and ran to completion on
/// the first try still shows `attempt_count: 0`, so it cannot distinguish
/// "genuinely dispatched this call" from "never touched". Implementations
/// must derive [`StartOutcomeKind::Dispatched`] from a ground-truth signal
/// instead — e.g. whether a task's `<workspace>/tasks/{task_id}/` directory
/// came into existence as a result of this call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcomeKind {
    /// One or more tasks were newly handed to the feeder by this call.
    Dispatched { task_ids: Vec<String> },
    /// The tasklist already had a task in flight (`InProgress`); this call
    /// re-kicked the feeder defensively but nothing new was dispatched.
    AlreadyRunning,
    /// The tasklist is `Active` but has no dispatchable `Pending` task left.
    NoPending,
}

/// Outcome returned by `TasklistServiceHandle::start_for_agent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartOutcome {
    pub tasklist_id: String,
    pub kind: StartOutcomeKind,
}

/// Trait abstraction over `ao_engine::TasklistService` that lets
/// `ao-engine-tools-core` (and tools built on top of it) hold a handle to
/// the service without introducing a circular crate dependency.
///
/// `ao-engine` depends on `ao-engine-tools-core` for `RunnerContext`, so
/// `ao-engine-tools-core` cannot in turn depend on `ao-engine`. The trait
/// is defined here; `ao-engine` implements it on its concrete `TasklistService`.
#[async_trait]
pub trait TasklistServiceHandle: Send + Sync {
    /// Return the single non-terminal tasklist for the agent, or None.
    async fn agent_active(&self, agent_id: &str) -> Result<Option<Tasklist>, AoError>;

    /// Create a new agent-scoped tasklist.
    async fn create_for_agent(
        &self,
        agent_id: &str,
        name: String,
        groups: Vec<TaskGroup>,
    ) -> Result<Tasklist, AoError>;

    /// Create an agent-scoped tasklist and atomically tag it with a project
    /// and/or the thread it was created from.
    ///
    /// When `project_id` is `Some`, the returned tasklist already carries the
    /// project tag so the completion loop routes the summary to the project
    /// channel rather than the agent's personal queue. When `None`, behaves
    /// identically to [`create_for_agent`].
    ///
    /// `thread_id` is stamped onto the tasklist so `todo_list.complete`
    /// completion handling (SSE tag, `QueuedMessage.thread_id`, and the
    /// on-disk transcript marker) can route back to the thread the
    /// `TodoCreate` call actually happened on rather than always falling back
    /// to the agent's default-thread transcript. `None` for tasklists created
    /// outside a thread-scoped run.
    ///
    /// Default impl falls back to `create_for_agent` + `stamp_project_id_for_agent`
    /// (two-step, with a brief unstamped window) and does not stamp
    /// `thread_id` at all (there is no `stamp_thread_id_for_agent` equivalent).
    /// Production implementations should override this to stamp both fields
    /// atomically at creation time.
    async fn create_for_agent_with_project(
        &self,
        agent_id: &str,
        name: String,
        groups: Vec<TaskGroup>,
        project_id: Option<String>,
        thread_id: Option<String>,
    ) -> Result<Tasklist, AoError> {
        let _ = thread_id;
        let tl = self.create_for_agent(agent_id, name, groups).await?;
        if let Some(pid) = &project_id {
            self.stamp_project_id_for_agent(agent_id, &tl.id, pid).await?;
        }
        Ok(tl)
    }

    /// Return the agent's configured max_instances value.
    async fn get_agent_max_instances(&self, agent_id: &str) -> Result<u32, AoError>;

    /// Append a new group of tasks to an existing agent-scoped tasklist.
    async fn add_group_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        tasks: Vec<Task>,
        mode: TaskGroupMode,
    ) -> Result<Tasklist, AoError>;

    /// Update fields (prompt, owner, expected_outputs) on a task in an agent-scoped tasklist.
    async fn update_task_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
        prompt: Option<String>,
        owner_agent_id: Option<String>,
        expected_outputs: Option<Vec<String>>,
    ) -> Result<Tasklist, AoError>;

    /// Mark a task as completed, triggering feeder advance for SEQ groups.
    async fn complete_task_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
    ) -> Result<(), AoError>;

    /// Attach a comment to a task in an agent-scoped tasklist.
    ///
    /// Default impl returns `Internal` so existing mocks compile without changes;
    /// override in production implementations.
    async fn add_comment_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        _task_id: &str,
        _body: String,
    ) -> Result<TaskComment, AoError> {
        Err(AoError::Internal("add_comment_for_agent not implemented".into()))
    }

    /// Register a one-shot watcher that fires when `tasklist_id` reaches a
    /// terminal state (Completed, Failed, or Cancelled). The guard must be
    /// created *before* starting the tasklist to avoid the race where the
    /// tasklist completes before the caller begins awaiting.
    ///
    /// Dropping the guard before calling `wait()` unregisters the sender so
    /// the feeder skips the send cleanly (no panic, no log spam).
    async fn terminal_watcher(
        &self,
        tasklist_id: &str,
    ) -> Result<TerminalWatcherGuard, AoError>;

    /// Cancel the agent's active tasklist: flip to Cancelled, mark all
    /// Pending/Blocked tasks Skipped, write a cancelled block to progress.jsonl,
    /// and return counts for the tool response. Returns an error if the agent
    /// has no active tasklist.
    async fn cancel_for_agent(&self, agent_id: &str) -> Result<CancelOutcome, AoError>;

    /// Compare-and-swap the assignment on a task row. Returns `true` when the
    /// write landed (token matched) and `false` when the token was stale (a
    /// concurrent classifier or edit already bumped it). A stale return is
    /// NOT an error — the caller should discard the now-superseded result.
    ///
    /// On success, `classifier_token` is incremented so subsequent CAS calls
    /// from in-flight classifiers that still hold the old token are rejected.
    async fn set_assignment(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
        assignment: Option<TaskAssignment>,
        expected_token: u64,
    ) -> Result<bool, AoError>;

    /// Transition the agent's active `Paused` tasklist to `Active`, then kick
    /// the feeder so pending tasks begin dispatching. Idempotent on an
    /// already-`Active` list (re-kicks `advance()` without error). Errors with
    /// `InvalidTasklistTransition` if the agent has no `Active` or `Paused`
    /// tasklist.
    ///
    /// The returned [`StartOutcome`] reflects what this call actually did —
    /// see [`StartOutcomeKind`] — rather than a fixed success payload.
    /// Implementations MUST surface a start path that could not reach or run
    /// the dispatcher (e.g. the feeder is unavailable) as an `Err`, never as
    /// an `Ok` outcome with nothing to show for it.
    ///
    /// Default impl returns `Internal` so existing mocks compile without
    /// changes; override in production implementations.
    async fn start_for_agent(&self, _agent_id: &str) -> Result<StartOutcome, AoError> {
        Err(AoError::Internal("start_for_agent not implemented".into()))
    }

    /// Resume the agent's most recent `Failed` tasklist: reset every `Failed`
    /// task back to `Pending` (clearing attempt counts and error logs), flip
    /// the tasklist to `Active`, and kick the feeder so tasks re-dispatch.
    /// Errors with `InvalidTasklistTransition` if the agent has no `Failed`
    /// tasklist, or if another `Active`/`Paused` tasklist already occupies the
    /// active slot.
    ///
    /// Default impl returns `Internal` so existing mocks compile without
    /// changes; override in production implementations.
    async fn resume_for_agent(&self, _agent_id: &str) -> Result<ResumeOutcome, AoError> {
        Err(AoError::Internal("resume_for_agent not implemented".into()))
    }

    /// Remove a single unstarted (`Pending`) task from the agent's active
    /// tasklist by marking it `Skipped`. Returns an error if the task is
    /// not `Pending` (in-progress, completed, or already terminal tasks
    /// cannot be deleted this way).
    ///
    /// Default impl returns `Internal` so existing mocks compile without
    /// changes; override in production implementations.
    async fn delete_task_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        _task_id: &str,
    ) -> Result<(), AoError> {
        Err(AoError::Internal("delete_task_for_agent not implemented".into()))
    }

    /// Reset a zombie `InProgress` task back to `Pending` so the feeder can
    /// re-dispatch it. Clears the task's `assignment` and bumps
    /// `classifier_token` to invalidate any in-flight classifier CAS.
    /// Only valid on a task currently `InProgress`; returns
    /// `InvalidTasklistTransition` for any other status. Safe to call when
    /// the runner is already dead — idempotent from the feeder's perspective.
    ///
    /// Default impl returns `Internal` so existing mocks compile without
    /// changes; override in production implementations.
    async fn requeue_task_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        _task_id: &str,
    ) -> Result<(), AoError> {
        Err(AoError::Internal("requeue_task_for_agent not implemented".into()))
    }

    /// Halt a single `InProgress` task by transitioning it to `Stopped`.
    /// Clears `assignment` and bumps `classifier_token` to invalidate any
    /// in-flight classifier CAS. Sibling tasks are never touched. In SEQ
    /// groups the stopped task blocks all tasks behind it until resumed; in
    /// PAR groups siblings continue unaffected.
    ///
    /// Only valid on a task currently `InProgress`; returns
    /// `InvalidTasklistTransition` for any other status. Safe when the runner
    /// is already dead — the runner's eventual completion or failure will
    /// transition the task out of `Stopped` normally.
    ///
    /// Default impl returns `Internal` so existing mocks compile without
    /// changes; override in production implementations.
    async fn stop_task_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        _task_id: &str,
    ) -> Result<(), AoError> {
        Err(AoError::Internal("stop_task_for_agent not implemented".into()))
    }

    /// Re-queue a `Stopped` task by transitioning it back to `Pending` so the
    /// feeder can re-dispatch it. Clears `assignment` and bumps
    /// `classifier_token`. In SEQ groups the feeder advances immediately after
    /// the transition; in PAR groups the advance does not disturb running siblings.
    ///
    /// Only valid on a task currently `Stopped`; returns
    /// `InvalidTasklistTransition` for any other status.
    ///
    /// Default impl returns `Internal` so existing mocks compile without
    /// changes; override in production implementations.
    async fn resume_task_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        _task_id: &str,
    ) -> Result<(), AoError> {
        Err(AoError::Internal("resume_task_for_agent not implemented".into()))
    }

    /// Stamp a `project_id` onto an existing agent-owned tasklist. Called by
    /// `TodoCreate` immediately after creation when the calling agent is running
    /// inside a project-scoped channel, so the resulting tasklist is linked back
    /// to the project for route listing and completion-loop routing.
    ///
    /// Default impl returns `Internal`; override in production implementations.
    async fn stamp_project_id_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        _project_id: &str,
    ) -> Result<(), AoError> {
        Err(AoError::Internal("stamp_project_id_for_agent not implemented".into()))
    }

    /// Scan the agent's active tasklist for `InProgress` tasks whose owning
    /// runner has zero active runs in the instance registry, returning one
    /// [`ZombieReport`] per zombie found.
    ///
    /// Tasks dispatched within `grace_secs` seconds are excluded to avoid
    /// falsely flagging a run that is still starting up. Tasks with no recorded
    /// dispatch timestamp (e.g. after a server restart) are included immediately
    /// — a stale on-disk `InProgress` with no in-memory tracking is exactly the
    /// zombie case we want to surface.
    ///
    /// Returns an empty `Vec` when the agent has no active tasklist, when no
    /// `InProgress` tasks are found, or when the instance registry is not wired.
    /// This is a non-destructive read — it never modifies task state. Callers
    /// that want auto-recovery should call `requeue_task_for_agent` for each
    /// returned zombie.
    ///
    /// Default impl returns `Internal` so existing mocks compile without
    /// changes; override in production implementations.
    async fn check_zombies_for_agent(
        &self,
        _agent_id: &str,
        _grace_secs: u64,
    ) -> Result<Vec<ZombieReport>, AoError> {
        Err(AoError::Internal("check_zombies_for_agent not implemented".into()))
    }
}
