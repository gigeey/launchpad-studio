use async_trait::async_trait;
use ao_protocol::assignment::TriggerEventContext;
use ao_protocol::error::AoError;
use ao_protocol::workflow::{PhaseDefinition, TaskSnapshot};

/// Trait abstraction over `ao_engine::WorkflowRunner` that lets
/// `ao-engine-tools-core` (and the tools built on top of it) hold a handle to
/// the runner without introducing a circular crate dependency.
///
/// `ao-engine` already depends on `ao-engine-tools-core` for `RunnerContext`.
/// Placing this trait here lets `ao-engine` implement it on its concrete
/// `WorkflowRunner` while tools in `ao-engine-tools-engine` call through this
/// surface without needing to know the concrete type.
#[async_trait]
pub trait WorkflowRunnerHandle: Send + Sync {
    /// Create a new workflow task. Returns the generated task_id.
    async fn create_task(
        &self,
        workflow_id: &str,
        project_name: &str,
        working_directory: Option<String>,
        context: Option<String>,
    ) -> Result<String, AoError>;

    /// Create a new workflow task, additionally attaching structured trigger
    /// event data (e.g. from a `ConnectorEvent` assignment fire) alongside
    /// the plain-text `context`. Deferred workflow-bound assignment fires are
    /// the intended caller — this is what lets a workflow task eventually see
    /// the same "what actually triggered this" data the plain-agent path
    /// gets via `fire_assignment`'s `event_context`.
    ///
    /// The default implementation folds `event`'s summary and JSON-encoded
    /// payload into `context` as an appended block and delegates to
    /// `create_task`, so every existing implementor gets this for free;
    /// override only if a richer representation is needed.
    async fn create_task_with_event(
        &self,
        workflow_id: &str,
        project_name: &str,
        working_directory: Option<String>,
        context: Option<String>,
        event: Option<TriggerEventContext>,
    ) -> Result<String, AoError> {
        let merged_context = match event {
            Some(ev) => {
                let payload_json = serde_json::to_string(&ev.payload).unwrap_or_default();
                let event_block =
                    format!("Trigger event: {}\nEvent payload (JSON): {}", ev.summary, payload_json);
                Some(match context {
                    Some(c) if !c.is_empty() => format!("{c}\n\n{event_block}"),
                    _ => event_block,
                })
            }
            None => context,
        };
        self.create_task(workflow_id, project_name, working_directory, merged_context)
            .await
    }

    /// Build a human-readable creation summary for a newly created task.
    async fn build_create_summary(
        &self,
        task_id: &str,
        workflow_id: &str,
    ) -> Result<String, AoError>;

    /// Write content to a phase output file.
    async fn write_phase_output(
        &self,
        task_id: &str,
        filename: &str,
        content: &str,
    ) -> Result<(), AoError>;

    /// Mark a phase as completed. Validates declared outputs exist first.
    async fn complete_phase(&self, task_id: &str, phase_id: &str) -> Result<(), AoError>;

    /// Mark a phase as skipped with an explanatory reason string.
    async fn skip_phase(&self, task_id: &str, phase_id: &str, reason: &str) -> Result<(), AoError>;

    /// Transition a pending task to running state.
    async fn start_task(&self, task_id: &str) -> Result<(), AoError>;

    /// Delete a task entirely from disk. Destructive — the on-disk directory
    /// and snapshot are removed and cannot be recovered. Returns
    /// `AoError::TaskNotFound` when the task id does not exist.
    async fn delete_task(&self, task_id: &str) -> Result<(), AoError>;

    /// Read the current persisted snapshot for a task.
    async fn get_task_state(&self, task_id: &str) -> Result<TaskSnapshot, AoError>;

    /// Return the next phase definition the workflow should execute, or
    /// `Ok(None)` if every phase is already completed or skipped. Used by
    /// the `WorkflowActionCompletePhase` / `WorkflowActionSkipPhase` tools
    /// to nudge the agent toward the next pre-fillable phase while the task
    /// is still `Pending`.
    async fn get_next_phase(
        &self,
        task_id: &str,
    ) -> Result<Option<PhaseDefinition>, AoError>;

    /// Notify the workflow queue manager that a phase has finished (either
    /// `complete_phase` or `skip_phase`). When the task is `Running`, the
    /// queue manager uses this signal to auto-dispatch the next phase. For
    /// `Pending` tasks it is a no-op on the queue side — the queue manager
    /// only advances tasks in `Running` state. Returns `Ok(())` when there
    /// is no queue handle wired (e.g. in unit tests that construct a bare
    /// `WorkflowRunner`); callers should treat this as best-effort.
    async fn notify_phase_completed(
        &self,
        task_id: &str,
        phase_id: &str,
    ) -> Result<(), AoError>;

    /// Fetch workflow summaries for system-prompt assembly.
    ///
    /// When `ids` is `Some`, returns only the summaries for the given workflow
    /// IDs (in order, skipping unknown IDs). When `ids` is `None`, returns all
    /// registered workflows. Used by `NativeAgentRunner` to build the
    /// "Workflows in scope" block without holding a direct reference to the
    /// concrete `WorkflowRegistry`.
    async fn get_workflow_summaries(
        &self,
        ids: Option<&[String]>,
    ) -> Vec<ao_protocol::workflow::WorkflowSummary>;

    /// Stop a task by marking it as Stopped and any in-flight Running phases
    /// as Stopped. Emits a WorkflowTaskStopped event. Returns the path to the
    /// task's output directory so the caller can include it in the success
    /// message.
    ///
    /// Does NOT check for terminal-state idempotency — callers should
    /// check `get_task_state` first if they need that behaviour.
    async fn stop_task(&self, task_id: &str) -> Result<std::path::PathBuf, AoError>;

    /// Return the required output filenames for the given phase in declaration
    /// order. Returns an empty `Vec` when the phase has no declared outputs
    /// (free-form phase). Returns `Err` when the task or phase cannot be found.
    async fn phase_required_outputs(
        &self,
        task_id: &str,
        phase_id: &str,
    ) -> Result<Vec<String>, AoError>;

    /// Build a phase-progress summary string for inclusion in the
    /// `WorkflowActionWriteOutput` result message.
    async fn phase_write_progress_summary(
        &self,
        task_id: &str,
        filename_just_written: &str,
    ) -> Option<String>;

    /// Reopen a terminal task (Completed, Failed, or Stopped) to a specific
    /// phase for re-run. Sets the task back to Pending, removes the target
    /// phase's state so `get_next_phase` will schedule it again (all other
    /// phase states are preserved). Does NOT modify output files.
    ///
    /// Returns the number of regular, non-hidden output files in the task's
    /// output directory (for use in the success message).
    async fn reopen_task(&self, task_id: &str, phase_id: &str) -> Result<usize, AoError>;
}
