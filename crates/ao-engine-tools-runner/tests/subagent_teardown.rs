/// Integration tests for parent-runner-teardown cancellation cascade.
///
/// Verifies that ending a session (or explicitly calling cancel_all) cancels
/// every live background agent, reaps the handles, and leaves live_count at
/// zero with no leaked tokio tasks.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::background_agents::child_runner::ChildRunner;
use ao_engine_tools_core::background_agents::{
    BackgroundAgentHandle, BackgroundAgentId, RunnerEvent,
    SubagentDefinition, SubagentRegistry, SubagentSpawner, TaskFinalReport,
};
use ao_engine_tools_core::RunnerContext;
use ao_protocol::error::AoError;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Child that blocks until its cancel token fires, then resolves Cancelled.
struct WaitForCancelChild;

impl ChildRunner for WaitForCancelChild {
    fn launch(
        &self,
        child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> JoinHandle<Result<TaskFinalReport, AoError>> {
        tokio::spawn(async move {
            child_ctx.cancel.cancelled().await;
            let _ = event_tx.send(RunnerEvent::Cancelled {
                background_agent_id,
            });
            // Cascade: cancel any grandchildren registered under this child.
            child_ctx
                .background_agents
                .cancel_all(Duration::from_millis(200))
                .await;
            Ok(TaskFinalReport::cancelled())
        })
    }
}

/// Child that inserts a grandchild handle into its own background_agents, then
/// blocks until cancelled. On cancellation it cascades to the grandchild.
struct CascadingChild {
    /// Shared cancel token so the test can assert the grandchild was reached.
    grandchild_cancel: CancellationToken,
    grandchild_id: BackgroundAgentId,
}

impl ChildRunner for CascadingChild {
    fn launch(
        &self,
        child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> JoinHandle<Result<TaskFinalReport, AoError>> {
        let gc_cancel = self.grandchild_cancel.clone();
        let gc_id = self.grandchild_id.clone();
        tokio::spawn(async move {
            // Register a grandchild that blocks on its own cancel token.
            let gc_cancel_inner = gc_cancel.clone();
            let gc_join = tokio::spawn(async move {
                gc_cancel_inner.cancelled().await;
                Ok::<TaskFinalReport, AoError>(TaskFinalReport::cancelled())
            });
            let (gc_tx, gc_rx) = broadcast::channel(1);
            let gc_handle = BackgroundAgentHandle {
                id: gc_id,
                subagent_name: "GrandchildAgent".to_string(),
                spawned_at: chrono::Utc::now(),
                cancel: gc_cancel,
                events: gc_rx,
                join: gc_join,
            };
            child_ctx
                .background_agents
                .insert(gc_handle)
                .await
                .expect("grandchild insert must succeed");
            drop(gc_tx);

            // Block until parent cancels this child.
            child_ctx.cancel.cancelled().await;
            let _ = event_tx.send(RunnerEvent::Cancelled {
                background_agent_id,
            });
            // Cascade to the grandchild.
            child_ctx
                .background_agents
                .cancel_all(Duration::from_millis(200))
                .await;
            Ok(TaskFinalReport::cancelled())
        })
    }
}

fn make_parent_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("teardown-session", "teardown-agent", PathBuf::from("/tmp"))
}

/// No built-in catalog ships with the engine, so every spawner here owns its
/// own registered fixture rather than depending on a catalog entry.
fn registry_with_test_fixture() -> SubagentRegistry {
    let mut reg = SubagentRegistry::new();
    reg.register(SubagentDefinition {
        id: "test-agent".to_string(),
        description: "Test fixture agent for teardown tests".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    });
    reg
}

/// Spawning 3 background children and then calling cancel_all cancels all of
/// them, reaps every handle, and leaves live_count at zero.
#[tokio::test]
async fn parent_teardown_cancels_all_three_children() {
    let spawner = SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
        .with_child_runner(Arc::new(WaitForCancelChild));

    let parent_ctx = make_parent_ctx();

    // Spawn 3 children in background; collect their ids.
    let mut ids = Vec::new();
    for _ in 0..3 {
        let (id, _rx) = spawner
            .spawn(&parent_ctx, "test-agent", "work".to_string())
            .await
            .expect("spawn must succeed");
        ids.push(id);
    }

    assert_eq!(
        parent_ctx.background_agents.live_count().await,
        3,
        "all three children must be live before teardown"
    );

    // Simulate parent runner teardown.
    tokio::time::timeout(
        Duration::from_secs(5),
        parent_ctx
            .background_agents
            .cancel_all(Duration::from_millis(500)),
    )
    .await
    .expect("teardown must complete within 5 seconds");

    assert_eq!(
        parent_ctx.background_agents.live_count().await,
        0,
        "live_count must be zero after teardown"
    );

    // The handles were already reaped by cancel_all; confirm none are still
    // present in the registry under their original ids.
    for id in &ids {
        assert!(
            parent_ctx.background_agents.get(id).await.is_none(),
            "handle for {id} must be reaped after teardown"
        );
    }
}

/// Each child's join handle resolves with Cancelled status after teardown.
#[tokio::test]
async fn children_resolve_cancelled_status_after_teardown() {
    let spawner = SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
        .with_child_runner(Arc::new(WaitForCancelChild));

    let parent_ctx = make_parent_ctx();

    // Spawn 3 children and save receivers to drain the terminal event.
    let mut receivers = Vec::new();
    for _ in 0..3 {
        let (_id, rx) = spawner
            .spawn(&parent_ctx, "test-agent", "work".to_string())
            .await
            .expect("spawn must succeed");
        receivers.push(rx);
    }

    // Fire teardown.
    tokio::time::timeout(
        Duration::from_secs(5),
        parent_ctx
            .background_agents
            .cancel_all(Duration::from_millis(500)),
    )
    .await
    .expect("teardown within 5 seconds");

    // Each receiver must have seen a Cancelled event.
    for mut rx in receivers {
        let mut saw_cancelled = false;
        loop {
            match rx.try_recv() {
                Ok(RunnerEvent::Cancelled { .. }) => {
                    saw_cancelled = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(saw_cancelled, "each child must emit a Cancelled event");
    }
}

/// A child that itself registers a grandchild cascades the cancellation down:
/// after parent teardown both the child and grandchild cancel tokens are fired.
#[tokio::test]
async fn parent_teardown_cascades_to_grandchildren() {
    let grandchild_cancel = CancellationToken::new();
    let grandchild_id = BackgroundAgentId::new();

    let spawner = SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
        .with_child_runner(Arc::new(CascadingChild {
            grandchild_cancel: grandchild_cancel.clone(),
            grandchild_id: grandchild_id.clone(),
        }));

    let parent_ctx = make_parent_ctx();

    let (_child_id, _rx) = spawner
        .spawn(&parent_ctx, "test-agent", "cascade work".to_string())
        .await
        .expect("spawn must succeed");

    // Give the child task a moment to insert the grandchild handle.
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        !grandchild_cancel.is_cancelled(),
        "grandchild must not be cancelled before teardown"
    );

    // Fire parent teardown.
    tokio::time::timeout(
        Duration::from_secs(5),
        parent_ctx
            .background_agents
            .cancel_all(Duration::from_millis(500)),
    )
    .await
    .expect("teardown within 5 seconds");

    // The cascade must have propagated to the grandchild.
    assert!(
        grandchild_cancel.is_cancelled(),
        "grandchild cancel token must be fired by the cascade"
    );

    assert_eq!(
        parent_ctx.background_agents.live_count().await,
        0,
        "parent registry must be empty after teardown"
    );
}

/// Double-teardown (cancel_all called twice) must not panic or deadlock.
#[tokio::test]
async fn double_teardown_is_safe() {
    let spawner = SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
        .with_child_runner(Arc::new(WaitForCancelChild));

    let parent_ctx = make_parent_ctx();

    let (_id, _rx) = spawner
        .spawn(&parent_ctx, "test-agent", "work".to_string())
        .await
        .expect("spawn must succeed");

    let registry = Arc::clone(&parent_ctx.background_agents);

    // First teardown.
    registry.cancel_all(Duration::from_millis(500)).await;
    assert_eq!(registry.live_count().await, 0);

    // Second teardown on an already-empty registry must be a no-op.
    tokio::time::timeout(
        Duration::from_secs(2),
        registry.cancel_all(Duration::from_millis(500)),
    )
    .await
    .expect("second teardown must not hang");

    assert_eq!(registry.live_count().await, 0, "still empty after double teardown");
}
