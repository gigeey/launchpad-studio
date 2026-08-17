use super::WorkflowActionCompletePhase;
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
async fn happy_path_completes_phase() {
    let ctx = ctx_with_runner();
    let out = WorkflowActionCompletePhase
        .invoke(
            json!({"task_id": "t1", "phase_id": "phase-1"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("phase-1")),
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_workflow_runner_returns_non_recoverable_error() {
    let ctx = ctx_no_runner();
    let out = WorkflowActionCompletePhase
        .invoke(
            json!({"task_id": "t1", "phase_id": "p1"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(!recoverable),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[test]
fn cli_compatible_is_true() {
    assert!(WorkflowActionCompletePhase.cli_compatible());
}
