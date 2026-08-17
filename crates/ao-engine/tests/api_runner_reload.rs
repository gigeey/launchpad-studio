//! Reload integration test — acceptance gate.
//!
//! Verifies the full transcript continuity surface across a simulated
//! runner restart: write half (tool_use + tool_result + response persisted with
//! matching turn_ids), read half (history::select + to_messages threading prior
//! turns into the next request), and coalescing (text + tool_use from the same
//! assistant turn merged into a single Message::Assistant).
//!
//! Four phases:
//! 1. Run an Api-mode agent through a tool-use + text turn; assert disk entries.
//! 2. Drop all runner state; rebuild a new runner from the same data directory.
//! 3. Fire a follow-up user turn against the rebuilt runner.
//! 4. Assert the follow-up CompletionRequest carries the full prior context in
//!    the expected order: [prior_user, assistant{text+tool_use}, tool_result,
//!    current_user].

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use ao_engine::agent_runner::{
    AgentRunRequest, AgentRunner, AgentRunnerMode, NativeAgentRunner, ProviderFactory,
    RunComplete, RunScope, RunningAgents,
};
use ao_engine::event_bus::EventBus;
use ao_engine::instance_registry::InstanceRegistry;
use ao_engine_tools_core::{Registry, SessionKind};
use ao_engine_tools_runner::{
    message::{ContentBlock, Message, MessageNormalizer, NormalizerError},
    provider::{
        CompletionEvent, CompletionRequest, CompletionStream, MockProviderClient,
        ProviderClient, ProviderError, StopReason,
    },
};
use ao_persistence::{paths::DataRoot, PersistenceLayer};
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
};
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use chrono::Utc;
use serde_json::{json, Value};

// ─────────────────────────────────────────────────────────────────────────────
// Test helpers
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

struct FixedProviderFactory {
    client: Arc<dyn ProviderClient>,
}

impl ProviderFactory for FixedProviderFactory {
    fn build(&self, _agent: &AgentProfile) -> Result<Arc<dyn ProviderClient>, ProviderError> {
        Ok(Arc::clone(&self.client))
    }
}

/// A provider that stores the first CompletionRequest it receives, then
/// immediately returns AssistantText("ok") + TurnComplete. Used in Phase 3
/// to capture the rebuilt runner's outgoing request.
struct CapturingProviderClient {
    captured: Arc<Mutex<Option<CompletionRequest>>>,
    normalizer: NoopNormalizer,
}

impl CapturingProviderClient {
    fn new(captured: Arc<Mutex<Option<CompletionRequest>>>) -> Self {
        Self { captured, normalizer: NoopNormalizer }
    }
}

#[async_trait]
impl ProviderClient for CapturingProviderClient {
    async fn complete(
        &self,
        request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        *self.captured.lock().await = Some(request);
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            let _ = tx.send(Ok(CompletionEvent::AssistantText("ok".to_string()))).await;
            let _ =
                tx.send(Ok(CompletionEvent::TurnComplete { stop_reason: StopReason::Natural }))
                    .await;
        });
        Ok(CompletionStream::new(rx))
    }

    fn message_normalizer(&self) -> &dyn MessageNormalizer {
        &self.normalizer
    }
}

/// Build a real PersistenceLayer from an existing directory path (idempotent).
async fn open_persistence(path: &std::path::Path) -> Arc<PersistenceLayer> {
    let data_root = DataRoot::new(path);
    data_root.ensure_directories().await.expect("ensure_directories");
    let p = PersistenceLayer::init_with_root(data_root).await.expect("init persistence");
    Arc::new(p)
}

