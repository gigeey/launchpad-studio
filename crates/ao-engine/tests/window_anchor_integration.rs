//! Window anchor end-to-end integration test — acceptance gate.
//!
//! Exercises `NativeAgentRunner` against a `RecordingProviderClient` that captures the
//! canonical `Vec<Message>` passed into each `ProviderClient::complete` call. The tests
//! assert:
//!
//! 1. `byte_prefix_stable_within_window` — two consecutive turns within the max_window
//!    boundary produce a byte-identical `messages[]` prefix (CACHE HIT path).
//! 2. `rotation_at_max_window_boundary` — appending entries past `max_window = 44` forces
//!    exactly one floor rotation; post-rotation turns resume prefix stability.
//! 3. `recall_history_produces_one_entry` — a `RecallHistory` tool call stored as a
//!    single `tool_result` transcript entry maps to exactly ONE `Message::ToolResult` in
//!    the `messages[]` array.
//! 4. `anchor_reset_on_simulated_restart` — dropping and reconstructing the
//!    `WindowAnchorRegistry` between turns simulates a process restart; the next turn
//!    pins a fresh anchor (registry state is verified directly from the registry).
//!
//! Only the provider is mocked. Everything else — `PersistenceLayer`, `WindowAnchorRegistry`,
//! `history::select`, `NativeAgentRunner` — is real.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use ao_engine::agent_runner::{
    AgentRunRequest, AgentRunner, AgentRunnerMode, NativeAgentRunner, ProviderFactory,
    RunScope, RunningAgents,
};
use ao_engine::event_bus::EventBus;
use ao_engine::history::anchor::{AnchorKey, WindowAnchorRegistry};
use ao_engine::instance_registry::InstanceRegistry;
use ao_engine_tools_core::{Registry, SessionKind};
use ao_engine_tools_runner::{
    message::{ContentBlock, Message, MessageNormalizer, NormalizerError},
    provider::{
        CompletionEvent, CompletionRequest, CompletionStream, ProviderClient, ProviderError,
        StopReason,
    },
};
use ao_persistence::{paths::DataRoot, PersistenceLayer};
use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use chrono::Utc;

// ─────────────────────────────────────────────────────────────────────────────
// Infrastructure helpers
// ─────────────────────────────────────────────────────────────────────────────

struct NoopNormalizer;

impl MessageNormalizer for NoopNormalizer {
    fn to_provider(&self, _messages: &[Message]) -> Result<Value, NormalizerError> {
        Ok(Value::Array(vec![]))
    }
    fn from_provider(&self, _value: Value) -> Result<Vec<Message>, NormalizerError> {
        Ok(vec![])
    }
}

/// Captures the `messages` vec from every `complete()` call and returns scripted
/// responses. Falls back to a bare "ok" turn when the script is exhausted.
struct RecordingProviderClient {
    recorded: Arc<Mutex<Vec<Vec<Message>>>>,
    scripts: Mutex<VecDeque<Vec<CompletionEvent>>>,
    normalizer: NoopNormalizer,
}

impl RecordingProviderClient {
    fn new(turns: Vec<Vec<CompletionEvent>>) -> Self {
        Self {
            recorded: Arc::new(Mutex::new(Vec::new())),
            scripts: Mutex::new(turns.into()),
            normalizer: NoopNormalizer,
        }
    }

    fn recorded_messages(&self) -> Arc<Mutex<Vec<Vec<Message>>>> {
        Arc::clone(&self.recorded)
    }
}

#[async_trait]
impl ProviderClient for RecordingProviderClient {
    async fn complete(
        &self,
        request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        self.recorded.lock().await.push(request.messages.clone());

        let turn = self
            .scripts
            .lock()
            .await
            .pop_front()
            .unwrap_or_else(|| {
                vec![
                    CompletionEvent::AssistantText("ok".to_string()),
                    CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
                ]
            });

        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            for event in turn {
                if tx.send(Ok(event)).await.is_err() {
                    break;
                }
            }
        });
        Ok(CompletionStream::new(rx))
    }

    fn message_normalizer(&self) -> &dyn MessageNormalizer {
        &self.normalizer
    }
}

struct FixedFactory {
    client: Arc<dyn ProviderClient>,
}

impl ProviderFactory for FixedFactory {
    fn build(&self, _agent: &AgentProfile) -> Result<Arc<dyn ProviderClient>, ProviderError> {
        Ok(Arc::clone(&self.client))
    }
}

