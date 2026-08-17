use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ao_engine_tools_core::{
    background_agents::{
        BackgroundAgentId, ChildRunner, RunnerEvent, SubagentSpawner, TaskFinalReport,
    },
    IoTool, RunnerContext, ToolOutput,
};
use ao_persistence::{paths::DataRoot, profiles::AgentProfileStore};
use ao_protocol::{
    agent::{AgentProfile, AgentRunnerMode, CliProviderConfig, DelegateTarget, InputMode, OutputFormat, ProviderConfig},
    error::AoError,
};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::broadcast;

use tracing_test::traced_test;

use super::Delegate;
// Serialise with config/skill tests: this test mutates the process-global
// LAUNCHPAD_STUDIO_DATA_DIR env var and must not race with them.
use crate::lock_env_var;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn make_profile(id: &str, name: &str) -> AgentProfile {
    use std::collections::HashMap;
    AgentProfile {
        id: id.to_string(),
        name: name.to_string(),
        description: "test agent".to_string(),
        emoji: None,
        provider: ProviderConfig::Cli(CliProviderConfig {
            command: "echo".to_string(),
            args: vec![],
            normalizer: None,
            output_format: OutputFormat::Text,
            input_mode: InputMode::Arg,
            model_arg: None,
            model_aliases: HashMap::new(),
            system_prompt_arg: None,
            session_arg: None,
            resume_args: vec![],
            session_id_fields: vec![],
            clear_env: false,
            no_output_timeout_ms: 30000,
            file_capabilities: None,
        }),
        model: None,
        skills: vec![],
        system_prompt: Some(format!("{} system prompt", name)),
        tools: None,
        env: HashMap::new(),
        max_instances: 1,
        timeout_seconds: 300,
        working_dir: None,
        home_dir: None,
        serialize: true,
        workflows: None,
        template: None,
        runner_mode: Default::default(),
        native_provider: None,
        thinking: None,
        max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
        enabled_plugins: HashMap::new(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
        owning_team_id: None,
        delegates_to: vec![],
        persona: None,
        special_instructions: None,
        legacy_system_prompt: None,
        max_delegation_depth: None,
        channels: vec![],
        max_turns: None,
    }
}

/// Spawner whose child runner returns a fixed text result.
fn make_spawner_with_result(text: &str) -> Arc<SubagentSpawner> {
    let result = text.to_string();
    let runner = FixedResultRunner { result };
    Arc::new(
        SubagentSpawner::new(Arc::new(
            ao_engine_tools_core::background_agents::SubagentRegistry::new(),
        ))
        .with_child_runner(Arc::new(runner)),
    )
}

/// Spawner that captures the directive sent to the child.
fn make_spawner_capturing_directive() -> (Arc<SubagentSpawner>, Arc<Mutex<Option<String>>>) {
    let captured = Arc::new(Mutex::new(None::<String>));
    let cap2 = Arc::clone(&captured);
    let runner = CapturingRunner { captured: cap2 };
    let spawner = Arc::new(
        SubagentSpawner::new(Arc::new(
            ao_engine_tools_core::background_agents::SubagentRegistry::new(),
        ))
        .with_child_runner(Arc::new(runner)),
    );
    (spawner, captured)
}

/// Spawner that captures the child RunnerContext's pending_user_messages Arc pointer as usize.
fn make_spawner_capturing_context() -> (Arc<SubagentSpawner>, Arc<Mutex<Option<usize>>>) {
    let captured = Arc::new(Mutex::new(None::<usize>));
    let cap2 = Arc::clone(&captured);
    let runner = ContextCapturingRunner { captured: cap2 };
    let spawner = Arc::new(
        SubagentSpawner::new(Arc::new(
            ao_engine_tools_core::background_agents::SubagentRegistry::new(),
        ))
        .with_child_runner(Arc::new(runner)),
    );
    (spawner, captured)
}

/// A spawner whose child runner blocks until cancelled.
fn make_spawner_blocking() -> Arc<SubagentSpawner> {
    let runner = BlockingRunner;
    Arc::new(
        SubagentSpawner::new(Arc::new(
            ao_engine_tools_core::background_agents::SubagentRegistry::new(),
        ))
        .with_child_runner(Arc::new(runner)),
    )
}

fn make_ctx(agent_id: &str) -> RunnerContext {
    RunnerContext::new_with_cwd(
        "test-session",
        agent_id,
        PathBuf::from("/tmp"),
    )
}

struct FixedResultRunner {
    result: String,
}

impl ChildRunner for FixedResultRunner {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
        let result = self.result.clone();
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            let _ = event_tx.send(RunnerEvent::Completed {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::completed(Some(result)))
        })
    }
}

struct CapturingRunner {
    captured: Arc<Mutex<Option<String>>>,
}

impl ChildRunner for CapturingRunner {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
        let cap = Arc::clone(&self.captured);
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            *cap.lock().unwrap() = Some(initial_prompt.clone());
            let _ = event_tx.send(RunnerEvent::Completed {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::completed(Some("ok".to_string())))
        })
    }
}

struct ContextCapturingRunner {
    captured: Arc<Mutex<Option<usize>>>,
}

impl ChildRunner for ContextCapturingRunner {
    fn launch(
        &self,
        child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
        let cap = Arc::clone(&self.captured);
        // Store pointer as usize — Send-safe and sufficient for identity comparison.
        let ptr = Arc::as_ptr(&child_ctx.pending_user_messages) as usize;
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            *cap.lock().unwrap() = Some(ptr);
            let _ = event_tx.send(RunnerEvent::Completed {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::completed(Some("ok".to_string())))
        })
    }
}

struct BlockingRunner;

impl ChildRunner for BlockingRunner {
    fn launch(
        &self,
        child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
        let bg_id = background_agent_id;
        let cancel = child_ctx.cancel.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            let _ = event_tx.send(RunnerEvent::Cancelled {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::cancelled())
        })
    }
}