fn make_native_runner(
    bus: Arc<EventBus>,
    factory: impl ProviderFactory + 'static,
    persistence: Arc<PersistenceLayer>,
) -> NativeAgentRunner {
    NativeAgentRunner::new(
        bus,
        Arc::new(InstanceRegistry::new()),
        Arc::new(RunningAgents::new()),
        Arc::new(factory),
        Arc::new(Registry::default()),
        persistence,
    )
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

fn make_run_request(agent: AgentProfile, prompt: &str) -> (AgentRunRequest, mpsc::Receiver<RunComplete>) {
    let (tx, rx) = mpsc::channel(4);
    let req = AgentRunRequest {
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
    };
    (req, rx)
}

// ─────────────────────────────────────────────────────────────────────────────
// Test
// ─────────────────────────────────────────────────────────────────────────────

/// End-to-end reload test for the full transcript continuity surface.
///
/// Phase 1: Run an Api-mode agent through a two-provider-turn session:
///   - Turn 1 emits AssistantText + ToolUse (tool fails as "Read" is not in
///     the empty registry → error tool_result still written to disk).
///   - Turn 2 emits AssistantText (the final response).
///   Asserts response, tool_use, and tool_result entries are on disk with
///   matching turn_id metadata (turn_id ties tool_use to tool_result, and
///   the response from turn 1's pre-tool text shares that same turn_id).
///
/// Phase 2: Drop all runner state; reopen PersistenceLayer from the same dir.
///
/// Phase 3: Fire a follow-up prompt against a fresh runner backed by a
///   CapturingProviderClient that records the outgoing CompletionRequest.
///
/// Phase 4: Assert messages = [User(prior), Assistant{Text+ToolUse}, ToolResult,
///   User(fresh)], verifying coalescing, ordering, and correct roles.
#[tokio::test]
async fn api_runner_reload_tool_use_round_trip() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let persistence = open_persistence(tmp.path()).await;
    let agent_id = "reload-test-agent";

    // ── Pre-populate the transcript with a user message ───────────────────────
    // Simulates the user message having been persisted before the native runner
    // was dispatched (as current_message_already_persisted=true expects).
    let initial_prompt = "read the test file";
    persistence
        .transcripts
        .append(
            agent_id,
            &TranscriptEntry {
                ts: Utc::now(),
                role: TranscriptRole::System("user".to_string()),
                content: initial_prompt.to_string(),
                event_type: "message".to_string(),
                metadata: None,
                hidden_from_user: false,
            },
        )
        .await
        .expect("pre-populate user message");

    // ── Phase 1: Tool-use round-trip ──────────────────────────────────────────
    // Turn 1: text chunk + tool-use + TurnComplete.
    // Turn 2: text-only response + TurnComplete.
    // "Read" is not in the empty Registry → executor produces an error
    // tool_result, which is still a valid ToolResult event the TimelineAdapter
    // records as a "tool_result" transcript entry.
    let phase1_client: Arc<dyn ProviderClient> =
        Arc::new(MockProviderClient::new(vec![
            vec![
                CompletionEvent::AssistantText("I'll read the file for you.".to_string()),
                CompletionEvent::ToolUse {
                    id: "tu-reload-1".to_string(),
                    name: "Read".to_string(),
                    input: json!({ "file_path": "/tmp/test.txt" }),
                },
                CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
            ],
            vec![
                CompletionEvent::AssistantText("The file content has been read.".to_string()),
                CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
            ],
        ]));

    let bus1 = Arc::new(EventBus::new(512));
    let runner1 = make_native_runner(
        Arc::clone(&bus1),
        FixedProviderFactory { client: phase1_client },
        Arc::clone(&persistence),
    );

    let (req1, _rx1) = make_run_request(make_agent(agent_id), initial_prompt);
    let result1 = timeout(Duration::from_secs(15), runner1.run(req1))
        .await
        .expect("Phase 1 run timed out")
        .expect("Phase 1 run errored");

    assert_eq!(result1.output_text, "The file content has been read.");

    // ── Phase 1 assertions: disk-persisted entries ────────────────────────────
    let all_entries = persistence.transcripts.read_all(agent_id).await.expect("read_all");

    // Expected: [user_msg(pre-pop), response(T1), tool_use(T1), tool_result(T1), response(T2)]
    // = 5 entries. Assert at minimum that the three critical entry types are present.
    assert!(
        all_entries.len() >= 4,
        "expected ≥4 entries (user+response+tool_use+tool_result); got {}",
        all_entries.len()
    );

    let tool_use_entry = all_entries
        .iter()
        .find(|e| e.event_type == "tool_use")
        .expect("tool_use entry must be persisted");
    let tool_result_entry = all_entries
        .iter()
        .find(|e| e.event_type == "tool_result")
        .expect("tool_result entry must be persisted");
    // The first response entry shares T1 with the tool_use.
    let first_response_entry = all_entries
        .iter()
        .find(|e| e.event_type == "response")
        .expect("at least one response entry must be persisted");

    let use_meta = tool_use_entry.metadata.as_ref().expect("tool_use metadata");
    let result_meta = tool_result_entry.metadata.as_ref().expect("tool_result metadata");
    let response_meta = first_response_entry.metadata.as_ref().expect("response metadata");

    let use_turn_id = use_meta["turn_id"].as_str().expect("tool_use turn_id");
    let result_turn_id = result_meta["turn_id"].as_str().expect("tool_result turn_id");
    let response_turn_id = response_meta["turn_id"].as_str().expect("response turn_id");

    assert_eq!(
        use_turn_id, result_turn_id,
        "tool_use and tool_result must share the same turn_id"
    );
    assert_eq!(
        use_turn_id, response_turn_id,
        "the first response and tool_use must share the same turn_id (same assistant turn)"
    );
    assert_eq!(
        use_meta["tool_use_id"].as_str(),
        Some("tu-reload-1"),
        "tool_use_id must match the provider-emitted id"
    );
    assert_eq!(
        use_meta["tool_name"].as_str(),
        Some("Read"),
        "tool_name must match the provider-emitted name"
    );
    assert_eq!(
        result_meta["tool_use_id"].as_str(),
        Some("tu-reload-1"),
        "tool_result must reference the tool_use by id"
    );

    // ── Phase 2: Drop runner; rebuild from same data directory ─────────────────
    drop(runner1);
    drop(persistence);

    let persistence2 = open_persistence(tmp.path()).await;

    // ── Phase 3: Follow-up turn against the rebuilt runner ────────────────────
    let captured: Arc<Mutex<Option<CompletionRequest>>> = Arc::new(Mutex::new(None));
    let phase3_client = Arc::new(CapturingProviderClient::new(Arc::clone(&captured)));

    let bus2 = Arc::new(EventBus::new(512));
    let runner2 = make_native_runner(
        Arc::clone(&bus2),
        FixedProviderFactory { client: phase3_client as Arc<dyn ProviderClient> },
        persistence2,
    );

    let follow_up = "what did you find?";
    let (req2, _rx2) = make_run_request(make_agent(agent_id), follow_up);
    timeout(Duration::from_secs(15), runner2.run(req2))
        .await
        .expect("Phase 3 run timed out")
        .expect("Phase 3 run errored");

    // ── Phase 4: Assert full prior context in the outgoing request ────────────
    let guard = captured.lock().await;
    let captured_req = guard.as_ref().expect("CapturingProviderClient was never called");
    let messages = &captured_req.messages;

    // history::select (current_message_already_persisted=true) drops the last
    // transcript entry (response(T2)="The file content has been read.") leaving:
    //   [user_msg, response(T1), tool_use(T1), tool_result(T1)]
    // to_messages coalesces response(T1)+tool_use(T1) (same turn_id) into one
    // Message::Assistant, then appends the current user turn.
    // Expected: 4 messages.
    assert_eq!(
        messages.len(),
        4,
        "expected [prior_user, assistant{{text+tool_use}}, tool_result, current_user]; got {} messages: {:?}",
        messages.len(),
        messages
    );

    // messages[0]: prior user message
    match &messages[0] {
        Message::User { content } => {
            let text = match content.as_slice() {
                [ContentBlock::Text { text }] => text.as_str(),
                other => panic!("messages[0] content must be [Text]; got {:?}", other),
            };
            assert_eq!(text, initial_prompt, "messages[0] must carry the prior user prompt");
        }
        other => panic!("messages[0] must be User; got {:?}", other),
    }

    // messages[1]: coalesced assistant turn — Text("I'll read…") + ToolUse{Read}
    match &messages[1] {
        Message::Assistant { content } => {
            let has_text = content.iter().any(|b| matches!(b, ContentBlock::Text { .. }));
            let tool_use_block =
                content.iter().find(|b| matches!(b, ContentBlock::ToolUse { .. }));

            assert!(has_text, "messages[1] must contain a Text block; content: {:?}", content);
            assert!(
                tool_use_block.is_some(),
                "messages[1] must contain a ToolUse block; content: {:?}",
                content
            );

            if let Some(ContentBlock::ToolUse { id, name, .. }) = tool_use_block {
                assert_eq!(id, "tu-reload-1", "ToolUse id must survive the reload");
                assert_eq!(name, "Read", "ToolUse name must survive the reload");
            }
        }
        other => panic!("messages[1] must be Assistant; got {:?}", other),
    }

    // messages[2]: tool_result referencing the same tool_use_id
    match &messages[2] {
        Message::ToolResult { tool_use_id, .. } => {
            assert_eq!(
                tool_use_id, "tu-reload-1",
                "ToolResult must reference the persisted tool_use by id"
            );
        }
        other => panic!("messages[2] must be ToolResult; got {:?}", other),
    }

    // messages[3]: fresh user prompt (current turn, not from transcript)
    match &messages[3] {
        Message::User { content } => {
            let text = match content.as_slice() {
                [ContentBlock::Text { text }] => text.as_str(),
                other => panic!("messages[3] content must be [Text]; got {:?}", other),
            };
            assert_eq!(text, follow_up, "messages[3] must be the current follow-up prompt");
        }
        other => panic!("messages[3] must be User; got {:?}", other),
    }
}
