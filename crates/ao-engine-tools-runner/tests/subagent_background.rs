/// Integration tests for async-delegation background polling and completion.
///
/// A gate-based `SlowChildRunner` emits phase-1 events, blocks until signalled,
/// then emits phase-2 events and completes. The gate is the synchronization
/// point that lets tests poll while the child is provably still running, making
/// the polling-while-running path deterministic. The child is launched through
/// the low-level `SubagentSpawner::spawn` primitive — the same path an async
/// `Delegate` uses — and progress is observed via `DelegateOutput`.
use std::path::PathBuf;
use std::sync::Arc;

use ao_engine_tools_core::background_agents::{
    BackgroundAgentId, ChildRunner, RunnerEvent, SubagentDefinition, SubagentRegistry,
    SubagentSpawner, TaskFinalReport,
};
use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput};
use ao_engine_tools_engine::DelegateOutput;
use ao_protocol::error::AoError;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

// ---- slow child runner ----

/// Emits phase-1 events immediately, then blocks at a `Notify` gate. Once the
/// gate is signalled it emits phase-2 events and completes with a fixed final
/// text.
struct SlowChildRunner {
    phase1_texts: Vec<String>,
    phase2_texts: Vec<String>,
    final_text: String,
    gate: Arc<tokio::sync::Notify>,
}

impl ChildRunner for SlowChildRunner {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> JoinHandle<Result<TaskFinalReport, AoError>> {
        let phase1 = self.phase1_texts.clone();
        let phase2 = self.phase2_texts.clone();
        let final_text = self.final_text.clone();
        let gate = self.gate.clone();
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            for text in phase1 {
                let _ = event_tx.send(RunnerEvent::AssistantText {
                    background_agent_id: bg_id.clone(),
                    text,
                });
            }
            gate.notified().await;
            for text in phase2 {
                let _ = event_tx.send(RunnerEvent::AssistantText {
                    background_agent_id: bg_id.clone(),
                    text,
                });
            }
            let _ = event_tx.send(RunnerEvent::Completed {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::completed(Some(final_text)))
        })
    }
}

// ---- helpers ----

/// No built-in catalog ships with the engine, so the spawner below owns its
/// own registered fixture rather than depending on a catalog entry.
fn registry_with_test_fixture() -> SubagentRegistry {
    let mut reg = SubagentRegistry::new();
    reg.register(SubagentDefinition {
        id: "test-agent".to_string(),
        description: "Test fixture agent for background-polling tests".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    });
    reg
}

/// Build a spawner wired to the gate-controlled slow child. Spawning through it
/// inserts a live handle into the caller's `background_agents` registry, exactly
/// as an async `Delegate` does.
fn make_slow_spawner(gate: Arc<tokio::sync::Notify>) -> Arc<SubagentSpawner> {
    Arc::new(
        SubagentSpawner::new(Arc::new(registry_with_test_fixture())).with_child_runner(
            Arc::new(SlowChildRunner {
                phase1_texts: vec![
                    "scanning project sources".to_string(),
                    "indexing symbol table".to_string(),
                ],
                phase2_texts: vec!["cross-reference analysis complete".to_string()],
                final_text: "synthesis complete: 3 relevant files located".to_string(),
                gate,
            }),
        ),
    )
}

fn make_parent_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("bg-session", "bg-agent", PathBuf::from("/tmp"))
}

// ---- tests ----

/// Polls `DelegateOutput` twice while the child is blocked at the gate (status
/// stays `"running"` with no duplicate events), then opens the gate, and
/// asserts the final poll returns `status="completed"` with the child's last
/// assistant text and the handle is reaped from the registry.
#[tokio::test]
async fn background_polling_drains_events_progressively_and_returns_final_result() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let spawner = make_slow_spawner(gate.clone());
    let ctx = make_parent_ctx();

    let sentinel = "synthesis complete: 3 relevant files located";

    // --- launch through the spawner primitive (the async-delegation path) ---

    let (bg_id, _rx) = spawner
        .spawn(
            &ctx,
            "test-agent",
            "analyze the project structure and summarize findings".to_string(),
        )
        .await
        .expect("spawn must succeed");
    let bg_id = bg_id.to_string();

    // Yield so the child task runs and emits its phase-1 events.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let poll_tool = DelegateOutput;

    // --- poll 1: child is blocked at gate, phase-1 events visible ---

    let poll1 = poll_tool.invoke(json!({"id": bg_id}), &ctx).await.unwrap();

    let events1 = match &poll1 {
        ToolOutput::Structured(v) => {
            assert_eq!(
                v["status"].as_str(),
                Some("running"),
                "first poll must report status=running while child is at the gate"
            );
            v["events"]
                .as_array()
                .expect("events must be an array")
                .clone()
        }
        _ => panic!("expected Structured for poll1, got: {poll1:?}"),
    };

    assert!(
        events1.len() >= 2,
        "first poll must drain the phase-1 events (got {} events, want ≥ 2)",
        events1.len()
    );

    assert_eq!(
        ctx.background_agents.live_count().await,
        1,
        "handle must remain in registry after a running poll"
    );

    // --- poll 2: gate still closed, cursor advanced — phase-1 must not repeat ---

    let poll2 = poll_tool.invoke(json!({"id": bg_id}), &ctx).await.unwrap();

    match &poll2 {
        ToolOutput::Structured(v) => {
            assert_eq!(
                v["status"].as_str(),
                Some("running"),
                "second poll must still report status=running (gate not yet opened)"
            );
            let events2 = v["events"].as_array().expect("events must be an array");

            // Phase-1 texts must not reappear — the cursor was advanced by poll 1.
            let phase1_texts: Vec<&str> = events1
                .iter()
                .filter_map(|e| e.get("text").and_then(|t| t.as_str()))
                .collect();

            for text in &phase1_texts {
                assert!(
                    !events2
                        .iter()
                        .any(|e| e.get("text").and_then(|t| t.as_str()) == Some(text)),
                    "phase-1 event '{text}' must not appear again in the second poll \
                     (progressive drain — no duplicates)"
                );
            }
        }
        _ => panic!("expected Structured for poll2, got: {poll2:?}"),
    }

    // --- open gate and let the child emit phase-2 events and complete ---

    gate.notify_one();
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // --- poll 3: child completed — final result visible, handle reaped ---

    let poll3 = poll_tool.invoke(json!({"id": bg_id}), &ctx).await.unwrap();

    match poll3 {
        ToolOutput::Structured(v) => {
            assert_eq!(
                v["status"].as_str(),
                Some("completed"),
                "third poll must report status=completed after child finishes"
            );
            assert_eq!(
                v["final_result"].as_str(),
                Some(sentinel),
                "final_result must match the child's last assistant text exactly"
            );
        }
        _ => panic!("expected Structured for poll3, got: {poll3:?}"),
    }

    assert_eq!(
        ctx.background_agents.live_count().await,
        0,
        "handle must be reaped from the registry after the completed poll"
    );
}
