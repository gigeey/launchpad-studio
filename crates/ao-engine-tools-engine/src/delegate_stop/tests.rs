use super::*;

use std::path::PathBuf;
use std::sync::Arc;

use ao_engine_tools_core::background_agents::{
    BackgroundAgentId, ChildRunner, RunnerEvent, SubagentDefinition, SubagentRegistry,
    SubagentSpawner, TaskFinalReport,
};
use ao_engine_tools_core::RunnerContext;
use ao_protocol::error::AoError;
use serde_json::json;
use tokio::sync::broadcast;

/// Blocks until its cancel token fires, then resolves as cancelled.
struct BlockingChildRunner;

impl ChildRunner for BlockingChildRunner {
    fn launch(
        &self,
        child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            child_ctx.cancel.cancelled().await;
            let _ = event_tx.send(RunnerEvent::Cancelled {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::cancelled())
        })
    }
}

fn make_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
}

/// A registry seeded with a single "Explore" test fixture. No built-in
/// catalog ships with the engine, so tests that spawn via the registry-based
/// catalog path need at least one registered type to resolve against.
fn registry_with_explore_fixture() -> SubagentRegistry {
    let mut reg = SubagentRegistry::new();
    reg.register(SubagentDefinition {
        id: "Explore".to_string(),
        description: "Test catalog subagent".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    });
    reg
}

fn make_spawner() -> Arc<SubagentSpawner> {
    Arc::new(
        SubagentSpawner::new(Arc::new(registry_with_explore_fixture()))
            .with_child_runner(Arc::new(BlockingChildRunner)),
    )
}

/// Spawn a background child directly through the spawner primitive (the same
/// path an async `Delegate` uses) and return its delegation id.
async fn spawn_background(spawner: Arc<SubagentSpawner>, ctx: &RunnerContext) -> String {
    let (bg_id, _rx) = spawner
        .spawn(ctx, "Explore", "go".to_string())
        .await
        .expect("spawn must succeed");
    bg_id.to_string()
}

#[tokio::test]
async fn cancel_running_child_returns_cancelled() {
    let spawner = make_spawner();
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    let tool = DelegateStop;
    let out = tool.invoke(json!({"id": bg_id}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"].as_str(), Some("cancelled"));
            assert_eq!(v["id"].as_str(), Some(bg_id.as_str()));
        }
        _ => panic!("expected Structured output, got: {out:?}"),
    }

    // Handle must remain in registry — DelegateStop does not reap.
    assert_eq!(
        ctx.background_agents.live_count().await,
        1,
        "handle must remain in registry after DelegateStop"
    );
}

#[tokio::test]
async fn double_cancel_is_idempotent() {
    let spawner = make_spawner();
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    let tool = DelegateStop;

    let out1 = tool.invoke(json!({"id": bg_id}), &ctx).await.unwrap();
    match &out1 {
        ToolOutput::Structured(v) => assert_eq!(v["status"].as_str(), Some("cancelled")),
        _ => panic!("first cancel expected Structured, got: {out1:?}"),
    }

    let out2 = tool.invoke(json!({"id": bg_id}), &ctx).await.unwrap();
    match &out2 {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"].as_str(), Some("already_cancelled"));
        }
        _ => panic!("second cancel expected Structured, got: {out2:?}"),
    }
}

#[tokio::test]
async fn unknown_id_returns_recoverable_error() {
    let ctx = make_ctx();
    let unknown_id = BackgroundAgentId::new().to_string();

    let tool = DelegateStop;
    let out = tool.invoke(json!({"id": unknown_id}), &ctx).await.unwrap();

    assert!(
        matches!(out, ToolOutput::Error { .. }),
        "unknown id must return an error, got: {out:?}"
    );
}

#[tokio::test]
async fn sibling_children_unaffected_by_cancel() {
    let ctx = make_ctx();
    let bg_id1 = spawn_background(make_spawner(), &ctx).await;
    let bg_id2 = spawn_background(make_spawner(), &ctx).await;

    assert_eq!(ctx.background_agents.live_count().await, 2);

    // Cancel only the first child.
    let tool = DelegateStop;
    let out = tool.invoke(json!({"id": bg_id1}), &ctx).await.unwrap();
    match out {
        ToolOutput::Structured(v) => assert_eq!(v["status"].as_str(), Some("cancelled")),
        _ => panic!("expected cancelled status for first child"),
    }

    // Both handles must still be in the registry.
    assert_eq!(
        ctx.background_agents.live_count().await,
        2,
        "both handles must remain in registry after single DelegateStop"
    );

    // The second child's cancel token must not have been fired.
    let snap_id2: BackgroundAgentId = bg_id2.parse().unwrap();
    let snapshot2 = ctx
        .background_agents
        .get(&snap_id2)
        .await
        .expect("second child must still have a live handle");
    assert!(
        !snapshot2.cancel.is_cancelled(),
        "second child's cancel token must not be fired"
    );
}

#[test]
fn tool_name_is_delegate_stop() {
    assert_eq!(DelegateStop.name(), "DelegateStop");
}

#[test]
fn is_not_concurrency_safe() {
    assert!(!DelegateStop.is_concurrency_safe());
}