/// Build a parent AgentProfile whose delegates_to points at `target_id`.
fn make_parent_with_delegate(
    parent_id: &str,
    parent_name: &str,
    target_id: &str,
    target_name: &str,
    share_context_allowed: bool,
) -> AgentProfile {
    let mut p = make_profile(parent_id, parent_name);
    p.delegates_to = vec![DelegateTarget {
        target_agent_id: target_id.to_string(),
        name: target_name.to_string(),
        purpose: "run tasks".to_string(),
        share_context_allowed,
    }];
    p
}

async fn setup_store_with_profiles(
    tmp: &TempDir,
    parent: &AgentProfile,
    target: &AgentProfile,
) -> Arc<AgentProfileStore> {
    let data_root = DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();
    let store = Arc::new(AgentProfileStore::new(data_root));
    store.create(parent).await.unwrap();
    store.create(target).await.unwrap();
    store
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn sync_fresh_happy_path() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_result("review done");
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let out = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "review this", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(
        matches!(&out, ToolOutput::Text(t) if t == "review done"),
        "sync fresh happy path must return child's final text, got: {:?}",
        out
    );
}

#[tokio::test]
async fn sync_fork_happy_path() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", true);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_result("fork result");
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent").with_system_prompt("parent prompt");

    let out = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "fork task", "mode": "sync", "share_context": true }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(
        matches!(&out, ToolOutput::Text(t) if t == "fork result"),
        "sync fork happy path must return child's final text, got: {:?}",
        out
    );
}

#[tokio::test]
async fn unknown_target_returns_recoverable_error() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_result("unused");
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let out = delegate
        .invoke(
            json!({ "target": "NonExistent", "directive": "do it", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    match &out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable, "unknown target must be recoverable");
            assert!(
                message.contains("NonExistent"),
                "error must name the unknown target"
            );
            assert!(
                message.contains("Reviewer"),
                "error must enumerate available targets"
            );
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn share_context_disallowed_returns_recoverable_error() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_result("unused");
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let out = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "task", "mode": "sync", "share_context": true }),
            &ctx,
        )
        .await
        .unwrap();

    match &out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable, "share_context disallowed must be recoverable");
            assert!(
                message.contains("share_context_allowed"),
                "error must mention share_context_allowed"
            );
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn cycle_in_chain_is_logged_not_rejected() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_result("cycle result");
    let delegate = Delegate::with_spawner_and_store(spawner, store);

    // Simulate that "reviewer" is already in the delegate_chain (mutual delegation)
    let ctx = make_ctx("parent-agent")
        .with_delegate_chain(vec!["reviewer".to_string()]);

    let out = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "do it anyway", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    // Should NOT be rejected — cycle detection is telemetry-only
    assert!(
        matches!(&out, ToolOutput::Text(t) if t == "cycle result"),
        "cycle in chain must run, not reject; got: {:?}",
        out
    );
}

#[tokio::test]
async fn depth_cap_at_8_exact_wording() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_result("unused");
    let delegate = Delegate::with_spawner_and_store(spawner, store);

    // delegate_chain.len() = 7 → len+1 = 8 >= DELEGATE_DEPTH_CAP(8) → refused
    let long_chain: Vec<String> = (0..7).map(|i| format!("agent-{}", i)).collect();
    let ctx = make_ctx("parent-agent").with_delegate_chain(long_chain);

    let out = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "deep task", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    match &out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable, "depth cap must be non-recoverable");
            assert_eq!(
                message,
                "Delegation chain limit reached (8 hops). Stopping here.",
                "exact wording must match"
            );
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn profile_capped_at_2_refuses_on_third_hop() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let mut target = make_profile("reviewer", "Reviewer");
    target.max_delegation_depth = Some(2);
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_result("unused");
    let delegate = Delegate::with_spawner_and_store(spawner, store);

    // delegate_chain.len() = 2 → len+1 = 3 >= effective_delegate_depth_cap(2) → refused
    let chain: Vec<String> = (0..2).map(|i| format!("agent-{}", i)).collect();
    let ctx = make_ctx("parent-agent").with_delegate_chain(chain);

    let out = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "deep task", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    match &out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable, "depth cap must be non-recoverable");
            assert_eq!(
                message,
                "Delegation chain limit reached (2 hops). Stopping here.",
                "error wording must include the resolved cap"
            );
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn orphan_target_returns_recoverable_error() {
    let tmp = TempDir::new().unwrap();
    // Parent has a delegate entry pointing at "ghost-agent" which doesn't exist in the store.
    let mut parent = make_profile("parent-agent", "Parent");
    parent.delegates_to = vec![DelegateTarget {
        target_agent_id: "ghost-agent".to_string(),
        name: "Ghost".to_string(),
        purpose: "haunt".to_string(),
        share_context_allowed: false,
    }];

    let data_root = DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();
    let store = Arc::new(AgentProfileStore::new(data_root));
    store.create(&parent).await.unwrap();
    // Note: ghost-agent profile is NOT created.

    let spawner = make_spawner_with_result("unused");
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let out = delegate
        .invoke(
            json!({ "target": "Ghost", "directive": "boo", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    match &out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable, "orphan target must be recoverable");
            assert!(
                message.contains("ghost-agent") || message.contains("Ghost"),
                "error must identify the orphan target"
            );
            assert!(
                message.contains("stale"),
                "error must mention stale address book"
            );
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn fresh_envelope_content_in_child_prompt() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let (spawner, captured) = make_spawner_capturing_directive();
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let _ = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "the actual task", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    let prompt = captured.lock().unwrap().clone().unwrap();
    assert!(
        prompt.contains("Delegated by Parent"),
        "fresh envelope must contain parent name"
    );
    assert!(
        prompt.contains("Handle this directive."),
        "fresh envelope must contain handle phrasing"
    );
    assert!(
        prompt.contains("the actual task"),
        "fresh envelope must contain the directive"
    );
    assert!(
        !prompt.contains("fork mode"),
        "fresh envelope must NOT mention fork mode"
    );
}

#[tokio::test]
async fn fork_envelope_content_in_child_prompt() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", true);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let (spawner, captured) = make_spawner_capturing_directive();
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent").with_system_prompt("parent prompt");

    let _ = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "fork this", "mode": "sync", "share_context": true }),
            &ctx,
        )
        .await
        .unwrap();

    let prompt = captured.lock().unwrap().clone().unwrap();
    assert!(
        prompt.contains("Delegated by Parent"),
        "fork envelope must contain parent name"
    );
    assert!(
        prompt.contains("Reviewer"),
        "fork envelope must contain child name"
    );
    assert!(
        prompt.contains("in fork mode"),
        "fork envelope must mention fork mode"
    );
    assert!(
        prompt.contains("sharing Parent's context"),
        "fork envelope must reference parent context"
    );
    assert!(
        prompt.contains("fork this"),
        "fork envelope must contain the directive"
    );
}

