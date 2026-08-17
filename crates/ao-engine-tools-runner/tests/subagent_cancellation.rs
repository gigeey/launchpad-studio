/// Integration tests for DelegateStop mid-run and cancellation cascade.
///
/// Verifies that DelegateStop cancels a specific background child without affecting
/// siblings, that the cancel call is idempotent, that DelegateOutput confirms the
/// cancelled status and reaps the handle, and that parent teardown cascades
/// to all remaining children including grandchildren.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::background_agents::{
    BackgroundAgentHandle, BackgroundAgentId, ChildRunner, RunnerEvent, SubagentDefinition,
    SubagentRegistry, SubagentSpawner, TaskFinalReport,
};
use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput};
use ao_engine_tools_engine::{DelegateOutput, DelegateStop};
use ao_protocol::error::AoError;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

// ---- child runner fixtures ----

/// Blocks on its cancel token, emits a Cancelled event, then resolves.
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
            child_ctx
                .background_agents
                .cancel_all(Duration::from_millis(200))
                .await;
            Ok(TaskFinalReport::cancelled())
        })
    }
}

/// Registers a grandchild into its own background_agents, then blocks until
/// cancelled. On cancellation it cascades the cancel down to the grandchild.
struct CascadingChild {
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

            child_ctx.cancel.cancelled().await;
            let _ = event_tx.send(RunnerEvent::Cancelled {
                background_agent_id,
            });
            child_ctx
                .background_agents
                .cancel_all(Duration::from_millis(200))
                .await;
            Ok(TaskFinalReport::cancelled())
        })
    }
}

// ---- helpers ----

fn make_parent_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd(
        "cancellation-session",
        "cancellation-agent",
        PathBuf::from("/tmp"),
    )
}

/// No built-in catalog ships with the engine, so every spawner here owns its
/// own registered fixture rather than depending on a catalog entry.
fn registry_with_test_fixture() -> SubagentRegistry {
    let mut reg = SubagentRegistry::new();
    reg.register(SubagentDefinition {
        id: "test-agent".to_string(),
        description: "Test fixture agent for cancellation tests".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    });
    reg
}

fn simple_spawner() -> SubagentSpawner {
    SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
        .with_child_runner(Arc::new(WaitForCancelChild))
}

// ---- tests ----

/// Stopping one child mid-run cancels only that child. The two sibling
/// children remain live. A second DelegateStop call on the same id (before the
/// handle is reaped) returns `already_cancelled`. DelegateOutput then confirms
/// the cancellation and reaps the handle.
#[tokio::test]
async fn stopping_child_mid_run_leaves_siblings_unaffected() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let spawner = simple_spawner();
        let ctx = make_parent_ctx();

        let (id1, _rx1) = spawner
            .spawn(&ctx, "test-agent", "child-1-work".to_string())
            .await
            .unwrap();
        let (id2, _rx2) = spawner
            .spawn(&ctx, "test-agent", "child-2-work".to_string())
            .await
            .unwrap();
        let (id3, _rx3) = spawner
            .spawn(&ctx, "test-agent", "child-3-work".to_string())
            .await
            .unwrap();

        assert_eq!(
            ctx.background_agents.live_count().await,
            3,
            "all three children must be live before any stop"
        );

        let stop = DelegateStop;

        // First stop: fires the cancel token and returns "cancelled".
        let out1 = stop
            .invoke(json!({"id": id2.to_string()}), &ctx)
            .await
            .unwrap();
        match out1 {
            ToolOutput::Structured(ref v) => assert_eq!(
                v["status"].as_str(),
                Some("cancelled"),
                "first DelegateStop must return status=cancelled"
            ),
            _ => panic!("expected Structured from DelegateStop, got: {out1:?}"),
        }

        // Second stop on the same id: cancel token is already fired, handle
        // still in the registry (DelegateOutput has not yet reaped it).
        let out2 = stop
            .invoke(json!({"id": id2.to_string()}), &ctx)
            .await
            .unwrap();
        match out2 {
            ToolOutput::Structured(ref v) => assert_eq!(
                v["status"].as_str(),
                Some("already_cancelled"),
                "second DelegateStop on the same id must return already_cancelled (idempotent)"
            ),
            _ => panic!("expected Structured from second DelegateStop, got: {out2:?}"),
        }

        // Give child 2's tokio task time to observe the cancel and finish.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // DelegateOutput confirms the cancellation and reaps the handle.
        let poll = DelegateOutput;
        let poll_out = poll
            .invoke(json!({"id": id2.to_string()}), &ctx)
            .await
            .unwrap();
        match poll_out {
            ToolOutput::Structured(ref v) => assert_eq!(
                v["status"].as_str(),
                Some("cancelled"),
                "DelegateOutput must report status=cancelled after DelegateStop"
            ),
            _ => panic!("expected Structured from DelegateOutput, got: {poll_out:?}"),
        }

        // Children 1 and 3 must still be live.
        assert_eq!(
            ctx.background_agents.live_count().await,
            2,
            "only child 2 was stopped; children 1 and 3 must still be live"
        );
        assert!(
            ctx.background_agents.get(&id1).await.is_some(),
            "child 1 must remain in the registry after child 2 was stopped"
        );
        assert!(
            ctx.background_agents.get(&id3).await.is_some(),
            "child 3 must remain in the registry after child 2 was stopped"
        );

        // Tear down to avoid leaking tasks.
        ctx.background_agents
            .cancel_all(Duration::from_millis(500))
            .await;

        assert_eq!(
            ctx.background_agents.live_count().await,
            0,
            "all children must be reaped after teardown"
        );
    })
    .await
    .expect("test must complete within 10 seconds — no tokio task leaks");
}

