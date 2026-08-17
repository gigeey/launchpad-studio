use super::AssignmentList;
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
        name: format!("Assignment {id}"),
        instruction: "do the thing".to_string(),
        working_directory: None,
        trigger: AssignmentTrigger::Cron {
            cron_expr: "0 9 * * *".to_string(),
            is_recurring: true,
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
async fn subagent_gate_blocks_list() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp")).with_depth(1);
    let out = AssignmentList.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("top-level agent"), "got: {message}");
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_store_returns_non_recoverable_error() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    let out = AssignmentList.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable);
            assert!(message.contains("not available"), "got: {message}");
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn empty_store_returns_no_assignments_text() {
    let (_dir, store) = temp_store().await;
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(store);
    let out = AssignmentList.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("No assignments found"), "got: {s}"),
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn defaults_to_calling_agent_assignments() {
    let (_dir, store) = temp_store().await;
    store.add(sample("a1", "agent-1")).await.unwrap();
    store.add(sample("a2", "agent-2")).await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(store);
    let out = AssignmentList.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("a1"), "got: {s}");
            assert!(!s.contains("a2"), "got: {s}");
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn agent_id_filter_overrides_calling_agent() {
    let (_dir, store) = temp_store().await;
    store.add(sample("a1", "agent-1")).await.unwrap();
    store.add(sample("a2", "agent-2")).await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(store);
    let out = AssignmentList
        .invoke(json!({"agent_id": "agent-2"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("a2"), "got: {s}");
            assert!(!s.contains("a1"), "got: {s}");
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[test]
fn cli_compatible_is_true() {
    assert!(AssignmentList.cli_compatible());
}