#[tokio::test]
async fn fork_prepends_parent_transcript_to_directive() {
    // Regression: share_context: true was silently a no-op — the spawner only
    // forwarded the parent's resolved system_prompt and tool registry, never
    // the conversation transcript. The runner reads history keyed by
    // child.agent_id (= target_profile.id), so the child saw an empty head.
    // This test pre-populates a TranscriptStore with two parent turns, calls
    // Delegate with share_context: true, and asserts the directive that
    // reaches the child runner contains both turns wrapped in a
    // [Conversation history] block ahead of the envelope text.
    use ao_persistence::transcript::TranscriptStore;
    use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

    let tmp = TempDir::new().unwrap();
    let data_root = DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();

    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", true);
    let target = make_profile("reviewer", "Reviewer");
    let store = Arc::new(AgentProfileStore::new(data_root.clone()));
    store.create(&parent).await.unwrap();
    store.create(&target).await.unwrap();

    // Build a TranscriptStore against the same data_root and seed it with two
    // parent-agent entries representing a tiny prior conversation.
    let transcripts = Arc::new(TranscriptStore::new(data_root));
    let user_entry = TranscriptEntry {
        ts: chrono::Utc::now() - chrono::Duration::minutes(5),
        role: TranscriptRole::System("user".to_string()),
        content: "find a bug in the payment flow please".to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    };
    let agent_entry = TranscriptEntry {
        ts: chrono::Utc::now() - chrono::Duration::minutes(4),
        role: TranscriptRole::Agent { agent: "parent-agent".to_string() },
        content: "I found a race in refund-reversal".to_string(),
        event_type: "response".to_string(),
        metadata: None,
        hidden_from_user: false,
    };
    transcripts.append("parent-agent", &user_entry).await.unwrap();
    transcripts.append("parent-agent", &agent_entry).await.unwrap();

    let (spawner, captured) = make_spawner_capturing_directive();
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent")
        .with_system_prompt("parent prompt")
        .with_transcript_store(Arc::clone(&transcripts));

    let _ = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "do the thing", "mode": "sync", "share_context": true }),
            &ctx,
        )
        .await
        .unwrap();

    let prompt = captured.lock().unwrap().clone().expect("child runner must have been launched");

    // Header is the load-bearing signal the model picks up on — assert it
    // explicitly rather than just checking entry content, so accidental
    // truncation of the prefix still fails the test.
    assert!(
        prompt.contains("[Conversation history]"),
        "fork directive must include the [Conversation history] header. Got:\n{prompt}"
    );
    assert!(
        prompt.contains("find a bug in the payment flow please"),
        "fork directive must include parent's earlier user message. Got:\n{prompt}"
    );
    assert!(
        prompt.contains("I found a race in refund-reversal"),
        "fork directive must include parent's earlier agent reply. Got:\n{prompt}"
    );
    // History block must precede the envelope — confirms ordering and that
    // the envelope text isn't accidentally swallowed by the prefix.
    let history_idx = prompt.find("[Conversation history]").unwrap();
    let envelope_idx = prompt.find("Delegated by Parent").unwrap();
    assert!(
        history_idx < envelope_idx,
        "history block must precede envelope; got history@{} envelope@{}",
        history_idx,
        envelope_idx
    );
}

#[tokio::test]
async fn fresh_mode_does_not_inject_parent_transcript() {
    // share_context: false is the clean-room path. Even if the parent has
    // a transcript and the RunnerContext carries a TranscriptStore, the
    // child must not see any history block — otherwise share_context loses
    // its meaning as a permission boundary.
    use ao_persistence::transcript::TranscriptStore;
    use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

    let tmp = TempDir::new().unwrap();
    let data_root = DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();

    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", true);
    let target = make_profile("reviewer", "Reviewer");
    let store = Arc::new(AgentProfileStore::new(data_root.clone()));
    store.create(&parent).await.unwrap();
    store.create(&target).await.unwrap();

    let transcripts = Arc::new(TranscriptStore::new(data_root));
    let entry = TranscriptEntry {
        ts: chrono::Utc::now() - chrono::Duration::minutes(5),
        role: TranscriptRole::System("user".to_string()),
        content: "secret parent context that must not leak".to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    };
    transcripts.append("parent-agent", &entry).await.unwrap();

    let (spawner, captured) = make_spawner_capturing_directive();
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent").with_transcript_store(Arc::clone(&transcripts));

    let _ = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "do the thing", "mode": "sync", "share_context": false }),
            &ctx,
        )
        .await
        .unwrap();

    let prompt = captured.lock().unwrap().clone().unwrap();
    assert!(
        !prompt.contains("[Conversation history]"),
        "fresh delegation must NOT include a history block. Got:\n{prompt}"
    );
    assert!(
        !prompt.contains("secret parent context that must not leak"),
        "fresh delegation must not leak parent transcript content. Got:\n{prompt}"
    );
}

