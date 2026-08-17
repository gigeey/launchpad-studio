use super::WorkflowActionReadState;
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
async fn happy_path_returns_state_json() {
    let ctx = ctx_with_runner();
    let out = WorkflowActionReadState
        .invoke(json!({"task_id": "t1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("Workflow Task State"));
            assert!(s.contains("```json"));
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_workflow_runner_returns_non_recoverable_error() {
    let ctx = ctx_no_runner();
    let out = WorkflowActionReadState
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
    assert!(WorkflowActionReadState.cli_compatible());
}