/// After stopping one child, parent teardown cancels every remaining child
/// and cascades down to any grandchildren those children have registered.
#[tokio::test]
async fn parent_teardown_cancels_remaining_and_cascades_to_grandchild() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let grandchild_cancel = CancellationToken::new();
        let grandchild_id = BackgroundAgentId::new();

        let cascade_spawner = SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
            .with_child_runner(Arc::new(CascadingChild {
                grandchild_cancel: grandchild_cancel.clone(),
                grandchild_id: grandchild_id.clone(),
            }));

        let spawner = simple_spawner();
        let ctx = make_parent_ctx();

        let (id1, _rx1) = spawner
            .spawn(&ctx, "test-agent", "child-1-work".to_string())
            .await
            .unwrap();
        let (id2, _rx2) = spawner
            .spawn(&ctx, "test-agent", "child-2-work".to_string())
            .await
            .unwrap();
        // Child 3 uses CascadingChild and will register a grandchild.
        let (_id3, _rx3) = cascade_spawner
            .spawn(&ctx, "test-agent", "child-3-work".to_string())
            .await
            .unwrap();

        // Give child 3 time to register its grandchild in its own registry.
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            !grandchild_cancel.is_cancelled(),
            "grandchild must not be cancelled before teardown"
        );

        // Stop child 2 mid-run.
        let stop = DelegateStop;
        let stop_out = stop
            .invoke(json!({"id": id2.to_string()}), &ctx)
            .await
            .unwrap();
        match stop_out {
            ToolOutput::Structured(ref v) => assert_eq!(
                v["status"].as_str(),
                Some("cancelled"),
                "DelegateStop must return status=cancelled"
            ),
            _ => panic!("expected Structured from DelegateStop, got: {stop_out:?}"),
        }

        // Give child 2 time to finish.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Reap child 2 via DelegateOutput; confirms cancellation.
        let poll = DelegateOutput;
        let poll_out = poll
            .invoke(json!({"id": id2.to_string()}), &ctx)
            .await
            .unwrap();
        match poll_out {
            ToolOutput::Structured(ref v) => assert_eq!(
                v["status"].as_str(),
                Some("cancelled"),
                "DelegateOutput must confirm status=cancelled for the stopped child"
            ),
            _ => panic!("expected Structured from DelegateOutput, got: {poll_out:?}"),
        }

        assert_eq!(
            ctx.background_agents.live_count().await,
            2,
            "child 2 is reaped; children 1 and 3 remain before parent teardown"
        );

        // Trigger parent teardown — cascades to child 3 and its grandchild.
        ctx.background_agents
            .cancel_all(Duration::from_millis(500))
            .await;

        assert_eq!(
            ctx.background_agents.live_count().await,
            0,
            "all remaining children must be reaped after parent teardown"
        );
        assert!(
            ctx.background_agents.get(&id1).await.is_none(),
            "child 1 must be reaped after parent teardown"
        );
        assert!(
            grandchild_cancel.is_cancelled(),
            "parent teardown must cascade from child 3 down to its grandchild"
        );
    })
    .await
    .expect("test must complete within 10 seconds — no tokio task leaks");
}
