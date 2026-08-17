use super::WorkflowActionDelete;
use super::super::tests::MockWorkflowRunner;
use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

fn ctx_with_runner() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_workflow_runner(Arc::new(MockWorkflowRunner))
}

fn ctx_no_runner() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
}

#[tokio::test]
async fn happy_path_deletes_task() {
    let ctx = ctx_with_runner();
    let out = WorkflowActionDelete
        .invoke(json!({"task_id": "task-abc"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("task-abc"), "expected task id in success message, got {:?}", s);
            assert!(s.contains("deleted"));
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn task_not_found_returns_recoverable_error() {
    let ctx = ctx_with_runner();
    let out = WorkflowActionDelete
        .invoke(json!({"task_id": "task-not-found"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected recoverable Error, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_workflow_runner_returns_non_recoverable_error() {
    let ctx = ctx_no_runner();
    let out = WorkflowActionDelete
        .invoke(json!({"task_id": "task-1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(!recoverable),
        other => panic!("expected non-recoverable Error, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_task_id_returns_recoverable_error() {
    let ctx = ctx_with_runner();
    let out = WorkflowActionDelete
        .invoke(json!({}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected recoverable Error, got {:?}", other),
    }
}

#[test]
fn cli_compatible_is_true() {
    assert!(WorkflowActionDelete.cli_compatible());
}
