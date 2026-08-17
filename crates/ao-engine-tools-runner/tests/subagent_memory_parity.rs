/// Integration tests for memory parity across all five categories at the
/// spawn boundary.
///
/// Verifies that a child runner's resolved system prompt contains every memory
/// category the parent sees — user, feedback, project, reference, and global —
/// so silent drops at the spawn boundary cannot land unnoticed.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ao_engine_tools_core::background_agents::{
    BackgroundAgentId, ChildRunner, RunnerEvent, SubagentDefinition, SubagentRegistry,
    SubagentSpawner, TaskFinalReport,
};
use ao_engine_tools_core::{IoTool, RunnerContext, StaticMemoryLoader};
use ao_engine_tools_engine::Delegate;
use ao_engine_tools_runner::background_agents::FileSidechainPersister;
use ao_persistence::{paths::DataRoot, profiles::AgentProfileStore};
use ao_protocol::error::AoError;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

// ---- sentinel values — one per memory category ----

const SENTINEL_USER: &str = "memory-category-user-sentinel-9f3a";
const SENTINEL_FEEDBACK: &str = "memory-category-feedback-sentinel-7b2c";
const SENTINEL_PROJECT: &str = "memory-category-project-sentinel-4e1d";
const SENTINEL_REFERENCE: &str = "memory-category-reference-sentinel-8a6f";
const SENTINEL_GLOBAL: &str = "memory-category-global-sentinel-2c5e";

fn five_category_blob() -> String {
    format!(
        "user: {SENTINEL_USER}\nfeedback: {SENTINEL_FEEDBACK}\nproject: {SENTINEL_PROJECT}\nreference: {SENTINEL_REFERENCE}\nglobal: {SENTINEL_GLOBAL}"
    )
}

/// An agent store with no profiles on disk. `AgentProfileStore::get` returns
/// `Ok(None)` for any id, so `Delegate` resolves `target` against the catalog
/// subagent registry — the spawn path these parity tests exercise.
fn empty_agent_store() -> Arc<AgentProfileStore> {
    let root = std::env::temp_dir().join("subagent_memory_parity_no_profiles");
    Arc::new(AgentProfileStore::new(DataRoot::new(root)))
}

/// Distinctive phrase in the test fixture's `system_prompt_fragment`, used to
/// assert the fragment is appended (not substituted) in the child prompt.
const TEST_AGENT_FRAGMENT_MARKER: &str = "test-agent subagent instructions";

/// No built-in catalog ships with the engine, so `Delegate` is wired to a
/// registry carrying this test's own fixture rather than a catalog entry.
fn registry_with_test_fixture() -> SubagentRegistry {
    let mut reg = SubagentRegistry::new();
    reg.register(SubagentDefinition {
        id: "test-agent".to_string(),
        description: "Test fixture agent for memory parity tests".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: format!("You are the {TEST_AGENT_FRAGMENT_MARKER}."),
        model_override: None,
    });
    reg
}

// ---- capturing child runner ----

/// Captures the child's resolved system_prompt into a shared slot so the test
/// can assert memory parity after spawn completes.
struct CapturingChild {
    captured_prompt: Arc<Mutex<Option<String>>>,
}

impl ChildRunner for CapturingChild {
    fn launch(
        &self,
        child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> JoinHandle<Result<TaskFinalReport, AoError>> {
        let slot = self.captured_prompt.clone();
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            *slot.lock().unwrap() = child_ctx.system_prompt.clone();
            let _ = event_tx.send(RunnerEvent::Completed {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::completed(None))
        })
    }
}

// ---- tests ----