#[tokio::test]
async fn fork_without_transcript_store_falls_back_silently() {
    // Test contexts and headless invocations may not configure a
    // TranscriptStore. The fork path must degrade gracefully — emit the
    // envelope alone rather than erroring or panicking. Without this guard,
    // any code path that builds a RunnerContext via `new_with_cwd` (which
    // leaves transcript_store: None) would refuse to fork.
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", true);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let (spawner, captured) = make_spawner_capturing_directive();
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    // No .with_transcript_store call — ctx.transcript_store stays None.
    let ctx = make_ctx("parent-agent");

    let out = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "fork w/o store", "mode": "sync", "share_context": true }),
            &ctx,
        )
        .await
        .unwrap();

    // Spawn must still succeed — no error from the missing store.
    assert!(
        matches!(&out, ToolOutput::Text(_)),
        "fork without transcript store must succeed (envelope-only). Got: {:?}",
        out
    );
    let prompt = captured.lock().unwrap().clone().unwrap();
    assert!(
        !prompt.contains("[Conversation history]"),
        "missing transcript store must NOT produce an empty/garbage history header"
    );
    assert!(
        prompt.contains("in fork mode"),
        "envelope must still mark this as fork mode for the child"
    );
    assert!(
        prompt.contains("fork w/o store"),
        "directive must still reach the child"
    );
}

#[tokio::test]
async fn tool_call_chip_name_and_cli_compatible() {
    let delegate = Delegate::new();
    assert_eq!(delegate.name(), "Delegate", "tool name must be 'Delegate' for chip matching");
    assert!(delegate.cli_compatible(), "must be cli_compatible for CLI catalog");
}

#[tokio::test]
async fn parent_cancellation_cascades_to_child() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_blocking();
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    // Cancel the parent shortly after invoking.
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        cancel.cancel();
    });

    let out = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "long task", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    match &out {
        ToolOutput::Error { message, .. } => {
            assert!(
                message.contains("cancelled"),
                "cancellation error must mention 'cancelled', got: {message}"
            );
        }
        other => panic!("expected Error (cancellation), got {:?}", other),
    }
}

#[tokio::test]
async fn child_inherits_parent_pending_user_messages_arc() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let (spawner, captured_ptr) = make_spawner_capturing_context();
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");
    let parent_ptr = Arc::as_ptr(&ctx.pending_user_messages) as usize;

    let _ = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "task", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    let child_ptr = captured_ptr
        .lock()
        .unwrap()
        .expect("child runner must have captured the context");

    assert_eq!(
        parent_ptr, child_ptr,
        "child must share the exact same pending_user_messages Arc as parent"
    );
}

// ─── async mode tests ─────────────────────────────────────────────────────────

/// A runner that returns `TaskFinalReport::cancelled()` immediately.
struct ImmediatelyCancelledRunner;

impl ChildRunner for ImmediatelyCancelledRunner {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            let _ = event_tx.send(RunnerEvent::Cancelled {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::cancelled())
        })
    }
}

/// A runner that completes after `delay_ms` milliseconds with a fixed result.
struct DelayedResultRunner {
    result: String,
    delay_ms: u64,
}

impl ChildRunner for DelayedResultRunner {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
        let result = self.result.clone();
        let delay = Duration::from_millis(self.delay_ms);
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = event_tx.send(RunnerEvent::Completed {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::completed(Some(result)))
        })
    }
}

fn make_spawner_with_cancelled() -> Arc<SubagentSpawner> {
    Arc::new(
        SubagentSpawner::new(Arc::new(
            ao_engine_tools_core::background_agents::SubagentRegistry::new(),
        ))
        .with_child_runner(Arc::new(ImmediatelyCancelledRunner)),
    )
}

fn make_spawner_with_delay(text: &str, delay_ms: u64) -> Arc<SubagentSpawner> {
    Arc::new(
        SubagentSpawner::new(Arc::new(
            ao_engine_tools_core::background_agents::SubagentRegistry::new(),
        ))
        .with_child_runner(Arc::new(DelayedResultRunner {
            result: text.to_string(),
            delay_ms,
        })),
    )
}

/// Spawner backed by a registry containing a single custom catalog entry
/// named `id`. No built-in catalog ships with the engine, so tests that
/// exercise the explicit-target catalog-spawn path (as opposed to
/// `spawn_named`, used by address-book and clone-parent delegation) must
/// register their own entry. `delay_ms = 0` completes effectively
/// immediately, for sync-style assertions.
fn make_spawner_with_catalog_type(id: &str, text: &str, delay_ms: u64) -> Arc<SubagentSpawner> {
    let mut reg = ao_engine_tools_core::background_agents::SubagentRegistry::new();
    reg.register(ao_engine_tools_core::background_agents::SubagentDefinition {
        id: id.to_string(),
        description: "Test catalog subagent".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    });
    Arc::new(
        SubagentSpawner::new(Arc::new(reg)).with_child_runner(Arc::new(DelayedResultRunner {
            result: text.to_string(),
            delay_ms,
        })),
    )
}

#[tokio::test]
async fn async_fresh_returns_immediately_with_delegation_id() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    // Use a slow runner to ensure we return before it completes.
    let spawner = make_spawner_with_delay("done", 200);
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let out = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "async task", "mode": "async" }),
            &ctx,
        )
        .await
        .unwrap();

    match &out {
        ToolOutput::Text(text) => {
            assert!(
                text.contains("Reviewer"),
                "response must mention the target name"
            );
            assert!(
                text.contains("delegation_id="),
                "response must contain a delegation_id"
            );
            assert!(
                text.contains("background"),
                "response must indicate background execution"
            );
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn async_completion_envelope_shape() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_result("the final answer");
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let _ = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "task", "mode": "async" }),
            &ctx,
        )
        .await
        .unwrap();

    // Give the notification task time to push to the queue.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let queue = ctx.pending_user_messages.lock().unwrap();
    assert_eq!(queue.len(), 1, "exactly one notification must be pushed");
    let notification = queue.front().unwrap();
    assert_eq!(
        notification,
        "[delegate \"Reviewer\" complete]\nthe final answer",
        "completion notification must match exact format"
    );
}

#[tokio::test]
async fn async_cancellation_envelope_shape() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_cancelled();
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let _ = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "task", "mode": "async" }),
            &ctx,
        )
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let queue = ctx.pending_user_messages.lock().unwrap();
    assert_eq!(queue.len(), 1, "exactly one cancellation notification must be pushed");
    let notification = queue.front().unwrap();
    assert_eq!(
        notification,
        "[delegate \"Reviewer\" cancelled]",
        "cancellation notification must match exact format"
    );
}

