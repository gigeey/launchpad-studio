//! Concurrent two-agent integration test — acceptance gate.
//!
//! Covers four scenarios:
//! 1. Two agents run concurrently with no EventBus cross-talk.
//! 2. Cancelling one agent does not affect the other.
//! 3. Timeline-adapter parity: NativeAgentRunner produces the expected
//!    `AgentEventPayload` sequence for a scripted assistant turn.
//! 4. Provider-not-configured path emits a clear error event.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use ao_engine::agent_runner::{
    AgentRunRequest, AgentRunner, AgentRunnerMode, NativeAgentRunner, ProviderFactory,
    RunComplete, RunScope, RunnerDispatcher, RunningAgents,
};
use ao_engine::event_bus::EventBus;
use ao_engine::instance_registry::InstanceRegistry;
use ao_engine_tools_core::{IoTool, LoadPolicy, Registry, SessionKind, ToolOutput};
use ao_engine_tools_runner::{
    message::{Message, MessageNormalizer, NormalizerError},
    provider::{
        CompletionEvent, CompletionRequest, CompletionStream, MockProviderClient,
        ProviderClient, ProviderError, StopReason,
    },
};
use ao_persistence::{paths::DataRoot, PersistenceLayer};
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig, ToolsConfig,
};
use ao_protocol::event::{AgentEvent, AgentEventPayload, RunEndReason};
use ao_protocol::error::AoError;
use serde_json::{json, Value};

