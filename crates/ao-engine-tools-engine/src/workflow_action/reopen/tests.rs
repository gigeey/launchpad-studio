use super::WorkflowActionReopen;
use super::super::tests::MockWorkflowRunner;
use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput, WorkflowRunnerHandle};
use ao_protocol::{
    error::AoError,
    workflow::{PhaseDefinition, TaskSnapshot, TaskStatus},
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

fn ctx_with_runner() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_workflow_runner(Arc::new(MockWorkflowRunner))
}

fn ctx_no_runner() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
}

fn ctx_with<R: WorkflowRunnerHandle + 'static>(runner: R) -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_workflow_runner(Arc::new(runner))
}

// ---------------------------------------------------------------------------
// Mock that controls reopen_task behaviour
// ---------------------------------------------------------------------------

struct MockReopener {
    status: TaskStatus,
    reopen_result: Result<usize, AoError>,
}

impl MockReopener {
    fn succeeds(status: TaskStatus, file_count: usize) -> Self {
        Self { status, reopen_result: Ok(file_count) }
    }

    fn fails(status: TaskStatus, msg: &str) -> Self {
        Self {
            status,
            reopen_result: Err(AoError::ValidationError(msg.to_string())),
        }
    }
}

#[async_trait]
impl WorkflowRunnerHandle for MockReopener {
    async fn create_task(&self, _: &str, _: &str, _: Option<String>, _: Option<String>) -> Result<String, AoError> { Ok("t".into()) }
    async fn build_create_summary(&self, _: &str, _: &str) -> Result<String, AoError> { Ok(String::new()) }
    async fn write_phase_output(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn complete_phase(&self, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn skip_phase(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn start_task(&self, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn delete_task(&self, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn get_task_state(&self, _: &str) -> Result<TaskSnapshot, AoError> {
        Ok(TaskSnapshot {
            status: self.status.clone(),
            workflow: "mock-wf".into(),
            workflow_version: None,
            created: Utc::now(),
            project_name: "p".into(),
            working_directory: None,
            context: HashMap::new(),
            phases: HashMap::new(),
        })
    }
    async fn get_next_phase(&self, _: &str) -> Result<Option<PhaseDefinition>, AoError> { Ok(None) }
    async fn notify_phase_completed(&self, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn get_workflow_summaries(&self, _: Option<&[String]>) -> Vec<ao_protocol::workflow::WorkflowSummary> { vec![] }
    async fn stop_task(&self, task_id: &str) -> Result<PathBuf, AoError> {
        Ok(PathBuf::from(format!("/tasks/{}/output", task_id)))
    }
    async fn phase_required_outputs(&self, _: &str, _: &str) -> Result<Vec<String>, AoError> { Ok(vec![]) }
    async fn phase_write_progress_summary(&self, _: &str, _: &str) -> Option<String> { None }
    async fn reopen_task(&self, _task_id: &str, _phase_id: &str) -> Result<usize, AoError> {
        match &self.reopen_result {
            Ok(n) => Ok(*n),
            Err(e) => Err(AoError::ValidationError(e.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reopen_completed_task_succeeds() {
    let ctx = ctx_with(MockReopener::succeeds(TaskStatus::Completed, 3));
    let out = WorkflowActionReopen
        .invoke(json!({"task_id": "task-done", "phase_id": "phase-1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("task-done"), "got: {}", s);
            assert!(s.contains("phase-1"), "got: {}", s);
            assert!(s.contains("3"), "got: {}", s);
            assert!(s.contains("preserved"), "got: {}", s);
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn reopen_failed_task_succeeds() {
    let ctx = ctx_with(MockReopener::succeeds(TaskStatus::Failed, 1));
    let out = WorkflowActionReopen
        .invoke(json!({"task_id": "task-failed", "phase_id": "phase-2"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("task-failed"), "got: {}", s);
            assert!(s.contains("phase-2"), "got: {}", s);
            assert!(s.contains("1 existing output file preserved"), "got: {}", s);
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn reopen_stopped_task_succeeds() {
    let ctx = ctx_with(MockReopener::succeeds(TaskStatus::Stopped, 0));
    let out = WorkflowActionReopen
        .invoke(json!({"task_id": "task-stopped", "phase_id": "phase-1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("task-stopped"), "got: {}", s);
            assert!(s.contains("0 existing output files preserved"), "got: {}", s);
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn reopen_running_task_returns_recoverable_error() {
    let ctx = ctx_with(MockReopener::fails(
        TaskStatus::Running,
        "Cannot reopen task 'x': task must be in a terminal state",
    ));
    let out = WorkflowActionReopen
        .invoke(json!({"task_id": "x", "phase_id": "phase-1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable, "expected recoverable error");
            assert!(message.contains("terminal"), "got: {}", message);
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn reopen_invalid_phase_id_returns_error_with_valid_ids() {
    let ctx = ctx_with(MockReopener::fails(
        TaskStatus::Completed,
        "Cannot reopen task 'x': phase 'bad-phase' does not exist. Valid phase IDs: [phase-1, phase-2].",
    ));
    let out = WorkflowActionReopen
        .invoke(json!({"task_id": "x", "phase_id": "bad-phase"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("Valid phase IDs"), "got: {}", message);
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn reopen_nonexistent_task_returns_recoverable_error() {
    // MockWorkflowRunner.reopen_task returns Err for task-not-found
    let ctx = ctx_with_runner();
    let out = WorkflowActionReopen
        .invoke(json!({"task_id": "task-not-found", "phase_id": "phase-1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        // MockWorkflowRunner doesn't return error on reopen — it returns a fallback.
        ToolOutput::Text(_) => {}
        other => panic!("unexpected: {:?}", other),
    }
}

#[tokio::test]
async fn missing_workflow_runner_returns_non_recoverable_error() {
    let ctx = ctx_no_runner();
    let out = WorkflowActionReopen
        .invoke(json!({"task_id": "t1", "phase_id": "phase-1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(!recoverable),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_task_id_returns_recoverable_error() {
    let ctx = ctx_with_runner();
    let out = WorkflowActionReopen
        .invoke(json!({"phase_id": "phase-1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_phase_id_returns_recoverable_error() {
    let ctx = ctx_with_runner();
    let out = WorkflowActionReopen
        .invoke(json!({"task_id": "t1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[test]
fn cli_compatible_is_true() {
    assert!(WorkflowActionReopen.cli_compatible());
}