#[tokio::test]
async fn async_three_parallel_notifications_all_arrive() {
    let tmp = TempDir::new().unwrap();
    // Parent with 3 delegates: A (30ms), B (10ms), C (20ms) — B completes first.
    let mut parent = make_profile("parent-agent", "Parent");
    parent.delegates_to = vec![
        DelegateTarget {
            target_agent_id: "agent-a".to_string(),
            name: "AgentA".to_string(),
            purpose: "task A".to_string(),
            share_context_allowed: false,
        },
        DelegateTarget {
            target_agent_id: "agent-b".to_string(),
            name: "AgentB".to_string(),
            purpose: "task B".to_string(),
            share_context_allowed: false,
        },
        DelegateTarget {
            target_agent_id: "agent-c".to_string(),
            name: "AgentC".to_string(),
            purpose: "task C".to_string(),
            share_context_allowed: false,
        },
    ];

    let target_a = make_profile("agent-a", "AgentA");
    let target_b = make_profile("agent-b", "AgentB");
    let target_c = make_profile("agent-c", "AgentC");

    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();
    let store = Arc::new(ao_persistence::profiles::AgentProfileStore::new(data_root));
    store.create(&parent).await.unwrap();
    store.create(&target_a).await.unwrap();
    store.create(&target_b).await.unwrap();
    store.create(&target_c).await.unwrap();

    let ctx = make_ctx("parent-agent");

    // Three delegates with different delays to control completion order: B(10) < C(20) < A(30).
    for (target_name, delay_ms, result) in [
        ("AgentA", 30u64, "result-A"),
        ("AgentB", 10u64, "result-B"),
        ("AgentC", 20u64, "result-C"),
    ] {
        let spawner = make_spawner_with_delay(result, delay_ms);
        let delegate = Delegate::with_spawner_and_store(spawner, Arc::clone(&store));
        let out = delegate
            .invoke(
                json!({ "target": target_name, "directive": "do it", "mode": "async" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            matches!(&out, ToolOutput::Text(t) if t.contains("background")),
            "async invoke must return background text for {}", target_name
        );
    }

    // Wait for all three notification tasks (max delay = 30ms + margin).
    tokio::time::sleep(Duration::from_millis(150)).await;

    let queue = ctx.pending_user_messages.lock().unwrap();
    assert_eq!(queue.len(), 3, "all three notifications must arrive");

    // All three agent names must appear in the queue.
    let all: Vec<&String> = queue.iter().collect();
    assert!(
        all.iter().any(|n| n.contains("AgentA")),
        "AgentA notification must be in queue"
    );
    assert!(
        all.iter().any(|n| n.contains("AgentB")),
        "AgentB notification must be in queue"
    );
    assert!(
        all.iter().any(|n| n.contains("AgentC")),
        "AgentC notification must be in queue"
    );
    // Completion order: B(10ms) first, then C(20ms), then A(30ms).
    let idx_b = all.iter().position(|n| n.contains("AgentB")).unwrap();
    let idx_c = all.iter().position(|n| n.contains("AgentC")).unwrap();
    let idx_a = all.iter().position(|n| n.contains("AgentA")).unwrap();
    assert!(idx_b < idx_c, "AgentB (10ms) must complete before AgentC (20ms)");
    assert!(idx_c < idx_a, "AgentC (20ms) must complete before AgentA (30ms)");
}

#[tokio::test]
async fn async_fork_envelope_in_child_prompt() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", true);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let (spawner, captured) = make_spawner_capturing_directive();
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent").with_system_prompt("parent prompt");

    let _ = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "async fork task", "mode": "async", "share_context": true }),
            &ctx,
        )
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let prompt = captured.lock().unwrap().clone().unwrap();
    assert!(
        prompt.contains("in fork mode"),
        "async fork envelope must contain 'in fork mode'"
    );
    assert!(
        prompt.contains("sharing Parent's context"),
        "async fork envelope must reference parent context"
    );
    assert!(
        prompt.contains("async fork task"),
        "async fork envelope must contain the directive"
    );
}

#[tokio::test]
async fn async_restart_safety_shared_queue_no_interference() {
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_result("delegate result");
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    // Simulate a RunSkill-style notification already in the queue.
    ctx.pending_user_messages
        .lock()
        .unwrap()
        .push_back("[skill \"MySkill\" loaded]\nSkill body here".to_string());

    let out = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "task", "mode": "async" }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(
        matches!(&out, ToolOutput::Text(t) if t.contains("background")),
        "async invoke must return background confirmation"
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    let queue = ctx.pending_user_messages.lock().unwrap();
    assert_eq!(queue.len(), 2, "both skill and delegate notifications must be in queue");

    let first = queue.front().unwrap();
    assert!(
        first.contains("skill") || first.contains("MySkill"),
        "RunSkill notification must still be in queue, got: {first}"
    );
    let items: Vec<&String> = queue.iter().collect();
    assert!(
        items.iter().any(|n| n.contains("delegate") && n.contains("Reviewer")),
        "delegate notification must also be in queue"
    );
}

// ─── delegation_usage log line ────────────────────────────────────────────────

