pub mod complete_phase;
pub mod create;
pub mod delete;
pub mod read_state;
pub mod reopen;
pub mod skip_phase;
pub mod start;
pub mod stop;
pub mod write_output;

pub use complete_phase::WorkflowActionCompletePhase;
pub use create::WorkflowActionCreate;
pub use delete::WorkflowActionDelete;
pub use read_state::WorkflowActionReadState;
pub use reopen::WorkflowActionReopen;
pub use skip_phase::WorkflowActionSkipPhase;
pub use start::WorkflowActionStart;
pub use stop::WorkflowActionStop;
pub use write_output::WorkflowActionWriteOutput;

use ao_engine_tools_core::Registry;
use std::sync::Arc;

pub fn register_workflow_action_tools(registry: &mut Registry) {
    registry.register_io(Arc::new(WorkflowActionCreate));
    registry.register_io(Arc::new(WorkflowActionWriteOutput));
    registry.register_io(Arc::new(WorkflowActionCompletePhase));
    registry.register_io(Arc::new(WorkflowActionSkipPhase));
    registry.register_io(Arc::new(WorkflowActionStart));
    registry.register_io(Arc::new(WorkflowActionReadState));
    registry.register_io(Arc::new(WorkflowActionDelete));
    registry.register_io(Arc::new(WorkflowActionStop));
    registry.register_io(Arc::new(WorkflowActionReopen));
}

#[cfg(test)]
pub(crate) mod tests {
    use ao_engine_tools_core::WorkflowRunnerHandle;
    use ao_protocol::{
        error::AoError,
        workflow::{PhaseDefinition, TaskSnapshot},
    };
    use async_trait::async_trait;
    use chrono::Utc;
    use std::collections::HashMap;

    /// Minimal mock runner for unit tests. All mutating operations succeed;
    /// `get_task_state` returns a predictable `Pending` snapshot.
    pub struct MockWorkflowRunner;

    #[async_trait]
    impl WorkflowRunnerHandle for MockWorkflowRunner {
        async fn create_task(
            &self,
            _workflow_id: &str,
            _project_name: &str,
            _working_directory: Option<String>,
            _context: Option<String>,
        ) -> Result<String, AoError> {
            Ok("task-mock-001".to_string())
        }

        async fn build_create_summary(
            &self,
            task_id: &str,
            workflow_id: &str,
        ) -> Result<String, AoError> {
            Ok(format!(
                "## Task Created\n- **Task ID**: `{}`\n- **Workflow**: `{}`",
                task_id, workflow_id
            ))
        }

        async fn write_phase_output(
            &self,
            _task_id: &str,
            _filename: &str,
            _content: &str,
        ) -> Result<(), AoError> {
            Ok(())
        }

        async fn complete_phase(
            &self,
            _task_id: &str,
            _phase_id: &str,
        ) -> Result<(), AoError> {
            Ok(())
        }

        async fn skip_phase(
            &self,
            _task_id: &str,
            _phase_id: &str,
            _reason: &str,
        ) -> Result<(), AoError> {
            Ok(())
        }

        async fn start_task(&self, _task_id: &str) -> Result<(), AoError> {
            Ok(())
        }

        async fn delete_task(&self, task_id: &str) -> Result<(), AoError> {
            // `task-not-found` is the magic id reserved for negative-path
            // tests; everything else succeeds.
            if task_id == "task-not-found" {
                Err(AoError::TaskNotFound(task_id.to_string()))
            } else {
                Ok(())
            }
        }

        async fn get_task_state(&self, task_id: &str) -> Result<TaskSnapshot, AoError> {
            Ok(TaskSnapshot {
                status: ao_protocol::workflow::TaskStatus::Pending,
                workflow: "mock-workflow".to_string(),
                workflow_version: None,
                created: Utc::now(),
                project_name: task_id.to_string(),
                working_directory: None,
                context: HashMap::new(),
                phases: HashMap::new(),
            })
        }

        async fn get_next_phase(
            &self,
            _task_id: &str,
        ) -> Result<Option<PhaseDefinition>, AoError> {
            // Mock returns None — all phases are considered completed.
            // Negative-path tests that need a "next phase exists" shape
            // can swap in a more elaborate mock when needed.
            Ok(None)
        }

        async fn notify_phase_completed(
            &self,
            _task_id: &str,
            _phase_id: &str,
        ) -> Result<(), AoError> {
            // No queue wired in unit tests.
            Ok(())
        }

        async fn get_workflow_summaries(
            &self,
            _ids: Option<&[String]>,
        ) -> Vec<ao_protocol::workflow::WorkflowSummary> {
            vec![]
        }

        async fn stop_task(&self, task_id: &str) -> Result<std::path::PathBuf, AoError> {
            if task_id == "task-not-found" {
                Err(AoError::TaskNotFound(task_id.to_string()))
            } else {
                Ok(std::path::PathBuf::from(format!("/tmp/tasks/{}/output", task_id)))
            }
        }

        async fn reopen_task(&self, task_id: &str, _phase_id: &str) -> Result<usize, AoError> {
            if task_id == "task-not-found" {
                Err(AoError::TaskNotFound(task_id.to_string()))
            } else {
                Ok(0)
            }
        }

        async fn phase_required_outputs(
            &self,
            _task_id: &str,
            _phase_id: &str,
        ) -> Result<Vec<String>, AoError> {
            Ok(vec![])
        }

        async fn phase_write_progress_summary(
            &self,
            _task_id: &str,
            _filename_just_written: &str,
        ) -> Option<String> {
            None
        }
    }
}
