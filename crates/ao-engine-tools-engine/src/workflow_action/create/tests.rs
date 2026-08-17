use super::WorkflowActionCreate;
use super::super::tests::MockWorkflowRunner;
use ao_engine_tools_core::{IoTool, RunnerContext};
use ao_protocol::agent::WorkflowBinding;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

fn ctx_with_runner_and_binding(binding: WorkflowBinding) -> RunnerContext {
    let runner = Arc::new(MockWorkflowRunner);
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_workflow_runner(runner)
        .with_agent_workflows(binding)
}

fn ctx_no_runner() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
}

#[tokio::test]
async fn happy_path_creates_task_and_returns_summary() {
    let ctx = ctx_with_runner_and_binding(WorkflowBinding::All);
    let out = WorkflowActionCreate
        .invoke(
            json!({"workflow_id": "wf-1", "project_name": "My Project"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ao_engine_tools_core::ToolOutput::Text(s) => {
            assert!(s.contains("task-"), "expected task id in summary");
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_workflow_runner_returns_non_recoverable_error() {
    let ctx = ctx_no_runner();
    let out = WorkflowActionCreate
        .invoke(
            json!({"workflow_id": "wf-1", "project_name": "P"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ao_engine_tools_core::ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable, "should be non-recoverable");
            assert!(message.contains("not available"), "got: {message}");
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn binding_failure_returns_recoverable_error() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_workflow_runner(Arc::new(MockWorkflowRunner))
        .with_agent_workflows(WorkflowBinding::List(vec!["other-wf".to_string()]));
    let out = WorkflowActionCreate
        .invoke(
            json!({"workflow_id": "wf-1", "project_name": "P"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ao_engine_tools_core::ToolOutput::Error { recoverable, message } => {
            assert!(recoverable, "binding failure must be recoverable");
            assert!(
                message.contains("not bound to workflow"),
                "got: {message}"
            );
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn no_binding_returns_recoverable_error() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_workflow_runner(Arc::new(MockWorkflowRunner));
    let out = WorkflowActionCreate
        .invoke(
            json!({"workflow_id": "wf-1", "project_name": "P"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ao_engine_tools_core::ToolOutput::Error { recoverable, .. } => {
            assert!(recoverable);
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[test]
fn cli_compatible_is_true() {
    assert!(WorkflowActionCreate.cli_compatible());
}