/// Every memory category visible to the parent appears verbatim in the child's
/// resolved system prompt after spawn, asserting no silent category drop at
/// the spawn boundary.
#[tokio::test]
async fn child_system_prompt_contains_all_five_memory_categories() {
    let temp = tempfile::TempDir::new().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", temp.path());
    let persister = FileSidechainPersister::resolve()
        .expect("resolver must succeed when LAUNCHPAD_STUDIO_DATA_DIR is set");

    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let spawner = SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
        .with_child_runner(Arc::new(CapturingChild {
            captured_prompt: captured.clone(),
        }))
        .with_sidechain_persister(persister);

    let delegate = Delegate::with_spawner_and_store(Arc::new(spawner), empty_agent_store());
    let ctx =
        RunnerContext::new_with_cwd("mem-parity-session", "mem-parity-agent", PathBuf::from("/tmp"))
            .with_system_prompt("parent-system-prompt")
            .with_memory_loader(StaticMemoryLoader::new(five_category_blob()));

    delegate
        .invoke(
            json!({
                "target": "test-agent",
                "directive": "inspect memory propagation across the spawn boundary",
                "mode": "sync"
            }),
            &ctx,
        )
        .await
        .expect("Delegate::invoke must succeed");

    let prompt = captured
        .lock()
        .unwrap()
        .clone()
        .expect("child system_prompt must have been captured");

    assert!(
        prompt.contains(SENTINEL_USER),
        "child system_prompt must contain user category sentinel; got:\n{prompt}"
    );
    assert!(
        prompt.contains(SENTINEL_FEEDBACK),
        "child system_prompt must contain feedback category sentinel; got:\n{prompt}"
    );
    assert!(
        prompt.contains(SENTINEL_PROJECT),
        "child system_prompt must contain project category sentinel; got:\n{prompt}"
    );
    assert!(
        prompt.contains(SENTINEL_REFERENCE),
        "child system_prompt must contain reference category sentinel; got:\n{prompt}"
    );
    assert!(
        prompt.contains(SENTINEL_GLOBAL),
        "child system_prompt must contain global category sentinel; got:\n{prompt}"
    );
}

/// The SubagentDefinition's system_prompt_fragment is appended after the
/// parent system prompt and memory blob — not substituted for either.
///
/// Asserts ordering: parent_system_prompt < memory_blob < fragment.
#[tokio::test]
async fn subagent_fragment_is_appended_not_substituted() {
    let temp = tempfile::TempDir::new().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", temp.path());
    let persister = FileSidechainPersister::resolve()
        .expect("resolver must succeed when LAUNCHPAD_STUDIO_DATA_DIR is set");

    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let spawner = SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
        .with_child_runner(Arc::new(CapturingChild {
            captured_prompt: captured.clone(),
        }))
        .with_sidechain_persister(persister);

    let delegate = Delegate::with_spawner_and_store(Arc::new(spawner), empty_agent_store());
    let parent_anchor = "parent-anchor-text-for-position-check";
    let ctx = RunnerContext::new_with_cwd(
        "frag-order-session",
        "frag-order-agent",
        PathBuf::from("/tmp"),
    )
    .with_system_prompt(parent_anchor)
    .with_memory_loader(StaticMemoryLoader::new(five_category_blob()));

    delegate
        .invoke(
            json!({
                "target": "test-agent",
                "directive": "verify system prompt fragment ordering",
                "mode": "sync"
            }),
            &ctx,
        )
        .await
        .expect("Delegate::invoke must succeed");

    let prompt = captured
        .lock()
        .unwrap()
        .clone()
        .expect("child system_prompt must have been captured");

    assert!(
        prompt.contains(parent_anchor),
        "parent system prompt must appear in child prompt; got:\n{prompt}"
    );
    assert!(
        prompt.contains(SENTINEL_USER),
        "memory blob must appear in child prompt; got:\n{prompt}"
    );
    // Distinctive phrase from the test fixture's system_prompt_fragment.
    assert!(
        prompt.contains(TEST_AGENT_FRAGMENT_MARKER),
        "test-agent system_prompt_fragment must appear in child prompt; got:\n{prompt}"
    );

    let parent_pos = prompt.find(parent_anchor).unwrap();
    let memory_pos = prompt.find(SENTINEL_USER).unwrap();
    let fragment_pos = prompt.find(TEST_AGENT_FRAGMENT_MARKER).unwrap();

    assert!(
        parent_pos < memory_pos,
        "parent system prompt must precede memory blob in child prompt"
    );
    assert!(
        memory_pos < fragment_pos,
        "memory blob must precede the test-agent fragment in child prompt"
    );
}