async fn open_persistence(path: &std::path::Path) -> Arc<PersistenceLayer> {
    let root = DataRoot::new(path);
    root.ensure_directories().await.expect("ensure_directories");
    Arc::new(PersistenceLayer::init_with_root(root).await.expect("init persistence"))
}

fn make_runner(
    persistence: Arc<PersistenceLayer>,
    factory: impl ProviderFactory + 'static,
    registry: Arc<WindowAnchorRegistry>,
) -> NativeAgentRunner {
    NativeAgentRunner::new(
        Arc::new(EventBus::new(512)),
        Arc::new(InstanceRegistry::new()),
        Arc::new(RunningAgents::new()),
        Arc::new(factory),
        Arc::new(Registry::default()),
        persistence,
    )
    .with_anchor_registry(registry)
}

fn make_agent(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: id.to_string(),
        description: String::new(),
        emoji: None,
        provider: ProviderConfig::Cli(CliProviderConfig {
            command: "echo".to_string(),
            args: vec![],
            normalizer: None,
            output_format: OutputFormat::Text,
            input_mode: InputMode::Arg,
            model_arg: None,
            model_aliases: Default::default(),
            system_prompt_arg: None,
            session_arg: None,
            resume_args: vec![],
            session_id_fields: vec![],
            clear_env: false,
            no_output_timeout_ms: 30_000,
            file_capabilities: None,
        }),
        model: None,
        skills: vec![],
        system_prompt: None,
        tools: None,
        env: Default::default(),
        max_instances: 1,
        timeout_seconds: 60,
        working_dir: None,
        home_dir: None,
        serialize: false,
        workflows: None,
        template: None,
        runner_mode: AgentRunnerMode::Api,
        enabled_plugins: Default::default(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
        owning_team_id: None,
        native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
        delegates_to: vec![],
        persona: None,
        special_instructions: None,
        legacy_system_prompt: None,
        max_delegation_depth: None,
        channels: vec![],
        max_turns: None,
    }
}

fn make_request(agent: AgentProfile, prompt: &str) -> AgentRunRequest {
    let (tx, _rx) = mpsc::channel(4);
    AgentRunRequest {
        agent,
        prompt: prompt.to_string(),
        attachments: vec![],
        run_complete_tx: tx,
        focus_path: None,
        scope: RunScope::Standalone,
        thread_id: None,
        session_kind: SessionKind::Interactive,
        pre_registered_run_id: None,
        ..Default::default()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Transcript entry constructors
// ─────────────────────────────────────────────────────────────────────────────

fn user_entry(content: &str) -> TranscriptEntry {
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("user".to_string()),
        content: content.to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    }
}

fn response_entry(content: &str, turn_id: &str) -> TranscriptEntry {
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Agent { agent: "test-agent".to_string() },
        content: content.to_string(),
        event_type: "response".to_string(),
        metadata: Some(
            serde_json::from_value(json!({ "turn_id": turn_id })).unwrap(),
        ),
        hidden_from_user: false,
    }
}

fn tool_use_entry(tool_use_id: &str, tool_name: &str, turn_id: &str) -> TranscriptEntry {
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Agent { agent: "test-agent".to_string() },
        content: String::new(),
        event_type: "tool_use".to_string(),
        metadata: Some(
            serde_json::from_value(json!({
                "tool_use_id": tool_use_id,
                "tool_name": tool_name,
                "turn_id": turn_id,
                "input": {}
            }))
            .unwrap(),
        ),
        hidden_from_user: false,
    }
}

fn tool_result_entry(tool_use_id: &str, output: &str) -> TranscriptEntry {
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("tool".to_string()),
        content: String::new(),
        event_type: "tool_result".to_string(),
        metadata: Some(
            serde_json::from_value(json!({
                "tool_use_id": tool_use_id,
                "output": output,
                "is_error": false
            }))
            .unwrap(),
        ),
        hidden_from_user: false,
    }
}

async fn append_entries(
    persistence: &PersistenceLayer,
    agent_id: &str,
    entries: &[TranscriptEntry],
) {
    for entry in entries {
        persistence.transcripts.append(agent_id, entry).await.expect("append");
    }
}