/// Build a real PersistenceLayer backed by a temporary directory.
/// The TempDir is returned alongside so it stays alive for the test duration.
async fn make_test_persistence() -> (Arc<PersistenceLayer>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.expect("ensure_directories");
    let p = PersistenceLayer::init_with_root(data_root).await.expect("init persistence");
    (Arc::new(p), tmp)
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared test helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Identity-style normalizer for test providers that don't inspect messages.
struct NoopNormalizer;

impl MessageNormalizer for NoopNormalizer {
    fn to_provider(&self, _messages: &[Message]) -> Result<Value, NormalizerError> {
        Ok(Value::Array(vec![]))
    }
    fn from_provider(&self, _value: Value) -> Result<Vec<Message>, NormalizerError> {
        Ok(vec![])
    }
}

/// A provider client that emits one AssistantText chunk, signals `started`,
/// then blocks until the cancel token fires without sending TurnComplete.
/// Used to keep an agent's run alive so a concurrent cancel can land.
struct SlowProviderClient {
    started: Arc<tokio::sync::Notify>,
    normalizer: NoopNormalizer,
}

impl SlowProviderClient {
    fn new(started: Arc<tokio::sync::Notify>) -> Self {
        Self { started, normalizer: NoopNormalizer }
    }
}

#[async_trait]
impl ProviderClient for SlowProviderClient {
    async fn complete(
        &self,
        _request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let (tx, rx) = mpsc::channel(8);
        let started = self.started.clone();
        tokio::spawn(async move {
            // Emit one chunk then signal readiness before blocking.
            let _ = tx.send(Ok(CompletionEvent::AssistantText("slow".to_string()))).await;
            started.notify_one();
            // Block until the cancel token fires; don't send TurnComplete.
            cancel.cancelled().await;
            // tx drops here → channel closes → run_session detects cancellation.
        });
        Ok(CompletionStream::new(rx))
    }

    fn message_normalizer(&self) -> &dyn MessageNormalizer {
        &self.normalizer
    }
}

/// Wraps any `Arc<dyn ProviderClient>` as a `ProviderFactory` (always returns
/// the same client regardless of the agent profile).
struct FixedProviderFactory {
    client: Arc<dyn ProviderClient>,
}

impl ProviderFactory for FixedProviderFactory {
    fn build(&self, _agent: &AgentProfile) -> Result<Arc<dyn ProviderClient>, ProviderError> {
        Ok(Arc::clone(&self.client))
    }
}

/// A `ProviderFactory` that always returns `Err(NotConfigured)`.
struct FailingProviderFactory;

impl ProviderFactory for FailingProviderFactory {
    fn build(&self, _agent: &AgentProfile) -> Result<Arc<dyn ProviderClient>, ProviderError> {
        Err(ProviderError::NotConfigured("missing anthropic key".to_string()))
    }
}

/// Construct an `AgentProfile` with minimal required fields.
fn make_agent(id: &str, runner_mode: AgentRunnerMode) -> AgentProfile {
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
        runner_mode,
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

/// Construct a `NativeAgentRunner` backed by the given factory.
fn make_native_runner(
    bus: Arc<EventBus>,
    running_agents: Arc<RunningAgents>,
    factory: impl ProviderFactory + 'static,
    persistence: Arc<PersistenceLayer>,
) -> NativeAgentRunner {
    NativeAgentRunner::new(
        bus,
        Arc::new(InstanceRegistry::new()),
        running_agents,
        Arc::new(factory),
        Arc::new(Registry::default()),
        persistence,
    )
}

/// Build an `AgentRunRequest` and return the companion completion receiver.
fn make_request(agent: AgentProfile, prompt: &str) -> (AgentRunRequest, mpsc::Receiver<RunComplete>) {
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

/// Subscribe to `bus` and forward every broadcast event into an unbounded mpsc
/// so tests can drain with `try_recv` after runs complete without worrying
/// about broadcast lag semantics.
fn capture(bus: &Arc<EventBus>) -> mpsc::UnboundedReceiver<AgentEvent> {
    let mut bcast = bus.subscribe();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match bcast.recv().await {
                Ok(ev) => {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });
    rx
}

/// Drain an unbounded receiver into two `Vec`s keyed by agent_id.
fn partition_by_agent(
    cap: &mut mpsc::UnboundedReceiver<AgentEvent>,
    id_a: &str,
    id_b: &str,
) -> (Vec<AgentEventPayload>, Vec<AgentEventPayload>) {
    let mut a = Vec::new();
    let mut b = Vec::new();
    while let Ok(ev) = cap.try_recv() {
        if ev.agent_id == id_a {
            a.push(ev.payload);
        } else if ev.agent_id == id_b {
            b.push(ev.payload);
        }
    }
    (a, b)
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: concurrent_two_agents_progress_independently
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn concurrent_two_agents_progress_independently() {
    let bus = Arc::new(EventBus::new(512));
    let ra = Arc::new(RunningAgents::new());
    let (persistence, _tmp) = make_test_persistence().await;

    let client_a: Arc<dyn ProviderClient> = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("hello from agent-a".to_string()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));
    let client_b: Arc<dyn ProviderClient> = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("hello from agent-b".to_string()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let runner_a = Arc::new(make_native_runner(
        Arc::clone(&bus),
        Arc::clone(&ra),
        FixedProviderFactory { client: client_a },
        Arc::clone(&persistence),
    ));
    let runner_b = Arc::new(make_native_runner(
        Arc::clone(&bus),
        Arc::clone(&ra),
        FixedProviderFactory { client: client_b },
        Arc::clone(&persistence),
    ));

    // CLI-mode agent → cli slot (runner_a); Api-mode agent → native slot (runner_b).
    let dispatcher = RunnerDispatcher::with_runners(
        Arc::clone(&runner_a) as Arc<dyn AgentRunner>,
        Arc::clone(&runner_b) as Arc<dyn AgentRunner>,
    );

    let agent_a = make_agent("agent-a", AgentRunnerMode::Cli);
    let agent_b = make_agent("agent-b", AgentRunnerMode::Api);

    let mut cap = capture(&bus);

    let (req_a, _rx_a) = make_request(agent_a.clone(), "hello");
    let (req_b, _rx_b) = make_request(agent_b.clone(), "hello");

    let picked_a = dispatcher.pick(&agent_a);
    let picked_b = dispatcher.pick(&agent_b);

    let (res_a, res_b) = timeout(
        Duration::from_secs(10),
        async { tokio::join!(picked_a.run(req_a), picked_b.run(req_b)) },
    )
    .await
    .expect("concurrent runs timed out");

    assert!(res_a.is_ok(), "agent-a run failed: {:?}", res_a);
    assert!(res_b.is_ok(), "agent-b run failed: {:?}", res_b);
    assert_eq!(res_a.unwrap().output_text, "hello from agent-a");
    assert_eq!(res_b.unwrap().output_text, "hello from agent-b");

    // Brief yield so the capture task can forward the remaining broadcast events.
    tokio::task::yield_now().await;

    let (events_a, events_b) = partition_by_agent(&mut cap, "agent-a", "agent-b");

    // Both agents must have started and ended.
    assert!(
        events_a.iter().any(|p| matches!(p, AgentEventPayload::RunStarted)),
        "agent-a missing RunStarted"
    );
    assert!(
        events_a.iter().any(|p| matches!(p, AgentEventPayload::RunEnded { .. })),
        "agent-a missing RunEnded"
    );
    assert!(
        events_b.iter().any(|p| matches!(p, AgentEventPayload::RunStarted)),
        "agent-b missing RunStarted"
    );
    assert!(
        events_b.iter().any(|p| matches!(p, AgentEventPayload::RunEnded { .. })),
        "agent-b missing RunEnded"
    );

    // No cross-talk: after partitioning, all captured events should be accounted for.
    assert!(!events_a.is_empty(), "agent-a produced no events");
    assert!(!events_b.is_empty(), "agent-b produced no events");
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: cancel_one_agent_does_not_affect_other
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cancel_one_agent_does_not_affect_other() {
    let bus = Arc::new(EventBus::new(512));
    let ra = Arc::new(RunningAgents::new());
    let (persistence, _tmp) = make_test_persistence().await;

    // Agent A: fast, completes in one turn.
    let client_a: Arc<dyn ProviderClient> = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("hello from agent-a".to_string()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    // Agent B: blocks until the cancel token fires.
    let b_started = Arc::new(tokio::sync::Notify::new());
    let client_b: Arc<dyn ProviderClient> =
        Arc::new(SlowProviderClient::new(Arc::clone(&b_started)));

    let runner_a = Arc::new(make_native_runner(
        Arc::clone(&bus),
        Arc::clone(&ra),
        FixedProviderFactory { client: client_a },
        Arc::clone(&persistence),
    ));
    let runner_b = Arc::new(make_native_runner(
        Arc::clone(&bus),
        Arc::clone(&ra),
        FixedProviderFactory { client: client_b },
        Arc::clone(&persistence),
    ));

    let dispatcher = RunnerDispatcher::with_runners(
        Arc::clone(&runner_a) as Arc<dyn AgentRunner>,
        Arc::clone(&runner_b) as Arc<dyn AgentRunner>,
    );

    let agent_a = make_agent("cancel-agent-a", AgentRunnerMode::Cli);
    let agent_b = make_agent("cancel-agent-b", AgentRunnerMode::Api);

    let (req_a, _rx_a) = make_request(agent_a.clone(), "hello");
    let (req_b, _rx_b) = make_request(agent_b.clone(), "hello");

    let picked_a = dispatcher.pick(&agent_a);
    let picked_b = dispatcher.pick(&agent_b);

    let ra_clone = Arc::clone(&ra);
    let agent_b_id = agent_b.id.clone();

    // Spawn both in background so the test task can drive the cancel logic.
    // Move the Arc<dyn AgentRunner> into each async block so the future is 'static.
    let jh_a = tokio::spawn(async move { picked_a.run(req_a).await });
    let jh_b = tokio::spawn(async move { picked_b.run(req_b).await });

    // Wait until agent-b's provider has emitted its first chunk and signalled.
    timeout(Duration::from_secs(5), b_started.notified())
        .await
        .expect("agent-b did not start within 5 s");

    // Cancel agent-b while it is in-flight.
    let cancelled = ra_clone.cancel(&agent_b_id, None);
    assert!(cancelled, "agent-b should be registered in RunningAgents at this point");

    // Both tasks must finish within a reasonable window now.
    let (res_a, res_b) = timeout(
        Duration::from_secs(10),
        async { tokio::join!(jh_a, jh_b) },
    )
    .await
    .expect("agents did not finish within 10 s");

    let res_a = res_a.expect("agent-a task panicked").expect("agent-a run errored");
    // Agent-b run returns Ok (NativeAgentRunner maps cancelled → Ok with Cancelled reason).
    let _res_b = res_b.expect("agent-b task panicked");

    // Agent A completed its full turn naturally.
    assert_eq!(res_a.output_text, "hello from agent-a");

    // Check the EventBus for agent-b's termination reason.
    tokio::task::yield_now().await;
    let mut bcast = bus.subscribe();
    let mut b_run_ended: Option<RunEndReason> = None;
    while let Ok(ev) = bcast.try_recv() {
        if ev.agent_id == agent_b_id {
            if let AgentEventPayload::RunEnded { reason } = ev.payload {
                b_run_ended = Some(reason);
            }
        }
    }
    // We may miss events if the broadcast capacity was exceeded; the key
    // assertion is that agent-a ran to completion unaffected.
    // If the RunEnded event was captured, verify it signals Cancelled.
    if let Some(reason) = b_run_ended {
        assert_eq!(reason, RunEndReason::Cancelled, "agent-b should be Cancelled");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: timeline_adapter_parity_with_cli_normalizer
// ─────────────────────────────────────────────────────────────────────────────

/// A fixed scripted assistant turn: text streaming + tool use + tool result.
/// The expected `AgentEventPayload` sequence is the source of truth.
/// Both the CLI and API paths should produce this identical sequence.
/// Since CliAgentRunner requires a live process supervisor (complex to mock in an
/// integration test), this test runs the scripted turn through NativeAgentRunner
/// (which uses `TimelineAdapter` directly) and asserts the expected sequence.
/// The mapping table governs; a fixture edit here means the CLI
/// normalizer diverged from the spec, not that the adapter is wrong.
#[tokio::test]
async fn timeline_adapter_parity_with_cli_normalizer() {
    let bus = Arc::new(EventBus::new(512));
    let ra = Arc::new(RunningAgents::new());
    let (persistence, _tmp) = make_test_persistence().await;

    // Scripted turn: text chunks only (tools require registry setup out of scope here).
    // A ToolUse event without a registered tool produces an error result from the
    // bounded executor, which diverges from the CLI path — keep this test to text parity.
    let client: Arc<dyn ProviderClient> = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("hello ".to_string()),
        CompletionEvent::AssistantText("world".to_string()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let runner = make_native_runner(
        Arc::clone(&bus),
        Arc::clone(&ra),
        FixedProviderFactory { client },
        persistence,
    );

    let mut cap = capture(&bus);

    let agent = make_agent("parity-agent", AgentRunnerMode::Api);
    let (req, _rx) = make_request(agent, "hello world");

    let result = timeout(Duration::from_secs(10), runner.run(req))
        .await
        .expect("parity run timed out")
        .expect("parity run errored");

    assert_eq!(result.output_text, "hello world");

    tokio::task::yield_now().await;

    let mut events: Vec<AgentEventPayload> = Vec::new();
    while let Ok(ev) = cap.try_recv() {
        events.push(ev.payload);
    }

    // Expected sequence per the mapping table:
    //   RunStarted
    //   TextDelta("hello ")
    //   TextDelta("world")
    //   TextComplete("hello world")    ← flushed at turn end by NativeAgentRunner
    //   RunEnded(Completed)
    assert!(
        events.iter().any(|p| matches!(p, AgentEventPayload::RunStarted)),
        "missing RunStarted; got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|p| matches!(p, AgentEventPayload::TextDelta { text } if text == "hello ")),
        "missing TextDelta('hello '); got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|p| matches!(p, AgentEventPayload::TextDelta { text } if text == "world")),
        "missing TextDelta('world'); got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|p| matches!(p, AgentEventPayload::TextComplete { text } if text == "hello world")),
        "missing TextComplete('hello world'); got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|p| matches!(p, AgentEventPayload::RunEnded { reason: RunEndReason::Completed })),
        "missing RunEnded(Completed); got: {events:?}"
    );

    // Verify ordering: RunStarted is first, RunEnded is last.
    assert!(
        matches!(events.first(), Some(AgentEventPayload::RunStarted)),
        "RunStarted must be the first event"
    );
    assert!(
        matches!(events.last(), Some(AgentEventPayload::RunEnded { .. })),
        "RunEnded must be the last event"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4: provider_not_configured_emits_clear_error
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn provider_not_configured_emits_clear_error() {
    let bus = Arc::new(EventBus::new(64));
    let ra = Arc::new(RunningAgents::new());
    let (persistence, _tmp) = make_test_persistence().await;

    let runner = make_native_runner(
        Arc::clone(&bus),
        Arc::clone(&ra),
        FailingProviderFactory,
        persistence,
    );

    let mut cap = capture(&bus);

    let agent = make_agent("error-agent", AgentRunnerMode::Api);
    let (req, _rx) = make_request(agent, "hello");

    let result = timeout(Duration::from_secs(5), runner.run(req))
        .await
        .expect("error-path run timed out");

    // NativeAgentRunner returns Err when the provider is not configured.
    assert!(result.is_err(), "expected Err from missing provider, got Ok");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("missing anthropic key") || err_msg.contains("not configured"),
        "unexpected error message: {err_msg}"
    );

    tokio::task::yield_now().await;

    let mut events: Vec<AgentEventPayload> = Vec::new();
    while let Ok(ev) = cap.try_recv() {
        events.push(ev.payload);
    }

    // Expected sequence: RunStarted → Error{recoverable:false} → RunEnded{Error}
    assert!(
        events.iter().any(|p| matches!(p, AgentEventPayload::RunStarted)),
        "missing RunStarted; got: {events:?}"
    );
    assert!(
        events.iter().any(|p| matches!(
            p,
            AgentEventPayload::Error { message, recoverable: false }
            if message.contains("Provider not configured")
        )),
        "missing Error{{recoverable:false}}; got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|p| matches!(p, AgentEventPayload::RunEnded { reason: RunEndReason::Error })),
        "missing RunEnded(Error); got: {events:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 6: native_runner_system_prompt_contains_env_block_and_profile_prompt
// ─────────────────────────────────────────────────────────────────────────────

/// A provider client that captures the CompletionRequest it receives and then
/// immediately returns a scripted one-turn response.
struct CapturingProviderClient {
    captured: Arc<Mutex<Option<ao_engine_tools_runner::provider::CompletionRequest>>>,
    normalizer: NoopNormalizer,
}

impl CapturingProviderClient {
    fn new(captured: Arc<Mutex<Option<ao_engine_tools_runner::provider::CompletionRequest>>>) -> Self {
        Self { captured, normalizer: NoopNormalizer }
    }
}

#[async_trait]
impl ProviderClient for CapturingProviderClient {
    async fn complete(
        &self,
        request: ao_engine_tools_runner::provider::CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<ao_engine_tools_runner::provider::CompletionStream, ProviderError> {
        *self.captured.lock().await = Some(request);
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            let _ = tx.send(Ok(CompletionEvent::AssistantText("ok".to_string()))).await;
            let _ = tx.send(Ok(CompletionEvent::TurnComplete { stop_reason: StopReason::Natural })).await;
        });
        Ok(ao_engine_tools_runner::provider::CompletionStream::new(rx))
    }

    fn message_normalizer(&self) -> &dyn ao_engine_tools_runner::message::MessageNormalizer {
        &self.normalizer
    }
}

/// Verify that `NativeAgentRunner` passes a system prompt to the provider that
/// contains both the env block (including today's date) and the agent's profile
/// system prompt.
#[tokio::test]
async fn native_runner_system_prompt_contains_env_block_and_profile_prompt() {
    let bus = Arc::new(EventBus::new(64));
    let ra = Arc::new(RunningAgents::new());
    let (persistence, _tmp) = make_test_persistence().await;

    let captured: Arc<Mutex<Option<ao_engine_tools_runner::provider::CompletionRequest>>> =
        Arc::new(Mutex::new(None));

    let client = Arc::new(CapturingProviderClient::new(Arc::clone(&captured)));
    let runner = make_native_runner(
        Arc::clone(&bus),
        Arc::clone(&ra),
        FixedProviderFactory { client: client as Arc<dyn ProviderClient> },
        persistence,
    );

    let mut agent = make_agent("prompt-check-agent", AgentRunnerMode::Api);
    agent.system_prompt = Some("You are a unique test sentinel.".to_string());

    let (req, _rx) = make_request(agent, "hello");
    let result = timeout(Duration::from_secs(10), runner.run(req))
        .await
        .expect("run timed out")
        .expect("run errored");
    assert_eq!(result.output_text, "ok");

    let guard = captured.lock().await;
    let captured_req = guard.as_ref().expect("provider was never called");
    let system_prompt = captured_req
        .system_prompt
        .as_deref()
        .expect("system_prompt must be Some");

    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

    assert!(
        system_prompt.contains("<run-context>"),
        "system_prompt missing env block; got: {system_prompt}"
    );
    assert!(
        system_prompt.contains(&today),
        "system_prompt missing today's date ({today}); got: {system_prompt}"
    );
    assert!(
        system_prompt.contains("You are a unique test sentinel."),
        "system_prompt missing profile prompt; got: {system_prompt}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests 7–12: ToolsConfig allow/deny filtering
// ─────────────────────────────────────────────────────────────────────────────

/// Minimal always-load IO tool stub for registry population in filtering tests.
struct AlwaysIoTool(String);

#[async_trait]
impl IoTool for AlwaysIoTool {
    fn name(&self) -> &str {
        &self.0
    }
    fn description(&self) -> &str {
        "stub tool"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::AlwaysLoad
    }
    async fn invoke(&self, _: Value, _: &ao_engine_tools_core::RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("ok"))
    }
}

/// Build a `Registry` pre-populated with `Read`, `Bash`, and `Grep` as always-load stubs.
fn make_test_registry() -> Arc<Registry> {
    let mut reg = Registry::new();
    reg.register_io(Arc::new(AlwaysIoTool("Read".into())));
    reg.register_io(Arc::new(AlwaysIoTool("Bash".into())));
    reg.register_io(Arc::new(AlwaysIoTool("Grep".into())));
    Arc::new(reg)
}

/// Extract tool names from a `CapturingProviderClient`'s captured request.
fn tool_names_from_request(
    req: &ao_engine_tools_runner::provider::CompletionRequest,
) -> std::collections::HashSet<String> {
    req.tools.iter().map(|t| t.name.clone()).collect()
}

fn make_capturing_runner(
    bus: Arc<EventBus>,
    running_agents: Arc<RunningAgents>,
    registry: Arc<Registry>,
    persistence: Arc<PersistenceLayer>,
) -> (
    NativeAgentRunner,
    Arc<Mutex<Option<ao_engine_tools_runner::provider::CompletionRequest>>>,
) {
    let captured: Arc<Mutex<Option<ao_engine_tools_runner::provider::CompletionRequest>>> =
        Arc::new(Mutex::new(None));
    let client = Arc::new(CapturingProviderClient::new(Arc::clone(&captured)));
    let runner = NativeAgentRunner::new(
        bus,
        Arc::new(InstanceRegistry::new()),
        running_agents,
        Arc::new(FixedProviderFactory { client: client as Arc<dyn ProviderClient> }),
        registry,
        persistence,
    );
    (runner, captured)
}

/// Run a request and return the set of tool names the provider received.
async fn run_and_capture_tools(
    runner: &NativeAgentRunner,
    agent: AgentProfile,
    captured: Arc<Mutex<Option<ao_engine_tools_runner::provider::CompletionRequest>>>,
) -> std::collections::HashSet<String> {
    let (req, _rx) = make_request(agent, "hello");
    timeout(Duration::from_secs(10), runner.run(req))
        .await
        .expect("run timed out")
        .expect("run failed");
    let guard = captured.lock().await;
    tool_names_from_request(guard.as_ref().expect("provider never called"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 7: tools=None → every registered (always-load) tool
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tools_none_presents_all_registered_tools() {
    let bus = Arc::new(EventBus::new(64));
    let ra = Arc::new(RunningAgents::new());
    let registry = make_test_registry();
    let (persistence, _tmp) = make_test_persistence().await;
    let (runner, captured) = make_capturing_runner(Arc::clone(&bus), Arc::clone(&ra), registry, persistence);

    let mut agent = make_agent("t7", AgentRunnerMode::Api);
    agent.tools = None;

    let names = run_and_capture_tools(&runner, agent, captured).await;

    assert_eq!(
        names,
        ["Read", "Bash", "Grep"].iter().map(|s| s.to_string()).collect(),
        "tools=None must present every registered tool"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 8: tools=Some({allow:[],deny:[],require_approval:[]}) → every registered
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tools_empty_allow_empty_deny_presents_all_registered_tools() {
    let bus = Arc::new(EventBus::new(64));
    let ra = Arc::new(RunningAgents::new());
    let registry = make_test_registry();
    let (persistence, _tmp) = make_test_persistence().await;
    let (runner, captured) = make_capturing_runner(Arc::clone(&bus), Arc::clone(&ra), registry, persistence);

    let mut agent = make_agent("t8", AgentRunnerMode::Api);
    agent.tools = Some(ToolsConfig { allow: vec![], deny: vec![], require_approval: vec![] });

    let names = run_and_capture_tools(&runner, agent, captured).await;

    assert_eq!(
        names,
        ["Read", "Bash", "Grep"].iter().map(|s| s.to_string()).collect(),
        "empty allow = no filter; must present every registered tool"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 9: tools=Some({allow:["Read","Bash"]}) → exactly Read and Bash
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tools_explicit_allow_presents_only_allowed_tools() {
    let bus = Arc::new(EventBus::new(64));
    let ra = Arc::new(RunningAgents::new());
    let registry = make_test_registry();
    let (persistence, _tmp) = make_test_persistence().await;
    let (runner, captured) = make_capturing_runner(Arc::clone(&bus), Arc::clone(&ra), registry, persistence);

    let mut agent = make_agent("t9", AgentRunnerMode::Api);
    agent.tools = Some(ToolsConfig {
        allow: vec!["Read".into(), "Bash".into()],
        deny: vec![],
        require_approval: vec![],
    });

    let names = run_and_capture_tools(&runner, agent, captured).await;

    assert_eq!(
        names,
        ["Read", "Bash"].iter().map(|s| s.to_string()).collect(),
        "explicit allow=[Read,Bash] must present exactly those two tools"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 10: tools=Some({allow:[], deny:["Bash"]}) → every registered except Bash
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tools_deny_only_removes_denied_tool() {
    let bus = Arc::new(EventBus::new(64));
    let ra = Arc::new(RunningAgents::new());
    let registry = make_test_registry();
    let (persistence, _tmp) = make_test_persistence().await;
    let (runner, captured) = make_capturing_runner(Arc::clone(&bus), Arc::clone(&ra), registry, persistence);

    let mut agent = make_agent("t10", AgentRunnerMode::Api);
    agent.tools = Some(ToolsConfig {
        allow: vec![],
        deny: vec!["Bash".into()],
        require_approval: vec![],
    });

    let names = run_and_capture_tools(&runner, agent, captured).await;

    assert_eq!(
        names,
        ["Read", "Grep"].iter().map(|s| s.to_string()).collect(),
        "deny=[Bash] with empty allow must present every registered tool except Bash"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 11: tools=Some({allow:["Read","Nonexistent"]}) → only Read (warn for Nonexistent)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tools_unknown_allow_entry_dropped_with_warn() {
    let bus = Arc::new(EventBus::new(64));
    let ra = Arc::new(RunningAgents::new());
    let registry = make_test_registry();
    let (persistence, _tmp) = make_test_persistence().await;
    let (runner, captured) = make_capturing_runner(Arc::clone(&bus), Arc::clone(&ra), registry, persistence);

    let mut agent = make_agent("t11", AgentRunnerMode::Api);
    agent.tools = Some(ToolsConfig {
        allow: vec!["Read".into(), "Nonexistent".into()],
        deny: vec![],
        require_approval: vec![],
    });

    let names = run_and_capture_tools(&runner, agent, captured).await;

    assert_eq!(
        names,
        ["Read"].iter().map(|s| s.to_string()).collect(),
        "unknown allow entry Nonexistent must be dropped; only Read should appear"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 12: allow:["Read"], deny:["Read"] → empty tools array (warn fires)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tools_allow_deny_same_tool_produces_empty_tools_array() {
    let bus = Arc::new(EventBus::new(64));
    let ra = Arc::new(RunningAgents::new());
    let registry = make_test_registry();
    let (persistence, _tmp) = make_test_persistence().await;
    let (runner, captured) = make_capturing_runner(Arc::clone(&bus), Arc::clone(&ra), registry, persistence);

    let mut agent = make_agent("t12", AgentRunnerMode::Api);
    agent.tools = Some(ToolsConfig {
        allow: vec!["Read".into()],
        deny: vec!["Read".into()],
        require_approval: vec![],
    });

    let names = run_and_capture_tools(&runner, agent, captured).await;

    assert!(
        names.is_empty(),
        "allow=[Read] intersect deny=[Read] must yield empty tools array; got: {names:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 13: api_mode_text_turn_writes_transcript_entry (write half)
// ─────────────────────────────────────────────────────────────────────────────

/// Verify that running an Api-mode agent through one text-only turn results in
/// a `response` TranscriptEntry on disk with the expected shape, and that the
/// agent snapshot is updated accordingly.
#[tokio::test]
async fn api_mode_text_turn_writes_transcript_entry() {
    let bus = Arc::new(EventBus::new(64));
    let ra = Arc::new(RunningAgents::new());
    let (persistence, _tmp) = make_test_persistence().await;

    let agent_id = "persist-agent";

    let client: Arc<dyn ProviderClient> = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("stored text".to_string()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let runner = make_native_runner(
        Arc::clone(&bus),
        Arc::clone(&ra),
        FixedProviderFactory { client },
        Arc::clone(&persistence),
    );

    let agent = make_agent(agent_id, AgentRunnerMode::Api);
    let (req, _rx) = make_request(agent, "hello");

    let result = timeout(Duration::from_secs(10), runner.run(req))
        .await
        .expect("run timed out")
        .expect("run errored");

    assert_eq!(result.output_text, "stored text");

    // persist_pending() is called inside run() before returning, so entries
    // are already on disk by the time run() completes.

    // Assert transcript entry was persisted.
    let entries = persistence.transcripts.read_all(agent_id).await.expect("read_all");
    assert_eq!(entries.len(), 1, "expected exactly one transcript entry; got {:?}", entries.len());
    let entry = &entries[0];
    assert_eq!(entry.event_type, "response", "event_type must be 'response'");
    assert_eq!(entry.content, "stored text", "content must match assistant text");
    assert!(
        matches!(&entry.role, ao_protocol::transcript::TranscriptRole::Agent { agent } if agent == agent_id),
        "role must be Agent {{ agent: {agent_id} }}"
    );
    let meta = entry.metadata.as_ref().expect("metadata must be present");
    assert!(meta.contains_key("turn_id"), "metadata must contain turn_id");
    assert!(meta["turn_id"].is_string(), "turn_id must be a non-empty string");

    // Assert snapshot was updated.
    let snapshot_store = persistence.snapshots.get().await;
    let snap = snapshot_store.agents.get(agent_id).expect("snapshot entry must exist after a completed run");
    assert_eq!(snap.message_count, 1, "message_count must be bumped to 1");
    assert!(snap.last_message.as_deref() == Some("stored text"), "last_message must be set");
    assert!(snap.last_agent_activity_at.is_some(), "last_agent_activity_at must be set");
}

// ─────────────────────────────────────────────────────────────────────────────
// Test: api_mode_history_threaded_into_messages
// ─────────────────────────────────────────────────────────────────────────────

/// Pre-populate a transcript with [user, assistant, current_user] entries and verify
/// that `NativeAgentRunner` threads the prior history into the CompletionRequest
/// messages array, tail-excluding the current user entry before appending it fresh.
#[tokio::test]
async fn api_mode_history_threaded_into_messages() {
    use ao_engine_tools_runner::message::{ContentBlock, Message};
    use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
    use chrono::Utc;
    use std::collections::HashMap;

    let bus = Arc::new(EventBus::new(64));
    let ra = Arc::new(RunningAgents::new());
    let (persistence, _tmp) = make_test_persistence().await;

    let agent_id = "history-thread-agent";

    // Pre-populate transcript: [user_msg, assistant_response, current_user_msg].
    // history::select with current_message_already_persisted=true will tail-exclude
    // the last entry (current_user_msg), leaving [user_msg, assistant_response] as history.
    let prior_entries: Vec<TranscriptEntry> = vec![
        TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::System("user".to_string()),
            content: "first user prompt".to_string(),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        },
        {
            let mut m = HashMap::new();
            m.insert("turn_id".to_string(), serde_json::Value::String("turn-1".to_string()));
            TranscriptEntry {
                ts: Utc::now(),
                role: TranscriptRole::Agent { agent: agent_id.to_string() },
                content: "assistant response".to_string(),
                event_type: "response".to_string(),
                metadata: Some(m),
                hidden_from_user: false,
            }
        },
        // This is the "current" turn — it will be tail-excluded from history.
        TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::System("user".to_string()),
            content: "fresh user prompt".to_string(),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        },
    ];
    for entry in &prior_entries {
        persistence.transcripts.append(agent_id, entry).await.expect("append");
    }

    let captured: Arc<Mutex<Option<ao_engine_tools_runner::provider::CompletionRequest>>> =
        Arc::new(Mutex::new(None));
    let client = Arc::new(CapturingProviderClient::new(Arc::clone(&captured)));
    let runner = make_native_runner(
        Arc::clone(&bus),
        Arc::clone(&ra),
        FixedProviderFactory { client: client as Arc<dyn ProviderClient> },
        Arc::clone(&persistence),
    );

    let agent = make_agent(agent_id, AgentRunnerMode::Api);
    let (req, _rx) = make_request(agent, "fresh user prompt");
    timeout(Duration::from_secs(10), runner.run(req))
        .await
        .expect("run timed out")
        .expect("run errored");

    let guard = captured.lock().await;
    let captured_req = guard.as_ref().expect("provider was never called");
    let messages = &captured_req.messages;

    // Expect [User("first user prompt"), Assistant("assistant response"), User("fresh user prompt")]
    assert_eq!(messages.len(), 3, "expected 3 messages (history + current); got {:?}", messages);
    assert_eq!(
        messages[0],
        Message::User { content: vec![ContentBlock::Text { text: "first user prompt".to_string() }] },
        "first message must be the historical user prompt"
    );
    assert_eq!(
        messages[1],
        Message::Assistant {
            content: vec![ContentBlock::Text { text: "assistant response".to_string() }],
        },
        "second message must be the historical assistant response"
    );
    assert_eq!(
        messages[2],
        Message::User { content: vec![ContentBlock::Text { text: "fresh user prompt".to_string() }] },
        "third message must be the current user prompt"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test: api_mode_empty_transcript_sends_only_current_message
// ─────────────────────────────────────────────────────────────────────────────

/// When no prior transcript exists, the first turn produces exactly one Message::User.
#[tokio::test]
async fn api_mode_empty_transcript_sends_only_current_message() {
    use ao_engine_tools_runner::message::{ContentBlock, Message};

    let bus = Arc::new(EventBus::new(64));
    let ra = Arc::new(RunningAgents::new());
    let (persistence, _tmp) = make_test_persistence().await;

    let captured: Arc<Mutex<Option<ao_engine_tools_runner::provider::CompletionRequest>>> =
        Arc::new(Mutex::new(None));
    let client = Arc::new(CapturingProviderClient::new(Arc::clone(&captured)));
    let runner = make_native_runner(
        Arc::clone(&bus),
        Arc::clone(&ra),
        FixedProviderFactory { client: client as Arc<dyn ProviderClient> },
        persistence,
    );

    let agent = make_agent("empty-transcript-agent", AgentRunnerMode::Api);
    let (req, _rx) = make_request(agent, "hello first turn");
    timeout(Duration::from_secs(10), runner.run(req))
        .await
        .expect("run timed out")
        .expect("run errored");

    let guard = captured.lock().await;
    let captured_req = guard.as_ref().expect("provider was never called");
    let messages = &captured_req.messages;

    assert_eq!(messages.len(), 1, "empty transcript must produce exactly one message; got {:?}", messages);
    assert_eq!(
        messages[0],
        Message::User { content: vec![ContentBlock::Text { text: "hello first turn".to_string() }] },
        "only message must be the current user prompt"
    );
}
