use super::AssignmentTrigger;
use super::super::tests::temp_store;
use ao_engine_tools_core::{AssignmentFireHandle, IoTool, RunnerContext, ToolOutput};
use ao_protocol::assignment::{
    Assignment, AssignmentRun, AssignmentRunStatus, AssignmentThreadPolicy, AssignmentTrigger as AssignmentTriggerModel,
    OutputMode,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn sample(id: &str, agent_id: &str, enabled: bool) -> Assignment {
    let now = Utc::now();
    Assignment {
        id: id.to_string(),
        agent_id: agent_id.to_string(),
        name: "Sample".to_string(),
        instruction: "do it".to_string(),
        working_directory: None,
        trigger: AssignmentTriggerModel::Webhook {
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
        enabled,
        expires_at: None,
        last_event_cursor: None,
        next_fire_at: None,
        last_run_at: None,
        liveness: Default::default(),
        created_ts: now,
        updated_ts: now,
    }
}

/// Test double for `AssignmentFireHandle` — records how many times it was
/// called and returns a fixed `Queued` run, standing in for the real
/// `ao_engine::assignment_runner::ManualAssignmentFirer` (which this crate
/// cannot depend on without a circular dependency; see the trait's docs).
struct FakeFireHandle {
    calls: AtomicUsize,
}

#[async_trait]
impl AssignmentFireHandle for FakeFireHandle {
    async fn fire_now(
        &self,
        assignment: &Assignment,
        _timezone: Option<&str>,
    ) -> Result<AssignmentRun, AoError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AssignmentRun {
            id: "run-1".to_string(),
            assignment_id: assignment.id.clone(),
            agent_id: assignment.agent_id.clone(),
            trigger_kind: ao_protocol::assignment::AssignmentTriggerKind::Manual,
            trigger_payload: None,
            status: AssignmentRunStatus::Queued,
            output_summary: None,
            thread_id: None,
            queued_at: Utc::now(),
            started_ts: None,
            finished_ts: None,
            error: None,
        })
    }
}

#[tokio::test]
async fn subagent_gate_blocks_trigger() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp")).with_depth(1);
    let out = AssignmentTrigger
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
async fn missing_store_returns_non_recoverable_error() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"));
    let out = AssignmentTrigger
        .invoke(json!({"assignment_id": "a1"}), &ctx)
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
async fn missing_fire_handle_returns_non_recoverable_error() {
    let (_dir, store) = temp_store().await;
    store.add(sample("a1", "agent-1", true)).await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(store);
    let out = AssignmentTrigger
        .invoke(json!({"assignment_id": "a1"}), &ctx)
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
async fn missing_assignment_returns_recoverable_error() {
    let (_dir, store) = temp_store().await;
    let fire: Arc<dyn AssignmentFireHandle + Send + Sync> =
        Arc::new(FakeFireHandle { calls: AtomicUsize::new(0) });
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(store)
        .with_assignment_fire(fire);
    let out = AssignmentTrigger
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
async fn disabled_assignment_refuses_to_fire() {
    let (_dir, store) = temp_store().await;
    store.add(sample("a1", "agent-1", false)).await.unwrap();
    let fake = Arc::new(FakeFireHandle { calls: AtomicUsize::new(0) });
    let fire: Arc<dyn AssignmentFireHandle + Send + Sync> = fake.clone();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(store)
        .with_assignment_fire(fire);
    let out = AssignmentTrigger
        .invoke(json!({"assignment_id": "a1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("disabled"), "got: {message}");
        }
        other => panic!("expected Error, got {:?}", other),
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0, "must not call fire_now for a disabled assignment");
}

#[tokio::test]
async fn happy_path_fires_through_the_handle() {
    let (_dir, store) = temp_store().await;
    store.add(sample("a1", "agent-1", true)).await.unwrap();
    let fake = Arc::new(FakeFireHandle { calls: AtomicUsize::new(0) });
    let fire: Arc<dyn AssignmentFireHandle + Send + Sync> = fake.clone();
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_assignment_store(store)
        .with_assignment_fire(fire);
    let out = AssignmentTrigger
        .invoke(json!({"assignment_id": "a1"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("run-1"), "got: {s}");
            assert!(s.contains("queued"), "got: {s}");
        }
        other => panic!("expected Text, got {:?}", other),
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn cli_compatible_is_true() {
    assert!(AssignmentTrigger.cli_compatible());
}