async fn run_turn(
    runner: &NativeAgentRunner,
    agent_id: &str,
    persistence: &PersistenceLayer,
    prompt: &str,
) {
    // Pre-persist the current user message so history::select can see it and
    // apply current_message_already_persisted=true (drops the last entry = this msg).
    append_entries(persistence, agent_id, &[user_entry(prompt)]).await;
    let req = make_request(make_agent(agent_id), prompt);
    timeout(Duration::from_secs(15), runner.run(req))
        .await
        .expect("run timed out")
        .expect("run errored");
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1 — byte_prefix_stable_within_window
// ─────────────────────────────────────────────────────────────────────────────

/// Within the anchor window (`slice.len() - anchor_idx <= max_window = 44`),
/// the `messages[]` prefix is byte-identical across consecutive turns.
#[tokio::test]
async fn byte_prefix_stable_within_window() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let persistence = open_persistence(tmp.path()).await;
    let agent_id = "anchor-prefix-test";

    // Seed 24 prior entries: 12 user + 12 response alternating.
    // With target=20, slice=24 entries: start=4, anchor at entry[4].
    let mut prior: Vec<TranscriptEntry> = Vec::with_capacity(24);
    for i in 0..12 {
        prior.push(user_entry(&format!("user-{i}")));
        prior.push(response_entry(&format!("resp-{i}"), &format!("t{i}")));
    }
    append_entries(&persistence, agent_id, &prior).await;

    let recording_client = Arc::new(RecordingProviderClient::new(vec![]));
    let recorded = recording_client.recorded_messages();
    let registry = Arc::new(WindowAnchorRegistry::new());
    let runner =
        make_runner(Arc::clone(&persistence), FixedFactory { client: Arc::clone(&recording_client) as Arc<dyn ProviderClient> }, Arc::clone(&registry));

    // Turn N: history::select sees 24 entries, pins anchor at index 4.
    run_turn(&runner, agent_id, &persistence, "turn-N").await;

    let recorded_guard = recorded.lock().await;
    let messages_n = recorded_guard[0].clone();
    drop(recorded_guard);

    // Turn N+1: CACHE HIT — anchor at index 4, 26-4=22 ≤ 44.
    run_turn(&runner, agent_id, &persistence, "turn-N").await;

    let recorded_guard = recorded.lock().await;
    let messages_n_plus_1 = recorded_guard[1].clone();
    drop(recorded_guard);

    // Two new messages were added (prior user_msg_N became part of history, plus
    // response_N from turn N's provider output).
    assert_eq!(
        messages_n.len() + 2,
        messages_n_plus_1.len(),
        "turn N+1 should have exactly 2 more messages than turn N; got {} and {}",
        messages_n.len(),
        messages_n_plus_1.len()
    );

    // Byte-identical prefix: every message from turn N appears at the same position
    // in turn N+1.
    assert_eq!(
        &messages_n_plus_1[..messages_n.len()],
        messages_n.as_slice(),
        "messages[] prefix must be byte-identical on a CACHE HIT turn"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2 — rotation_at_max_window_boundary
// ─────────────────────────────────────────────────────────────────────────────

/// When the slice grows past `max_window = pinned_target*2 + GRACE = 44`,
/// the anchor rotates (CACHE MISS). Post-rotation turns resume prefix stability.
#[tokio::test]
async fn rotation_at_max_window_boundary() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let persistence = open_persistence(tmp.path()).await;
    let agent_id = "anchor-rotation-test";

    // Same seed as Test 1: 24 entries, anchor at index 4 after turn N.
    let mut prior: Vec<TranscriptEntry> = Vec::with_capacity(24);
    for i in 0..12 {
        prior.push(user_entry(&format!("user-{i}")));
        prior.push(response_entry(&format!("resp-{i}"), &format!("t{i}")));
    }
    append_entries(&persistence, agent_id, &prior).await;

    let recording_client = Arc::new(RecordingProviderClient::new(vec![]));
    let recorded = recording_client.recorded_messages();
    let registry = Arc::new(WindowAnchorRegistry::new());
    let runner =
        make_runner(Arc::clone(&persistence), FixedFactory { client: Arc::clone(&recording_client) as Arc<dyn ProviderClient> }, Arc::clone(&registry));

    // Turn N: pins anchor at index 4 in the 24-entry slice (target=20, start=4).
    run_turn(&runner, agent_id, &persistence, "turn-N").await;
    let messages_n = recorded.lock().await[0].clone();

    // Append 32 extra entries (16 pairs), pushing the slice well past max_window=44.
    // After turn N the transcript holds: 24 prior + user_msg_N + response_N = 26.
    // Adding 32 more entries grows the next slice to 26+32=58 entries (minus 1 for tail).
    // slice.len() - anchor_idx = 57 - 4 = 53 > 44 → CACHE MISS → rotation.
    let mut extra: Vec<TranscriptEntry> = Vec::with_capacity(32);
    for i in 0..16 {
        extra.push(user_entry(&format!("extra-user-{i}")));
        extra.push(response_entry(&format!("extra-resp-{i}"), &format!("ext{i}")));
    }
    append_entries(&persistence, agent_id, &extra).await;

    // Turn N+M: rotation — new anchor is pinned at a different floor.
    run_turn(&runner, agent_id, &persistence, "turn-NM").await;
    let messages_n_plus_m = recorded.lock().await[1].clone();

    // The rotation shifted the floor: the first 20 messages no longer match.
    assert_ne!(
        messages_n_plus_m.len().min(messages_n.len()),
        0,
        "both message arrays must be non-empty"
    );
    let compare_len = messages_n.len().min(messages_n_plus_m.len());
    assert_ne!(
        &messages_n_plus_m[..compare_len],
        &messages_n[..compare_len],
        "post-rotation messages[] should NOT byte-equal the pre-rotation prefix (floor shifted)"
    );

    // Turn N+M+1: CACHE HIT on the new floor → prefix stable again.
    run_turn(&runner, agent_id, &persistence, "turn-NM").await;
    let messages_n_plus_m_plus_1 = recorded.lock().await[2].clone();

    assert_eq!(
        messages_n_plus_m.len() + 2,
        messages_n_plus_m_plus_1.len(),
        "post-rotation turn N+M+1 should have exactly 2 more messages than turn N+M"
    );
    assert_eq!(
        &messages_n_plus_m_plus_1[..messages_n_plus_m.len()],
        messages_n_plus_m.as_slice(),
        "post-rotation prefix must be byte-identical on the CACHE HIT turn"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3 — recall_history_produces_one_entry
// ─────────────────────────────────────────────────────────────────────────────

/// A `RecallHistory` tool call stored as a SINGLE `tool_result` transcript entry
/// maps to exactly ONE `Message::ToolResult` — not N separate ToolResult blocks.
/// This verifies that the entire recall is one entry, regardless of how much
/// content it contains.
#[tokio::test]
async fn recall_history_produces_one_entry() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let persistence = open_persistence(tmp.path()).await;
    let agent_id = "recall-history-test";

    // A few normal prior entries to make the history non-trivial.
    let prior = vec![
        user_entry("hello"),
        response_entry("hi there", "t0"),
        user_entry("what do you know?"),
        response_entry("quite a lot", "t1"),
    ];
    append_entries(&persistence, agent_id, &prior).await;

    // Simulate a RecallHistory tool call: one tool_use + one tool_result whose
    // `output` encodes the recall result (could be many history entries as text).
    let recall_id = "recall-tu-1";
    let recall_turn_id = "t-recall";
    let recall_output = (0..30)
        .map(|i| format!("[Entry {i}] user: prior message {i}"))
        .collect::<Vec<_>>()
        .join("\n");

    append_entries(
        &persistence,
        agent_id,
        &[
            tool_use_entry(recall_id, "RecallHistory", recall_turn_id),
            tool_result_entry(recall_id, &recall_output),
        ],
    )
    .await;

    let recording_client = Arc::new(RecordingProviderClient::new(vec![]));
    let recorded = recording_client.recorded_messages();
    let registry = Arc::new(WindowAnchorRegistry::new());
    let runner = make_runner(
        Arc::clone(&persistence),
        FixedFactory { client: Arc::clone(&recording_client) as Arc<dyn ProviderClient> },
        Arc::clone(&registry),
    );

    run_turn(&runner, agent_id, &persistence, "what did you recall?").await;

    let messages = recorded.lock().await[0].clone();

    // Count ToolResult messages: must be exactly ONE (the RecallHistory result).
    let tool_result_count = messages
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .count();

    assert_eq!(
        tool_result_count, 1,
        "RecallHistory result must produce exactly ONE Message::ToolResult (not N); got {tool_result_count}. messages: {messages:?}"
    );

    // The single ToolResult references the correct tool_use_id.
    let tool_result_msg = messages
        .iter()
        .find(|m| matches!(m, Message::ToolResult { .. }))
        .unwrap();

    match tool_result_msg {
        Message::ToolResult { tool_use_id, content, .. } => {
            assert_eq!(
                tool_use_id, recall_id,
                "ToolResult must reference the RecallHistory tool_use_id"
            );
            // The output is the full recall text — still one ContentBlock::Text.
            assert_eq!(content.len(), 1, "ToolResult content must have exactly one block");
            match &content[0] {
                ContentBlock::Text { text } => {
                    assert!(
                        text.contains("prior message 0"),
                        "ToolResult content must carry the recall output"
                    );
                }
                other => panic!("expected Text block, got {:?}", other),
            }
        }
        other => panic!("expected ToolResult, got {:?}", other),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4 — anchor_reset_on_simulated_restart (stretch)
// ─────────────────────────────────────────────────────────────────────────────

/// Dropping and reconstructing `WindowAnchorRegistry` simulates a process restart.
/// The next turn re-pins a fresh anchor (observable via `registry.get()`).
/// The invariant: "one cache miss per restart, then stability resumes."
#[tokio::test]
async fn anchor_reset_on_simulated_restart() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let persistence = open_persistence(tmp.path()).await;
    let agent_id = "anchor-restart-test";

    // Seed 24 prior entries (same as Test 1).
    let mut prior: Vec<TranscriptEntry> = Vec::with_capacity(24);
    for i in 0..12 {
        prior.push(user_entry(&format!("user-{i}")));
        prior.push(response_entry(&format!("resp-{i}"), &format!("t{i}")));
    }
    append_entries(&persistence, agent_id, &prior).await;

    let recording_client = Arc::new(RecordingProviderClient::new(vec![]));
    let anchor_key = AnchorKey::Personal(agent_id.to_string());

    // ── Turn N: use registry_1 — pins anchor.
    let registry_1 = Arc::new(WindowAnchorRegistry::new());
    {
        let runner_1 = make_runner(
            Arc::clone(&persistence),
            FixedFactory { client: Arc::clone(&recording_client) as Arc<dyn ProviderClient> },
            Arc::clone(&registry_1),
        );
        run_turn(&runner_1, agent_id, &persistence, "pre-restart").await;
    }
    let anchor_before = registry_1.get(&anchor_key);
    assert!(anchor_before.is_some(), "turn N must have pinned an anchor in registry_1");

    // ── Simulated restart: drop registry_1, create fresh registry_2.
    drop(registry_1);
    let registry_2 = Arc::new(WindowAnchorRegistry::new());
    assert!(
        registry_2.get(&anchor_key).is_none(),
        "fresh registry must have no anchor"
    );

    // ── Turn N+1: use registry_2 — empty registry forces CACHE MISS, pins fresh anchor.
    {
        let runner_2 = make_runner(
            Arc::clone(&persistence),
            FixedFactory { client: Arc::clone(&recording_client) as Arc<dyn ProviderClient> },
            Arc::clone(&registry_2),
        );
        run_turn(&runner_2, agent_id, &persistence, "post-restart").await;
    }
    let anchor_after = registry_2.get(&anchor_key);
    assert!(
        anchor_after.is_some(),
        "turn N+1 with fresh registry must pin a new anchor (Fresh path)"
    );

    // ── Turn N+2: registry_2 now has an anchor → CACHE HIT → prefix stable.
    let recording_client_2 = Arc::new(RecordingProviderClient::new(vec![]));
    let recorded_2 = recording_client_2.recorded_messages();
    {
        let runner_3 = make_runner(
            Arc::clone(&persistence),
            FixedFactory {
                client: Arc::clone(&recording_client_2) as Arc<dyn ProviderClient>,
            },
            Arc::clone(&registry_2),
        );
        run_turn(&runner_3, agent_id, &persistence, "post-restart").await;
        run_turn(&runner_3, agent_id, &persistence, "post-restart").await;
    }
    let guard = recorded_2.lock().await;
    // Both turns captured — second turn's prefix should include first turn's messages.
    assert_eq!(
        guard.len(),
        2,
        "two turns should produce two recorded message arrays"
    );
    let first = &guard[0];
    let second = &guard[1];
    assert_eq!(
        first.len() + 2,
        second.len(),
        "CACHE HIT: second turn has exactly 2 more messages than first"
    );
    assert_eq!(
        &second[..first.len()],
        first.as_slice(),
        "CACHE HIT after restart: prefix must be byte-identical"
    );
}
