/// Integration tests for concurrency-cap refusal surfaces as a recoverable
/// SpawnerError variant, and a spawn retried after a live child completes and
/// is reaped succeeds.
///
/// One test scenario covering the full lifecycle:
///   1. A context capped at 2 fills the cap with two background spawns.
///   2. A third spawn is refused as ConcurrencyCapExceeded (recoverable: true).
///   3. One live child is cancelled and reaped via DelegateOutput poll.
///   4. The retried spawn now succeeds.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::background_agents::{
    BackgroundAgentId, BackgroundAgentRegistry, ChildRunner, RunnerEvent, SpawnerError,
    SubagentDefinition, SubagentRegistry, SubagentSpawner, TaskFinalReport,
};
use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput};
use ao_engine_tools_engine::DelegateOutput;
use ao_protocol::error::AoError;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

// ---- child fixture ----

/// Minimal child that blocks on its cancel token then resolves as cancelled.
struct IdleChild;

impl ChildRunner for IdleChild {
    fn launch(
        &self,
        child_ctx: RunnerContext,
        _prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> JoinHandle<Result<TaskFinalReport, AoError>> {
        tokio::spawn(async move {
            child_ctx.cancel.cancelled().await;
            let _ = event_tx.send(RunnerEvent::Cancelled {
                background_agent_id,
            });
            Ok(TaskFinalReport::cancelled())
        })
    }
}

// ---- helpers ----

/// No built-in catalog ships with the engine, so this test owns its own
/// registered fixture rather than depending on a catalog entry.
fn registry_with_test_fixture() -> SubagentRegistry {
    let mut reg = SubagentRegistry::new();
    reg.register(SubagentDefinition {
        id: "test-agent".to_string(),
        description: "Test fixture agent for concurrency-cap tests".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    });
    reg
}

fn make_spawner() -> SubagentSpawner {
    SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
        .with_child_runner(Arc::new(IdleChild))
}

fn make_ctx_with_cap(cap: usize) -> RunnerContext {
    RunnerContext::new_with_cwd("cap-session", "cap-agent", PathBuf::from("/tmp"))
        .with_background_agents(Arc::new(BackgroundAgentRegistry::new(cap)))
}

// ---- tests ----

/// Fills the concurrency cap (2), asserts the next spawn is refused as
/// ConcurrencyCapExceeded with recoverable=true, cancels and reaps one child
/// via DelegateOutput, then asserts the retried spawn succeeds.
#[tokio::test]
async fn concurrency_cap_refused_and_retry_after_reap_succeeds() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let ctx = make_ctx_with_cap(2);
        let spawner = make_spawner();

        // --- Fill the cap ---

        let (_child1_id, _rx1) = spawner
            .spawn(&ctx, "test-agent", "first background task".to_string())
            .await
            .expect("first spawn must succeed when the cap is not yet reached");

        assert_eq!(ctx.background_agents.live_count().await, 1);

        let (child2_id, _rx2) = spawner
            .spawn(&ctx, "test-agent", "second background task".to_string())
            .await
            .expect("second spawn must succeed and fill the cap");

        assert_eq!(ctx.background_agents.live_count().await, 2);

        // --- Third spawn must be refused ---

        let err = spawner
            .spawn(
                &ctx,
                "test-agent",
                "third task (expected to be refused)".to_string(),
            )
            .await
            .unwrap_err();

        // Variant match — must be ConcurrencyCapExceeded, not any other guard.
        assert!(
            matches!(&err, SpawnerError::ConcurrencyCapExceeded),
            "expected ConcurrencyCapExceeded, got: {err:?}"
        );

        // ConcurrencyCapExceeded is the only recoverable error variant.
        match err.to_tool_output() {
            ToolOutput::Error { recoverable, .. } => {
                assert!(
                    recoverable,
                    "ConcurrencyCapExceeded must be recoverable so the model can retry"
                );
            }
            other => panic!(
                "expected ToolOutput::Error from to_tool_output(), got: {other:?}"
            ),
        }

        // Both original children are still live — the refusal left the registry intact.
        assert_eq!(
            ctx.background_agents.live_count().await,
            2,
            "cap refusal must not affect the two live children"
        );

        // --- Cancel and reap child2 ---

        // Fire child2's cancel token so it exits, then wait for the task to settle.
        let snapshot = ctx
            .background_agents
            .get(&child2_id)
            .await
            .expect("child2 must still be in the registry before reap");
        snapshot.cancel.cancel();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let poll_tool = DelegateOutput;
        let reap_out = poll_tool
            .invoke(json!({"id": child2_id.to_string()}), &ctx)
            .await
            .unwrap();

        match &reap_out {
            ToolOutput::Structured(v) => assert_eq!(
                v["status"].as_str(),
                Some("cancelled"),
                "DelegateOutput on a cancelled child must report status=cancelled"
            ),
            _ => panic!("expected Structured output from DelegateOutput, got: {reap_out:?}"),
        }

        assert_eq!(
            ctx.background_agents.live_count().await,
            1,
            "one slot must be freed after child2 is reaped"
        );

        // --- Retry the third spawn — must now succeed ---

        let (child3_id, _rx3) = spawner
            .spawn(
                &ctx,
                "test-agent",
                "third task (retry after reap)".to_string(),
            )
            .await
            .expect("third spawn must succeed after a slot was freed by reaping child2");

        assert_eq!(ctx.background_agents.live_count().await, 2);
        assert!(
            ctx.background_agents.get(&child3_id).await.is_some(),
            "child3 handle must be present in the registry after the successful retry"
        );

        // --- Teardown ---
        ctx.background_agents
            .cancel_all(Duration::from_millis(500))
            .await;
        assert_eq!(ctx.background_agents.live_count().await, 0);
    })
    .await
    .expect("test must complete within 10 seconds");
}
