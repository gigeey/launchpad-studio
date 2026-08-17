use super::AssignmentCreate;
use super::super::tests::temp_store;
use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::assignment::{AssignmentThreadPolicy, AssignmentTrigger};
use serde_json::json;
use std::path::PathBuf;

#[tokio::test]
async fn subagent_gate_blocks_create() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp")).with_depth(1);
    let out = AssignmentCreate
        .invoke(
            json!({
                "name": "test",
                "instruction": "do the thing",
                "trigger": {"type": "schedule", "cron_expr": "0 9 * * *"}
            }),
            &ctx,
        )
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
async fn missing_store_returns_non_recoverable_error() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    let out = AssignmentCreate
        .invoke(
            json!({
                "name": "test",
                "instruction": "do the thing",
                "trigger": {"type": "schedule", "cron_expr": "0 9 * * *"}
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable);
            assert!(message.contains("not available"), "got: {message}");
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn happy_path_schedule_trigger_defaults_thread_policy_main() {
    let (_dir, store) = temp_store().await;
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));
    let out = AssignmentCreate
        .invoke(
            json!({
                "name": "Daily check",
                "instruction": "Summarize overnight activity",
                "trigger": {"type": "schedule", "cron_expr": "0 9 * * *"}
            }),
            &ctx,
        )
        .await
        .unwrap();
    let id = match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("[Assignment created:"), "got: {s}");
            assert!(s.contains("trigger=schedule"), "got: {s}");
            assert!(s.contains("next_fire_at="), "got: {s}");
            s.split("id=\"").nth(1).unwrap().split('"').next().unwrap().to_string()
        }
        other => panic!("expected Text, got {:?}", other),
    };
    let stored = store.get(&id).await.expect("assignment persisted");
    assert_eq!(stored.thread_policy, AssignmentThreadPolicy::Main);
    assert!(matches!(stored.trigger, AssignmentTrigger::Cron { .. }));
}

#[tokio::test]
async fn happy_path_webhook_trigger_defaults_thread_policy_fresh() {
    let (_dir, store) = temp_store().await;
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));
    let out = AssignmentCreate
        .invoke(
            json!({
                "name": "Inbound hook",
                "instruction": "Handle the inbound event",
                "trigger": {"type": "webhook", "token": "secret"}
            }),
            &ctx,
        )
        .await
        .unwrap();
    let id = match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("trigger=webhook"), "got: {s}");
            s.split("id=\"").nth(1).unwrap().split('"').next().unwrap().to_string()
        }
        other => panic!("expected Text, got {:?}", other),
    };
    let stored = store.get(&id).await.expect("assignment persisted");
    assert_eq!(stored.thread_policy, AssignmentThreadPolicy::Fresh);
    assert!(matches!(stored.trigger, AssignmentTrigger::Webhook { token: Some(ref t), .. } if t == "secret"));
}

#[tokio::test]
async fn explicit_thread_policy_overrides_default() {
    let (_dir, store) = temp_store().await;
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));
    AssignmentCreate
        .invoke(
            json!({
                "name": "Custom",
                "instruction": "do it",
                "trigger": {"type": "schedule", "cron_expr": "0 9 * * *"},
                "thread_policy": "dedicated"
            }),
            &ctx,
        )
        .await
        .unwrap();
    let all = store.list_for_agent("agent-1").await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].thread_policy, AssignmentThreadPolicy::Dedicated);
}

#[tokio::test]
async fn invalid_cron_expression_returns_recoverable_error() {
    let (_dir, store) = temp_store().await;
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(store);
    let out = AssignmentCreate
        .invoke(
            json!({
                "name": "Bad",
                "instruction": "do it",
                "trigger": {"type": "schedule", "cron_expr": "not a cron"}
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("invalid cron expression"), "got: {message}");
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_name_returns_recoverable_error() {
    let (_dir, store) = temp_store().await;
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(store);
    let out = AssignmentCreate
        .invoke(
            json!({
                "instruction": "do it",
                "trigger": {"type": "schedule", "cron_expr": "0 9 * * *"}
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_trigger_returns_recoverable_error() {
    let (_dir, store) = temp_store().await;
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(store);
    let out = AssignmentCreate
        .invoke(json!({"name": "test", "instruction": "do it"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("trigger is required"), "got: {message}");
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn connector_event_trigger_is_accepted_and_persisted() {
    let (_dir, store) = temp_store().await;
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));
    let out = AssignmentCreate
        .invoke(
            json!({
                "name": "Watch inbox",
                "instruction": "react to new email",
                "trigger": {
                    "type": "connector_event",
                    "server_name": "gmail",
                    "poll": {"tool_name": "list_messages"},
                    "poll_interval_secs": 60
                }
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("[Assignment created:"), "got: {s}");
            assert!(s.contains("trigger=connector_event"), "got: {s}");
            assert!(s.contains("next_fire_at="), "got: {s}");
        }
        other => panic!("expected Text, got {:?}", other),
    }
    let all = store.list_for_agent("agent-1").await;
    assert_eq!(all.len(), 1, "the connector_event assignment should be persisted");
    let stored = &all[0];
    match &stored.trigger {
        AssignmentTrigger::ConnectorEvent {
            server_name,
            poll,
            poll_interval_secs,
        } => {
            assert_eq!(server_name, "gmail");
            assert_eq!(poll.tool_name, "list_messages");
            assert_eq!(*poll_interval_secs, 60);
        }
        other => panic!("expected ConnectorEvent trigger, got {:?}", other),
    }
    // Connector-event assignments default to a fresh thread per fire and are
    // scheduled to poll immediately so the first tick seeds the dedup baseline.
    assert_eq!(stored.thread_policy, AssignmentThreadPolicy::Fresh);
    assert!(stored.next_fire_at.is_some(), "should be scheduled to poll ASAP");
    assert!(stored.last_event_cursor.is_none(), "no cursor until the first poll");
}

#[tokio::test]
async fn bindings_round_trip() {
    let (_dir, store) = temp_store().await;
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));
    AssignmentCreate
        .invoke(
            json!({
                "name": "With bindings",
                "instruction": "do it",
                "trigger": {"type": "webhook"},
                "bindings": [{"kind": "mcp_server", "ref_id": "gmail"}]
            }),
            &ctx,
        )
        .await
        .unwrap();
    let all = store.list_for_agent("agent-1").await;
    assert_eq!(all[0].bindings.len(), 1);
    assert_eq!(all[0].bindings[0].kind, "mcp_server");
    assert_eq!(all[0].bindings[0].ref_id, "gmail");
}

#[test]
fn cli_compatible_is_true() {
    assert!(AssignmentCreate.cli_compatible());
}