/// Integration test: a successful Delegate invocation emits a structured
/// `delegation_usage` log line with the expected fields (kind, target,
/// delegate_count, ratio) after the fire-and-forget counter write completes.
#[tokio::test]
#[traced_test]
async fn delegation_usage_log_emitted_on_delegate_call() {
    // Hold the shared env-var lock for the whole test: we mutate the global
    // LAUNCHPAD_STUDIO_DATA_DIR below, which would otherwise clobber concurrent
    // config/skill tests reading the same var.
    let _guard = lock_env_var();
    let tmp = TempDir::new().unwrap();

    // Redirect data root to temp dir so the counter write succeeds and the log
    // line is emitted (rather than silently falling back to the home dir).
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let parent = make_parent_with_delegate("log-test-agent", "LogTester", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_result("done");
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("log-test-agent");

    let _ = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "log emission test", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    // Give the fire-and-forget spawn time to write the counter and emit the log.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    assert!(
        logs_contain("delegation_usage"),
        "must emit a log line containing 'delegation_usage' on each delegate invocation"
    );
    assert!(
        logs_contain(r#"kind="delegate""#),
        "delegation_usage log line must identify kind as \"delegate\""
    );
    assert!(
        logs_contain("Reviewer"),
        "delegation_usage log line must include the target name"
    );
    assert!(
        logs_contain("delegate_count=1"),
        "delegation_usage log line must show post-increment delegate_count=1"
    );
}

// ─── unified generic-subagent path (Agent-like delegation) ───────────────────

struct ProfileCapturingRunner {
    captured: Arc<Mutex<Option<Option<ao_protocol::agent::AgentProfile>>>>,
}

impl ChildRunner for ProfileCapturingRunner {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
        let cap = Arc::clone(&self.captured);
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            *cap.lock().unwrap() = Some(target_profile);
            let _ = event_tx.send(RunnerEvent::Completed { background_agent_id: bg_id });
            Ok(TaskFinalReport::completed(Some("captured".to_string())))
        })
    }
}

fn make_spawner_capturing_profile() -> (Arc<SubagentSpawner>, Arc<Mutex<Option<Option<ao_protocol::agent::AgentProfile>>>>) {
    let captured = Arc::new(Mutex::new(None));
    let cap2 = Arc::clone(&captured);
    let runner = ProfileCapturingRunner { captured: cap2 };
    let spawner = Arc::new(
        SubagentSpawner::new(Arc::new(
            ao_engine_tools_core::background_agents::SubagentRegistry::new(),
        ))
        .with_child_runner(Arc::new(runner)),
    );
    (spawner, captured)
}

#[tokio::test]
async fn no_target_with_parent_profile_clones_parent_sync() {
    // When target is omitted AND the caller has a stored profile, the clone-parent
    // path must run the parent's own profile as the child — NOT general-purpose.
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_result("clone done");
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let out = delegate
        .invoke(json!({ "directive": "do general work", "mode": "sync" }), &ctx)
        .await
        .unwrap();

    assert!(
        matches!(&out, ToolOutput::Text(t) if t == "clone done"),
        "no-target sync delegation with parent profile must clone parent and return result, got: {:?}",
        out
    );
}

#[tokio::test]
async fn no_target_with_parent_profile_passes_profile_to_child() {
    // The clone-parent path must pass the parent's AgentProfile as target_profile
    // to ChildRunner::launch, so the child runner (ProfileAwareChildRunner) knows
    // which profile to run.
    let tmp = TempDir::new().unwrap();
    let parent = make_profile("parent-agent", "Parent");
    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();
    let store = Arc::new(ao_persistence::profiles::AgentProfileStore::new(data_root));
    store.create(&parent).await.unwrap();

    let (spawner, captured) = make_spawner_capturing_profile();
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let _ = delegate
        .invoke(json!({ "directive": "do it", "mode": "sync" }), &ctx)
        .await
        .unwrap();

    let profile_opt = captured.lock().unwrap().clone().expect("runner was called");
    let profile = profile_opt.expect("clone-parent must pass Some(profile), not None");
    assert_eq!(
        profile.id, "parent-agent",
        "clone-parent must pass the parent's own profile to the child runner"
    );
    assert_eq!(
        profile.name, "Parent",
        "clone-parent profile name must match the parent's name"
    );
}

