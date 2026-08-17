use super::AssignmentUpdate;
use super::super::tests::temp_store;
use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::assignment::{Assignment, AssignmentThreadPolicy, AssignmentTrigger, OutputMode};
use ao_protocol::watch_contract::{
    ChangeSpec, IdentitySpec, IdentityStrategy, PredicateSpec, WatchContract, WatchMode,
    WatchSource,
};
use chrono::Utc;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;

fn sample(id: &str, agent_id: &str) -> Assignment {
    let now = Utc::now();
    Assignment {
        id: id.to_string(),
        agent_id: agent_id.to_string(),
        name: "Original".to_string(),
        instruction: "original instruction".to_string(),
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

fn sample_watch_contract() -> WatchContract {
    WatchContract {
        contract_version: 1,
        authored_at: "2026-07-27T09:00:00Z".to_string(),
        authored_by_run: "run-1".to_string(),
        source: WatchSource {
            kind: "example".to_string(),
            ref_: "abc-123".to_string(),
        },
        identity: IdentitySpec {
            strategy: IdentityStrategy::NativeId,
            source_field: Some("unique_identifier".to_string()),
            format: None,
            fields: vec![],
            rationale: "test fixture".to_string(),
        },
        change: ChangeSpec {
            material_fields: vec!["status".to_string()],
            version_hint_field: None,
        },
        predicate: PredicateSpec {
            natural_language: "always fires".to_string(),
            fields: vec![],
            // Vacuously true (an empty `And`) — the legacy grammar had no
            // bare boolean literal, so this is the typed equivalent of what
            // this fixture always meant.
            predicate: ao_protocol::predicate::Predicate::And(vec![]),
        },
        mode: WatchMode::PredicateTransition,
        fields: HashMap::new(),
    }
}

fn sample_agent_watch(id: &str, agent_id: &str) -> Assignment {
    let now = Utc::now();
    Assignment {
        id: id.to_string(),
        agent_id: agent_id.to_string(),
        name: "Watch for finance emails".to_string(),
        instruction: "Summarize the new email from finance.".to_string(),
        working_directory: None,
        trigger: AssignmentTrigger::AgentWatch {
            instruction: "Check my inbox for a new email from finance".to_string(),
            poll_interval_secs: 900,
            connector_scope: Some("gmail".to_string()),
            contract: Some(sample_watch_contract()),
            extraction: None,
            extraction_tool: None,
            extraction_args: None,
            extraction_output_schema_declared: false,
        },
        bindings: vec![],
        output_mode: OutputMode::Background,
        thread_policy: AssignmentThreadPolicy::default(),
        dedicated_thread_id: None,
        enabled: true,
        expires_at: None,
        last_event_cursor: None,
        next_fire_at: Some(now),
        last_run_at: None,
        liveness: Default::default(),
        created_ts: now,
        updated_ts: now,
    }
}

#[tokio::test]
async fn subagent_gate_blocks_update() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp")).with_depth(1);
    let out = AssignmentUpdate
        .invoke(json!({"assignment_id": "a1", "name": "New"}), &ctx)
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
    let out = AssignmentUpdate
        .invoke(json!({"assignment_id": "ghost", "name": "New"}), &ctx)
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
async fn partial_update_only_touches_given_fields() {
    let (_dir, store) = temp_store().await;
    store.add(sample("a1", "agent-1")).await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));

    AssignmentUpdate
        .invoke(json!({"assignment_id": "a1", "name": "Renamed"}), &ctx)
        .await
        .unwrap();

    let got = store.get("a1").await.unwrap();
    assert_eq!(got.name, "Renamed");
    assert_eq!(got.instruction, "original instruction");
    assert!(matches!(got.trigger, AssignmentTrigger::Cron { .. }));
}

#[tokio::test]
async fn enabled_false_disables_without_deleting() {
    let (_dir, store) = temp_store().await;
    store.add(sample("a1", "agent-1")).await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));

    AssignmentUpdate
        .invoke(json!({"assignment_id": "a1", "enabled": false}), &ctx)
        .await
        .unwrap();

    let got = store.get("a1").await.unwrap();
    assert!(!got.enabled);
}

#[tokio::test]
async fn trigger_replacement_recomputes_next_fire_at() {
    let (_dir, store) = temp_store().await;
    store.add(sample("a1", "agent-1")).await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));

    let out = AssignmentUpdate
        .invoke(
            json!({"assignment_id": "a1", "trigger": {"type": "webhook", "token": "tok"}}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("next_fire_at=n/a"), "got: {s}"),
        other => panic!("expected Text, got {:?}", other),
    }

    let got = store.get("a1").await.unwrap();
    assert!(matches!(got.trigger, AssignmentTrigger::Webhook { token: Some(ref t), .. } if t == "tok"));
    assert!(got.next_fire_at.is_none());
}

