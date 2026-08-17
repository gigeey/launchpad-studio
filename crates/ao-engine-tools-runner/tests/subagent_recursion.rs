/// Integration tests for depth-cap and name-recursion refusals surface as
/// distinct, named SpawnerError variants — not stringly-typed messages.
///
/// Two test scenarios:
///   1. A context at depth 3 (one below DEFAULT_DEPTH_CAP) attempts to spawn a
///      child; the refusal is DepthExceeded with the exact depth and cap values.
///   2. A context whose spawn chain already contains "AlphaAgent" attempts to
///      spawn "AlphaAgent" again; the refusal is RecursionDetected with the
///      exact subagent_type and chain values.
///
/// Each test also asserts that the refusal surfaces as ToolOutput::Error with
/// recoverable=false, and that a valid sibling spawn is unaffected.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::background_agents::{
    BackgroundAgentId, ChildRunner, RunnerEvent, SpawnerError, SubagentDefinition, SubagentRegistry,
    SubagentSpawner, TaskFinalReport,
};
use ao_engine_tools_core::{RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

// ---- shared child fixture ----

/// Minimal child that blocks on its cancel token then resolves cleanly.
/// Used only where the guards pass and an actual launch is needed.
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

// ---- spawner factories ----

/// No built-in catalog ships with the engine, so every spawner here owns its
/// own registered fixture rather than depending on a catalog entry.
fn test_agent_definition() -> SubagentDefinition {
    SubagentDefinition {
        id: "test-agent".to_string(),
        description: "Test fixture agent for recursion-guard tests".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    }
}

fn spawner_with_test_fixture() -> SubagentSpawner {
    let mut registry = SubagentRegistry::new();
    registry.register(test_agent_definition());
    SubagentSpawner::new(Arc::new(registry)).with_child_runner(Arc::new(IdleChild))
}

/// Registry that includes "AlphaAgent" and "BetaAgent" in addition to the
/// test fixture, so the depth and recursion guards are reached (not
/// short-circuited by the unknown-type guard).
fn spawner_with_custom_types() -> SubagentSpawner {
    let mut registry = SubagentRegistry::new();
    registry.register(test_agent_definition());
    registry.register(SubagentDefinition {
        id: "AlphaAgent".to_string(),
        description: "Guard-test agent alpha".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    });
    registry.register(SubagentDefinition {
        id: "BetaAgent".to_string(),
        description: "Guard-test agent beta".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    });
    SubagentSpawner::new(Arc::new(registry)).with_child_runner(Arc::new(IdleChild))
}

// ---- tests ----

/// A context at depth 3 (parent = great-grandchild) tries to spawn a child.
/// The child would live at depth 4, which equals DEFAULT_DEPTH_CAP (4), so
/// the spawn is refused as DepthExceeded { depth: 4, cap: 4 }.
///
/// The refusal must:
///   - match the DepthExceeded variant (not a string comparison),
///   - carry depth == 4 and cap == 4,
///   - produce ToolOutput::Error { recoverable: false }, and
///   - leave the spawner unaffected so a shallower context can still spawn.
#[tokio::test]
async fn depth_cap_refusal_carries_exact_depth_and_cap_values() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let spawner = spawner_with_test_fixture();

        // Simulate being inside a great-grandchild (depth 3 = DEFAULT_DEPTH_CAP - 1).
        let ctx = RunnerContext::new_with_cwd("rec-depth-sess", "depth-agent", PathBuf::from("/tmp"))
            .with_depth(3);

        let err = spawner
            .spawn(&ctx, "test-agent", "locate relevant files".to_string())
            .await
            .unwrap_err();

        // Guard: variant match, not a string assertion.
        assert!(
            matches!(&err, SpawnerError::DepthExceeded { depth: 4, cap: 4 }),
            "expected DepthExceeded {{ depth: 4, cap: 4 }}, got: {err:?}"
        );

        // Field values confirmed by destructuring.
        match &err {
            SpawnerError::DepthExceeded { depth, cap } => {
                assert_eq!(*depth, 4, "depth must be parent.depth + 1");
                assert_eq!(*cap, 4, "cap must equal DEFAULT_DEPTH_CAP");
            }
            _ => unreachable!(),
        }

        // The refusal surfaces as a non-recoverable tool error.
        match err.to_tool_output() {
            ToolOutput::Error { recoverable, .. } => {
                assert!(!recoverable, "DepthExceeded must be non-recoverable");
            }
            other => panic!("expected ToolOutput::Error from to_tool_output(), got: {other:?}"),
        }

        // Sibling: a parent context at depth 0 can still spawn — the refusal
        // does not corrupt the spawner state.
        let parent_ctx =
            RunnerContext::new_with_cwd("rec-depth-sess", "parent-agent", PathBuf::from("/tmp"));
        spawner
            .check_guards(&parent_ctx, "test-agent", None)
            .await
            .expect("spawner must remain operational after a DepthExceeded refusal");
    })
    .await
    .expect("test must complete within 10 seconds");
}

/// A context whose spawn chain is ["AlphaAgent", "BetaAgent"] attempts to
/// spawn "AlphaAgent" again, creating a cycle.
/// The spawn is refused as RecursionDetected with the exact subagent_type and
/// chain values.
///
/// The refusal must:
///   - match the RecursionDetected variant (not a string comparison),
///   - carry subagent_type == "AlphaAgent" and chain == ["AlphaAgent", "BetaAgent"],
///   - produce ToolOutput::Error { recoverable: false }, and
///   - leave the spawner unaffected so a non-cyclic sibling type still passes.
#[tokio::test]
async fn name_recursion_refusal_carries_exact_type_and_chain() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let spawner = spawner_with_custom_types();

        // Simulate being inside BetaAgent, which was itself spawned by AlphaAgent.
        // Attempting to spawn AlphaAgent here would close the cycle.
        let ctx =
            RunnerContext::new_with_cwd("rec-chain-sess", "beta-agent", PathBuf::from("/tmp"))
                .with_spawn_chain(vec!["AlphaAgent".to_string(), "BetaAgent".to_string()]);

        let err = spawner
            .spawn(&ctx, "AlphaAgent", "re-enter alpha scope".to_string())
            .await
            .unwrap_err();

        // Guard: variant match, not a string assertion.
        assert!(
            matches!(
                &err,
                SpawnerError::RecursionDetected { subagent_type, chain }
                    if subagent_type == "AlphaAgent"
                        && chain == &["AlphaAgent", "BetaAgent"]
            ),
            "expected RecursionDetected with matching type and chain, got: {err:?}"
        );

        // Field values confirmed by destructuring.
        match &err {
            SpawnerError::RecursionDetected {
                subagent_type,
                chain,
            } => {
                assert_eq!(subagent_type, "AlphaAgent");
                assert_eq!(chain, &["AlphaAgent".to_string(), "BetaAgent".to_string()]);
            }
            _ => unreachable!(),
        }

        // The refusal surfaces as a non-recoverable tool error.
        match err.to_tool_output() {
            ToolOutput::Error { recoverable, .. } => {
                assert!(!recoverable, "RecursionDetected must be non-recoverable");
            }
            other => panic!("expected ToolOutput::Error from to_tool_output(), got: {other:?}"),
        }

        // Sibling: the same context can still spawn a type absent from its chain.
        // The refusal did not corrupt the spawner or the context's guard state.
        spawner
            .check_guards(&ctx, "test-agent", None)
            .await
            .expect(
                "spawner must allow spawning a type not present in the chain \
                 after a RecursionDetected refusal",
            );
    })
    .await
    .expect("test must complete within 10 seconds");
}
