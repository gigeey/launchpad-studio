/// Integration tests for Delegate tool sync mode round trip.
///
/// Verifies that Delegate in sync mode blocks until the child completes, returns
/// the child's final assistant text as ToolOutput::Text, reaps the handle
/// from the registry, and does not emit an AsyncLaunched event on the parent
/// runner_events stream.
use std::path::PathBuf;
use std::sync::Arc;

use ao_engine_tools_core::background_agents::{
    BackgroundAgentId, ChildRunner, RunnerEvent, SubagentDefinition, SubagentRegistry,
    SubagentSpawner, TaskFinalReport,
};
use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_engine_tools_engine::Delegate;
use ao_engine_tools_runner::background_agents::FileSidechainPersister;
use ao_persistence::{paths::DataRoot, profiles::AgentProfileStore};
use ao_protocol::error::AoError;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

// ---- scripted child runner ----

/// Emits a sequence of intermediate text events then completes with a
/// predetermined final assistant text.
struct ScriptedChild {
    texts: Vec<String>,
    final_text: Option<String>,
}

impl ChildRunner for ScriptedChild {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> JoinHandle<Result<TaskFinalReport, AoError>> {
        let texts = self.texts.clone();
        let final_text = self.final_text.clone();
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            for text in texts {
                let _ = event_tx.send(RunnerEvent::AssistantText {
                    background_agent_id: bg_id.clone(),
                    text,
                });
            }
            let _ = event_tx.send(RunnerEvent::Completed {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::completed(final_text))
        })
    }
}

// ---- helpers ----

/// An agent store with no profiles on disk. `AgentProfileStore::get` returns
/// `Ok(None)` for any id here, so `Delegate` skips the address-book / clone-
/// parent paths and resolves `target` against the catalog subagent registry —
/// the path this suite exercises.
fn empty_agent_store() -> Arc<AgentProfileStore> {
    let root = std::env::temp_dir().join("subagent_sync_no_profiles");
    Arc::new(AgentProfileStore::new(DataRoot::new(root)))
}

/// No built-in catalog ships with the engine, so `Delegate` is wired to a
/// registry carrying this suite's own fixture rather than a catalog entry.
fn registry_with_test_fixture() -> SubagentRegistry {
    let mut reg = SubagentRegistry::new();
    reg.register(SubagentDefinition {
        id: "test-agent".to_string(),
        description: "Test fixture agent for sync-mode tests".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    });
    reg
}

fn make_delegate(texts: Vec<String>, final_text: Option<String>) -> Delegate {
    let spawner = SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
        .with_child_runner(Arc::new(ScriptedChild { texts, final_text }));
    Delegate::with_spawner_and_store(Arc::new(spawner), empty_agent_store())
}

fn make_parent_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("sync-session", "sync-agent", PathBuf::from("/tmp"))
}

// ---- tests ----

/// Delegate in sync mode returns the child's last assistant text exactly.
///
/// Uses FileSidechainPersister::resolve() so the LAUNCHPAD_STUDIO_DATA_DIR
/// env var override is exercised alongside the sync round-trip.
#[tokio::test]
async fn sync_mode_returns_child_final_assistant_text() {
    let temp = tempfile::TempDir::new().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", temp.path());
    let persister = FileSidechainPersister::resolve()
        .expect("resolver must succeed when LAUNCHPAD_STUDIO_DATA_DIR is set");

    let sentinel = "synthesis complete: all relevant files located";

    let spawner = SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
        .with_child_runner(Arc::new(ScriptedChild {
            texts: vec!["scanning sources".to_string(), "indexing results".to_string()],
            final_text: Some(sentinel.to_string()),
        }))
        .with_sidechain_persister(persister);

    let delegate = Delegate::with_spawner_and_store(Arc::new(spawner), empty_agent_store());
    let ctx = make_parent_ctx();

    let out = delegate
        .invoke(
            json!({
                "target": "test-agent",
                "directive": "find all relevant files in the codebase",
                "mode": "sync"
            }),
            &ctx,
        )
        .await
        .unwrap();

    match out {
        ToolOutput::Text(s) => assert_eq!(
            s, sentinel,
            "sync result must be exactly the child's final assistant text"
        ),
        _ => panic!("expected Text output from sync Delegate, got: {out:?}"),
    }
}

/// The child's join handle is removed from the registry after sync completion.
#[tokio::test]
async fn sync_mode_reaps_handle_after_completion() {
    let delegate = make_delegate(vec![], Some("task finished".to_string()));
    let ctx = make_parent_ctx();

    delegate.invoke(
        json!({
            "target": "test-agent",
            "directive": "probe the registry state"
        }),
        &ctx,
    )
    .await
    .unwrap();

    assert_eq!(
        ctx.background_agents.live_count().await,
        0,
        "handle must be reaped from the registry after sync completion"
    );
}

/// Sync mode does not emit an AsyncLaunched event on the parent runner_events stream.
#[tokio::test]
async fn sync_mode_does_not_emit_async_launched_event() {
    let delegate = make_delegate(vec![], Some("sync output".to_string()));
    let ctx = make_parent_ctx();

    let mut events_rx = ctx.runner_events.subscribe();

    delegate.invoke(
        json!({
            "target": "test-agent",
            "directive": "inspect the event stream",
            "mode": "sync"
        }),
        &ctx,
    )
    .await
    .unwrap();

    let mut saw_async_launched = false;
    loop {
        match events_rx.try_recv() {
            Ok(RunnerEvent::AsyncLaunched { .. }) => {
                saw_async_launched = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    assert!(
        !saw_async_launched,
        "sync mode must not emit AsyncLaunched on the parent runner_events stream"
    );
}