#[tokio::test]
async fn invalid_cron_replacement_leaves_assignment_unchanged() {
    let (_dir, store) = temp_store().await;
    store.add(sample("a1", "agent-1")).await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));

    let out = AssignmentUpdate
        .invoke(
            json!({"assignment_id": "a1", "trigger": {"type": "schedule", "cron_expr": "garbage"}}),
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
    let got = store.get("a1").await.unwrap();
    assert_eq!(got.name, "Original", "unchanged on validation failure");
}

#[tokio::test]
async fn connector_event_replacement_is_accepted_and_replaces_trigger() {
    let (_dir, store) = temp_store().await;
    store.add(sample("a1", "agent-1")).await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));

    let out = AssignmentUpdate
        .invoke(
            json!({
                "assignment_id": "a1",
                "trigger": {
                    "type": "connector_event",
                    "server_name": "gmail",
                    "poll": {"tool_name": "list_messages"},
                    "poll_interval_secs": 30
                }
            }),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("updated"), "got: {s}");
        }
        other => panic!("expected Text, got {:?}", other),
    }
    let got = store.get("a1").await.unwrap();
    match &got.trigger {
        AssignmentTrigger::ConnectorEvent {
            server_name,
            poll,
            poll_interval_secs,
        } => {
            assert_eq!(server_name, "gmail");
            assert_eq!(poll.tool_name, "list_messages");
            assert_eq!(*poll_interval_secs, 30);
        }
        other => panic!("expected trigger replaced with ConnectorEvent, got {:?}", other),
    }
    // Replacing the trigger reschedules an immediate poll to seed the baseline.
    assert!(
        got.next_fire_at.is_some(),
        "connector_event replacement schedules an ASAP poll"
    );
}

#[test]
fn cli_compatible_is_true() {
    assert!(AssignmentUpdate.cli_compatible());
}

#[tokio::test]
async fn agent_watch_update_with_only_poll_interval_changed_preserves_contract() {
    let (_dir, store) = temp_store().await;
    store.add(sample_agent_watch("w1", "agent-1")).await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));

    AssignmentUpdate
        .invoke(
            json!({
                "assignment_id": "w1",
                "trigger": {
                    "type": "agent_watch",
                    "instruction": "Check my inbox for a new email from finance",
                    "poll_interval_secs": 1800,
                    "connector_scope": "gmail"
                }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let got = store.get("w1").await.unwrap();
    match &got.trigger {
        AssignmentTrigger::AgentWatch {
            poll_interval_secs,
            contract,
            ..
        } => {
            assert_eq!(*poll_interval_secs, 1800);
            assert!(
                contract.is_some(),
                "contract must be preserved when instruction/connector_scope are unchanged"
            );
        }
        other => panic!("expected AgentWatch trigger, got {:?}", other),
    }
}

#[tokio::test]
async fn agent_watch_update_with_changed_instruction_clears_contract() {
    let (_dir, store) = temp_store().await;
    store.add(sample_agent_watch("w2", "agent-1")).await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));

    AssignmentUpdate
        .invoke(
            json!({
                "assignment_id": "w2",
                "trigger": {
                    "type": "agent_watch",
                    "instruction": "Check my inbox for a new email from legal",
                    "poll_interval_secs": 900,
                    "connector_scope": "gmail"
                }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let got = store.get("w2").await.unwrap();
    match &got.trigger {
        AssignmentTrigger::AgentWatch { contract, .. } => {
            assert!(contract.is_none(), "contract must be cleared when instruction changes");
        }
        other => panic!("expected AgentWatch trigger, got {:?}", other),
    }
}

#[tokio::test]
async fn agent_watch_update_with_changed_connector_scope_clears_contract() {
    let (_dir, store) = temp_store().await;
    store.add(sample_agent_watch("w3", "agent-1")).await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(std::sync::Arc::clone(&store));

    AssignmentUpdate
        .invoke(
            json!({
                "assignment_id": "w3",
                "trigger": {
                    "type": "agent_watch",
                    "instruction": "Check my inbox for a new email from finance",
                    "poll_interval_secs": 900,
                    "connector_scope": "outlook"
                }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let got = store.get("w3").await.unwrap();
    match &got.trigger {
        AssignmentTrigger::AgentWatch { contract, .. } => {
            assert!(contract.is_none(), "contract must be cleared when connector_scope changes");
        }
        other => panic!("expected AgentWatch trigger, got {:?}", other),
    }
}