#[tokio::test]
async fn no_target_no_parent_profile_returns_recoverable_error() {
    // No target AND no stored profile to clone: there is no default stranger
    // agent to fall back to. This must be a recoverable error naming the fix
    // (retry with an explicit target), not a silent spawn of some generic
    // agent.
    let tmp = TempDir::new().unwrap();
    let data_root = DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();
    let store = Arc::new(AgentProfileStore::new(data_root));
    // Note: no profile is created for "rootless-agent".

    let spawner = make_spawner_with_result("unused");
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("rootless-agent");

    let out = delegate
        .invoke(json!({ "directive": "work", "mode": "sync" }), &ctx)
        .await
        .unwrap();

    match &out {
        ToolOutput::Error { recoverable, message } => {
            assert!(*recoverable, "no-target/no-profile must be recoverable");
            assert!(
                message.contains("target"),
                "error must name an explicit target as the fix; got: {message}"
            );
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn explicit_catalog_subagent_type_runs_generic_path() {
    // Naming a registered catalog subagent type that is NOT in the address
    // book must resolve via the catalog rather than erroring as an unknown
    // target. No built-in catalog ships with the engine, so this test
    // registers its own entry to exercise the still-live catalog-spawn path.
    let tmp = TempDir::new().unwrap();
    let parent = make_profile("parent-agent", "Parent"); // no delegates_to
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_catalog_type("Researcher", "explore result", 0);
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let out = delegate
        .invoke(
            json!({ "target": "Researcher", "directive": "find the bug", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(
        matches!(&out, ToolOutput::Text(t) if t == "explore result"),
        "explicit catalog target must run the generic subagent path, got: {:?}",
        out
    );
}

#[tokio::test]
async fn unknown_target_lists_both_namespaces() {
    // An unresolvable target must enumerate both the address-book targets and
    // the built-in subagent types so the model can self-correct.
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let spawner = make_spawner_with_result("unused");
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let out = delegate
        .invoke(
            json!({ "target": "Nope", "directive": "x", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    match &out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable, "unknown target must be recoverable");
            assert!(message.contains("Nope"), "error must name the unknown target");
            assert!(
                message.contains("Reviewer"),
                "error must list address-book targets"
            );
            assert!(
                message.contains("Available subagent types: []"),
                "no built-in catalog ships with the engine, so the catalog list must be empty; got: {message}"
            );
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn clone_parent_async_returns_delegation_id() {
    // Async clone-parent delegation (no target + stored parent profile) returns
    // immediately with a pollable delegation_id. The response names the parent
    // profile (not "general-purpose") because the child runs the parent's profile.
    let tmp = TempDir::new().unwrap();
    let parent = make_profile("parent-agent", "Parent");
    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();
    let store = Arc::new(ao_persistence::profiles::AgentProfileStore::new(data_root));
    store.create(&parent).await.unwrap();

    let spawner = make_spawner_with_delay("done", 200);
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let out = delegate
        .invoke(json!({ "directive": "bg research", "mode": "async" }), &ctx)
        .await
        .unwrap();

    match &out {
        ToolOutput::Text(text) => {
            assert!(
                text.contains("Parent"),
                "clone-parent async response must mention the parent profile name; got: {text}"
            );
            assert!(
                text.contains("delegation_id="),
                "response must contain a delegation_id; got: {text}"
            );
            assert!(
                text.contains("background"),
                "response must indicate background execution; got: {text}"
            );
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn no_target_no_parent_profile_returns_recoverable_error_in_async_mode() {
    // The no-target/no-profile check runs before the sync/async mode branch,
    // so async mode must also get the recoverable error — not a
    // delegation_id — when the caller has no target and no profile to clone.
    let tmp = TempDir::new().unwrap();
    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();
    let store = Arc::new(ao_persistence::profiles::AgentProfileStore::new(data_root));
    // No profile stored — nothing to clone.

    let spawner = make_spawner_with_delay("done", 200);
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("rootless-agent");

    let out = delegate
        .invoke(json!({ "directive": "bg research", "mode": "async" }), &ctx)
        .await
        .unwrap();

    match &out {
        ToolOutput::Error { recoverable, message } => {
            assert!(
                *recoverable,
                "no-target/no-profile must be recoverable in async mode too"
            );
            assert!(
                message.contains("target"),
                "error must name an explicit target as the fix; got: {message}"
            );
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn async_catalog_spawn_emits_async_launched_on_parent_event_stream() {
    // Coverage preserved from the removed `Task` background contract: the
    // catalog async path must publish a `RunnerEvent::AsyncLaunched` on the
    // parent's own `runner_events` stream so in-app observers can react
    // immediately. No built-in catalog ships with the engine, so this test
    // registers its own entry and targets it explicitly.
    // Note: the clone-parent path (spawn_named_async) does NOT emit
    // AsyncLaunched — it returns an inline background confirmation. This
    // test uses an explicit catalog target to reach the path that does.
    let tmp = TempDir::new().unwrap();
    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();
    let store = Arc::new(ao_persistence::profiles::AgentProfileStore::new(data_root));
    // No profile stored for "rootless-agent" — irrelevant to the catalog path.

    // Slow runner so the handle stays live through the assertion window.
    let spawner = make_spawner_with_catalog_type("Researcher", "done", 200);
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("rootless-agent");

    // Subscribe BEFORE invoking so the broadcast can't be missed.
    let mut events = ctx.runner_events.subscribe();

    let out = delegate
        .invoke(
            json!({ "target": "Researcher", "directive": "bg research", "mode": "async" }),
            &ctx,
        )
        .await
        .unwrap();

    let delegation_id = match &out {
        ToolOutput::Text(text) => text
            .split("delegation_id=")
            .nth(1)
            .and_then(|s| s.split(')').next())
            .map(|s| s.to_string())
            .expect("async response must carry a delegation_id"),
        other => panic!("expected Text, got {:?}", other),
    };

    // Drain until the AsyncLaunched event arrives (bounded so a missing event
    // fails the test rather than hanging).
    let event = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            match events.recv().await {
                Ok(RunnerEvent::AsyncLaunched {
                    background_agent_id,
                    subagent_type,
                    parent_agent_id,
                    ..
                }) => return (background_agent_id, subagent_type, parent_agent_id),
                Ok(_) => continue,
                Err(e) => panic!("event stream closed before AsyncLaunched: {e:?}"),
            }
        }
    })
    .await
    .expect("AsyncLaunched must be emitted on the parent stream within 500ms");

    let (background_agent_id, subagent_type, parent_agent_id) = event;
    assert_eq!(
        background_agent_id.to_string(),
        delegation_id,
        "AsyncLaunched id must match the delegation_id returned to the caller"
    );
    assert_eq!(
        subagent_type, "Researcher",
        "async delegation must report the resolved catalog subagent type"
    );
    assert_eq!(
        parent_agent_id, "rootless-agent",
        "AsyncLaunched must attribute the launch to the spawning parent agent"
    );
}

#[tokio::test]
async fn address_book_target_takes_precedence_over_catalog() {
    // When `target` matches an address-book entry it must use the
    // address-book (AgentProfile) path, not the catalog — even if a built-in
    // of the same name existed. Here "Reviewer" is only in the address book.
    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    let (spawner, captured) = make_spawner_capturing_directive();
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let _ = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "review this", "mode": "sync" }),
            &ctx,
        )
        .await
        .unwrap();

    // The address-book path wraps the directive in a delegation envelope; the
    // generic catalog path would forward the raw directive untouched.
    let prompt = captured.lock().unwrap().clone().unwrap();
    assert!(
        prompt.contains("Delegated by Parent"),
        "address-book target must use the envelope (delegate) path, got:\n{prompt}"
    );
}

// ─── regression — fork-mode history injection unchanged for Api targets ─────

#[tokio::test]
async fn fork_with_api_target_still_injects_parent_history() {
    // Regression guard: setting runner_mode = Api on the target profile must NOT
    // bypass the parent-transcript injection in fork mode. The directive construction
    // path (delegate/mod.rs ~273-291) is gated on share_context, not on runner_mode;
    // runner_mode only controls which child runner executes the directive.
    use ao_persistence::transcript::TranscriptStore;
    use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

    let tmp = TempDir::new().unwrap();
    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();

    let parent = make_parent_with_delegate("parent-agent", "Parent", "api-reviewer", "ApiReviewer", true);
    let mut target = make_profile("api-reviewer", "ApiReviewer");
    target.runner_mode = AgentRunnerMode::Api;

    let store = Arc::new(ao_persistence::profiles::AgentProfileStore::new(data_root.clone()));
    store.create(&parent).await.unwrap();
    store.create(&target).await.unwrap();

    let transcripts = Arc::new(TranscriptStore::new(data_root));
    let user_entry = TranscriptEntry {
        ts: chrono::Utc::now() - chrono::Duration::minutes(5),
        role: TranscriptRole::System("user".to_string()),
        content: "investigate the auth regression".to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    };
    let agent_entry = TranscriptEntry {
        ts: chrono::Utc::now() - chrono::Duration::minutes(4),
        role: TranscriptRole::Agent { agent: "parent-agent".to_string() },
        content: "traced it to the token refresh handler".to_string(),
        event_type: "response".to_string(),
        metadata: None,
        hidden_from_user: false,
    };
    transcripts.append("parent-agent", &user_entry).await.unwrap();
    transcripts.append("parent-agent", &agent_entry).await.unwrap();

    let (spawner, captured) = make_spawner_capturing_directive();
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent")
        .with_system_prompt("parent prompt")
        .with_transcript_store(Arc::clone(&transcripts));

    let _ = delegate
        .invoke(
            json!({ "target": "ApiReviewer", "directive": "finish the fix", "mode": "sync", "share_context": true }),
            &ctx,
        )
        .await
        .unwrap();

    let prompt = captured.lock().unwrap().clone().expect("child runner must have been launched");

    assert!(
        prompt.contains("[Conversation history]"),
        "fork with Api target must still inject parent history. Got:\n{prompt}"
    );
    assert!(
        prompt.contains("investigate the auth regression"),
        "fork with Api target must include parent user message. Got:\n{prompt}"
    );
    assert!(
        prompt.contains("traced it to the token refresh handler"),
        "fork with Api target must include parent agent reply. Got:\n{prompt}"
    );
    let history_idx = prompt.find("[Conversation history]").unwrap();
    let envelope_idx = prompt.find("Delegated by Parent").unwrap();
    assert!(
        history_idx < envelope_idx,
        "history block must precede envelope; got history@{} envelope@{}",
        history_idx,
        envelope_idx
    );
}

// ─── transcript_path in async launch result ───────────────────────────────────

/// Verifies that an async Delegate launch (address-book path) returns both a
/// delegation_id and a transcript_path pointing to the child's sidechain JSONL
/// under the configured data root.
#[tokio::test]
async fn async_launch_result_includes_transcript_path() {
    let guard = crate::test_env::DataDirGuard::new();

    let tmp = TempDir::new().unwrap();
    let parent = make_parent_with_delegate("parent-agent", "Parent", "reviewer", "Reviewer", false);
    let target = make_profile("reviewer", "Reviewer");
    let store = setup_store_with_profiles(&tmp, &parent, &target).await;

    // Slow runner so the async call returns before the child finishes.
    let spawner = make_spawner_with_delay("done", 200);
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("parent-agent");

    let out = delegate
        .invoke(
            json!({ "target": "Reviewer", "directive": "async task", "mode": "async" }),
            &ctx,
        )
        .await
        .unwrap();

    match &out {
        ToolOutput::Text(text) => {
            assert!(
                text.contains("delegation_id="),
                "async launch result must contain delegation_id; got: {text}"
            );
            assert!(
                text.contains("transcript_path="),
                "async launch result must contain transcript_path; got: {text}"
            );

            // Extract the delegation_id and verify the path references it.
            let delegation_id = text
                .split("delegation_id=")
                .nth(1)
                .and_then(|s| s.split(')').next())
                .expect("must have delegation_id in parentheses");

            let expected_filename = format!("{}.jsonl", delegation_id);
            assert!(
                text.contains(&expected_filename),
                "transcript_path must end with <delegation_id>.jsonl; got: {text}"
            );

            // The transcript path must be rooted under the test data dir.
            let data_root_str = guard.data_dir().display().to_string();
            assert!(
                text.contains(&data_root_str),
                "transcript_path must be under the configured data root ({}); got: {text}",
                data_root_str
            );

            assert!(
                text.contains("DelegateOutput"),
                "async result must include a DelegateOutput hint; got: {text}"
            );
        }
        other => panic!("expected Text output from async Delegate, got: {:?}", other),
    }
}

/// Verifies that the catalog subagent async path also returns a
/// transcript_path in the launch result. No built-in catalog ships with the
/// engine, so this test registers its own entry and targets it explicitly.
#[tokio::test]
async fn async_catalog_launch_result_includes_transcript_path() {
    let guard = crate::test_env::DataDirGuard::new();

    let tmp = TempDir::new().unwrap();
    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();
    let store = Arc::new(ao_persistence::profiles::AgentProfileStore::new(data_root));
    // No profile stored — irrelevant to the catalog path.

    let spawner = make_spawner_with_catalog_type("Researcher", "done", 200);
    let delegate = Delegate::with_spawner_and_store(spawner, store);
    let ctx = make_ctx("rootless-agent");

    let out = delegate
        .invoke(
            json!({ "target": "Researcher", "directive": "bg research", "mode": "async" }),
            &ctx,
        )
        .await
        .unwrap();

    match &out {
        ToolOutput::Text(text) => {
            assert!(
                text.contains("transcript_path="),
                "generic async launch result must contain transcript_path; got: {text}"
            );

            let delegation_id = text
                .split("delegation_id=")
                .nth(1)
                .and_then(|s| s.split(')').next())
                .expect("must have delegation_id");

            let expected_filename = format!("{}.jsonl", delegation_id);
            assert!(
                text.contains(&expected_filename),
                "transcript_path must reference <delegation_id>.jsonl; got: {text}"
            );

            let data_root_str = guard.data_dir().display().to_string();
            assert!(
                text.contains(&data_root_str),
                "transcript_path must be under the configured data root ({}); got: {text}",
                data_root_str
            );
        }
        other => panic!("expected Text output, got: {:?}", other),
    }
}
