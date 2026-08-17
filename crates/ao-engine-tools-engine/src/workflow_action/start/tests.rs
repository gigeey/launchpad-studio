use super::WorkflowActionStart;
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
async fn happy_path_starts_task() {
    let ctx = ctx_with_runner();
    let out = WorkflowActionStart
        .invoke(json!({"task_id": "task-abc"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("task-abc"));
            assert!(s.contains("started"));
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_workflow_runner_returns_non_recoverable_error() {
    let ctx = ctx_no_runner();
    let out = WorkflowActionStart
        .invoke(json!({"task_id": "t1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(!recoverable),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[test]
fn cli_compatible_is_true() {
    assert!(WorkflowActionStart.cli_compatible());
}
