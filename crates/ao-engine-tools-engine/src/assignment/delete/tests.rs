use super::AssignmentDelete;
use super::super::tests::temp_store;
use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::assignment::{Assignment, AssignmentThreadPolicy, AssignmentTrigger, OutputMode};
use chrono::Utc;
use serde_json::json;
use std::path::PathBuf;

fn sample(id: &str, agent_id: &str) -> Assignment {
    let now = Utc::now();
    Assignment {
        id: id.to_string(),
        agent_id: agent_id.to_string(),
        name: "Sample".to_string(),
        instruction: "do it".to_string(),
        working_directory: None,
        trigger: AssignmentTrigger::Webhook {
            token: None,
            route_name: None,
            secret_ref: None,
            events: vec![],
            filters: None,
            prompt_template: None,
            deliver: Default::default(),
        },
        bindings: vec![],
        output_mode: OutputMode::Background,
        thread_policy: AssignmentThreadPolicy::default(),
        dedicated_thread_id: None,
        enabled: true,
        expires_at: None,
        last_event_cursor: None,
        next_fire_at: None,
        last_run_at: None,
        liveness: Default::default(),
        created_ts: now,
        updated_ts: now,
    }
}

#[tokio::test]
async fn subagent_gate_blocks_delete() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp")).with_depth(1);
    let out = AssignmentDelete
        .invoke(json!({"assignment_id": "a1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("top-level agent"), "got: {message}");
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_assignment_returns_recoverable_error() {
    let (_dir, store) = temp_store().await;
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(store);
    let out = AssignmentDelete
        .invoke(json!({"assignment_id": "ghost"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("not found"), "got: {message}");
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn happy_path_removes_assignment() {
    let (_dir, store) = temp_store().await;
    store.add(sample("a1", "agent-1")).await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));

    let out = AssignmentDelete
        .invoke(json!({"assignment_id": "a1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("deleted"), "got: {s}"),
        other => panic!("expected Text, got {:?}", other),
    }
    assert!(store.get("a1").await.is_none());
}

#[test]
fn cli_compatible_is_true() {
    assert!(AssignmentDelete.cli_compatible());
}
