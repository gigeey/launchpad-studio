//! Unit tests for the agent-watch subsystem.
//!
//! Declared from `agent_watch.rs` as `#[cfg(test)] mod tests;` — `tests.rs` is
//! the same module as the inline `mod tests` block it replaces, so private
//! items of `agent_watch` remain in scope here via `use super::*`.
//!
//! The shared `ScriptedDetector` / `ScriptedAuthoringDetector` fakes stay in
//! `agent_watch.rs` rather than moving here: they are `pub(crate)` and
//! `schedule_runner`'s tests construct them by that path.

use super::*;
use crate::agent_runner::{AgentRunner, RunComplete};

use std::collections::HashMap;

use async_trait::async_trait as async_trait_attr;
use tokio::sync::{broadcast, mpsc};

use ao_persistence::paths::DataRoot;
use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use ao_protocol::assignment::{AssignmentThreadPolicy, AssignmentTrigger, OutputMode};
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEvent;
use ao_protocol::message::QueuedMessage;
use ao_protocol::watch_contract::{ChangeSpec, FieldSpec, IdentitySpec, PredicateSpec, WatchMode, WatchSource};
use chrono::Utc;

// ---------------------------------------------------------------------------
// Test helpers — mirrors assignment_runner.rs's own test harness.
// ---------------------------------------------------------------------------

struct RecordingDispatcher {
    tx: mpsc::Sender<(String, QueuedMessage)>,
}

#[async_trait_attr]
impl NotificationDispatcher for RecordingDispatcher {
    async fn submit_to_agent(&self, agent_id: &str, message: QueuedMessage) -> Result<(), AoError> {
        self.tx
            .send((agent_id.to_string(), message))
            .await
            .map_err(|e| AoError::Internal(format!("recording dispatcher send error: {e}")))?;
        Ok(())
    }
}

struct FailingDispatcher;

#[async_trait_attr]
impl NotificationDispatcher for FailingDispatcher {
    async fn submit_to_agent(&self, _agent_id: &str, _message: QueuedMessage) -> Result<(), AoError> {
        Err(AoError::Internal("dispatch deliberately failed for test".to_string()))
    }
}

async fn make_persistence() -> (tempfile::TempDir, Arc<PersistenceLayer>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_root = DataRoot::new(tmp.path());
    let layer = PersistenceLayer::init_with_root(data_root).await.expect("init persistence");
    (tmp, Arc::new(layer))
}

fn make_agent(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Agent {id}"),
        description: String::new(),
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
            no_output_timeout_ms: 30_000,
            file_capabilities: None,
        }),
        model: None,
        skills: vec![],
        system_prompt: None,
        tools: None,
        env: HashMap::new(),
        max_instances: 2,
        timeout_seconds: 60,
        working_dir: None,
        home_dir: None,
        serialize: true,
        workflows: None,
        template: None,
        runner_mode: Default::default(),
        native_provider: None,
        thinking: None,
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
            max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
        max_turns: None,
}
}

fn agent_watch_assignment(id: &str, agent_id: &str) -> Assignment {
    let now = Utc::now();
    Assignment {
        id: id.to_string(),
        agent_id: agent_id.to_string(),
        name: "New finance email watcher".to_string(),
        instruction: "Summarize the new finance email.".to_string(),
        working_directory: None,
        trigger: AssignmentTrigger::AgentWatch {
            instruction: "Check my inbox for a new email from finance".to_string(),
            poll_interval_secs: 300,
            connector_scope: None,
            contract: None,
            extraction: None,
            extraction_tool: None,
            extraction_args: None,
            extraction_output_schema_declared: false,
        },
        bindings: vec![],
        output_mode: OutputMode::Background,
        thread_policy: AssignmentThreadPolicy::default(),
        dedicated_thread_id: None,
        enabled: true,
        expires_at: None,
        next_fire_at: Some(now),
        last_run_at: None,
        last_event_cursor: None,
        liveness: ao_protocol::assignment::LivenessState::default(),
        created_ts: now,
        updated_ts: now,
    }
}

fn candidate(id: &str) -> AgentWatchCandidate {
    AgentWatchCandidate {
        id: id.to_string(),
        summary: format!("New item {id}"),
        payload: serde_json::json!({ "id": id }),
    }
}

/// Like [`candidate`], but with a caller-chosen payload — used by the
/// contract-bound tests below, which need to drive `predicate`/`change`
/// evaluation off fields other than `id` (e.g. a `tag`).
fn candidate_with_payload(id: &str, payload: serde_json::Value) -> AgentWatchCandidate {
    AgentWatchCandidate { id: id.to_string(), summary: format!("New item {id}"), payload }
}

fn make_recording_dispatcher() -> (Arc<dyn NotificationDispatcher>, mpsc::Receiver<(String, QueuedMessage)>) {
    let (tx, rx) = mpsc::channel(16);
    (Arc::new(RecordingDispatcher { tx }) as Arc<dyn NotificationDispatcher>, rx)
}

/// Drains every event currently queued on `rx` and counts how many were
/// [`AgentEventPayload::SystemMessage`] — the health-event channel the
/// truncation latch tests assert on across multiple ticks.
fn drain_system_message_count(rx: &mut broadcast::Receiver<AgentEvent>) -> usize {
    let mut count = 0;
    while let Ok(event) = rx.try_recv() {
        if matches!(event.payload, AgentEventPayload::SystemMessage { .. }) {
            count += 1;
        }
    }
    count
}

/// Like [`drain_system_message_count`], but returns the text of every
/// [`AgentEventPayload::SystemMessage`] drained — used by tests that need
/// to tell which health event fired (e.g. a truncation warning vs. a
/// contract-amendment re-seed notice) rather than merely counting them.
fn drain_system_message_texts(rx: &mut broadcast::Receiver<AgentEvent>) -> Vec<String> {
    drain_system_messages(rx).into_iter().map(|(text, _)| text).collect()
}

/// Like [`drain_system_message_texts`], but keeps each message's
/// [`SystemMessageSeverity`] alongside its text — used by the authoring
/// convergence/retry/freeze tests, which assert on tone as well as
/// content.
fn drain_system_messages(rx: &mut broadcast::Receiver<AgentEvent>) -> Vec<(String, Option<SystemMessageSeverity>)> {
    let mut messages = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEventPayload::SystemMessage { text, severity } = event.payload {
            messages.push((text, severity));
        }
    }
    messages
}

/// Attaches `contract` to an otherwise-default [`agent_watch_assignment`]
/// — every test below exercising [`run_contract_bound_tick`] starts here.
fn agent_watch_assignment_with_contract(id: &str, agent_id: &str, contract: WatchContract) -> Assignment {
    let mut assignment = agent_watch_assignment(id, agent_id);
    if let AssignmentTrigger::AgentWatch { contract: slot, .. } = &mut assignment.trigger {
        *slot = Some(contract);
    }
    assignment
}

/// Like [`agent_watch_assignment_with_contract`], but additionally binds
/// an [`ExtractionPlan`] (and the `connector_scope`/`extraction_tool` the
/// deterministic-extraction tests below need to key their
/// `payload_stash` fixtures) — every test exercising
/// `select_agent_watch_candidates`'s deterministic/probabilistic branch
/// starts here.
fn agent_watch_assignment_with_extraction(
    id: &str,
    agent_id: &str,
    contract: WatchContract,
    server: &str,
    tool: &str,
    plan: ExtractionPlan,
    output_schema_declared: bool,
) -> Assignment {
    let mut assignment = agent_watch_assignment_with_contract(id, agent_id, contract);
    if let AssignmentTrigger::AgentWatch {
        connector_scope,
        extraction,
        extraction_tool,
        extraction_output_schema_declared,
        ..
    } = &mut assignment.trigger
    {
        *connector_scope = Some(server.to_string());
        *extraction = Some(plan);
        *extraction_tool = Some(tool.to_string());
        *extraction_output_schema_declared = output_schema_declared;
    }
    assignment
}

/// Like [`agent_watch_assignment_with_extraction`], but additionally
/// freezes `extraction_args` — the direct-invoke tests below need this
/// set to reach `resolve_with_plan`'s direct-invoke branch instead of
/// the plain stash cache-read every other extraction test above
/// exercises.
fn agent_watch_assignment_with_extraction_and_args(
    id: &str,
    agent_id: &str,
    contract: WatchContract,
    server: &str,
    tool: &str,
    args: serde_json::Value,
    plan: ExtractionPlan,
    output_schema_declared: bool,
) -> Assignment {
    let mut assignment =
        agent_watch_assignment_with_extraction(id, agent_id, contract, server, tool, plan, output_schema_declared);
    if let AssignmentTrigger::AgentWatch { extraction_args: args_slot, .. } = &mut assignment.trigger {
        *args_slot = Some(args);
    }
    assignment
}

/// Stashes `structured` for `(server, tool)` via the same process-wide
/// `payload_stash::global()` singleton `select_agent_watch_candidates`
/// reads from — the fixture step every deterministic-extraction test
/// below uses in place of a real MCP tool call. `args_hash` is fixed
/// per call site (the stash's `latest_for` lookup ignores it and always
/// returns the most recently recorded entry for the pair), so a second
/// call for the same `(server, tool)` with new content simply becomes
/// the new "latest" — exactly what a second poll observing updated
/// content looks like in production.
fn stash_structured_payload(server: &str, tool: &str, structured: serde_json::Value) {
    payload_stash::global().record(payload_stash::StashedPayload {
        server: server.to_string(),
        tool: tool.to_string(),
        args: serde_json::json!({}),
        args_hash: "test-args".to_string(),
        captured_at: Utc::now(),
        structured: Some(structured),
        text: None,
    });
}

/// Like [`stash_structured_payload`], but for the servers that never
/// populate `structuredContent` at all and only ever return a plain text
/// content block — `structured: None`, `text: Some(raw)`. The text-only
/// direct-invoke/deterministic-path tests below use this in place of a
/// real MCP tool call whose server happens to `JSON.stringify(...)` its
/// response into text instead of setting `structuredContent`.
fn stash_text_payload(server: &str, tool: &str, raw: &str) {
    payload_stash::global().record(payload_stash::StashedPayload {
        server: server.to_string(),
        tool: tool.to_string(),
        args: serde_json::json!({}),
        args_hash: "test-args".to_string(),
        captured_at: Utc::now(),
        structured: None,
        text: Some(raw.to_string()),
    });
}

/// One scripted outcome for [`FakeConnectorTool::invoke`] — mirrors the
/// three shapes a real MCP call can come back as: content worth stashing
/// (`Stash`), a call that succeeds but leaves nothing extractable
/// (`NoStash`, matching `mcp_result_to_tool_output`'s own "only stash
/// when there's something to stash" guard), and a tool-level error
/// (`ToolError`, matching how `McpToolAdapter::invoke` represents an MCP
/// `isError` result or a recovered transport failure as `Ok(ToolOutput::Error)`
/// rather than `Result::Err`).
enum FakeConnectorOutcome {
    Stash(serde_json::Value),
    NoStash,
    ToolError(String),
}

/// Test-only [`IoTool`] double standing in for a real MCP connector tool,
/// registered under its `mcp__{server}__{tool}` qualified name exactly
/// like [`McpToolAdapter`](ao_engine_tools_runner::mcp::adapter::McpToolAdapter)
/// would be. Each `invoke()` call pops the next scripted
/// [`FakeConnectorOutcome`] and, for `Stash`, records a stash entry keyed
/// by the exact call args — exactly what `mcp_result_to_tool_output`
/// does for a real MCP call — so `direct_invoke_payload`'s exact-key
/// stash readback has something real to find. `call_count` lets a test
/// prove the connector was actually invoked a given number of times,
/// the difference between a genuine per-poll fetch and a cache replay
/// that never calls out at all.
struct FakeConnectorTool {
    qualified_name: String,
    server: String,
    raw_tool: String,
    responses: std::sync::Mutex<std::collections::VecDeque<FakeConnectorOutcome>>,
    call_count: std::sync::atomic::AtomicUsize,
}

impl FakeConnectorTool {
    fn new(server: &str, raw_tool: &str, responses: Vec<FakeConnectorOutcome>) -> Self {
        Self {
            qualified_name: format!("mcp__{server}__{raw_tool}"),
            server: server.to_string(),
            raw_tool: raw_tool.to_string(),
            responses: std::sync::Mutex::new(responses.into_iter().collect()),
            call_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.call_count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait_attr]
impl ao_engine_tools_core::tool::IoTool for FakeConnectorTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        "test-only connector tool double for agent-watch direct-invoke tests"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({ "type": "object" })
    }

    async fn invoke(&self, input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let outcome = self
            .responses
            .lock()
            .expect("FakeConnectorTool mutex poisoned")
            .pop_front()
            .expect("FakeConnectorTool.invoke called more times than scripted");
        match outcome {
            FakeConnectorOutcome::Stash(structured) => {
                payload_stash::global().record(payload_stash::StashedPayload {
                    server: self.server.clone(),
                    tool: self.raw_tool.clone(),
                    args: input.clone(),
                    args_hash: payload_stash::hash_args(&input),
                    captured_at: Utc::now(),
                    structured: Some(structured),
                    text: None,
                });
                Ok(ToolOutput::text("ok"))
            }
            FakeConnectorOutcome::NoStash => Ok(ToolOutput::text("no extractable content")),
            FakeConnectorOutcome::ToolError(message) => Ok(ToolOutput::error(message, true)),
        }
    }
}

/// Wraps `tool` in a fresh [`Registry`] under its own qualified name,
/// the same shape `AppState::tools_registry` presents to
/// `select_agent_watch_candidates` in production.
fn registry_with_tool(tool: Arc<FakeConnectorTool>) -> Arc<Registry> {
    let mut registry = Registry::new();
    registry.register_io(tool);
    Arc::new(registry)
}

/// Minimal deterministic-tier [`ExtractionPlan`] fixture: selects every
/// element of the `items` array from structured content, identifies each
/// by its own `id` field (matching [`dedup_contract`]'s `NativeId`
/// strategy, so the two layers agree on what "this item" means), and
/// matches whenever `id` is non-blank — mirroring [`dedup_contract`]'s
/// own `"not_empty(id)"` fixture predicate, just expressed as a typed
/// `Predicate` since `ExtractionPlan::predicate` has no legacy string
/// form to parse.
fn items_by_id_extraction_plan() -> ExtractionPlan {
    ExtractionPlan {
        selector: extractor_contract::Selector {
            kind: extractor_contract::ExtractorKind::JsonPath { path: "items".to_string() },
            expr: "items".to_string(),
        },
        identity: extractor_contract::ExtractorKind::JsonPath { path: "id".to_string() },
        predicate: extractor_contract::Predicate::NotEmpty { path: "id".to_string() },
    }
}

/// Like [`items_by_id_extraction_plan`], but for a payload that's
/// already a root-level array (an empty selector `expr` resolves to the
/// whole document, per `resolve_json_path`'s own empty-path handling) —
/// what `author_extraction_plan` produces for a `Value::Array` body,
/// which is exactly the shape a text-only `[{"id":"a"},...]` rescue
/// parses into.
fn items_at_root_extraction_plan() -> ExtractionPlan {
    ExtractionPlan {
        selector: extractor_contract::Selector {
            kind: extractor_contract::ExtractorKind::JsonPath { path: String::new() },
            expr: String::new(),
        },
        identity: extractor_contract::ExtractorKind::JsonPath { path: "id".to_string() },
        predicate: extractor_contract::Predicate::NotEmpty { path: "id".to_string() },
    }
}

/// `Hash`-kind [`ExtractionPlan`] fixture: `infer_tier` always maps a
/// `Hash` selector to `Tier::ChangeDetectionOnly` regardless of what
/// content is supplied, so a poll bound to this plan must go straight
/// to the model — used by the direct-invoke test proving that tier
/// never calls the connector even with frozen `extraction_args`.
fn change_detection_only_extraction_plan() -> ExtractionPlan {
    ExtractionPlan {
        selector: extractor_contract::Selector { kind: extractor_contract::ExtractorKind::Hash, expr: String::new() },
        identity: extractor_contract::ExtractorKind::Hash,
        predicate: extractor_contract::Predicate::NotEmpty { path: "id".to_string() },
    }
}

/// Minimal `WatchContract` fixture for the dedup tests below:
/// `identity.strategy: native_id` keyed on the candidate payload's own
/// `id` field (matching [`candidate`]/[`candidate_with_payload`]'s
/// shape), with `mode`/`predicate.expr`/`change.material_fields` left to
/// the caller since those are exactly what each test varies.
fn dedup_contract(mode: WatchMode, predicate_expr: &str, material_fields: Vec<&str>) -> WatchContract {
    WatchContract {
        contract_version: 1,
        authored_at: "2026-07-27T09:00:00Z".to_string(),
        authored_by_run: "run-1".to_string(),
        source: WatchSource { kind: "test".to_string(), ref_: "test".to_string() },
        identity: IdentitySpec {
            strategy: IdentityStrategy::NativeId,
            source_field: Some("id".to_string()),
            format: None,
            fields: vec![],
            rationale: "test fixture: native id keyed on payload.id".to_string(),
        },
        change: ChangeSpec {
            material_fields: material_fields.into_iter().map(str::to_string).collect(),
            version_hint_field: None,
        },
        predicate: PredicateSpec {
            natural_language: String::new(),
            fields: vec![],
            predicate: ao_protocol::watch_contract::legacy_expr::parse(predicate_expr)
                .expect("dedup_contract callers only ever pass valid legacy-grammar fixtures"),
        },
        mode,
        fields: HashMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests — contract-bound dedup (`run_contract_bound_tick`)
//
// These seven mirror the pre-contract suite's names and intent 1:1 — only the dedup
// mechanism each one exercises changed, from `seen_ids` string equality
// to identity/version/predicate hashing against `snapshots`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn first_poll_seeds_baseline_without_firing() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    // Both candidates already match the predicate — under the plain
    // decision table (row 1: no prev, matching -> FIRE) this would fire
    // for both. The very first poll of a brand-new watch is a deliberate
    // override of that table: seed only, so a
    // pre-existing backlog never floods the user.
    //
    // This test is the tripwire for the locked never-fire-from-history
    // policy (see the "Locked policy" section in this module's header).
    // If a change makes it fail, that change would have sent real,
    // irreversible messages for every pre-existing item — treat the
    // failure as the bug, not the assertion. Relaxing it needs a product
    // decision, not a test edit.
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-1", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a"), candidate("b")])]));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()),
        &assignment,
        "Check my inbox for a new email from finance",
        None,
    )
    .await;

    assert!(!fired, "the first-ever poll must seed a baseline, not fire — even though both items already match");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched");

    let scratchpad = persistence.assignment_scratchpads.get("watch-1").await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), 2, "both candidates must be seeded into the snapshot store");
    for snapshot in &scratchpad.snapshots {
        assert!(snapshot.predicate_value, "the seeded snapshot must reflect what was actually observed");
        assert_eq!(snapshot.edge_counter, 0, "seeding is not itself a transition");
    }
}

#[tokio::test]
async fn first_contract_bound_poll_seeds_n_matching_rows_then_a_new_row_fires_once() {
    // End-to-end regression for the demo-blocking flood: a first
    // contract-bound poll over a backlog of N already-matching rows must
    // record all N as a baseline and fire zero times, and only a
    // genuinely new row on a later poll may fire — exactly once, for
    // exactly that row.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-seed-n", "agent-1", contract);

    let seed_candidates = vec![candidate("a"), candidate("b"), candidate("c")];
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(seed_candidates.clone()),
        Ok(vec![candidate("a"), candidate("b"), candidate("c"), candidate("d")]),
    ]));

    let seeding_fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(!seeding_fired, "the first contract-bound poll over N already-matching rows must fire zero times");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched on the seeding poll");

    let scratchpad = persistence.assignment_scratchpads.get("watch-seed-n").await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), seed_candidates.len(), "all N already-matching rows must be seeded");

    let second_fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(second_fired, "a genuinely new row on the next poll must fire");

    let (_agent_id, message) = rx.try_recv().expect("exactly one message must have been dispatched");
    assert!(message.content.contains("New item d"), "got: {}", message.content);
    assert!(rx.try_recv().is_err(), "the new row must fire exactly once, not once per seeded snapshot");
}

#[tokio::test]
async fn seed_only_with_matching_candidates_emits_one_disclosure_naming_them() {
    // Same fixture as `first_poll_seeds_baseline_without_firing` — both
    // candidates already match — but asserted from the event-bus side:
    // the exclusion must come with exactly one message naming what it
    // excluded, not silence.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-seed-disclose", "agent-1", contract);

    let mut health_rx = event_bus.subscribe();

    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a"), candidate("b")])]));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired, "seeding must never fire, even though both candidates already match");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched to the agent");

    let texts = drain_system_message_texts(&mut health_rx);
    assert_eq!(texts.len(), 1, "exactly one disclosure message must be emitted for the whole seeding tick");
    assert!(texts[0].contains('2'), "message must state the count of already-matching candidates: {}", texts[0]);
    assert!(texts[0].contains("New item a"), "message must name the first matching candidate by summary");
    assert!(texts[0].contains("New item b"), "message must name the second matching candidate by summary");
    assert!(
        texts[0].to_lowercase().contains("not"),
        "message must state these items were not acted on: {}",
        texts[0]
    );
}

#[tokio::test]
async fn seed_only_with_zero_matches_emits_no_disclosure() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    // Predicate nothing observed here satisfies.
    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(never_present, 'x')", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-seed-no-match", "agent-1", contract);

    let mut health_rx = event_bus.subscribe();

    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a"), candidate("b")])]));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired);
    assert_eq!(
        drain_system_message_count(&mut health_rx),
        0,
        "a seeding poll where nothing already matches must not emit a disclosure health event"
    );
}

#[tokio::test]
async fn seed_only_disclosure_truncates_over_the_named_cap() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-seed-truncate", "agent-1", contract);

    let mut health_rx = event_bus.subscribe();

    // One more than SEED_DISCLOSURE_MAX_NAMED so exactly one candidate
    // gets folded into the "...and N more" tail.
    let overflow_candidates: Vec<AgentWatchCandidate> =
        (0..(SEED_DISCLOSURE_MAX_NAMED + 1)).map(|i| candidate(&format!("seed-item-{i}"))).collect();
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(overflow_candidates)]));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired);
    let texts = drain_system_message_texts(&mut health_rx);
    assert_eq!(texts.len(), 1, "still exactly one message even when the backlog exceeds the naming cap");
    assert!(
        texts[0].contains("...and 1 more"),
        "the one candidate beyond the cap must be folded into an '...and N more' tail, got: {}",
        texts[0]
    );
    let named_lines = texts[0].lines().filter(|l| l.trim_start().starts_with("- ")).count();
    assert_eq!(
        named_lines,
        SEED_DISCLOSURE_MAX_NAMED,
        "at most SEED_DISCLOSURE_MAX_NAMED candidates may be named individually"
    );
}

#[tokio::test]
async fn unchanged_candidates_stay_quiet_after_seeding() {
    // Decision table row 6 (yes prev / not matching / not matching -> skip).
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'vip')", vec!["tag"]);
    let assignment = agent_watch_assignment_with_contract("watch-2", "agent-1", contract);

    let not_vip = || candidate_with_payload("a", serde_json::json!({ "id": "a", "tag": "normal" }));
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![not_vip()]), // seeds baseline
        Ok(vec![not_vip()]), // same item again, still not matching — nothing new
    ]));

    let first = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let second = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!first);
    assert!(!second, "an unchanged candidate set must stay quiet");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched");

    let scratchpad = persistence.assignment_scratchpads.get("watch-2").await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), 1);
    assert!(!scratchpad.snapshots[0].predicate_value);
    assert_eq!(scratchpad.snapshots[0].edge_counter, 0);
}

#[tokio::test]
async fn new_candidate_after_seed_fires_and_persists_scratchpad() {
    // Decision table row 1 (no prev / matching -> FIRE), reached on a
    // poll *after* the seeding poll — unlike `first_poll_seeds_baseline_without_firing`,
    // this is the ordinary, non-overridden case.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-3", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate("a")]),                 // seeds baseline with "a"
        Ok(vec![candidate("a"), candidate("b")]), // "b" is new
    ]));

    let first = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let second = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!first);
    assert!(second, "a genuinely new candidate must fire");

    let (agent_id, message) = rx.try_recv().expect("a message must have been dispatched");
    assert_eq!(agent_id, "agent-1");
    assert!(message.content.contains("New item b"), "got: {}", message.content);

    let scratchpad = persistence.assignment_scratchpads.get("watch-3").await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), 2, "both a and b must have a stored snapshot");
    let b_snapshot = scratchpad.snapshots.iter().find(|s| s.payload["id"] == "b").expect("b must be snapshotted");
    assert_eq!(b_snapshot.edge_counter, 1, "b's first matching observation is a real edge");

    // The AssignmentRun row is real and visible, exactly like any other
    // trigger kind's fire.
    let run = persistence.assignment_runs.list_for_assignment("watch-3").await.unwrap();
    assert_eq!(run.len(), 1);
    assert_eq!(run[0].trigger_kind, AssignmentTriggerKind::AgentWatch);
}

#[tokio::test]
async fn multiple_new_candidates_in_one_poll_fire_exactly_once() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-4", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![]),                                // seeds an empty baseline
        Ok(vec![candidate("x"), candidate("y")]),  // both new at once
    ]));

    let first = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let second = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!first);
    assert!(second);

    // Exactly one message dispatched, covering both new items.
    let (_agent_id, message) = rx.try_recv().expect("one message must have been dispatched");
    assert!(message.content.contains("New item x"), "got: {}", message.content);
    assert!(message.content.contains("New item y"), "got: {}", message.content);
    assert!(rx.try_recv().is_err(), "must fire exactly once for a burst of new items, not once per item");

    let scratchpad = persistence.assignment_scratchpads.get("watch-4").await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), 2);
}

#[tokio::test]
async fn detector_error_stays_quiet_but_still_persists_the_model_call_it_already_spawned() {
    // Regression: `observe_via_detector` records the model call BEFORE
    // calling `detector.observe`, since the child session — and its cost
    // — is already spawned by the time this poll finds out the
    // observation itself failed. `run_agent_watch_tick` used to return
    // early on this `Err(())` without ever persisting the scratchpad, so
    // that already-incurred call silently never reached
    // `model_calls_by_day` — a real (if narrow) source of the cost
    // counter under-reporting what actually ran. Nothing else about a
    // failed tick is meant to persist (no snapshot, no fire, no streak
    // movement), only the call count.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-5", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Err(
        AgentWatchDetectError::Failed("observation failed for this test".to_string()),
    )]));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired, "a detector error must never fire");
    assert!(rx.try_recv().is_err());
    let scratchpad = persistence
        .assignment_scratchpads
        .get("watch-5")
        .await
        .unwrap()
        .expect("the spawned-but-failed model call must still reach persisted state");
    assert_eq!(model_calls_today(&scratchpad), 1);
    assert!(scratchpad.snapshots.is_empty(), "a failed tick observes nothing to snapshot");
}

#[tokio::test]
async fn dispatch_failure_records_pending_with_no_run_id_and_emits_unhealthy_event() {
    // A dispatch failure is no longer silent: the match-time write
    // persists this tick's snapshots and a `Pending` ledger entry for
    // "b" *before* `fire_assignment` is even attempted, so a failed
    // dispatch still leaves a durable, user-visible trace instead of
    // vanishing without a record.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let mut health_rx = event_bus.subscribe();
    // Deliberately no agent created — `FailingDispatcher` always errors
    // anyway, but this also confirms `fire_assignment`'s own failure
    // path (e.g. AgentNotFound) is handled the same way.
    let dispatcher: Arc<dyn NotificationDispatcher> = Arc::new(FailingDispatcher);
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let expected_identity_key = identity_key(&contract, &serde_json::json!({ "id": "b" })).unwrap();
    let assignment = agent_watch_assignment_with_contract("watch-dispatch-fail", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate("a")]),                 // seeds baseline — no dispatch attempted
        Ok(vec![candidate("a"), candidate("b")]), // "b" is new, but dispatch will fail
    ]));

    let first = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(!first, "the seeding tick never dispatches");
    // "a" already matches the predicate at seed time, so the seeding
    // tick emits its own baseline-disclosure health event — drain it
    // here so it doesn't get counted alongside the dispatch-failure
    // event this test actually asserts on.
    drain_system_message_texts(&mut health_rx);

    let second = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!second, "a fire that fails to dispatch must report false, not true");

    let scratchpad = persistence.assignment_scratchpads.get("watch-dispatch-fail").await.unwrap().unwrap();
    assert_eq!(
        scratchpad.snapshots.len(),
        2,
        "the match-time write persists this tick's snapshots even though dispatch failed"
    );
    assert_eq!(scratchpad.seen_deliveries.len(), 1, "the failed dispatch still records a ledger entry");
    let entry = &scratchpad.seen_deliveries[0];
    assert_eq!(entry.status, DeliveryStatus::Pending, "a failed dispatch is not a confirmed delivery");
    assert_eq!(entry.run_id, None, "the dispatch never got far enough to produce a run to correlate against");
    assert_eq!(entry.identity_key.as_deref(), Some(expected_identity_key.as_str()));

    let texts = drain_system_message_texts(&mut health_rx);
    assert_eq!(texts.len(), 1, "exactly one unhealthy event for the one dispatch attempt that failed");
    assert!(!texts[0].is_empty(), "the health event must carry a non-empty reason");
    assert!(texts[0].contains(&expected_identity_key), "message must name the stuck item: {}", texts[0]);
}

// ---------------------------------------------------------------------------
// Tests — two-phase delivery ledger (`reconcile_pending_deliveries`,
// `AssignmentScratchpad::record_pending_action`/`attach_dispatch_run`/
// `confirm_pending_delivery`/`clear_pending_delivery`): closing the
// silent-drop hole where `fire_assignment` returning `Ok` only means
// "enqueued," not "the turn actually ran" — and the inverse hole where a
// permanently stuck item would otherwise never fire again.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn successful_enqueue_records_pending_not_confirmed() {
    // The core of the fix: `fire_assignment` returning `Ok` only proves
    // the message reached the target agent's queue, not that the queued
    // turn ran — so the ledger entry it produces must start `Pending`,
    // correlated to the dispatched `AssignmentRun::id`, not immediately
    // `Confirmed`.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let expected_identity_key = identity_key(&contract, &serde_json::json!({ "id": "b" })).unwrap();
    let assignment = agent_watch_assignment_with_contract("watch-pending-record", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate("a")]),                 // seeds baseline
        Ok(vec![candidate("a"), candidate("b")]), // "b" is new
    ]));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let second =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(second, "a genuinely new candidate must fire");
    rx.try_recv().expect("a message must have been dispatched");

    let runs = persistence.assignment_runs.list_for_assignment("watch-pending-record").await.unwrap();
    assert_eq!(runs.len(), 1);
    let run_id = runs[0].id.clone();

    let scratchpad = persistence.assignment_scratchpads.get("watch-pending-record").await.unwrap().unwrap();
    assert_eq!(scratchpad.seen_deliveries.len(), 1, "the fired transition must record exactly one ledger entry");
    let entry = &scratchpad.seen_deliveries[0];
    assert_eq!(
        entry.status,
        DeliveryStatus::Pending,
        "a successful enqueue is not yet a confirmed delivery — it must not be marked Confirmed until the \
         dispatched turn is independently observed to complete"
    );
    assert_eq!(entry.run_id.as_deref(), Some(run_id.as_str()));
    assert_eq!(entry.identity_key.as_deref(), Some(expected_identity_key.as_str()));
}

#[tokio::test]
async fn pending_delivery_within_retry_threshold_is_not_double_dispatched() {
    // Simulates a crash (or a stuck queue) between a successful enqueue
    // and the dispatched turn ever running: the `AssignmentRun` row is
    // left in `Queued` forever. While the entry's poll count stays at or
    // under the retry threshold, it must stay Pending, must not be
    // re-dispatched, and must not yet be reported unhealthy — an
    // ordinary in-flight turn (a queue backlog, a slow model call) is
    // not itself a failure.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-pending-inflight", "agent-1", contract);

    let mut health_rx = event_bus.subscribe();

    let mut script = vec![Ok(vec![candidate("a")]), Ok(vec![candidate("a"), candidate("b")])];
    for _ in 0..PENDING_DELIVERY_RETRY_POLL_THRESHOLD {
        script.push(Ok(vec![candidate("a"), candidate("b")]));
    }
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(script));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(fired);
    rx.try_recv().expect("b's fire must have dispatched a message");
    drain_system_message_texts(&mut health_rx);

    let runs = persistence.assignment_runs.list_for_assignment("watch-pending-inflight").await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, AssignmentRunStatus::Queued);

    // Run exactly PENDING_DELIVERY_RETRY_POLL_THRESHOLD more polls — the
    // entry's poll count lands exactly at the threshold, still within it.
    for _ in 0..PENDING_DELIVERY_RETRY_POLL_THRESHOLD {
        let quiet = run_agent_watch_tick(
            &persistence,
            &dispatcher,
            &event_bus,
            &detector,
            &Arc::new(Registry::new()), &assignment, "watch", None).await;
        assert!(!quiet, "reconciling a still-in-flight pending delivery is not itself a fire");
    }

    assert!(
        rx.try_recv().is_err(),
        "a pending delivery within the retry threshold must never be double-dispatched"
    );
    assert_eq!(
        drain_system_message_count(&mut health_rx),
        0,
        "an entry still within the retry threshold is not yet unhealthy"
    );

    let scratchpad = persistence.assignment_scratchpads.get("watch-pending-inflight").await.unwrap().unwrap();
    assert_eq!(scratchpad.seen_deliveries.len(), 1, "still the single original entry, not cleared or duplicated");
    let entry = &scratchpad.seen_deliveries[0];
    assert_eq!(entry.status, DeliveryStatus::Pending);
    assert_eq!(entry.pending_poll_count, PENDING_DELIVERY_RETRY_POLL_THRESHOLD);
}

#[tokio::test]
async fn pending_delivery_past_retry_threshold_is_retried_and_reported_unhealthy() {
    // One more poll past `pending_delivery_within_retry_threshold_is_not_double_dispatched`'s
    // setup: once the entry's poll count exceeds the threshold, it must
    // no longer be left silently stuck — it is cleared (ledger entry and
    // item snapshot alike) so the very same poll's ordinary diff fires on
    // "b" again as a fresh transition, and an unhealthy health event
    // discloses that the retry happened and why.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let expected_identity_key = identity_key(&contract, &serde_json::json!({ "id": "b" })).unwrap();
    let assignment = agent_watch_assignment_with_contract("watch-pending-retry", "agent-1", contract);

    let mut health_rx = event_bus.subscribe();

    // Seed, fire, PENDING_DELIVERY_RETRY_POLL_THRESHOLD quiet polls
    // (lands exactly at the threshold), then one more poll to cross it.
    let mut script = vec![Ok(vec![candidate("a")]), Ok(vec![candidate("a"), candidate("b")])];
    for _ in 0..=PENDING_DELIVERY_RETRY_POLL_THRESHOLD {
        script.push(Ok(vec![candidate("a"), candidate("b")]));
    }
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(script));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(fired);
    rx.try_recv().expect("b's first fire must have dispatched a message");
    drain_system_message_texts(&mut health_rx);

    let first_runs = persistence.assignment_runs.list_for_assignment("watch-pending-retry").await.unwrap();
    assert_eq!(first_runs.len(), 1);
    let stuck_run_id = first_runs[0].id.clone();

    for _ in 0..PENDING_DELIVERY_RETRY_POLL_THRESHOLD {
        run_agent_watch_tick(
            &persistence,
            &dispatcher,
            &event_bus,
            &detector,
            &Arc::new(Registry::new()), &assignment, "watch", None).await;
    }
    assert!(rx.try_recv().is_err(), "still within threshold — no retry dispatch yet");

    // One more poll crosses the threshold: retry-and-refire happens
    // within this same tick's ordinary diff, since the retry clears the
    // stale snapshot before the diff loop runs.
    let retried = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(retried, "crossing the retry threshold must dispatch a fresh fire for the still-stuck item");
    rx.try_recv().expect("the retry must dispatch a second message");

    let runs_after = persistence.assignment_runs.list_for_assignment("watch-pending-retry").await.unwrap();
    assert_eq!(runs_after.len(), 2, "the retry creates a second AssignmentRun rather than reusing the stuck one");

    let scratchpad = persistence.assignment_scratchpads.get("watch-pending-retry").await.unwrap().unwrap();
    assert_eq!(scratchpad.seen_deliveries.len(), 1, "the stuck entry is replaced by a fresh one for the retry");
    let entry = &scratchpad.seen_deliveries[0];
    assert_eq!(entry.status, DeliveryStatus::Pending);
    assert_eq!(entry.pending_poll_count, 0, "a fresh retry starts its own poll count over");
    assert_ne!(entry.run_id.as_deref(), Some(stuck_run_id.as_str()), "must correlate to the new run, not the stuck one");

    let texts = drain_system_message_texts(&mut health_rx);
    assert!(
        texts.iter().any(|t| t.to_lowercase().contains("retried") && t.contains(&expected_identity_key)),
        "must disclose that the stuck item was retried, naming it: {texts:?}"
    );
}

#[tokio::test]
async fn pending_delivery_promotes_to_confirmed_once_run_completes() {
    // Mirrors what the production queue-manager pump does once the
    // runner's completion signal (`RunComplete`, or the outer
    // runner-failure watcher) lands: `mark_assignment_run_succeeded`
    // writes a terminal status onto the `AssignmentRun` row. The next
    // tick's reconciliation pass must see that and promote the ledger
    // entry — without treating the promotion itself as a new fire or an
    // unhealthy event.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-pending-confirm", "agent-1", contract);

    let mut health_rx = event_bus.subscribe();

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate("a")]),                 // seeds baseline
        Ok(vec![candidate("a"), candidate("b")]), // "b" is new — fires
        Ok(vec![candidate("a"), candidate("b")]), // quiet tick — reconciliation only
    ]));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(fired);
    rx.try_recv().expect("b's fire must have dispatched a message");
    drain_system_message_texts(&mut health_rx);

    let mut runs = persistence.assignment_runs.list_for_assignment("watch-pending-confirm").await.unwrap();
    assert_eq!(runs.len(), 1);
    let mut run = runs.remove(0);
    run.status = AssignmentRunStatus::Succeeded;
    run.finished_ts = Some(Utc::now());
    persistence.assignment_runs.update("watch-pending-confirm", &run).await.unwrap();

    let third =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(!third, "reconciliation promoting a pending entry is not itself a fire");

    let scratchpad = persistence.assignment_scratchpads.get("watch-pending-confirm").await.unwrap().unwrap();
    assert_eq!(scratchpad.seen_deliveries.len(), 1);
    let entry = &scratchpad.seen_deliveries[0];
    assert_eq!(
        entry.status,
        DeliveryStatus::Confirmed,
        "a dispatched run that reached Succeeded must promote the ledger entry"
    );
    assert_eq!(entry.run_id, None, "nothing left to reconcile once confirmed");
    assert_eq!(entry.identity_key, None);

    assert_eq!(
        drain_system_message_count(&mut health_rx),
        0,
        "promoting a delivery that completed normally must not itself emit an unhealthy health event"
    );
}

#[tokio::test]
async fn scratchpad_state_persists_across_evaluations_via_a_fresh_store_handle() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let fingerprint = contract.fingerprint();
    let assignment = agent_watch_assignment_with_contract("watch-7", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a"), candidate("b")])]));
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    // A brand-new store handle over the same data root must see the
    // same persisted scratchpad — proves this is durable, not just
    // in-process state.
    let reloaded_persistence = {
        let data_root = DataRoot::new(_tmp.path());
        Arc::new(PersistenceLayer::init_with_root(data_root).await.unwrap())
    };
    let scratchpad = reloaded_persistence
        .assignment_scratchpads
        .get("watch-7")
        .await
        .unwrap()
        .expect("scratchpad must persist across store instances");
    assert_eq!(scratchpad.snapshots.len(), 2, "both seeded candidates must persist through a fresh store handle");
    assert_eq!(scratchpad.contract_fingerprint.as_deref(), Some(fingerprint.as_str()));
}

// ---------------------------------------------------------------------------
// Tests — decision table, one per row, plus the re-entry case and the v1
// guardrails.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn decision_table_row1_brand_new_item_already_matching_fires() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'vip')", vec!["tag"]);
    let assignment = agent_watch_assignment_with_contract("watch-row1", "agent-1", contract);

    let other = candidate_with_payload("other", serde_json::json!({ "id": "other", "tag": "normal" }));
    let new_and_matching = candidate_with_payload("new-client", serde_json::json!({ "id": "new-client", "tag": "vip" }));

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![other.clone()]),           // poll 1: seeds baseline, past the first-poll override
        Ok(vec![other, new_and_matching]), // poll 2: a brand-new item, already matching
    ]));

    let first = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let second = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!first);
    assert!(second, "a brand-new item that is already matching on first sight must fire (row 1)");
    let (_agent_id, message) = rx.try_recv().expect("a message must have been dispatched");
    assert!(message.content.contains("New item new-client"), "got: {}", message.content);
}

#[tokio::test]
async fn decision_table_row2_brand_new_item_not_matching_is_snapshotted_without_firing() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'vip')", vec!["tag"]);
    let assignment = agent_watch_assignment_with_contract("watch-row2", "agent-1", contract);

    let other = candidate_with_payload("other", serde_json::json!({ "id": "other", "tag": "normal" }));
    let new_not_matching = candidate_with_payload("new-client", serde_json::json!({ "id": "new-client", "tag": "normal" }));

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![other.clone()]),
        Ok(vec![other, new_not_matching]),
    ]));

    let first = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let second = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!first);
    assert!(!second, "a brand-new item that isn't matching must not fire (row 2)");
    assert!(rx.try_recv().is_err());

    let scratchpad = persistence.assignment_scratchpads.get("watch-row2").await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), 2, "the non-matching new item must still be snapshotted");
}

#[tokio::test]
async fn decision_table_row3_existing_item_starts_matching_fires() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'vip')", vec!["tag"]);
    let assignment = agent_watch_assignment_with_contract("watch-row3", "agent-1", contract);

    let not_vip = candidate_with_payload("client-1", serde_json::json!({ "id": "client-1", "tag": "normal" }));
    let now_vip = candidate_with_payload("client-1", serde_json::json!({ "id": "client-1", "tag": "vip" }));

    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![not_vip]), Ok(vec![now_vip])]));

    let first = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let second = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!first);
    assert!(second, "an already-tracked item that starts matching must fire (row 3)");
    assert!(rx.try_recv().is_ok());
}

#[tokio::test]
async fn decision_table_row4_item_that_stays_matching_forever_fires_exactly_once() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'vip')", vec!["tag"]);
    let assignment = agent_watch_assignment_with_contract("watch-row4", "agent-1", contract);

    let not_vip = || candidate_with_payload("client-1", serde_json::json!({ "id": "client-1", "tag": "normal" }));
    let vip = || candidate_with_payload("client-1", serde_json::json!({ "id": "client-1", "tag": "vip" }));

    const REPEATED_MATCHING_POLLS: usize = 10;
    let mut responses = vec![Ok(vec![not_vip()])]; // poll 1: seeds, not yet VIP
    responses.extend((0..1 + REPEATED_MATCHING_POLLS).map(|_| Ok(vec![vip()]))); // flips to VIP, then stays VIP

    let total_polls = responses.len();
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(responses));

    let mut fire_count = 0;
    for _ in 0..total_polls {
        if run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await {
            fire_count += 1;
            // Mirror the production queue-manager pump: the dispatched
            // run reaches a terminal status shortly after firing, well
            // within the two-phase ledger's retry-poll threshold, so the
            // many remaining "still VIP" polls below confirm the
            // transition-detection invariant this test is actually about
            // rather than tripping the ledger's unrelated stuck-delivery
            // retry (a run that never advances past `Queued` for many
            // consecutive polls is *by design* now treated as stuck and
            // retried — see `PENDING_DELIVERY_RETRY_POLL_THRESHOLD`).
            let runs = persistence.assignment_runs.list_for_assignment("watch-row4").await.unwrap();
            let mut run = runs
                .into_iter()
                .find(|r| r.status == AssignmentRunStatus::Queued)
                .expect("the run this tick just dispatched");
            run.status = AssignmentRunStatus::Succeeded;
            run.finished_ts = Some(Utc::now());
            persistence.assignment_runs.update("watch-row4", &run).await.unwrap();
        }
    }

    assert_eq!(fire_count, 1, "an item that becomes and stays VIP must fire exactly once, ever — this is what the pre-contract system got wrong");
    assert!(rx.try_recv().is_ok(), "the one fire must have dispatched a message");
    assert!(rx.try_recv().is_err(), "no further messages after the single fire");

    let scratchpad = persistence.assignment_scratchpads.get("watch-row4").await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots[0].edge_counter, 1, "exactly one false->true transition across the whole run");
}

#[tokio::test]
async fn decision_table_row5_leaving_matching_state_does_not_fire() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'vip')", vec!["tag"]);
    let assignment = agent_watch_assignment_with_contract("watch-row5", "agent-1", contract);

    let not_vip = || candidate_with_payload("client-1", serde_json::json!({ "id": "client-1", "tag": "normal" }));
    let vip = || candidate_with_payload("client-1", serde_json::json!({ "id": "client-1", "tag": "vip" }));

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![not_vip()]), // seed
        Ok(vec![vip()]),     // enters matching state — fires
        Ok(vec![not_vip()]), // leaves matching state — must NOT fire (on_exit is v2)
    ]));

    let p1 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let p2 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let p3 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!p1);
    assert!(p2, "entering the matching state must fire");
    assert!(!p3, "leaving the matching state must not fire (row 5)");

    assert!(rx.try_recv().is_ok(), "exactly one message, from p2");
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn re_entering_matching_state_with_identical_data_fires_again() {
    // The re-entry case: matching -> not matching -> matching with
    // byte-identical data must fire again, via `edge_counter` folded
    // into `delivery_key` — this is what a plain "have I seen this exact
    // payload" cache would miss.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'vip')", vec!["tag"]);
    let assignment = agent_watch_assignment_with_contract("watch-reentry", "agent-1", contract);

    let not_vip = || candidate_with_payload("client-1", serde_json::json!({ "id": "client-1", "tag": "normal" }));
    let vip = || candidate_with_payload("client-1", serde_json::json!({ "id": "client-1", "tag": "vip" }));

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![not_vip()]), // seed
        Ok(vec![vip()]),     // enters — fires
        Ok(vec![not_vip()]), // leaves — no fire
        Ok(vec![vip()]),     // re-enters with identical data — must fire again
    ]));

    let p1 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let p2 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let p3 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let p4 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!p1);
    assert!(p2, "entering the matching state must fire");
    assert!(!p3, "leaving the matching state must not fire");
    assert!(p4, "re-entering the matching state with identical data must fire again");

    assert!(rx.try_recv().is_ok());
    assert!(rx.try_recv().is_ok());
    assert!(rx.try_recv().is_err(), "exactly two fires total: the first entry and the re-entry");

    let scratchpad = persistence.assignment_scratchpads.get("watch-reentry").await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), 1);
    assert_eq!(scratchpad.snapshots[0].edge_counter, 2, "two false->true transitions have now occurred");
}

// The six-row decision table above is specifically for `predicate_transition`
// These two cover the coarser
// level-triggered modes deliverable #1 also requires.

#[tokio::test]
async fn new_or_changed_mode_fires_on_material_field_change_even_without_a_transition() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::NewOrChanged, "not_empty(id)", vec!["status"]);
    let assignment = agent_watch_assignment_with_contract("watch-new-or-changed", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate_with_payload("a", serde_json::json!({ "id": "a", "status": "open" }))]), // seeds
        Ok(vec![candidate_with_payload("a", serde_json::json!({ "id": "a", "status": "closed" }))]), // material field changed
    ]));

    let first = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let second = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!first);
    assert!(second, "NewOrChanged mode must fire on a material field change, even with no predicate transition");
    assert!(rx.try_recv().is_ok());
}

#[tokio::test]
async fn new_only_mode_never_fires_again_once_an_item_has_been_seen() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::NewOnly, "not_empty(id)", vec!["status"]);
    let assignment = agent_watch_assignment_with_contract("watch-new-only", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate_with_payload("a", serde_json::json!({ "id": "a", "status": "open" }))]), // seeds
        Ok(vec![candidate_with_payload("a", serde_json::json!({ "id": "a", "status": "closed" }))]), // changed, but NewOnly ignores that
    ]));

    let first = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let second = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!first);
    assert!(!second, "NewOnly mode must never re-fire on an already-seen item, even if its fields changed");
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn format_mismatch_quarantines_candidate_and_never_fires() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let mut contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    contract.identity.format = Some("^[0-9]+$".to_string());
    let assignment = agent_watch_assignment_with_contract("watch-quarantine", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate("42")]),                          // seeds baseline with a well-formed id
        Ok(vec![candidate("42"), candidate("not-a-number")]), // "not-a-number" fails the declared format
    ]));

    let first = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let second = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!first);
    assert!(
        !second,
        "a candidate that fails the declared id format must never fire — it is quarantined, not treated as new"
    );
    assert!(rx.try_recv().is_err(), "no message should have been dispatched for the quarantined candidate");

    let scratchpad = persistence.assignment_scratchpads.get("watch-quarantine").await.unwrap().unwrap();
    assert_eq!(
        scratchpad.snapshots.len(),
        1,
        "the quarantined candidate must never get a snapshot — only the well-formed \"42\" does"
    );
}

#[tokio::test]
async fn quiet_tick_still_persists_the_updated_snapshot() {
    // The exact regression: before this fix, a tick that found
    // nothing to fire on returned early WITHOUT persisting, so this
    // poll's observation would never make it to disk.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'vip')", vec!["tag"]);
    let assignment = agent_watch_assignment_with_contract("watch-quiet", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate_with_payload("a", serde_json::json!({ "id": "a", "tag": "normal", "note": "first" }))]),
        Ok(vec![candidate_with_payload("a", serde_json::json!({ "id": "a", "tag": "normal", "note": "second" }))]),
    ]));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(!fired, "tag never became vip, so this tick must stay quiet");

    let scratchpad = persistence.assignment_scratchpads.get("watch-quiet").await.unwrap().unwrap();
    assert_eq!(
        scratchpad.snapshots[0].payload["note"], "second",
        "a quiet tick must still persist its observation — the old early-return-without-persisting \
         path would leave this stuck on the first poll's payload"
    );
}

#[tokio::test]
async fn contract_fingerprint_change_reseeds_without_firing() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment_id = "watch-amend";

    // First poll under the original contract: seed a non-matching baseline.
    let original_contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'vip')", vec!["tag"]);
    let assignment_v1 = agent_watch_assignment_with_contract(assignment_id, "agent-1", original_contract);
    let detector_v1: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![
        candidate_with_payload("a", serde_json::json!({ "id": "a", "tag": "normal" })),
    ])]));
    let seeded = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_v1,
        &Arc::new(Registry::new()), &assignment_v1, "watch", None).await;
    assert!(!seeded);

    // The contract is amended (a real edit, not a no-op) and the same
    // item now matches under the new predicate.
    let amended_contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'normal')", vec!["tag"]);
    let amended_fingerprint = amended_contract.fingerprint();
    let assignment_v2 = agent_watch_assignment_with_contract(assignment_id, "agent-1", amended_contract);
    let detector_v2: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![
        candidate_with_payload("a", serde_json::json!({ "id": "a", "tag": "normal" })),
    ])]));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_v2,
        &Arc::new(Registry::new()), &assignment_v2, "watch", None).await;
    assert!(!fired, "a contract amendment must re-seed rather than flood-fire on the next tick");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched on the re-seed tick");

    let scratchpad = persistence.assignment_scratchpads.get(assignment_id).await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), 1, "the re-seed must repopulate a fresh baseline");
    assert!(scratchpad.snapshots[0].predicate_value, "the baseline must reflect what the NEW contract's predicate says");
    assert_eq!(scratchpad.snapshots[0].edge_counter, 0, "a re-seed is not itself a transition");
    assert_eq!(scratchpad.contract_fingerprint.as_deref(), Some(amended_fingerprint.as_str()));
}

#[tokio::test]
async fn stale_keygen_version_reseeds_without_firing() {
    // Same contract, same candidate, across two ticks — nothing about
    // the contract or the observed payload changes. The only thing that
    // moves between ticks is `identity_keygen_version`, forced back to a
    // stale value to simulate a scratchpad written before the identity
    // hashing rules changed. That mismatch alone must force a re-seed
    // exactly like a fingerprint change does, never a fire on the
    // already-matching backlog.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment_id = "watch-stale-keygen";

    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'vip')", vec!["tag"]);
    let assignment = agent_watch_assignment_with_contract(assignment_id, "agent-1", contract);
    let payload = serde_json::json!({ "id": "a", "tag": "vip" });
    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate_with_payload("a", payload.clone())])]));

    let seeded = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(!seeded, "first poll is always a seed, never a fire");

    // Force the persisted keygen version stale, as if this scratchpad
    // predates `IDENTITY_KEYGEN_VERSION`'s current value.
    let mut scratchpad = persistence.assignment_scratchpads.get(assignment_id).await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), 1, "the first poll must have seeded a baseline");
    scratchpad.identity_keygen_version = Some(1);
    persistence.assignment_scratchpads.set(assignment_id, &scratchpad).await.unwrap();

    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate_with_payload("a", payload)])]));
    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(!fired, "a stale identity_keygen_version must force a re-seed, never a fire on the existing backlog");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched on the keygen re-seed tick");

    let scratchpad = persistence.assignment_scratchpads.get(assignment_id).await.unwrap().unwrap();
    assert_eq!(
        scratchpad.identity_keygen_version,
        Some(IDENTITY_KEYGEN_VERSION),
        "the re-seed tick must stamp the current keygen version"
    );
}

#[tokio::test]
async fn bind_parse_fix_keygen_bump_reseeds_pre_existing_rows_then_fires_only_on_a_genuinely_new_one() {
    // Regression coverage for the self-heal path of the bind-mode parser
    // fix: an install upgrading across the `IDENTITY_KEYGEN_VERSION` 2 ->
    // 3 bump must treat its pre-existing rows as a baseline to re-seed
    // (never as a flood of "new" items), and only fire on a row that is
    // genuinely new relative to that re-seeded baseline. Candidates are
    // supplied directly via `ScriptedDetector` (bypassing the parser —
    // this test is about `run_contract_bound_tick`'s version-mismatch
    // handling, not parsing itself; the parser's own bind-mode behavior
    // has its own coverage under `parse_candidates` above).
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment_id = "watch-bind-parse-fix-upgrade";

    let contract = bind_mode_contract(
        IdentityStrategy::CompositeNative,
        None,
        vec!["first_name", "last_name"],
        vec!["company", "first_name", "last_name", "status"],
    );
    let assignment = agent_watch_assignment_with_contract(assignment_id, "agent-1", contract);

    let row_jane = serde_json::json!({ "company": "Acme", "first_name": "Jane", "last_name": "Doe", "status": "new" });
    let row_bob = serde_json::json!({ "company": "Acme", "first_name": "Bob", "last_name": "Lee", "status": "new" });

    // First poll: seeds a baseline of the two pre-existing rows, as a
    // healthy install (already on the current keygen version) would.
    let seed_detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![
        candidate_with_payload("unused-a", row_jane.clone()),
        candidate_with_payload("unused-b", row_bob.clone()),
    ])]));
    let seeded =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &seed_detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(!seeded, "first poll is always a seed, never a fire");

    // Force the persisted keygen version back to the pre-fix value, as
    // if this scratchpad predates the bind-mode parser fix.
    let mut scratchpad = persistence.assignment_scratchpads.get(assignment_id).await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), 2, "the seed poll must have recorded both pre-existing rows");
    scratchpad.identity_keygen_version = Some(2);
    persistence.assignment_scratchpads.set(assignment_id, &scratchpad).await.unwrap();

    // Second poll: the same two pre-existing rows come back, unchanged.
    // The stale keygen version alone must force a silent re-seed — never
    // a fire on either of them.
    let detector2: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![
        candidate_with_payload("unused-a", row_jane.clone()),
        candidate_with_payload("unused-b", row_bob.clone()),
    ])]));
    let fired2 =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector2,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(fired2 == false, "the version-bump re-seed tick must never fire on pre-existing rows");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched on the version-bump re-seed tick");

    let scratchpad = persistence.assignment_scratchpads.get(assignment_id).await.unwrap().unwrap();
    assert_eq!(
        scratchpad.identity_keygen_version,
        Some(IDENTITY_KEYGEN_VERSION),
        "the re-seed tick must stamp the current keygen version"
    );
    assert_eq!(scratchpad.snapshots.len(), 2, "the re-seed must repopulate both pre-existing rows");

    // Third poll: the same two rows plus one genuinely new one. Only the
    // new row may fire.
    let row_new = serde_json::json!({ "company": "Acme", "first_name": "New", "last_name": "Person", "status": "new" });
    let detector3: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![
        candidate_with_payload("unused-a", row_jane),
        candidate_with_payload("unused-b", row_bob),
        candidate_with_payload("unused-c", row_new),
    ])]));
    let fired3 =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector3,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(fired3, "a genuinely new row after the re-seed tick must fire");

    let (_agent_id, queued) = rx.try_recv().expect("the new row must have dispatched exactly one message");
    assert!(
        queued.content.contains("New") || queued.content.contains("Person"),
        "the fired message should reference the new row, not the pre-existing ones: {}",
        queued.content
    );
}

#[tokio::test]
async fn fingerprint_changed_reseed_with_matching_candidates_also_discloses_them() {
    // Same "contract amended -> re-seed" fixture as
    // `contract_fingerprint_change_reseeds_without_firing`, but asserted
    // on the event bus: a rebind's re-seed excludes already-matching
    // candidates from firing exactly like a genuine first poll does, so
    // it gets the same disclosure treatment rather than going silent.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment_id = "watch-amend-disclose";

    let original_contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'vip')", vec!["tag"]);
    let assignment_v1 = agent_watch_assignment_with_contract(assignment_id, "agent-1", original_contract);
    let detector_v1: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![
        candidate_with_payload("a", serde_json::json!({ "id": "a", "tag": "normal" })),
    ])]));
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_v1,
        &Arc::new(Registry::new()), &assignment_v1, "watch", None).await;

    let mut health_rx = event_bus.subscribe();

    let amended_contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'normal')", vec!["tag"]);
    let assignment_v2 = agent_watch_assignment_with_contract(assignment_id, "agent-1", amended_contract);
    let detector_v2: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![
        candidate_with_payload("a", serde_json::json!({ "id": "a", "tag": "normal" })),
    ])]));
    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_v2,
        &Arc::new(Registry::new()), &assignment_v2, "watch", None)
            .await;
    assert!(!fired, "a contract amendment must still re-seed rather than fire");

    let texts = drain_system_message_texts(&mut health_rx);
    assert_eq!(
        texts.len(),
        2,
        "the rebind tick emits both the existing amendment re-seed notice and the new match disclosure: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.to_lowercase().contains("amended")),
        "the existing contract-amended re-seed notice must still fire: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("New item a")),
        "the new disclosure must name the already-matching candidate: {texts:?}"
    );
}

#[tokio::test]
async fn snapshot_cap_overflow_emits_exactly_one_aggregated_health_event_per_tick() {
    // A source with many times more items than SNAPSHOT_CAP forces
    // `record_snapshot` to evict repeatedly within a single tick — once
    // per candidate that pushes the store over cap. Emitting from inside
    // that per-candidate loop turns one degraded tick into a storm of one
    // event per eviction. This drives a single tick with 2x SNAPSHOT_CAP
    // never-before-seen candidates and asserts exactly one health event
    // fires for the whole tick, reporting the real aggregate dropped
    // count (SNAPSHOT_CAP) rather than the per-push count of 1
    // `record_snapshot` itself returns on every individual push.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::NewOnly, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-overflow", "agent-1", contract);

    let total_candidates = SNAPSHOT_CAP * 2;
    let overflow_candidates: Vec<AgentWatchCandidate> =
        (0..total_candidates).map(|i| candidate(&format!("item-{i}"))).collect();
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(overflow_candidates)]));

    let mut rx = event_bus.subscribe();

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    let mut health_event_texts = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEventPayload::SystemMessage { text, .. } = event.payload {
            health_event_texts.push(text);
        }
    }

    assert_eq!(
        health_event_texts.len(),
        1,
        "a single tick that overflows the cap many times over must still emit exactly one health event, not one per eviction"
    );
    let text = &health_event_texts[0];
    let expected_dropped = total_candidates - SNAPSHOT_CAP;
    assert!(
        text.contains(&expected_dropped.to_string()),
        "the health event must report the real aggregate dropped count ({expected_dropped}), not the per-push count of 1: {text}"
    );
    assert!(text.contains("2000"), "the health event must name the cap so the user understands why: {text}");
    assert!(
        text.to_lowercase().contains("dropped"),
        "the health event must say what happened, in plain language: {text}"
    );
    assert!(
        text.to_lowercase().contains("again") || text.to_lowercase().contains("second time"),
        "the health event must convey that a dropped item reappearing later may be reported to the user again: {text}"
    );

    let scratchpad = persistence.assignment_scratchpads.get("watch-overflow").await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), SNAPSHOT_CAP, "snapshots must remain capped after the overflow tick");
    assert!(scratchpad.truncation_notified, "the latch must be set after a tick that actually dropped observations");
}

#[tokio::test]
async fn snapshot_cap_overflow_latch_holds_across_consecutive_over_cap_ticks() {
    // A watch that stays over cap tick after tick must warn once for the
    // life of that condition, not on every poll — otherwise a source
    // that never recovers pages the user forever. Drives two consecutive
    // ticks, each independently pushing the store past cap with a fresh
    // batch of never-before-seen ids, and asserts only the first tick's
    // eviction produces a health event.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    // A predicate over a field no candidate payload carries never
    // matches, so this contract never fires under any mode regardless of
    // seed_only — keeping these ticks focused purely on the truncation
    // latch, not on transition/dispatch machinery.
    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(never_present, 'x')", vec![]);
    let assignment_id = "watch-overflow-latch";
    let assignment = agent_watch_assignment_with_contract(assignment_id, "agent-1", contract);

    let mut rx = event_bus.subscribe();

    let tick1_candidates: Vec<AgentWatchCandidate> =
        (0..(SNAPSHOT_CAP * 2)).map(|i| candidate(&format!("tick1-item-{i}"))).collect();
    let detector1: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(tick1_candidates)]));
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector1,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    let tick2_candidates: Vec<AgentWatchCandidate> =
        (0..(SNAPSHOT_CAP * 2)).map(|i| candidate(&format!("tick2-item-{i}"))).collect();
    let detector2: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(tick2_candidates)]));
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector2,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    let health_event_count = drain_system_message_count(&mut rx);
    assert_eq!(
        health_event_count, 1,
        "two consecutive over-cap ticks must produce exactly one health event total — the latch must hold on the second"
    );

    let scratchpad = persistence.assignment_scratchpads.get(assignment_id).await.unwrap().unwrap();
    assert!(scratchpad.truncation_notified, "the latch must remain set while the watch is still over cap");
}

#[tokio::test]
async fn snapshot_cap_overflow_latch_clears_when_the_tracked_set_drops_below_cap_and_rearms_later() {
    // The latch clears only on genuine recovery, not merely on a tick
    // that drops nothing: `record_snapshot` drains back to exactly
    // `SNAPSHOT_CAP` and never below once the cap is hit, so a tick's
    // drop count can be zero while the tracked set is still fully at cap
    // (see the flapping regression test below, which covers exactly
    // that). The only product code path that shrinks `snapshots` today
    // is a contract amendment — `run_contract_bound_tick`'s
    // `fingerprint_changed` branch clears `snapshots` before re-seeding
    // — so that's what this test uses to drive a genuine recovery.
    // Drives: an over-cap tick under contract A (warns, sets the latch)
    // -> an amendment tick to contract B with a tiny candidate batch
    // (re-seeds `snapshots` from empty, landing it far under cap,
    // clearing the latch) -> another over-cap tick under contract B with
    // a fresh id batch (must warn again).
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment_id = "watch-overflow-rearm";
    let contract_a = dedup_contract(WatchMode::PredicateTransition, "equals(never_present, 'x')", vec![]);
    let assignment_a = agent_watch_assignment_with_contract(assignment_id, "agent-1", contract_a);

    let mut rx = event_bus.subscribe();

    let tick1_candidates: Vec<AgentWatchCandidate> =
        (0..(SNAPSHOT_CAP * 2)).map(|i| candidate(&format!("tick1-item-{i}"))).collect();
    let detector1: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(tick1_candidates)]));
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector1,
        &Arc::new(Registry::new()), &assignment_a, "watch", None).await;

    let tick1_dropped_events =
        drain_system_message_texts(&mut rx).into_iter().filter(|t| t.to_lowercase().contains("dropped")).count();
    assert_eq!(tick1_dropped_events, 1, "tick 1 must emit exactly one truncation health event");

    let scratchpad_after_tick1 = persistence.assignment_scratchpads.get(assignment_id).await.unwrap().unwrap();
    assert!(scratchpad_after_tick1.truncation_notified, "sanity: tick 1 must have set the latch");
    assert_eq!(scratchpad_after_tick1.snapshots.len(), SNAPSHOT_CAP);

    // A different predicate expression moves the contract's fingerprint,
    // so tick 2 takes the `fingerprint_changed` re-seed path: `snapshots`
    // is cleared before this poll's two-item batch is seeded into it,
    // landing the tracked set far under `SNAPSHOT_CAP`.
    let contract_b = dedup_contract(WatchMode::PredicateTransition, "equals(never_present, 'y')", vec![]);
    let assignment_b = agent_watch_assignment_with_contract(assignment_id, "agent-1", contract_b);
    let tick2_candidates = vec![candidate("tick2-item-0"), candidate("tick2-item-1")];
    let detector2: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(tick2_candidates)]));
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector2,
        &Arc::new(Registry::new()), &assignment_b, "watch", None).await;

    let tick2_dropped_events =
        drain_system_message_texts(&mut rx).into_iter().filter(|t| t.to_lowercase().contains("dropped")).count();
    assert_eq!(tick2_dropped_events, 0, "the re-seed tick must not itself warn about dropping anything");

    let scratchpad_after_tick2 = persistence.assignment_scratchpads.get(assignment_id).await.unwrap().unwrap();
    assert!(
        !scratchpad_after_tick2.truncation_notified,
        "a tick that genuinely shrinks the tracked set below cap must clear the latch"
    );
    assert_eq!(scratchpad_after_tick2.snapshots.len(), 2, "sanity: the re-seed left the tracked set far under cap");

    let tick3_candidates: Vec<AgentWatchCandidate> =
        (0..(SNAPSHOT_CAP * 2)).map(|i| candidate(&format!("tick3-item-{i}"))).collect();
    let detector3: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(tick3_candidates)]));
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector3,
        &Arc::new(Registry::new()), &assignment_b, "watch", None).await;

    let tick3_dropped_events =
        drain_system_message_texts(&mut rx).into_iter().filter(|t| t.to_lowercase().contains("dropped")).count();
    assert_eq!(
        tick3_dropped_events, 1,
        "the over-cap condition clearing and later recurring must be treated as two separate episodes, each warning once"
    );
}

#[tokio::test]
async fn snapshot_cap_overflow_latch_does_not_flap_on_ticks_that_drop_nothing_while_still_at_cap() {
    // Regression for the bug where the latch cleared on tick-local drop
    // count instead of on genuine recovery: `record_snapshot` drains
    // back to exactly `SNAPSHOT_CAP` and never below once the cap is
    // hit, so a tick that drops zero observations does NOT mean the
    // tracked set shrank — it can still be pinned at cap from a prior
    // tick. Drives three ticks against the same over-cap watch:
    //   tick 1 (new ids):   4000 fresh ids against the 2000 cap -> one
    //                       aggregated health event, latch set.
    //   tick 2 (no ids):    an empty poll (`Ok(vec![])`) -> zero health
    //                       events, and the latch must still be set
    //                       afterward, since nothing shrank the set.
    //   tick 3 (new ids):   a fresh batch that evicts more entries from
    //                       the still-at-cap set -> zero health events.
    //                       This is the assertion the old drop-count-based
    //                       clear fails: it cleared on tick 2's zero-drop
    //                       result and would re-fire here.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(never_present, 'x')", vec![]);
    let assignment_id = "watch-overflow-flap";
    let assignment = agent_watch_assignment_with_contract(assignment_id, "agent-1", contract);

    let mut rx = event_bus.subscribe();

    let tick1_candidates: Vec<AgentWatchCandidate> =
        (0..(SNAPSHOT_CAP * 2)).map(|i| candidate(&format!("flap-tick1-item-{i}"))).collect();
    let detector1: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(tick1_candidates)]));
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector1,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert_eq!(
        drain_system_message_count(&mut rx),
        1,
        "tick 1 must emit exactly one aggregated health event for the overflow"
    );

    let scratchpad_after_tick1 = persistence.assignment_scratchpads.get(assignment_id).await.unwrap().unwrap();
    assert!(scratchpad_after_tick1.truncation_notified, "sanity: tick 1 must have set the latch");
    assert_eq!(scratchpad_after_tick1.snapshots.len(), SNAPSHOT_CAP);

    // Tick 2: an empty poll. Nothing is observed, so nothing is evicted
    // — but the tracked set is still pinned at SNAPSHOT_CAP from tick 1,
    // so the watch is still fully degraded.
    let detector2: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![])]));
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector2,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert_eq!(drain_system_message_count(&mut rx), 0, "an empty poll must not itself emit a health event");

    let scratchpad_after_tick2 = persistence.assignment_scratchpads.get(assignment_id).await.unwrap().unwrap();
    assert!(
        scratchpad_after_tick2.truncation_notified,
        "the latch must still be set after a tick that dropped nothing but never shrank the tracked set below cap"
    );
    assert_eq!(
        scratchpad_after_tick2.snapshots.len(),
        SNAPSHOT_CAP,
        "sanity: an empty poll must not change the tracked set's size"
    );

    // Tick 3: a fresh batch of never-before-seen ids which, because the
    // set is still pinned at cap from tick 1, evicts more oldest
    // entries. Under the buggy drop-count-based clear, tick 2 would have
    // already cleared the latch, so this tick would incorrectly re-fire.
    let tick3_candidates: Vec<AgentWatchCandidate> =
        (0..500).map(|i| candidate(&format!("flap-tick3-item-{i}"))).collect();
    let detector3: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(tick3_candidates)]));
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector3,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert_eq!(
        drain_system_message_count(&mut rx),
        0,
        "the latch must still hold on tick 3 — it never cleared, so this is the same episode as tick 1, not a new one"
    );

    let scratchpad_after_tick3 = persistence.assignment_scratchpads.get(assignment_id).await.unwrap().unwrap();
    assert!(scratchpad_after_tick3.truncation_notified, "the latch must remain set");
    assert_eq!(
        scratchpad_after_tick3.snapshots.len(),
        SNAPSHOT_CAP,
        "the tracked set stays pinned at cap after further eviction"
    );
}

#[tokio::test]
async fn scratchpad_upgraded_from_legacy_to_a_bound_contract_reseeds_without_flooding() {
    // A watch that ran on the legacy `seen_ids` path (so its scratchpad
    // already exists — `is_first_poll` is false) and then gets a
    // contract attached for the first time must not treat every
    // already-matching candidate as a fresh row-1 match on the very next
    // tick — that would flood the user on the poll that "upgraded" the
    // watch. `contract_fingerprint` starts `None` in this scenario (never
    // bound before), so the fingerprint gate must catch it the same way
    // it catches an actual amendment.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment_id = "watch-upgrade";

    // Poll 1: legacy path, no contract yet — seeds `seen_ids`.
    let legacy_assignment = agent_watch_assignment(assignment_id, "agent-1");
    let legacy_detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a")])]));
    let seeded = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &legacy_detector,
        &Arc::new(Registry::new()), &legacy_assignment, "watch", None,
    )
    .await;
    assert!(!seeded);

    // Poll 2: the same assignment now carries a contract under which "a"
    // already matches — this must re-seed, not fire.
    let contract = dedup_contract(WatchMode::PredicateTransition, "equals(tag, 'vip')", vec!["tag"]);
    let bound_assignment = agent_watch_assignment_with_contract(assignment_id, "agent-1", contract);
    let bound_detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![
        candidate_with_payload("a", serde_json::json!({ "id": "a", "tag": "vip" })),
    ])]));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &bound_detector,
        &Arc::new(Registry::new()), &bound_assignment, "watch", None,
    )
    .await;

    assert!(!fired, "gaining a contract for the first time must re-seed, not flood-fire on an already-matching backlog");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched on the upgrade tick");

    let scratchpad = persistence.assignment_scratchpads.get(assignment_id).await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), 1, "the upgrade tick must seed a fresh contract-bound baseline");
    assert_eq!(scratchpad.snapshots[0].edge_counter, 0, "seeding is not itself a transition");
    assert_eq!(
        scratchpad.seen_ids,
        vec![legacy_candidate_key(&candidate("a"))],
        "the legacy seen_ids history must survive the upgrade untouched"
    );
}

#[tokio::test]
async fn legacy_watch_without_a_contract_still_dedups_via_seen_ids() {
    // Backward compatibility: an assignment that hasn't authored a
    // `WatchContract` yet (`contract: None`) must keep working exactly
    // as before — `run_legacy_seen_ids_tick`, untouched by this change.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-legacy", "agent-1");

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate("a")]),
        Ok(vec![candidate("a"), candidate("b")]),
    ]));

    let first = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let second = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!first);
    assert!(second, "an assignment with no bound contract yet must keep working via the legacy seen_ids diff");

    let scratchpad = persistence.assignment_scratchpads.get("watch-legacy").await.unwrap().unwrap();
    assert_eq!(scratchpad.seen_ids, vec![legacy_candidate_key(&candidate("a")), legacy_candidate_key(&candidate("b"))]);
    assert!(scratchpad.snapshots.is_empty(), "the legacy path must never populate contract-bound snapshots");

    let (_agent_id, message) = rx.try_recv().expect("a message must have been dispatched");
    assert!(message.content.contains("New item b"));
}

#[tokio::test]
async fn legacy_path_dedupes_the_same_row_content_across_drifting_model_minted_ids() {
    // Regression test for the phantom-refire bug this whole fix exists
    // for (a live demo watch emailed the same person 3 times): a
    // detector is free to mint a different `id` for the same physical
    // row on every single poll (see `AgentWatchCandidate::id`'s own
    // doc), so the legacy fallback must dedupe on the row's own content
    // via `legacy_candidate_key`, never on that disposable id. Presents
    // the SAME row payload under 3 DIFFERENT model-minted ids across 3
    // polls (mirroring the actual ids the model minted for one physical
    // row in the incident this guards against) and asserts it fires
    // exactly once — on its first appearance, never again after.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-content-dedupe", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let row_payload = serde_json::json!({ "name": "Peter Grace", "company": "Peter's Pool Construction" });
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![]), // poll 0: the assignment's actual first poll — seeds an empty baseline.
        Ok(vec![candidate_with_payload("peter-grace", row_payload.clone())]),
        Ok(vec![candidate_with_payload("peter-grace-peters-pool-construction", row_payload.clone())]),
        Ok(vec![candidate_with_payload("row-1", row_payload.clone())]),
    ]));

    let mut fired = Vec::new();
    for _ in 0..4 {
        fired.push(
            run_agent_watch_tick(
                &persistence,
                &dispatcher,
                &event_bus,
                &detector,
                &Arc::new(Registry::new()),
                &assignment,
                "watch",
                None,
            )
            .await,
        );
    }

    assert_eq!(
        fired,
        vec![false, true, false, false],
        "the row must fire exactly once — on its first appearance — regardless of the model minting 3 \
         different ids for it across the next 3 polls"
    );

    let (_agent_id, _message) = rx.try_recv().expect("the row's first appearance must have dispatched a message");
    assert!(rx.try_recv().is_err(), "the same row under a different id must never dispatch a second message");
}

#[tokio::test]
async fn legacy_seen_ids_upgrade_from_old_format_does_not_fire_for_already_seen_items() {
    // Backward compatibility: a scratchpad persisted before this fix
    // carries model-minted ids in `seen_ids` that can never again match
    // a content-derived `legacy_candidate_key`. Diffing against them
    // naively would make every already-seen item look brand new on the
    // very first poll after upgrade and firestorm the moment this ships.
    // The first poll that finds an old-format `seen_ids` must instead
    // re-baseline onto the new key space without firing; only the poll
    // after that resumes real diffing — and must recognize the same row
    // as already-seen even under yet another different model-minted id.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-legacy-upgrade", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    // Pre-existing scratchpad in the OLD (pre-fix) format: a raw
    // model-minted id, not a content hash. `seen_ids` non-empty also
    // makes this poll NOT the assignment's first (`is_first_poll` false).
    let mut pre_existing = AssignmentScratchpad::default();
    pre_existing.seen_ids = vec!["peter-grace".to_string()];
    persistence.assignment_scratchpads.set(&assignment.id, &pre_existing).await.unwrap();

    let row_payload = serde_json::json!({ "name": "Peter Grace", "company": "Peter's Pool Construction" });
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate_with_payload("peter-grace", row_payload.clone())]),
        Ok(vec![candidate_with_payload("row-1-peter-grace", row_payload.clone())]),
    ]));

    let upgrade_poll_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;
    assert!(!upgrade_poll_fired, "the first poll after upgrade must re-baseline onto the new key space, not fire");

    let scratchpad_after_upgrade = persistence.assignment_scratchpads.get(&assignment.id).await.unwrap().unwrap();
    assert_eq!(
        scratchpad_after_upgrade.seen_ids,
        vec![legacy_candidate_key(&candidate_with_payload("peter-grace", row_payload.clone()))],
        "the upgrade poll must replace the stale old-format id with the new content-derived key"
    );

    let next_poll_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;
    assert!(
        !next_poll_fired,
        "the same row under yet another different model-minted id must not fire once the new key space is seeded"
    );
    assert!(rx.try_recv().is_err(), "no message should ever have been dispatched for this already-seen row");
}

#[tokio::test]
async fn ceiling_crossing_poll_does_not_fire_via_the_unstable_legacy_path() {
    // Regression test for the ceiling off-by-one: the poll that pushes
    // `authoring_failure_streak` from CEILING-1 to CEILING used to be
    // passed `seed_only` computed from the PRE-poll streak snapshot (the
    // same one the branch-selection check reads), so that exact poll
    // still fell through to `run_legacy_seen_ids_tick` with
    // `seed_only=false` even though it just proved this watch has no
    // stable identity to diff on. Primes the streak to CEILING-1 against
    // an unrelated baseline, then drives the ceiling-crossing poll with a
    // candidate whose content was never seen before and asserts it does
    // NOT fire.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-ceiling-crossing", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let mut pre_existing = AssignmentScratchpad::default();
    pre_existing.authoring_failure_streak = AUTHORING_FAILURE_CEILING - 1;
    pre_existing.seen_ids = vec![legacy_candidate_key(&candidate("unrelated-baseline-item"))];
    persistence.assignment_scratchpads.set(&assignment.id, &pre_existing).await.unwrap();

    // An invalid identity.format regex: not same-tick repairable, so
    // this poll's authoring attempt is rejected outright (`NotBound`),
    // pushing the streak to the ceiling.
    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply {
            candidates: vec![candidate("brand-new-never-seen-row")],
            proposed_contract: Some(unrepairable_proposal()),
        })],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;

    assert!(!fired, "the poll that crosses AUTHORING_FAILURE_CEILING must not fire via the unstable legacy path");

    let scratchpad = persistence.assignment_scratchpads.get(&assignment.id).await.unwrap().unwrap();
    assert_eq!(
        scratchpad.authoring_failure_streak, AUTHORING_FAILURE_CEILING,
        "sanity: this poll must be the one that actually reaches the ceiling"
    );
    assert!(rx.try_recv().is_err(), "no message should have been dispatched on the ceiling-crossing poll");
}

// ---------------------------------------------------------------------------
// Tests — contract authoring: proposal validation,
// the stability probe, and the amendment trigger.
// ---------------------------------------------------------------------------

/// A minimal, otherwise-valid `native_id` contract proposal — the shape
/// an authoring-mode reply's `contract` object takes (the
/// `CONTRACT_PROPOSAL_SHAPE`). Individual tests mutate the returned
/// `Value` to introduce exactly one validation defect.
fn native_id_proposal(format: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "source": { "kind": "test", "ref": "test" },
        "identity": {
            "strategy": "native_id",
            "source_field": "uid",
            "format": format,
            "fields": [],
            "rationale": "this source exposes a stable per-row key"
        },
        "change": { "material_fields": ["tag"] },
        // Targets "tag" (the material field), not "uid" — "uid" is
        // declared `required: true` above, and `WatchContract::validate`
        // now rejects a `NotEmpty` predicate paired with `required: true`
        // on the same field (the two are contradictory: `required`
        // quarantines a blank value before any predicate runs, so a
        // `NotEmpty` on that same field could never fire). This fixture
        // is meant to be an "otherwise-valid" proposal for tests that
        // don't care about the predicate's own semantics, so it must not
        // trip that check itself.
        "predicate": { "natural_language": "", "fields": [], "expr": "not_empty(tag)" },
        "fields": { "uid": { "type": "string", "required": true } }
    })
}

fn candidate_with_uid(id: &str, uid: &str, tag: &str) -> AgentWatchCandidate {
    candidate_with_payload(id, serde_json::json!({ "uid": uid, "tag": tag }))
}

/// A proposal that always fails `WatchContract::validate` for a reason
/// NONE of `run_authoring_attempts`'s same-tick repair arms recognize (an
/// invalid `identity.format` regex) — the shared "this rejection is
/// never same-tick-repairable, so every poll below spends exactly one
/// authoring attempt" fixture for the ceiling/streak/cross-poll tests
/// below. Deliberately NOT empty `change.material_fields`: that rejection
/// reason became same-tick repairable (see
/// `a_second_attempt_repairs_empty_material_fields_within_the_same_tick`),
/// which is exactly what these tests must stay decoupled from.
fn unrepairable_proposal() -> serde_json::Value {
    native_id_proposal(Some("(unclosed"))
}

/// The authoring tests' primary assertion surface: fetches the live
/// `contract` slot off the persisted assignment record. A rejected
/// proposal's whole point is that this stays `None`.
async fn stored_contract(persistence: &PersistenceLayer, assignment_id: &str) -> Option<WatchContract> {
    let stored = persistence.assignments.get(assignment_id).await.expect("assignment must exist");
    match stored.trigger {
        AssignmentTrigger::AgentWatch { contract, .. } => contract,
        _ => panic!("expected an AgentWatch trigger"),
    }
}

#[tokio::test]
async fn authoring_rejects_a_proposal_with_an_unknown_identity_strategy() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-author-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let mut proposal = native_id_proposal(None);
    proposal["identity"]["strategy"] = serde_json::json!("row_number"); // not in the closed enum

    // An unrecognized enum value is a `ProposalRejection::Malformed`,
    // which the same-tick repair loop now retries once (see
    // `RepairContext::Malformed`) — this still-bad second attempt is
    // needed to let that retry play out; this test is about the
    // proposal never binding, not about the repair loop itself.
    let still_bad_proposal = proposal.clone();

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(proposal) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(still_bad_proposal) }),
        ],
        vec![],
    ));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired, "an authoring poll must never fire");
    assert!(
        stored_contract(&persistence, "watch-author-1").await.is_none(),
        "a proposal with an unrecognized identity strategy must never be persisted"
    );
}

#[tokio::test]
async fn authoring_rejects_a_proposal_with_an_unparseable_predicate_expr() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-author-2", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let mut proposal = native_id_proposal(None);
    proposal["predicate"]["expr"] = serde_json::json!("contains(tag, 'x'"); // unterminated

    // An unparseable predicate is exactly the rejection the same-tick
    // repair loop retries once (see the `a_rejected_predicate_triggers_*`
    // test), so this still-bad second attempt is needed to let that
    // retry play out — this test is about the proposal never binding,
    // not about the repair loop itself.
    let still_bad_proposal = proposal.clone();

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(proposal) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(still_bad_proposal) }),
        ],
        vec![],
    ));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired);
    assert!(stored_contract(&persistence, "watch-author-2").await.is_none());
}

#[tokio::test]
async fn authoring_rejects_a_proposal_with_an_invalid_format_regex() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-author-3", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let proposal = native_id_proposal(Some("(unclosed"));

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(proposal) })],
        vec![],
    ));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired);
    assert!(stored_contract(&persistence, "watch-author-3").await.is_none());
}

#[tokio::test]
async fn authoring_rejects_a_proposal_with_empty_material_fields() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-author-4", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let mut proposal = native_id_proposal(None);
    proposal["change"]["material_fields"] = serde_json::json!([]);

    // Empty material_fields on a non-`new_only` proposal is same-tick
    // repairable (see `a_second_attempt_repairs_empty_material_fields_within_the_same_tick`),
    // so this still-bad second attempt is needed to let that retry play
    // out — this test is about the proposal never binding, not about
    // the repair loop itself.
    let still_bad_proposal = proposal.clone();

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(proposal) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(still_bad_proposal) }),
        ],
        vec![],
    ));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired);
    assert!(stored_contract(&persistence, "watch-author-4").await.is_none());
}

#[tokio::test]
async fn authoring_drops_a_format_regex_that_does_not_match_observed_values_but_still_binds() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-author-5", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    // The proposed format claims a UUID shape, but the candidate's own
    // observed `uid` value is a plain integer — the agent's own poll
    // contradicts its own regex.
    let proposal = native_id_proposal(Some(r"^[0-9a-f-]{36}$"));
    let poll1 = vec![candidate_with_uid("a", "12345", "x")];
    // Same value on the probe's second poll, so the probe itself would
    // pass — only the format check is in play here.
    let poll2 = vec![candidate_with_uid("a", "12345", "x")];

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: poll1, proposed_contract: Some(proposal) })],
        vec![Ok(poll2)],
    ));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired, "an authoring run — even a fully successful one — must never fire");
    let contract = stored_contract(&persistence, "watch-author-5")
        .await
        .expect("a regex that contradicts the agent's own observed value must not block the rest of an \
                 otherwise-valid proposal from binding");
    assert_eq!(
        contract.identity.format, None,
        "the non-matching format must be dropped rather than persisted verbatim"
    );
    assert_eq!(
        contract.identity.strategy,
        IdentityStrategy::NativeId,
        "only the format is dropped — the native_id strategy itself is untouched"
    );
}

/// The exact trap this behavior exists to prevent (ground truth from a
/// live Notion workspace probe): a watch's contract was previously
/// authored against a page-fetch tool, whose urls carried a `/p/`
/// segment (`^https://app\.notion\.com/p/[0-9a-f]{32}$`). Swapping the
/// watch to a data-source query tool returns rows whose own `url` field
/// has no `/p/` segment at all — so if that stale regex were ever
/// persisted, every row would fail the format check and be quarantined,
/// forever, while the watch still looked bound and healthy. Proves the
/// regex is dropped, not persisted, against the two real observed urls.
#[tokio::test]
async fn authoring_drops_a_stale_notion_page_url_format_against_data_source_query_urls() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-author-notion", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let mut proposal = native_id_proposal(Some(r"^https://app\.notion\.com/p/[0-9a-f]{32}$"));
    proposal["identity"]["source_field"] = serde_json::json!("url");

    let poll1 = vec![candidate_with_payload(
        "a",
        serde_json::json!({
            "url": "https://app.notion.com/a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "tag": "x"
        }),
    )];
    let poll2 = vec![candidate_with_payload(
        "a",
        serde_json::json!({
            "url": "https://app.notion.com/a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "tag": "x"
        }),
    )];

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: poll1, proposed_contract: Some(proposal) })],
        vec![Ok(poll2)],
    ));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired);
    let contract = stored_contract(&persistence, "watch-author-notion")
        .await
        .expect("the contract must still bind despite the stale format guess");
    assert_eq!(
        contract.identity.format, None,
        "a /p/-shaped regex must never be persisted against urls that don't carry a /p/ segment — a \
         mismatched format that survives persistence quarantines every future candidate"
    );
}

#[tokio::test]
async fn authoring_probe_confirms_a_stable_field_and_records_a_rationale() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-author-6", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let proposal = native_id_proposal(Some(r"^\d+$"));
    let poll1 = vec![candidate_with_uid("a", "42", "x"), candidate_with_uid("b", "43", "y")];
    let poll2 = vec![candidate_with_uid("a", "42", "x"), candidate_with_uid("b", "43", "y")];

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: poll1, proposed_contract: Some(proposal) })],
        vec![Ok(poll2)],
    ));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired, "an authoring run — even a fully successful one — must never fire");

    let contract = stored_contract(&persistence, "watch-author-6")
        .await
        .expect("a proposal that passes every check must be persisted");
    assert_eq!(
        contract.identity.strategy,
        IdentityStrategy::NativeId,
        "the field was confirmed stable, so there is no rung to drop"
    );
    assert!(contract.identity.rationale.contains("Verified stable"), "got: {}", contract.identity.rationale);
}

#[tokio::test]
async fn authoring_probe_disqualifies_an_unstable_field_and_drops_a_rung() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-author-7", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let mut proposal = native_id_proposal(None);
    proposal["fields"] = serde_json::json!({
        "uid": { "type": "string", "required": true },
        "name": { "type": "string", "required": true }
    });
    let poll1 = vec![
        candidate_with_payload("a", serde_json::json!({ "uid": "row-1", "name": "Alice", "tag": "x" })),
        candidate_with_payload("b", serde_json::json!({ "uid": "row-3", "name": "Bob", "tag": "x" })),
    ];
    // Both rows' `uid` values moved between the two polls, and poll1
    // already carried two distinct values — total churn of that size is
    // the positive Unstable finding the probe exists to catch. (A
    // single-row churn is deliberately Inconclusive, not Unstable — see
    // the subset-semantics tests above; one value can't distinguish a
    // rewrite from a delete-then-add.)
    let poll2 = vec![
        candidate_with_payload("a", serde_json::json!({ "uid": "row-2", "name": "Alice", "tag": "x" })),
        candidate_with_payload("b", serde_json::json!({ "uid": "row-4", "name": "Bob", "tag": "x" })),
    ];

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: poll1, proposed_contract: Some(proposal) })],
        vec![Ok(poll2)],
    ));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired);

    let contract = stored_contract(&persistence, "watch-author-7")
        .await
        .expect("an unstable native_id must still fall back to a persisted composite_native contract");
    assert_eq!(
        contract.identity.strategy,
        IdentityStrategy::CompositeNative,
        "the probe must drop a rung, not reject the proposal outright"
    );
    assert!(contract.identity.source_field.is_none());
    assert!(!contract.identity.fields.is_empty());
    assert!(
        contract.identity.rationale.contains("uid"),
        "the rationale must name the disqualified field in plain language: {}",
        contract.identity.rationale
    );
}

// ---------------------------------------------------------------------------
// FIX 1 — `probe_identity_stability` must compare candidates by the
// proposed `native_id` field's own VALUE, never the reply's free-text
// `id` tag, and an Inconclusive probe must keep the proposed rung rather
// than being folded into Unstable.
// ---------------------------------------------------------------------------

#[test]
fn probe_identity_stability_is_stable_when_source_field_values_match_despite_differing_free_text_id_tags() {
    // Each probe poll spins a fresh child session
    // (`AGENT_WATCH_SYSTEM_PROMPT` itself documents the reply's `id` tag
    // as "not a dedup key"), so the same underlying row routinely gets a
    // different free-text tag on its second observation. The probe must
    // still recognize this as stable by comparing `uid`'s own value.
    let poll1 = vec![candidate_with_uid("child-session-1-tag-a", "https://source.example/row/42", "x")];
    let poll2 = vec![candidate_with_uid("completely-different-tag-on-round-2", "https://source.example/row/42", "x")];

    match probe_identity_stability("uid", &poll1, &poll2) {
        ProbeOutcome::Stable { checked } => assert_eq!(checked, 1),
        ProbeOutcome::Unstable => panic!("must not read as Unstable — the field's value never changed"),
        ProbeOutcome::Inconclusive(_) => {
            panic!("must not read as Inconclusive — a matching id tag was never required")
        }
    }
}

#[test]
fn probe_identity_stability_is_inconclusive_when_the_field_is_absent_from_every_candidate_in_a_poll() {
    let poll1 = vec![candidate_with_uid("a", "row-1", "x")];
    // Second poll's candidates don't carry the proposed field at all —
    // nothing to compare it against, which is not evidence of instability.
    let poll2 = vec![candidate_with_payload("a", serde_json::json!({ "tag": "x" }))];

    match probe_identity_stability("uid", &poll1, &poll2) {
        ProbeOutcome::Inconclusive(ProbeInconclusiveCause::NoObservations) => {}
        ProbeOutcome::Inconclusive(_) => {
            panic!("must read as the NoObservations cause — the field was absent from an entire poll")
        }
        ProbeOutcome::Stable { .. } => panic!("must not read as Stable — the field was never observed twice"),
        ProbeOutcome::Unstable => {
            panic!("must not read as Unstable — an absent value is not a positive instability finding")
        }
    }
}

// ---------------------------------------------------------------------------
// REGRESSION FIX — `probe_identity_stability` must use SUBSET semantics,
// not exact set-equality: a watch exists because new rows appear between
// polls, so poll2 containing values poll1 never saw (membership growth)
// must not be read as identity instability. Set-equality previously
// declared a perfectly stable key Unstable the moment a single row was
// added between the two probe polls, triggering a rung-drop that could
// empty `identity.fields` entirely on databases where every non-identity
// field is material (the reported Notion "My client list" incident).
// ---------------------------------------------------------------------------

#[test]
fn probe_identity_stability_growth_between_polls_is_stable_not_unstable() {
    // The regression guard for the demo case: poll1 sees one row, a new
    // row is added to the source between polls, poll2 sees the original
    // row PLUS the new one. Every value the probe actually observed on
    // poll1 is still there on poll2 — that is exactly what "stable"
    // means for a watch. Growth during the authoring window must not
    // destabilise the identity.
    let poll1 = vec![candidate_with_uid("a", "A", "x")];
    let poll2 = vec![candidate_with_uid("a", "A", "x"), candidate_with_uid("b", "B", "x")];

    match probe_identity_stability("uid", &poll1, &poll2) {
        ProbeOutcome::Stable { checked } => assert_eq!(checked, 1),
        ProbeOutcome::Unstable => panic!("must not read as Unstable — poll1's only value survived into poll2"),
        ProbeOutcome::Inconclusive(_) => panic!("must not read as Inconclusive — subset is a definite Stable"),
    }
}

#[test]
fn probe_identity_stability_identical_sets_are_still_stable() {
    // Equality is a special case of subset — must keep working exactly
    // as before the subset rewrite.
    let poll1 = vec![candidate_with_uid("a", "A", "x"), candidate_with_uid("b", "B", "x")];
    let poll2 = vec![candidate_with_uid("a", "A", "x"), candidate_with_uid("b", "B", "x")];

    match probe_identity_stability("uid", &poll1, &poll2) {
        ProbeOutcome::Stable { checked } => assert_eq!(checked, 2),
        ProbeOutcome::Unstable => panic!("must not read as Unstable — the sets are identical"),
        ProbeOutcome::Inconclusive(_) => panic!("must not read as Inconclusive — equality is a definite Stable"),
    }
}

#[test]
fn probe_identity_stability_total_churn_of_two_or_more_values_is_unstable() {
    // Two polls sharing NOTHING, with two or more distinct values on
    // poll1, is the positive finding a rung-drop legitimately exists to
    // catch: the field is re-minted / session-scoped.
    let poll1 = vec![candidate_with_uid("a", "A", "x"), candidate_with_uid("b", "B", "x")];
    let poll2 = vec![candidate_with_uid("a", "C", "x"), candidate_with_uid("b", "D", "x")];

    match probe_identity_stability("uid", &poll1, &poll2) {
        ProbeOutcome::Unstable => {}
        ProbeOutcome::Stable { .. } => panic!("must not read as Stable — the sets share nothing"),
        ProbeOutcome::Inconclusive(_) => {
            panic!("must not read as Inconclusive — two-value total churn is a definite finding")
        }
    }
}

#[test]
fn probe_identity_stability_single_value_disjoint_is_inconclusive_not_unstable() {
    // A lone value that disappears and is replaced by a different lone
    // value cannot be told apart from "that row was deleted and an
    // unrelated row was added" — n=1 carries no power to prove the key
    // was rewritten in place, so it must not be allowed to declare the
    // key volatile the way a two-or-more-value total churn can.
    let poll1 = vec![candidate_with_uid("a", "A", "x")];
    let poll2 = vec![candidate_with_uid("a", "B", "x")];

    match probe_identity_stability("uid", &poll1, &poll2) {
        ProbeOutcome::Inconclusive(ProbeInconclusiveCause::SingleValueDisjoint) => {}
        ProbeOutcome::Inconclusive(_) => panic!("must carry the SingleValueDisjoint cause specifically"),
        ProbeOutcome::Unstable => {
            panic!("must not read as Unstable — a single disjoint value cannot prove volatility")
        }
        ProbeOutcome::Stable { .. } => panic!("must not read as Stable — nothing from poll1 survived"),
    }
}

#[test]
fn probe_identity_stability_partial_overlap_is_inconclusive() {
    // Something from poll1 persisted (A) but something else vanished
    // (B) — without a join key this can't be distinguished from B's row
    // being deleted (ordinary membership churn) versus B's value being
    // rewritten (genuine instability), so it must not be forced into
    // either definite verdict.
    let poll1 = vec![candidate_with_uid("a", "A", "x"), candidate_with_uid("b", "B", "x")];
    let poll2 = vec![candidate_with_uid("a", "A", "x"), candidate_with_uid("c", "C", "x")];

    match probe_identity_stability("uid", &poll1, &poll2) {
        ProbeOutcome::Inconclusive(ProbeInconclusiveCause::PartialOverlap { persisted, vanished }) => {
            assert_eq!(persisted, 1);
            assert_eq!(vanished, 1);
        }
        ProbeOutcome::Inconclusive(_) => panic!("must carry the PartialOverlap cause specifically"),
        ProbeOutcome::Stable { .. } => panic!("must not read as Stable — `B` did not survive into poll2"),
        ProbeOutcome::Unstable => {
            panic!("must not read as Unstable — `A` surviving into poll2 is not total churn")
        }
    }
}

#[tokio::test]
async fn authoring_probe_partial_overlap_keeps_the_proposed_native_id_rung_and_flags_a_cause_specific_reason() {
    // An Inconclusive from a vanished value (partial overlap) must still
    // record a non-empty, cause-specific reason on the scratchpad and
    // must NOT drop a rung — the bound contract keeps the proposed
    // native_id strategy exactly like the NoObservations cause does.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-partial-overlap-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let proposal = native_id_proposal(None);
    let poll1 = vec![candidate_with_uid("a", "A", "x"), candidate_with_uid("b", "B", "x")];
    // `A` persists into poll2, `B` vanishes (replaced by `C`) — partial
    // overlap, not total churn and not a subset.
    let poll2 = vec![candidate_with_uid("a", "A", "x"), candidate_with_uid("c", "C", "x")];

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: poll1, proposed_contract: Some(proposal) })],
        vec![Ok(poll2)],
    ));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None)
    .await;

    assert!(!fired, "an authoring run must never fire");

    let contract = stored_contract(&persistence, "watch-partial-overlap-1")
        .await
        .expect("an inconclusive probe must still bind — the rung is kept, never dropped, on Inconclusive");
    assert_eq!(
        contract.identity.strategy,
        IdentityStrategy::NativeId,
        "partial overlap is Inconclusive, not Unstable — dropping a rung is reserved for a positive finding"
    );

    let scratchpad = persistence.assignment_scratchpads.get("watch-partial-overlap-1").await.unwrap().unwrap();
    assert!(
        scratchpad.identity_probe_inconclusive,
        "the inconclusive probe outcome must be recorded on the scratchpad, not left silent"
    );
    let reason = scratchpad
        .identity_probe_inconclusive_reason
        .as_ref()
        .expect("a human-readable, cause-specific reason must accompany the flag");
    assert!(!reason.is_empty(), "the reason must not be empty");
    assert!(reason.contains("uid"), "the reason must name the field that was probed: {reason}");
    assert!(
        reason.contains("deleted"),
        "the partial-overlap reason must describe the vanished-value cause, not a generic message: {reason}"
    );
}

#[tokio::test]
async fn authoring_probe_inconclusive_keeps_the_proposed_native_id_rung_and_flags_it_on_the_scratchpad() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-inconclusive-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let proposal = native_id_proposal(None);
    let poll1 = vec![candidate_with_uid("a", "row-1", "x")];
    // The probe's second poll comes back with zero candidates — there is
    // nothing to compare `uid`'s value against, so the probe cannot
    // reach a verdict either way.
    let poll2: Vec<AgentWatchCandidate> = vec![];

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: poll1, proposed_contract: Some(proposal) })],
        vec![Ok(poll2)],
    ));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None)
    .await;

    assert!(!fired, "an authoring run must never fire");

    let contract = stored_contract(&persistence, "watch-inconclusive-1")
        .await
        .expect("an inconclusive probe must still bind — the rung is kept, never dropped, on Inconclusive");
    assert_eq!(
        contract.identity.strategy,
        IdentityStrategy::NativeId,
        "Inconclusive must never be treated as Unstable — dropping a rung is reserved for a positive finding"
    );
    assert!(
        contract.identity.rationale.contains("Not verified"),
        "the rationale must disclose that the identity was bound unverified: {}",
        contract.identity.rationale
    );

    let scratchpad = persistence.assignment_scratchpads.get("watch-inconclusive-1").await.unwrap().unwrap();
    assert!(
        scratchpad.identity_probe_inconclusive,
        "the inconclusive probe outcome must be recorded on the scratchpad, not left silent"
    );
    assert!(
        scratchpad.identity_probe_inconclusive_reason.is_some(),
        "a human-readable reason must accompany the flag"
    );
}

// ---------------------------------------------------------------------------
// FIX 2 — `composite_fallback_fields` must subtract `change.material_fields`
// before choosing a rung-drop's composite key, and abort (never construct
// a doomed contract) when the subtraction leaves nothing.
// ---------------------------------------------------------------------------

fn notion_style_fields(names_and_required: &[(&str, bool)]) -> HashMap<String, FieldSpec> {
    names_and_required
        .iter()
        .map(|(name, required)| {
            ((*name).to_string(), FieldSpec { field_type: "string".to_string(), required: *required })
        })
        .collect()
}

#[test]
fn composite_fallback_fields_subtracts_material_fields_before_choosing_a_composite_key() {
    // Mirrors the reported incident's shape exactly: every required
    // extraction field (company/first_name/last_name) is ALSO declared
    // material — a constructor that didn't subtract first would hand
    // `WatchContract::validate` an identity guaranteed to be rejected.
    let fields =
        notion_style_fields(&[("company", true), ("first_name", true), ("last_name", true), ("row_id", true)]);
    let material_fields =
        vec!["company".to_string(), "first_name".to_string(), "last_name".to_string()];

    let fallback = composite_fallback_fields(&fields, &material_fields)
        .expect("a non-material required field (row_id) remains available");
    assert_eq!(fallback, vec!["row_id".to_string()], "every material field must be excluded from the fallback");
}

#[test]
fn composite_fallback_fields_aborts_when_subtracting_material_fields_empties_the_set() {
    // Same incident shape, but with no non-material field available at
    // all: every field the extraction contract knows about is material.
    let fields = notion_style_fields(&[("company", true), ("first_name", true), ("last_name", true)]);
    let material_fields =
        vec!["company".to_string(), "first_name".to_string(), "last_name".to_string()];

    assert!(
        composite_fallback_fields(&fields, &material_fields).is_err(),
        "when every available field is material, the constructor must abort rather than build a contract \
         `WatchContract::validate` is guaranteed to reject"
    );
}

/// Builds a `native_id` proposal shaped exactly like the reported
/// incident: a Notion-database-style watch whose only declared fields
/// are also declared material, plus whichever extra fields the caller
/// names (non-material, so a valid composite fallback exists). No
/// `identity.format` — the format cross-check is a separate, already
/// covered concern and would only add noise here.
fn material_overlap_scenario_proposal(extra_available_fields: &[&str]) -> serde_json::Value {
    let mut fields = serde_json::Map::new();
    for name in ["company", "first_name", "last_name"] {
        fields.insert(name.to_string(), serde_json::json!({ "type": "string", "required": true }));
    }
    for name in extra_available_fields {
        fields.insert((*name).to_string(), serde_json::json!({ "type": "string", "required": true }));
    }
    serde_json::json!({
        "source": { "kind": "database", "ref": "clients" },
        "identity": {
            "strategy": "native_id",
            "source_field": "url",
            "format": null,
            "fields": [],
            "rationale": "this source exposes a stable per-row url"
        },
        "change": { "material_fields": ["company", "first_name", "last_name"] },
        // Targets "url" (the identity source_field, present on every
        // candidate payload but not itself one of `fields`'s
        // `required: true` entries) rather than "company" — see the
        // matching comment on `native_id_proposal` for why a `NotEmpty`
        // predicate must not target a field this proposal also marks
        // required.
        "predicate": { "natural_language": "", "fields": [], "expr": "not_empty(url)" },
        "fields": serde_json::Value::Object(fields)
    })
}

#[tokio::test]
async fn rung_drop_subtracts_material_fields_and_binds_a_contract_that_passes_validate() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-rung-drop-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let proposal = material_overlap_scenario_proposal(&["row_id"]);
    let poll1 = vec![
        candidate_with_payload(
            "a",
            serde_json::json!({ "url": "u1", "company": "Acme", "first_name": "Jane", "last_name": "Doe", "row_id": "r1" }),
        ),
        candidate_with_payload(
            "b",
            serde_json::json!({ "url": "u3", "company": "Acme", "first_name": "Jane", "last_name": "Doe", "row_id": "r2" }),
        ),
    ];
    // Both rows' `url` values change between the two probe polls, and
    // poll1 already carried two distinct values — total churn of that
    // size is the positive Unstable finding that triggers the rung-drop
    // this test is about (an Inconclusive probe would keep the
    // native_id rung instead — see the subset-semantics tests above;
    // a single-row churn can't tell a rewrite apart from a delete-then-add).
    let poll2 = vec![
        candidate_with_payload(
            "a",
            serde_json::json!({ "url": "u2", "company": "Acme", "first_name": "Jane", "last_name": "Doe", "row_id": "r1" }),
        ),
        candidate_with_payload(
            "b",
            serde_json::json!({ "url": "u4", "company": "Acme", "first_name": "Jane", "last_name": "Doe", "row_id": "r2" }),
        ),
    ];

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: poll1, proposed_contract: Some(proposal) })],
        vec![Ok(poll2)],
    ));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None)
    .await;

    assert!(!fired);

    let contract = stored_contract(&persistence, "watch-rung-drop-1")
        .await
        .expect("a rung-drop with a non-material field available must still bind");
    assert_eq!(contract.identity.strategy, IdentityStrategy::CompositeNative);
    assert_eq!(
        contract.identity.fields,
        vec!["row_id".to_string()],
        "the material fields must be excluded from the composite identity"
    );
    assert!(
        contract.validate().is_ok(),
        "the constructed contract must pass the exact validator that will judge it on every future poll"
    );
}

#[tokio::test]
async fn rung_drop_aborts_with_a_correctly_attributed_error_when_every_available_field_is_material() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-rung-drop-2", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();
    let mut health_rx = event_bus.subscribe();

    // The exact reported incident: no field besides company/first_name/
    // last_name is declared at all, and all three are material.
    let proposal = material_overlap_scenario_proposal(&[]);
    // Two rows whose `url` totally churns between polls (two distinct
    // values on poll1, fully disjoint from poll2) — the positive
    // Unstable finding needed to trigger the rung-drop this test is
    // about; a single-row churn would be Inconclusive instead.
    let poll1 = vec![
        candidate_with_payload(
            "a",
            serde_json::json!({ "url": "u1", "company": "Acme", "first_name": "Jane", "last_name": "Doe" }),
        ),
        candidate_with_payload(
            "b",
            serde_json::json!({ "url": "u3", "company": "Acme", "first_name": "Jane", "last_name": "Doe" }),
        ),
    ];
    let poll2 = vec![
        candidate_with_payload(
            "a",
            serde_json::json!({ "url": "u2", "company": "Acme", "first_name": "Jane", "last_name": "Doe" }),
        ),
        candidate_with_payload(
            "b",
            serde_json::json!({ "url": "u4", "company": "Acme", "first_name": "Jane", "last_name": "Doe" }),
        ),
    ];

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: poll1, proposed_contract: Some(proposal) })],
        vec![Ok(poll2)],
    ));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None)
    .await;

    assert!(!fired);
    assert!(
        stored_contract(&persistence, "watch-rung-drop-2").await.is_none(),
        "a rung-drop that cannot construct any composite identity must never bind"
    );

    let scratchpad = persistence.assignment_scratchpads.get("watch-rung-drop-2").await.unwrap().unwrap();
    let reason = scratchpad
        .last_authoring_rejection_reason
        .as_deref()
        .expect("the rejection reason must be persisted (FIX 3) even for this engine-side failure");
    assert!(
        !reason.to_lowercase().contains("proposal failed validation"),
        "must not read as though the model proposed something invalid; got: {reason}"
    );
    assert!(
        reason.to_lowercase().contains("material"),
        "the reason must name the actual cause (every field is material); got: {reason}"
    );

    let texts = drain_system_message_texts(&mut health_rx);
    assert!(
        texts.iter().any(|t| t.to_lowercase().contains("material") && !t.contains("proposed a contract that didn't pass validation")),
        "the health event must attribute the failure to the engine's construction limit, not the proposal; got: {texts:?}"
    );
}

#[tokio::test]
async fn authoring_run_fires_nothing_even_when_every_check_passes() {
    // Distinguishes "an authoring run never fires" from "an authoring
    // run never fires because it happens to be a watch's first poll" —
    // this covers the fully-successful-authoring path specifically.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-author-8", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    // content_hash: no probe, so this authors successfully in one poll.
    let mut proposal = native_id_proposal(None);
    proposal["identity"]["strategy"] = serde_json::json!("content_hash");
    proposal["identity"]["fields"] = serde_json::json!(["name"]);
    proposal["identity"].as_object_mut().unwrap().remove("source_field");
    let poll1 = vec![candidate_with_payload("a", serde_json::json!({ "name": "Alice", "tag": "Very Important" }))];

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: poll1, proposed_contract: Some(proposal) })],
        vec![],
    ));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired, "seed, do not act — even a same-tick authoring success must not fire");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched on an authoring run");
    assert!(stored_contract(&persistence, "watch-author-8").await.is_some());
}

#[tokio::test]
async fn authoring_binds_a_new_only_proposal_with_empty_material_fields() {
    // The exact proposal shape that was structurally unsatisfiable
    // before this fix: `mode: new_only` with `change.material_fields`
    // left empty — existence of the item is the whole event, so there is
    // no prior version to diff and nothing to declare as material.
    // Before `WatchContract::validate` branched on `mode`, this proposal
    // was rejected outright no matter how the model phrased it.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-author-new-only-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let mut proposal = content_hash_proposal();
    proposal["mode"] = serde_json::json!("new_only");
    proposal["change"]["material_fields"] = serde_json::json!([]);
    let poll1 = vec![candidate_with_payload("a", serde_json::json!({ "id": "a", "tag": "Very Important" }))];

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: poll1, proposed_contract: Some(proposal) })],
        vec![],
    ));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired, "seed, do not act — even a same-tick authoring success must not fire");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched on an authoring run");
    let contract = stored_contract(&persistence, "watch-author-new-only-1")
        .await
        .expect("a new_only proposal with empty material_fields must bind, not be rejected");
    assert_eq!(contract.mode, WatchMode::NewOnly);
    assert!(contract.change.material_fields.is_empty());
}

#[test]
fn a_new_only_proposal_that_omits_change_entirely_deserializes_to_an_empty_change_spec() {
    // The live failure this guards against: the authoring prompt tells
    // the model `change` is only required when `mode` isn't `new_only`
    // (`CONTRACT_PROPOSAL_SHAPE`), so a model taking that literally for
    // a "new item appeared" watch omits the `change` key from its
    // proposal entirely — it doesn't just leave `material_fields`
    // empty inside it. `ContractProposal`'s hand-written `Deserialize`
    // must accept that shape, or every such proposal dies at gate one
    // with a "missing field `change`" error that the same-tick repair
    // loop has no arm for, and the watch can never author at all.
    let raw = serde_json::json!({
        "source": { "kind": "notion_database", "ref": "clients-db" },
        "identity": {
            "strategy": "composite_native",
            "fields": ["name", "company"],
            "rationale": "no native page id observed; name+company together identify a row"
        },
        "mode": "new_only",
        "predicate": { "natural_language": "a new client row appeared", "fields": [], "expr": "not_empty(name)" },
        "fields": { "name": { "type": "string", "required": true } }
    });
    let proposal: ContractProposal =
        serde_json::from_value(raw).expect("a new_only proposal must deserialize without a `change` key at all");
    assert_eq!(proposal.mode, WatchMode::NewOnly);
    assert!(proposal.change.material_fields.is_empty());
    assert!(proposal.change.version_hint_field.is_none());
}

#[test]
fn a_predicate_transition_proposal_that_omits_change_is_rejected_at_the_shape_gate() {
    // The other half of the same fix: `new_only` is the ONLY mode that
    // gets to skip `change` — every other mode, including the default
    // when `mode` itself is omitted, must still fail here exactly as it
    // did before `mode` existed. If `change` regressed to "always
    // optional," a `predicate_transition` proposal that forgot it would
    // silently default to an empty `ChangeSpec` and only fail three
    // steps later as an `EmptyMaterialFields` validation error, instead
    // of the immediate, precise shape error it gets today.
    let raw = serde_json::json!({
        "source": { "kind": "notion_database", "ref": "clients-db" },
        "identity": {
            "strategy": "composite_native",
            "fields": ["name", "company"],
            "rationale": "no native page id observed; name+company together identify a row"
        },
        "predicate": { "natural_language": "status changed", "fields": ["status"], "expr": "not_empty(status)" },
        "fields": { "status": { "type": "string", "required": false } }
    });
    let err = serde_json::from_value::<ContractProposal>(raw)
        .expect_err("omitting `change` outside new_only must still fail to deserialize");
    assert!(err.to_string().contains("change"), "error must name the missing field; got: {err}");
}

#[tokio::test]
async fn authoring_binds_a_new_only_proposal_that_omits_change_entirely() {
    // End-to-end reproduction of the reported live failure: a watch
    // over a "new entry added" condition, authored by a model that
    // followed the shape's own guidance and left `change` out of its
    // proposal rather than including it with an empty
    // `material_fields`. Before this fix, `author_contract`'s
    // `serde_json::from_value::<ContractProposal>` call rejected this
    // proposal as `Malformed` — a rejection class the same-tick repair
    // loop has no arm for — so the watch oscillated between rejections
    // forever with no way to converge on a bound contract.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-author-new-only-2", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let mut proposal = content_hash_proposal();
    proposal["mode"] = serde_json::json!("new_only");
    proposal.as_object_mut().unwrap().remove("change");
    let poll1 = vec![candidate_with_payload("a", serde_json::json!({ "id": "a", "tag": "Very Important" }))];

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: poll1, proposed_contract: Some(proposal) })],
        vec![],
    ));

    let fired =
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired, "seed, do not act — even a same-tick authoring success must not fire");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched on an authoring run");
    let contract = stored_contract(&persistence, "watch-author-new-only-2")
        .await
        .expect("a new_only proposal that omits `change` entirely must bind, not be rejected as malformed");
    assert_eq!(contract.mode, WatchMode::NewOnly);
    assert!(contract.change.material_fields.is_empty());
}

// ---------------------------------------------------------------------------
// Tests — same-tick repair loop and the authoring failure ceiling.
// ---------------------------------------------------------------------------

/// [`native_id_proposal`], but switched to `content_hash` (keyed on the
/// candidate payload's own `id` field) so it authors successfully without
/// paying the `native_id` stability probe's extra poll — the shared
/// "eventually succeeds" fixture for the repair-loop and ceiling-reset
/// tests below, which don't want to also have to script a probe response.
fn content_hash_proposal() -> serde_json::Value {
    let mut proposal = native_id_proposal(None);
    proposal["identity"]["strategy"] = serde_json::json!("content_hash");
    proposal["identity"]["fields"] = serde_json::json!(["id"]);
    proposal["identity"].as_object_mut().unwrap().remove("source_field");
    proposal
}

/// The authoring tests' extraction-freeze assertion surface, alongside
/// [`stored_contract`]: fetches `extraction_tool`/`extraction_args` off
/// the persisted assignment record's `AgentWatch` trigger.
async fn stored_extraction(persistence: &PersistenceLayer, assignment_id: &str) -> (Option<String>, Option<serde_json::Value>) {
    let stored = persistence.assignments.get(assignment_id).await.expect("assignment must exist");
    match stored.trigger {
        AssignmentTrigger::AgentWatch { extraction_tool, extraction_args, .. } => (extraction_tool, extraction_args),
        _ => panic!("expected an AgentWatch trigger"),
    }
}

#[tokio::test]
async fn authoring_freezes_extraction_tool_and_args_byte_identical_to_the_self_report() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-freeze-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let mut proposal = content_hash_proposal();
    proposal["tool_used"] = serde_json::json!("list_finance_emails");
    proposal["arguments_used"] = serde_json::json!({ "folder": "finance", "unread_only": true });

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(proposal) })],
        vec![],
    ));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired, "an authoring poll must never fire");
    assert!(stored_contract(&persistence, "watch-freeze-1").await.is_some(), "a valid proposal must still bind");

    let (tool, args) = stored_extraction(&persistence, "watch-freeze-1").await;
    assert_eq!(tool.as_deref(), Some("list_finance_emails"), "the self-reported tool name must freeze verbatim");
    assert_eq!(
        args,
        Some(serde_json::json!({ "folder": "finance", "unread_only": true })),
        "the self-reported arguments must freeze byte-identical to what the model reported"
    );

    let scratchpad = persistence.assignment_scratchpads.get("watch-freeze-1").await.unwrap().unwrap();
    assert!(
        !scratchpad.extraction_plan_degraded,
        "a watch that froze a self-reported tool must not read as degraded"
    );
    assert_eq!(scratchpad.extraction_plan_degraded_reason, None);
}

#[tokio::test]
async fn authoring_without_a_self_report_leaves_extraction_none_persists_a_reason_and_never_infers_from_the_stash() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-freeze-2", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    // Stashed by some other call entirely — proves a bound contract with
    // no self-report never reaches into the stash to guess a tool, even
    // when one exists for a name/args combination a heuristic might
    // otherwise be tempted to pick up.
    payload_stash::global().record(payload_stash::StashedPayload {
        server: "finance_mail".to_string(),
        tool: "list_finance_emails".to_string(),
        args: serde_json::json!({}),
        args_hash: payload_stash::hash_args(&serde_json::json!({})),
        captured_at: Utc::now(),
        structured: Some(serde_json::json!([{ "id": "1" }])),
        text: None,
    });

    // No `tool_used`/`arguments_used` keys at all — the omission case.
    let proposal = content_hash_proposal();

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(proposal) })],
        vec![],
    ));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(!fired, "an authoring poll must never fire");
    assert!(stored_contract(&persistence, "watch-freeze-2").await.is_some(), "the contract itself must still bind");

    let (tool, args) = stored_extraction(&persistence, "watch-freeze-2").await;
    assert_eq!(tool, None, "omitting the self-report must leave extraction_tool None, never inferred from the stash");
    assert_eq!(args, None, "omitting the self-report must leave extraction_args None");

    let scratchpad = persistence.assignment_scratchpads.get("watch-freeze-2").await.unwrap().unwrap();
    assert!(
        scratchpad.extraction_plan_degraded,
        "an omitted self-report must be visible through the same degraded channel the health badge reads"
    );
    let reason = scratchpad.extraction_plan_degraded_reason.expect("a reason must be persisted, not a silent no-op");
    assert!(!reason.trim().is_empty());
}

#[test]
fn authoring_prompt_instructs_read_only_and_stable_arguments_only() {
    let prompt = build_authoring_prompt("watch something", None);

    assert!(
        prompt.to_lowercase().contains("read-only"),
        "the authoring prompt must explicitly constrain tool self-report to read-only tools"
    );
    for verb in ["creates", "updates", "deletes", "moves", "archives", "duplicates", "sends"] {
        assert!(
            prompt.to_lowercase().contains(verb),
            "the authoring prompt must spell out '{verb}' as a disqualifying mutation, not just say \"read-only\""
        );
    }
    assert!(
        prompt.contains("cursor") || prompt.to_lowercase().contains("pagination"),
        "the authoring prompt must warn against freezing pagination cursors"
    );
    assert!(
        prompt.to_lowercase().contains("token"),
        "the authoring prompt must warn against freezing page/continuation tokens"
    );
    assert!(
        prompt.to_lowercase().contains("date") || prompt.to_lowercase().contains("time"),
        "the authoring prompt must warn against freezing absolute date/time bounds"
    );
}

#[tokio::test]
async fn a_rejected_predicate_triggers_a_same_tick_repair_attempt_with_the_error_carried_forward() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-repair-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let mut bad_proposal = native_id_proposal(None);
    bad_proposal["predicate"]["expr"] = serde_json::json!("contains(tag, 'x'"); // unterminated -> parser error
    let good_proposal = content_hash_proposal();

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(good_proposal) }),
        ],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None,
    )
    .await;

    assert!(!fired, "an authoring tick must never fire");
    assert!(
        stored_contract(&persistence, "watch-repair-1").await.is_some(),
        "the repaired second attempt must bind — the queue only had two entries, so this also proves it ran \
         within the same tick"
    );

    let repairs = detector.observed_repairs();
    assert_eq!(repairs.len(), 2, "exactly two authoring attempts must run within the same tick");
    assert!(
        repairs[0].is_none(),
        "the first attempt must carry no repair context — this is this watch's first-ever poll, so there is \
         no persisted cross-poll rejection reason to seed either"
    );
    match repairs[1].as_ref().expect("the second attempt must carry the first attempt's rejection") {
        RepairContext::InvalidPredicate { rejected_expr, error } => {
            assert_eq!(rejected_expr, "contains(tag, 'x'", "the repair context must carry the exact rejected expr");
            assert!(
                error.contains("position"),
                "the repair context must carry the parser's verbatim error message; got: {error}"
            );
        }
        other => panic!("expected an InvalidPredicate repair context, got: {other:?}"),
    }
}

#[tokio::test]
async fn a_repair_attempt_is_not_offered_for_a_non_predicate_rejection() {
    // The repair loop only recognizes a small, closed set of rejection
    // reasons precise enough to hand straight back to the model (see
    // `run_authoring_attempts`). An invalid `identity.format` regex
    // isn't one of them — proven by the authoring queue only ever
    // needing one entry.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-repair-2", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(unrepairable_proposal()) })],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None,
    )
    .await;

    assert!(!fired);
    assert!(stored_contract(&persistence, "watch-repair-2").await.is_none());
    assert_eq!(
        detector.observed_repairs().len(),
        1,
        "a rejection reason no repair arm recognizes must not spend a second authoring attempt"
    );
}

// ---------------------------------------------------------------------------
// FIX 3 — the rejection reason must persist across polls and reach the
// next authoring attempt, and a model-proposed (not engine-constructed)
// `identity`/`material_fields` overlap must be same-tick repairable.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_model_proposed_identity_material_overlap_triggers_a_same_tick_repair_attempt() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-repair-overlap-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    // The model proposes a composite_native identity directly (not via
    // the engine's own rung-drop, which FIX 2 makes overlap-proof) whose
    // `fields` overlaps `change.material_fields` on "tag" —
    // `WatchContract::validate` catches this immediately, before any
    // stability probe runs (composite_native never probes).
    let mut bad_proposal = native_id_proposal(None);
    bad_proposal["identity"]["strategy"] = serde_json::json!("composite_native");
    bad_proposal["identity"]["fields"] = serde_json::json!(["uid", "tag"]);
    let good_proposal = content_hash_proposal();

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(good_proposal) }),
        ],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None,
    )
    .await;

    assert!(!fired);
    assert!(
        stored_contract(&persistence, "watch-repair-overlap-1").await.is_some(),
        "the repaired second attempt must bind within the same tick"
    );

    let repairs = detector.observed_repairs();
    assert_eq!(repairs.len(), 2, "exactly two authoring attempts must run within the same tick");
    assert!(repairs[0].is_none());
    match repairs[1].as_ref().expect("the second attempt must carry the first attempt's rejection") {
        RepairContext::IdentityMaterialFieldOverlap { fields } => {
            assert_eq!(fields, &vec!["tag".to_string()], "must name the exact overlapping field");
        }
        other => panic!("expected an IdentityMaterialFieldOverlap repair context, got: {other:?}"),
    }
}

#[tokio::test]
async fn a_second_attempt_repairs_empty_material_fields_within_the_same_tick() {
    // Unlocks the `new_only` mode this whole feature exists for: before
    // this repair arm existed, a proposal with an empty
    // `change.material_fields` (the exact shape a `new_only` proposal
    // should be free to submit) was rejected outright and only retried a
    // whole poll later. This proves the mechanical fix — declare a
    // material field, or switch `mode` — is now offered same-tick.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-repair-empty-material-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let mut bad_proposal = native_id_proposal(None);
    bad_proposal["change"]["material_fields"] = serde_json::json!([]);
    let good_proposal = content_hash_proposal();

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(good_proposal) }),
        ],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None,
    )
    .await;

    assert!(!fired);
    assert!(
        stored_contract(&persistence, "watch-repair-empty-material-1").await.is_some(),
        "the repaired second attempt must bind within the same tick"
    );

    let repairs = detector.observed_repairs();
    assert_eq!(repairs.len(), 2, "exactly two authoring attempts must run within the same tick");
    assert!(repairs[0].is_none());
    match repairs[1].as_ref().expect("the second attempt must carry the first attempt's rejection") {
        RepairContext::EmptyMaterialFields => {}
        other => panic!("expected an EmptyMaterialFields repair context, got: {other:?}"),
    }
}

#[tokio::test]
async fn a_required_field_targeted_by_not_empty_is_auto_repaired_in_code_without_a_model_call() {
    // `WatchContract::validate`'s contradiction check (`required: true`
    // paired with a `not_empty` predicate leaf on the same field) used
    // to bounce back to the model as a same-tick repair. It is now fixed
    // deterministically in code (`auto_repair_contract`) before the
    // proposal is ever rejected — proven here by the scripted queue
    // holding only ONE entry: a second authoring call would panic the
    // detector, so this test can only pass if the contradiction never
    // reaches `run_authoring_attempts` at all.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-repair-required-not-empty-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    // `content_hash_proposal`'s own fixture deliberately targets "tag"
    // (not "uid", the required field) to stay otherwise-valid for every
    // OTHER test that reuses it — retargeting the predicate at "uid"
    // here is what turns it into this exact contradiction. Built from
    // the content_hash (not native_id) fixture specifically so this
    // test never has to script the native_id stability probe's extra
    // `observe` poll — irrelevant to what this test is about.
    let mut proposal = content_hash_proposal();
    proposal["predicate"]["expr"] = serde_json::json!("not_empty(uid)");

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(proposal) })],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None,
    )
    .await;

    assert!(!fired, "an authoring tick must never fire");
    let contract = stored_contract(&persistence, "watch-repair-required-not-empty-1")
        .await
        .expect("the auto-repaired proposal must bind on its very first attempt");
    assert_eq!(
        contract.fields.get("uid").map(|f| f.required),
        Some(false),
        "the contradiction must be resolved by dropping `required`, not by rejecting the proposal"
    );
    assert!(contract.validate().is_ok(), "the auto-repaired contract must pass the validator it was repaired for");

    let repairs = detector.observed_repairs();
    assert_eq!(
        repairs.len(),
        1,
        "the repair happens in code before any rejection is constructed — only one authoring call is ever made"
    );
    assert!(repairs[0].is_none(), "the one attempt made carries no repair context — nothing was ever rejected");
}

#[test]
fn contract_proposal_deserializes_with_change_omitted_when_mode_is_new_only() {
    // `CONTRACT_PROPOSAL_SHAPE` documents `change` as "required unless
    // mode is new_only." Before `ChangeSpec` derived `Default` and
    // `ContractProposal`'s hand-written `Deserialize` grew the matching
    // `None if mode == NewOnly` carve-out, a proposal that followed that
    // documented shape literally — omitting `change` under `new_only` —
    // was rejected as `Malformed` on a technicality the shape's own text
    // said shouldn't apply. This is that exact shape.
    let mut json = content_hash_proposal();
    json["mode"] = serde_json::json!("new_only");
    json.as_object_mut().expect("proposal fixture must be a JSON object").remove("change");

    let proposal: ContractProposal = serde_json::from_value(json)
        .expect("omitting `change` under mode: new_only must deserialize, not raise a missing-field error");
    assert_eq!(proposal.mode, WatchMode::NewOnly);
    assert!(proposal.change.material_fields.is_empty(), "an omitted `change` must default, not carry stale data");
}

#[tokio::test]
async fn two_distinct_same_tick_rejections_are_both_carried_into_the_next_polls_repair_context() {
    // Proves the accumulation fix (2) directly: two DIFFERENT proposals,
    // rejected for two DIFFERENT reasons, both within the same tick's
    // same-tick repair budget — then a second tick's first attempt must
    // be told about BOTH, not just whichever was rejected most recently.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-accumulate-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let mut empty_material_proposal = content_hash_proposal();
    empty_material_proposal["change"]["material_fields"] = serde_json::json!([]);

    let mut overlap_proposal = content_hash_proposal();
    overlap_proposal["identity"]["fields"] = serde_json::json!(["id", "tag"]); // "tag" is also a material field

    let good_proposal = content_hash_proposal();

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            // Tick 1, attempt 0: rejected (EmptyMaterialFields).
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(empty_material_proposal) }),
            // Tick 1, attempt 1 (same-tick repair offered): rejected
            // again, for a DIFFERENT reason (IdentityMaterialFieldOverlap)
            // — exhausts this tick's same-tick budget without binding.
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(overlap_proposal) }),
            // Tick 2, attempt 0: a fully valid proposal — must bind.
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(good_proposal) }),
        ],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    let fired_first = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None,
    )
    .await;
    assert!(!fired_first);
    assert!(
        stored_contract(&persistence, "watch-accumulate-1").await.is_none(),
        "neither of the first tick's two proposals is valid — nothing should bind yet"
    );

    let scratchpad_after_first =
        persistence.assignment_scratchpads.get("watch-accumulate-1").await.unwrap().unwrap();
    assert_eq!(
        scratchpad_after_first.authoring_rejection_history.len(),
        2,
        "both distinct rejections from the first tick must be accumulated, not just the latest; got: {:?}",
        scratchpad_after_first.authoring_rejection_history
    );

    let fired_second = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None,
    )
    .await;
    assert!(!fired_second, "an authoring tick must never fire");
    assert!(
        stored_contract(&persistence, "watch-accumulate-1").await.is_some(),
        "the second tick's valid proposal must bind"
    );

    let repairs = detector.observed_repairs();
    assert_eq!(repairs.len(), 3, "tick 1 spends two attempts, tick 2 binds on its first");
    assert!(repairs[0].is_none(), "tick 1's first attempt has no prior rejection to seed from");
    match repairs[2].as_ref().expect("tick 2's first attempt must carry the accumulated cross-poll history") {
        RepairContext::Accumulated(items) => {
            assert_eq!(items.len(), 2, "both of tick 1's distinct rejections must reach tick 2 at once");
            let joined = items
                .iter()
                .map(|item| match item {
                    RepairContext::CrossPollRejection { reason } => reason.clone(),
                    other => panic!("expected a CrossPollRejection entry, got: {other:?}"),
                })
                .collect::<Vec<_>>()
                .join(" | ");
            assert!(
                joined.to_lowercase().contains("no material fields declared"),
                "must carry the EmptyMaterialFields reason; got: {joined}"
            );
            assert!(
                joined.to_lowercase().contains("both contain"),
                "must carry the IdentityMaterialFieldOverlap reason; got: {joined}"
            );
        }
        other => panic!("expected an Accumulated repair context carrying both rejections, got: {other:?}"),
    }
}

#[tokio::test]
async fn the_reported_two_cycle_no_longer_oscillates_and_binds_on_the_first_proposal() {
    // The live incident this task exists to fix: an authoring model
    // alternated forever between a proposal with a `required`/`NotEmpty`
    // contradiction on one field and a proposal that omitted `change`
    // entirely — each attempt "fixed" whichever constraint it was told
    // about most recently while silently reintroducing the other, since
    // only the newest rejection was ever shown back to the model.
    //
    // Fix (1) makes this impossible at the root, not just less likely:
    // the contradiction is now repaired in code before it can ever
    // become a rejection at all, so the model's second proposal (the
    // one that used to omit `change`) is never even asked for — proven
    // here by a two-entry scripted queue where only the first entry is
    // ever consumed.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-two-cycle-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    // Half 1 of the reported cycle: "company" is `required: true` and
    // also targeted by `not_empty(company)`.
    let mut contradiction_proposal = content_hash_proposal();
    contradiction_proposal["fields"]["company"] = serde_json::json!({ "type": "string", "required": true });
    contradiction_proposal["predicate"]["expr"] = serde_json::json!("not_empty(company)");

    // Half 2 of the reported cycle: `mode: new_only` with `change`
    // omitted — must never be consumed if fix (1) works, since the
    // queue only has these two entries and the first must bind alone.
    let mut missing_change_proposal = content_hash_proposal();
    missing_change_proposal["mode"] = serde_json::json!("new_only");
    missing_change_proposal.as_object_mut().unwrap().remove("change");

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(contradiction_proposal) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(missing_change_proposal) }),
        ],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None,
    )
    .await;

    assert!(!fired, "an authoring tick must never fire");
    let contract = stored_contract(&persistence, "watch-two-cycle-1")
        .await
        .expect("the first proposal must bind on its own — the cycle's second half must never be needed");
    assert_eq!(
        contract.fields.get("company").map(|f| f.required),
        Some(false),
        "the contradiction must have been auto-repaired, not left to reject the proposal"
    );

    assert_eq!(
        detector.observed_repairs().len(),
        1,
        "the second (missing-change) proposal must never be consumed — the cycle no longer has a second half"
    );
}

#[tokio::test]
async fn a_rejection_reason_persists_across_polls_and_seeds_the_next_authoring_attempts_repair_context() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-cross-poll-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    // An invalid identity.format regex: not same-tick repairable (see
    // the sibling rejection-reason test above), so each of the two polls
    // below spends exactly one authoring attempt each — the cross-poll
    // seeding this test is about is the only mechanism that could carry
    // a rejection reason from the first poll into the second's `repair`.
    let bad_proposal = unrepairable_proposal();

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal.clone()) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal) }),
        ],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None,
    )
    .await;
    let scratchpad_after_first =
        persistence.assignment_scratchpads.get("watch-cross-poll-1").await.unwrap().unwrap();
    let persisted_reason = scratchpad_after_first
        .last_authoring_rejection_reason
        .clone()
        .expect("the first poll's rejection reason must be persisted onto the scratchpad (FIX 3a)");
    assert!(persisted_reason.to_lowercase().contains("regex"), "got: {persisted_reason}");

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None,
    )
    .await;

    let repairs = detector.observed_repairs();
    assert_eq!(
        repairs.len(),
        2,
        "each poll spends exactly one attempt — this rejection reason isn't same-tick repairable"
    );
    assert!(repairs[0].is_none(), "the very first poll ever has no persisted reason yet to seed");
    match repairs[1].as_ref().expect("the second poll's first attempt must carry the persisted cross-poll reason")
    {
        RepairContext::CrossPollRejection { reason } => {
            assert_eq!(
                reason, &persisted_reason,
                "the exact reason persisted after the first poll must be what's fed into the next attempt"
            );
        }
        other => panic!("expected a CrossPollRejection repair context, got: {other:?}"),
    }
}

// -- AssignmentScratchpad::contract_bound_after_failed_attempts ---------

#[tokio::test]
async fn contract_bound_after_failed_attempts_is_none_when_a_watch_binds_cleanly() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-clean-bind-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(content_hash_proposal()) })],
        vec![],
    ));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None)
    .await;

    assert!(
        stored_contract(&persistence, "watch-clean-bind-1").await.is_some(),
        "sanity: the proposal must have bound"
    );
    let scratchpad = persistence.assignment_scratchpads.get("watch-clean-bind-1").await.unwrap().unwrap();
    assert_eq!(
        scratchpad.contract_bound_after_failed_attempts, None,
        "a contract bound on its very first attempt has nothing to report as 'repaired'"
    );
}

#[tokio::test]
async fn contract_bound_after_failed_attempts_counts_prior_failed_polls() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-repaired-bind-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(unrepairable_proposal()) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(content_hash_proposal()) }),
        ],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    // Poll 1: rejected — not same-tick repairable, so this spends
    // exactly one attempt and leaves the cross-poll streak at 1.
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None)
    .await;
    assert!(
        stored_contract(&persistence, "watch-repaired-bind-1").await.is_none(),
        "sanity: poll 1 must not bind"
    );

    // Poll 2: a clean proposal binds — the "loud rejection, then a later
    // poll quietly converges" scenario the panel's convergence banner
    // exists to surface (see `WatchContractStatus::Bound::bound_after_repairs`).
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None)
    .await;

    assert!(
        stored_contract(&persistence, "watch-repaired-bind-1").await.is_some(),
        "poll 2's clean proposal must bind"
    );
    let scratchpad = persistence.assignment_scratchpads.get("watch-repaired-bind-1").await.unwrap().unwrap();
    assert_eq!(
        scratchpad.contract_bound_after_failed_attempts,
        Some(1),
        "the bind must remember it only succeeded after 1 prior failed poll"
    );
    assert_eq!(scratchpad.authoring_failure_streak, 0, "a successful bind always resets the streak");
}

#[tokio::test]
async fn authoring_failure_streak_increments_and_stops_reprompting_at_the_ceiling() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-ceiling-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    // An invalid identity.format regex: not repairable, so every poll
    // below spends exactly one authoring attempt.
    let bad_proposal = unrepairable_proposal();

    let authoring_responses: Vec<_> = (0..AUTHORING_FAILURE_CEILING)
        .map(|_| {
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal.clone()) })
        })
        .collect();
    let detector = Arc::new(ScriptedAuthoringDetector::new(authoring_responses, vec![Ok(vec![candidate("a")])]));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    let mut rx = event_bus.subscribe();

    for _ in 0..AUTHORING_FAILURE_CEILING {
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None)
            .await;
    }

    let scratchpad = persistence.assignment_scratchpads.get("watch-ceiling-1").await.unwrap().unwrap();
    assert_eq!(scratchpad.authoring_failure_streak, AUTHORING_FAILURE_CEILING);

    let ceiling_events: Vec<String> =
        drain_system_message_texts(&mut rx).into_iter().filter(|t| t.contains("stop re-prompting")).collect();
    assert_eq!(ceiling_events.len(), 1, "the ceiling must fire exactly one unhealthy event; got: {ceiling_events:?}");
    assert!(
        ceiling_events[0].contains("regex"),
        "the unhealthy event must carry the actual last validation error; got: {}",
        ceiling_events[0]
    );

    // A poll past the ceiling must not touch the (already-drained)
    // authoring queue at all — if it did, ScriptedAuthoringDetector would
    // panic here ("polled more times than scripted").
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    let scratchpad_after = persistence.assignment_scratchpads.get("watch-ceiling-1").await.unwrap().unwrap();
    assert_eq!(
        scratchpad_after.authoring_failure_streak, AUTHORING_FAILURE_CEILING,
        "the streak must stay frozen once the ceiling is hit"
    );
}

/// Companion to [`authoring_failure_streak_increments_and_stops_reprompting_at_the_ceiling`]
/// above, asserting on message *tone* rather than just count: every
/// per-attempt rejection below the ceiling must read as an ordinary
/// retry (no severity), and only the rejection on the poll that actually
/// reaches the ceiling — plus that poll's own freeze summary — may be
/// tagged [`SystemMessageSeverity::Error`]. A demo watch that converges
/// in 3 attempts must never show the same alarming styling as one that
/// is genuinely stuck.
#[tokio::test]
async fn authoring_rejection_severity_stays_neutral_below_the_ceiling_and_turns_to_error_at_it() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-ceiling-severity-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    // An invalid identity.format regex: not repairable, so every poll
    // below spends exactly one authoring attempt.
    let bad_proposal = unrepairable_proposal();

    let authoring_responses: Vec<_> = (0..AUTHORING_FAILURE_CEILING)
        .map(|_| {
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal.clone()) })
        })
        .collect();
    let detector = Arc::new(ScriptedAuthoringDetector::new(authoring_responses, vec![Ok(vec![candidate("a")])]));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    let mut rx = event_bus.subscribe();

    for _ in 0..AUTHORING_FAILURE_CEILING {
        run_agent_watch_tick(
            &persistence,
            &dispatcher,
            &event_bus,
            &detector_dyn,
            &Arc::new(Registry::new()),
            &assignment,
            "watch",
            None,
        )
        .await;
    }

    let messages = drain_system_messages(&mut rx);
    let per_attempt: Vec<(String, Option<SystemMessageSeverity>)> = messages
        .iter()
        .filter(|(text, _)| text.contains("adjusting its watch contract") || text.contains("didn't pass validation"))
        .cloned()
        .collect();
    assert_eq!(
        per_attempt.len(),
        AUTHORING_FAILURE_CEILING as usize,
        "one per-attempt rejection message per poll; got: {messages:?}"
    );

    for (text, severity) in &per_attempt[..per_attempt.len() - 1] {
        assert_eq!(
            *severity, None,
            "a sub-ceiling rejection is a normal retry, not an error, and must carry no severity; got: {text}"
        );
        assert!(
            text.contains("adjusting its watch contract"),
            "a sub-ceiling rejection must read as progress, not failure; got: {text}"
        );
    }
    let (last_text, last_severity) = per_attempt.last().unwrap();
    assert_eq!(
        *last_severity,
        Some(SystemMessageSeverity::Error),
        "the rejection on the poll that reaches the ceiling must be tagged as an error; got: {last_text}"
    );

    let summaries: Vec<_> = messages.iter().filter(|(t, _)| t.contains("stop re-prompting")).collect();
    assert_eq!(summaries.len(), 1, "the freeze summary must fire exactly once; got: {messages:?}");
    assert_eq!(
        summaries[0].1,
        Some(SystemMessageSeverity::Error),
        "the freeze summary is the genuine, user-actionable failure and must be tagged as an error"
    );
}

/// Regression test for the phantom-refire bug: once a watch is frozen at
/// [`AUTHORING_FAILURE_CEILING`], it has no stable identity to diff
/// on — only the model's disposable free-text `id`, which the system
/// prompt itself tells the model to regenerate however it likes across
/// polls. Before the `seed_only` fix, `run_legacy_seen_ids_tick` diffed
/// on that drifting `id` regardless, so a frozen watch re-fired on the
/// same underlying row every single poll. This drives a watch to the
/// ceiling, then polls it twice more with two DIFFERENT free-text ids
/// (simulating the model reporting the same real-world item differently
/// each time) and asserts neither poll fires.
#[tokio::test]
async fn frozen_watch_does_not_refire_on_drifting_free_text_ids() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-frozen-drift-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    // An invalid identity.format regex: not repairable, so every poll
    // below spends exactly one authoring attempt and never binds.
    let bad_proposal = unrepairable_proposal();

    let authoring_responses: Vec<_> = (0..AUTHORING_FAILURE_CEILING)
        .map(|_| {
            Ok(AuthoringReply { candidates: vec![candidate("seed")], proposed_contract: Some(bad_proposal.clone()) })
        })
        .collect();
    // Two frozen-tick polls after the ceiling, each reporting the SAME
    // conceptual row under a DIFFERENT model-authored free-text `id`.
    let observe_responses =
        vec![Ok(vec![candidate("peter-grace-row-poll-6")]), Ok(vec![candidate("peter-grace-row-poll-7")])];
    let detector = Arc::new(ScriptedAuthoringDetector::new(authoring_responses, observe_responses));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    for _ in 0..AUTHORING_FAILURE_CEILING {
        run_agent_watch_tick(
            &persistence,
            &dispatcher,
            &event_bus,
            &detector_dyn,
            &Arc::new(Registry::new()),
            &assignment,
            "watch",
            None,
        )
        .await;
    }
    let scratchpad = persistence.assignment_scratchpads.get("watch-frozen-drift-1").await.unwrap().unwrap();
    assert_eq!(scratchpad.authoring_failure_streak, AUTHORING_FAILURE_CEILING);

    let fired_poll_6 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;
    assert!(!fired_poll_6, "a frozen watch must not fire off the legacy id-diff at all");

    // Before the fix, this poll's different free-text id would have
    // diffed as "new" against poll 6's seen_ids and fired again — the
    // phantom refire this test guards against.
    let fired_poll_7 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;
    assert!(!fired_poll_7, "a frozen watch must not re-fire just because the model's free-text id drifted");

    let scratchpad_after = persistence.assignment_scratchpads.get("watch-frozen-drift-1").await.unwrap().unwrap();
    assert_eq!(scratchpad_after.last_new_item_at, None, "no poll should have ever recorded a fire");
    assert!(rx.try_recv().is_err(), "a frozen, drifting-id watch must not dispatch anything to the agent");
}

#[tokio::test]
async fn a_successful_bind_resets_the_authoring_failure_streak() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-reset-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let bad_proposal = unrepairable_proposal();
    let good_proposal = content_hash_proposal();

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal.clone()) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(good_proposal) }),
        ],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    for _ in 0..2 {
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None)
            .await;
    }
    let scratchpad_before_bind = persistence.assignment_scratchpads.get("watch-reset-1").await.unwrap().unwrap();
    assert_eq!(
        scratchpad_before_bind.authoring_failure_streak, 2,
        "sanity: two consecutive failures must have accrued"
    );
    assert!(
        scratchpad_before_bind.last_authoring_rejection_reason.is_some(),
        "sanity: a rejection reason must have accrued alongside the streak"
    );

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    let scratchpad_after_bind = persistence.assignment_scratchpads.get("watch-reset-1").await.unwrap().unwrap();
    assert_eq!(scratchpad_after_bind.authoring_failure_streak, 0, "a successful bind must reset the streak");
    assert_eq!(
        scratchpad_after_bind.last_authoring_rejection_reason, None,
        "a successful bind must clear the persisted rejection reason too"
    );
    assert!(stored_contract(&persistence, "watch-reset-1").await.is_some());
}

/// The demo bug this module's authoring UX exists to fix: two rejections
/// followed by a bind (mirrors [`a_successful_bind_resets_the_authoring_failure_streak`]'s
/// fixture) must not go silent on the poll that actually converges —
/// exactly one [`SystemMessageSeverity::Success`] message fires, naming
/// the attempt it bound on.
#[tokio::test]
async fn authoring_convergence_emits_a_success_message_naming_the_attempt() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-convergence-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let bad_proposal = unrepairable_proposal();
    let good_proposal = content_hash_proposal();

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal.clone()) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal) }),
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(good_proposal) }),
        ],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    let mut rx = event_bus.subscribe();

    for _ in 0..3 {
        run_agent_watch_tick(
            &persistence,
            &dispatcher,
            &event_bus,
            &detector_dyn,
            &Arc::new(Registry::new()),
            &assignment,
            "watch",
            None,
        )
        .await;
    }

    assert!(stored_contract(&persistence, "watch-convergence-1").await.is_some(), "sanity: the contract must bind");

    let messages = drain_system_messages(&mut rx);
    let success: Vec<_> =
        messages.iter().filter(|(_, severity)| *severity == Some(SystemMessageSeverity::Success)).collect();
    assert_eq!(success.len(), 1, "convergence must emit exactly one success message; got: {messages:?}");
    assert!(
        success[0].0.contains("attempt 3 of"),
        "the success message must name which attempt converged; got: {}",
        success[0].0
    );
    assert!(
        success[0].0.to_lowercase().contains("authored"),
        "the success message must say the contract was authored; got: {}",
        success[0].0
    );
}

/// A user reading the convergence success message must be able to tell
/// whether the bound contract matches what they asked for without
/// separately looking up the persisted contract — so the message must
/// also name which mode was settled on and which fields make it fire,
/// not just that authoring succeeded.
#[tokio::test]
async fn authoring_convergence_success_message_names_mode_and_material_fields() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-convergence-mode-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    // `content_hash_proposal` carries `change.material_fields: ["tag"]`
    // and no explicit `mode`, so it settles on the default
    // `predicate_transition`.
    let good_proposal = content_hash_proposal();
    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(good_proposal) })],
        vec![],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    let mut rx = event_bus.subscribe();
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;

    assert!(
        stored_contract(&persistence, "watch-convergence-mode-1").await.is_some(),
        "sanity: the contract must bind"
    );

    let messages = drain_system_messages(&mut rx);
    let success: Vec<_> =
        messages.iter().filter(|(_, severity)| *severity == Some(SystemMessageSeverity::Success)).collect();
    assert_eq!(success.len(), 1, "convergence must emit exactly one success message; got: {messages:?}");
    assert!(
        success[0].0.contains("predicate_transition"),
        "the success message must name which mode the contract settled on; got: {}",
        success[0].0
    );
    assert!(
        success[0].0.contains("tag"),
        "the success message must name the material field(s) that make it fire; got: {}",
        success[0].0
    );
}

/// Regression test for the never-bound terminal-failure case: a watch
/// that has *never* bound a contract has no `contract_fingerprint` for
/// the orphaned-fingerprint reset in `run_agent_watch_tick` to key off
/// (that field is only ever set once a contract binds — see
/// `orphaned_fingerprint_and_contract_derived_state_are_reset_once_the_contract_is_cleared_externally`),
/// so once `authoring_failure_streak` hits the ceiling it must stay
/// parked there indefinitely absent an edit — confirming the diagnosis
/// this module's authoring-recovery path exists to fix, not just the one
/// poll immediately after the ceiling is hit.
#[tokio::test]
async fn never_bound_authoring_ceiling_stays_frozen_across_many_polls_with_no_edit() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-never-bound-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let bad_proposal = unrepairable_proposal();

    let authoring_responses: Vec<_> = (0..AUTHORING_FAILURE_CEILING)
        .map(|_| {
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal.clone()) })
        })
        .collect();
    // Three extra observe() polls past the ceiling, none of which may
    // touch the (already-drained) authoring_responses queue.
    let observe_responses = vec![Ok(vec![candidate("a")]), Ok(vec![candidate("a")]), Ok(vec![candidate("a")])];
    let detector = Arc::new(ScriptedAuthoringDetector::new(authoring_responses, observe_responses));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    for _ in 0..AUTHORING_FAILURE_CEILING {
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None)
            .await;
    }

    let scratchpad = persistence.assignment_scratchpads.get("watch-never-bound-1").await.unwrap().unwrap();
    assert_eq!(scratchpad.authoring_failure_streak, AUTHORING_FAILURE_CEILING);
    assert_eq!(
        scratchpad.contract_fingerprint, None,
        "sanity: this watch never bound a contract, so the orphaned-fingerprint reset has nothing to key off of"
    );

    let mut rx = event_bus.subscribe();
    for _ in 0..3 {
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None)
            .await;
    }

    let scratchpad_after = persistence.assignment_scratchpads.get("watch-never-bound-1").await.unwrap().unwrap();
    assert_eq!(
        scratchpad_after.authoring_failure_streak, AUTHORING_FAILURE_CEILING,
        "with no edit, the never-bound watch must stay parked at the ceiling indefinitely, not just for one poll"
    );
    let texts = drain_system_message_texts(&mut rx);
    assert!(
        !texts.iter().any(|t| t.contains("resuming")),
        "no edit occurred, so authoring must never silently resume; got: {texts:?}"
    );
}

/// Regression test for the fix itself: editing the `AgentWatch`
/// trigger's `instruction` (or `connector_scope`) after a never-bound
/// watch has parked at the authoring ceiling must lift the freeze on the
/// very next poll — the health event's stated remedy ("edit the
/// instruction") must actually do something, not be a no-op.
#[tokio::test]
async fn editing_instruction_after_the_authoring_ceiling_resumes_authoring_and_clears_the_streak() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-edit-recovers-1", "agent-1");
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let bad_proposal = unrepairable_proposal();

    // AUTHORING_FAILURE_CEILING attempts to climb to the ceiling, plus
    // one more for the post-edit retry this test drives.
    let authoring_responses: Vec<_> = (0..(AUTHORING_FAILURE_CEILING + 1))
        .map(|_| {
            Ok(AuthoringReply { candidates: vec![candidate("a")], proposed_contract: Some(bad_proposal.clone()) })
        })
        .collect();
    let detector = Arc::new(ScriptedAuthoringDetector::new(authoring_responses, vec![]));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    for _ in 0..AUTHORING_FAILURE_CEILING {
        run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch", None)
            .await;
    }
    let scratchpad_before_edit =
        persistence.assignment_scratchpads.get("watch-edit-recovers-1").await.unwrap().unwrap();
    assert_eq!(
        scratchpad_before_edit.authoring_failure_streak, AUTHORING_FAILURE_CEILING,
        "sanity: the watch must be parked at the ceiling before the simulated edit"
    );

    let mut rx = event_bus.subscribe();
    // The instruction argument changing between polls is exactly what a
    // real edit produces — `tick_agent_watches` always passes the live
    // `AssignmentTrigger::AgentWatch.instruction` on every poll.
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()), &assignment, "watch v2", None)
        .await;

    let scratchpad_after_edit =
        persistence.assignment_scratchpads.get("watch-edit-recovers-1").await.unwrap().unwrap();
    assert_eq!(
        scratchpad_after_edit.authoring_failure_streak, 1,
        "the edit must reset the streak to 0 and this poll must make a genuine (still-failing) authoring \
         attempt, landing at 1 — not stay pinned at the ceiling"
    );
    assert_eq!(
        detector.observed_repairs().len() as u32,
        AUTHORING_FAILURE_CEILING + 1,
        "the post-edit poll must have actually called observe_for_authoring rather than taking the frozen \
         fallback-observe path"
    );

    let texts = drain_system_message_texts(&mut rx);
    assert!(
        texts.iter().any(|t| t.contains("resuming")),
        "the edit-triggered resume must itself be a visible health event; got: {texts:?}"
    );
}

/// [`dedup_contract`] plus one `required: true` extraction field — the
/// fixture the amendment-trigger tests below drive missing-field polls
/// against.
fn contract_with_required_field(field: &str) -> WatchContract {
    let mut contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    contract
        .fields
        .insert(field.to_string(), FieldSpec { field_type: "string".to_string(), required: true });
    contract
}

#[tokio::test]
async fn amendment_trigger_does_not_fire_after_a_single_missing_required_field_poll() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = contract_with_required_field("name");
    let assignment = agent_watch_assignment_with_contract("watch-amend-1", "agent-1", contract);
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let missing_name = || candidate_with_payload("a", serde_json::json!({ "id": "a" })); // no "name"
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![missing_name()])]));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    assert!(
        stored_contract(&persistence, "watch-amend-1").await.is_some(),
        "a single missing-required-field poll (N=1) must not trigger the amendment"
    );
}

#[tokio::test]
async fn amendment_trigger_fires_at_exactly_two_consecutive_missing_required_field_polls() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = contract_with_required_field("name");
    let assignment = agent_watch_assignment_with_contract("watch-amend-2", "agent-1", contract);
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let missing_name = || candidate_with_payload("a", serde_json::json!({ "id": "a" }));
    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![missing_name()]), Ok(vec![missing_name()])]));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(stored_contract(&persistence, "watch-amend-2").await.is_some(), "not yet at the N=2 threshold");

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(
        stored_contract(&persistence, "watch-amend-2").await.is_none(),
        "two consecutive missing-required-field polls (N=2) must clear the contract so authoring re-runs"
    );
}

#[tokio::test]
async fn missing_required_field_streak_does_not_move_on_a_zero_candidate_poll() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = contract_with_required_field("name");
    let assignment = agent_watch_assignment_with_contract("watch-zero-candidates", "agent-1", contract);
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let missing_name = || candidate_with_payload("a", serde_json::json!({ "id": "a" }));
    // Poll 1 accrues one missing-field poll; polls 2/3 observe nothing
    // parseable at all — a detector failure to extract anything is not
    // evidence about whether the *contract's* required fields are
    // wrong, so these must leave the streak exactly where poll 1 left
    // it, neither incrementing it towards the amendment threshold nor
    // resetting it back to 0.
    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![missing_name()]), Ok(vec![]), Ok(vec![])]));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let after_poll_1 =
        persistence.assignment_scratchpads.get("watch-zero-candidates").await.unwrap().unwrap();
    assert_eq!(after_poll_1.missing_required_field_streak, 1, "sanity: the first missing-field poll must count");

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let after_poll_2 =
        persistence.assignment_scratchpads.get("watch-zero-candidates").await.unwrap().unwrap();
    assert_eq!(
        after_poll_2.missing_required_field_streak, 1,
        "a poll that observed zero candidates must leave the streak untouched, not increment it"
    );

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let after_poll_3 =
        persistence.assignment_scratchpads.get("watch-zero-candidates").await.unwrap().unwrap();
    assert_eq!(after_poll_3.missing_required_field_streak, 1);
    assert!(
        stored_contract(&persistence, "watch-zero-candidates").await.is_some(),
        "zero-candidate polls must never accumulate toward the amendment threshold"
    );
}

/// Regression test for the production incident this module's amendment
/// logic was rewritten to fix: an assignment whose scratchpad still
/// carried a `contract_fingerprint` (and derived streaks) after its
/// `trigger.contract` had already gone `null` through a door with no
/// scratchpad access of its own (an assignment PATCH, an
/// `AssignmentUpdate` tool call — see `carry_forward_watch_contract`).
/// `run_agent_watch_tick`'s orphan-fingerprint fallback must notice this
/// on the very next poll and reset every field only meaningful under
/// the contract that is now gone.
#[tokio::test]
async fn orphaned_fingerprint_and_contract_derived_state_are_reset_once_the_contract_is_cleared_externally() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = contract_with_required_field("name");
    let fingerprint = contract.fingerprint();
    let assignment = agent_watch_assignment_with_contract("watch-orphan-1", "agent-1", contract);
    persistence.assignments.add(assignment.clone()).await.unwrap();

    persistence
        .assignment_scratchpads
        .set(
            "watch-orphan-1",
            &AssignmentScratchpad {
                contract_fingerprint: Some(fingerprint),
                snapshots: vec![ItemSnapshot {
                    identity_key: "a".to_string(),
                    version_key: "v1".to_string(),
                    predicate_value: true,
                    edge_counter: 1,
                    last_seen_at: "2026-07-27T09:00:00Z".to_string(),
                    payload: serde_json::json!({ "id": "a", "name": "Andrew" }),
                }],
                missing_required_field_streak: 1,
                truncation_notified: true,
                authoring_failure_streak: 2,
                contract_amendment_cycle_count: 1,
                all_candidates_quarantined_streak: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // The other side of the fix (already present in the working tree):
    // whatever door clears the contract, it goes through here — an
    // assignment PATCH or `AssignmentUpdate` call sets `contract: None`
    // on the record without touching the scratchpad at all.
    let mut cleared = persistence.assignments.get("watch-orphan-1").await.unwrap();
    if let AssignmentTrigger::AgentWatch { contract: slot, .. } = &mut cleared.trigger {
        *slot = None;
    }
    persistence.assignments.update(cleared.clone()).await.unwrap();

    // A detector that fails outright, so nothing downstream of the
    // orphan-reset itself gets a chance to persist the scratchpad again
    // and mask what is under test here.
    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Err(AgentWatchDetectError::Failed("boom".to_string()))]));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &cleared, "watch", None).await;

    let scratchpad = persistence.assignment_scratchpads.get("watch-orphan-1").await.unwrap().unwrap();
    assert_eq!(scratchpad.contract_fingerprint, None, "the orphaned fingerprint must be cleared");
    assert!(scratchpad.snapshots.is_empty(), "snapshots computed under the now-gone contract must be cleared");
    assert_eq!(scratchpad.missing_required_field_streak, 0);
    assert!(!scratchpad.truncation_notified);
    assert_eq!(scratchpad.authoring_failure_streak, 0);
    assert_eq!(
        scratchpad.contract_amendment_cycle_count, 0,
        "a contract cleared through a door with no scratchpad access is a fresh start, not a continuation \
         of the old contract's amendment history"
    );
    assert_eq!(
        scratchpad.all_candidates_quarantined_streak, 0,
        "the all-quarantined streak is per-contract derived state too, and must reset the same way \
         missing_required_field_streak/authoring_failure_streak do"
    );
}

// ---------------------------------------------------------------------------
// Tests — "bound and matching nothing" (this feature): a poll that observed
// at least one candidate and quarantined every one of them must be visibly
// distinct from a poll that observed nothing at all, on the very poll it
// happens — not after a multi-poll ceiling like the missing-required-field
// amendment trigger above.
// ---------------------------------------------------------------------------

/// Regression test for the production incident this feature exists to fix:
/// a watch observed candidates, quarantined all of them, and surfaced
/// nothing distinguishable from a healthy-but-quiet watch. Drives a poll
/// with two candidates that both fail the same required-field check and
/// asserts exactly one aggregated health event fires for the whole poll
/// (not one per candidate — that's already covered by the existing
/// per-candidate `quarantine_candidate` events), naming the dominant
/// rejection reason.
#[tokio::test]
async fn bound_matching_nothing_emits_exactly_one_aggregated_event_naming_the_dominant_reason() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = contract_with_required_field("name");
    let assignment = agent_watch_assignment_with_contract("watch-bound-nothing", "agent-1", contract);
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let missing_name = |id: &str| candidate_with_payload(id, serde_json::json!({ "id": id }));
    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![missing_name("a"), missing_name("b")])]));

    let mut rx = event_bus.subscribe();

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(!fired, "a poll where every candidate is quarantined must never fire");

    let texts = drain_system_message_texts(&mut rx);
    let aggregated: Vec<&String> = texts.iter().filter(|t| t.contains("Dominant reason")).collect();
    assert_eq!(
        aggregated.len(),
        1,
        "exactly one aggregated 'bound and matching nothing' event must fire per poll, not one per \
         candidate and not zero: {texts:?}"
    );
    assert!(
        aggregated[0].contains("missing required field \"name\""),
        "the aggregated event must name the dominant rejection reason, not just assert 'unhealthy': {}",
        aggregated[0]
    );
    assert!(
        aggregated[0].contains("2 candidate"),
        "the aggregated event must report how many candidates were observed: {}",
        aggregated[0]
    );

    let scratchpad = persistence.assignment_scratchpads.get("watch-bound-nothing").await.unwrap().unwrap();
    assert_eq!(scratchpad.last_poll_observed_candidates, 2);
    assert_eq!(scratchpad.last_poll_surviving_candidates, 0);
    assert_eq!(scratchpad.all_candidates_quarantined_streak, 1);
}

/// A poll that observes zero candidates is genuinely quiet, not "bound
/// and matching nothing" — the two must never be conflated. Drives a
/// first poll that quarantines everything (to arm the streak), then a
/// second, zero-candidate poll, and asserts the aggregated event does
/// not fire again and the streak is left exactly where poll 1 set it.
#[tokio::test]
async fn zero_candidate_poll_emits_no_bound_matching_nothing_event_and_leaves_streak_untouched() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = contract_with_required_field("name");
    let assignment = agent_watch_assignment_with_contract("watch-bound-nothing-quiet", "agent-1", contract);
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let missing_name = |id: &str| candidate_with_payload(id, serde_json::json!({ "id": id }));
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![missing_name("a"), missing_name("b")]), // poll 1: everything quarantined
        Ok(vec![]),                                     // poll 2: genuinely quiet
    ]));

    let mut rx = event_bus.subscribe();

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let after_poll_1 =
        persistence.assignment_scratchpads.get("watch-bound-nothing-quiet").await.unwrap().unwrap();
    assert_eq!(after_poll_1.all_candidates_quarantined_streak, 1, "sanity: poll 1 must arm the streak");
    drain_system_message_texts(&mut rx); // discard poll 1's events, not under test here

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    let texts = drain_system_message_texts(&mut rx);
    assert!(
        texts.iter().all(|t| !t.contains("Dominant reason")),
        "a poll that observed zero candidates must never emit the aggregated 'bound and matching nothing' \
         event — that is what tells it apart from a genuinely quiet poll: {texts:?}"
    );

    let after_poll_2 =
        persistence.assignment_scratchpads.get("watch-bound-nothing-quiet").await.unwrap().unwrap();
    assert_eq!(
        after_poll_2.last_poll_observed_candidates, 0,
        "the zero-candidate poll's own count must be recorded, overwriting poll 1's"
    );
    assert_eq!(after_poll_2.last_poll_surviving_candidates, 0);
    assert_eq!(
        after_poll_2.all_candidates_quarantined_streak, 1,
        "a poll that observed zero candidates must leave the streak untouched, mirroring \
         missing_required_field_streak's own zero-candidate behavior"
    );
}

/// The streak must reset the instant a poll has at least one surviving
/// candidate — it is not a one-way ratchet, and a watch that recovers
/// must stop being reported as unhealthy immediately, not after some
/// decay window.
#[tokio::test]
async fn all_quarantined_streak_resets_the_moment_one_candidate_survives() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = contract_with_required_field("name");
    let assignment = agent_watch_assignment_with_contract("watch-bound-nothing-recovers", "agent-1", contract);
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let missing_name = |id: &str| candidate_with_payload(id, serde_json::json!({ "id": id }));
    let has_name = |id: &str| candidate_with_payload(id, serde_json::json!({ "id": id, "name": "Andrew" }));
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![missing_name("a"), missing_name("b")]), // poll 1: everything quarantined
        Ok(vec![has_name("c")]),                        // poll 2: one candidate survives
    ]));

    let mut rx = event_bus.subscribe();

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let after_poll_1 =
        persistence.assignment_scratchpads.get("watch-bound-nothing-recovers").await.unwrap().unwrap();
    assert_eq!(after_poll_1.all_candidates_quarantined_streak, 1, "sanity: poll 1 must arm the streak");
    drain_system_message_texts(&mut rx); // discard poll 1's events, not under test here

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    let texts = drain_system_message_texts(&mut rx);
    assert!(
        texts.iter().all(|t| !t.contains("Dominant reason")),
        "a poll with at least one surviving candidate must not emit the aggregated event: {texts:?}"
    );

    let after_poll_2 =
        persistence.assignment_scratchpads.get("watch-bound-nothing-recovers").await.unwrap().unwrap();
    assert_eq!(after_poll_2.last_poll_observed_candidates, 1);
    assert_eq!(after_poll_2.last_poll_surviving_candidates, 1);
    assert_eq!(
        after_poll_2.all_candidates_quarantined_streak, 0,
        "the streak must reset to 0 the moment a poll has at least one surviving candidate"
    );
}

/// Regression test for the amend/reseed livelock: a contract whose
/// re-authored replacement keeps failing the same required-field check
/// must not amend forever. This drives `run_contract_bound_tick`
/// directly (rather than through the full authoring dance) so the test
/// stays focused on the cycle-bounding logic itself.
#[tokio::test]
async fn amendment_cycle_is_bounded_and_lands_in_a_visible_unhealthy_state_instead_of_looping_forever() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = contract_with_required_field("name");
    let assignment = agent_watch_assignment_with_contract("watch-cycle-1", "agent-1", contract.clone());
    persistence.assignments.add(assignment.clone()).await.unwrap();

    let missing_name = || candidate_with_payload("a", serde_json::json!({ "id": "a" }));
    let mut rx = event_bus.subscribe();

    // Two missing-field polls per amendment cycle (the threshold), run
    // one cycle past the ceiling so the transition into "frozen" is
    // actually observed rather than merely approached.
    let total_polls = REQUIRED_FIELD_FAILURE_AMENDMENT_THRESHOLD * (CONTRACT_AMENDMENT_CYCLE_CEILING + 1);
    for i in 0..total_polls {
        let scratchpad =
            persistence.assignment_scratchpads.get("watch-cycle-1").await.unwrap().unwrap_or_default();
        run_contract_bound_tick(
            &persistence,
            &dispatcher,
            &event_bus,
            &assignment,
            None,
            &contract,
            scratchpad,
            i == 0,
            false,
            vec![missing_name()],
            ExtractionPath::Llm,
            None,
        )
        .await;
    }

    let scratchpad = persistence.assignment_scratchpads.get("watch-cycle-1").await.unwrap().unwrap();
    assert_eq!(
        scratchpad.contract_amendment_cycle_count,
        CONTRACT_AMENDMENT_CYCLE_CEILING + 1,
        "sanity: the drive above must land exactly one cycle past the ceiling"
    );

    let texts = drain_system_message_texts(&mut rx);
    let clearing_events = texts
        .iter()
        .filter(|t| t.contains("re-authoring its contract") || t.contains("re-authoring one last time"))
        .count();
    assert_eq!(
        clearing_events, CONTRACT_AMENDMENT_CYCLE_CEILING as usize,
        "the amendment must auto-clear at most CONTRACT_AMENDMENT_CYCLE_CEILING times, not loop forever; \
         got events: {texts:?}"
    );

    let unhealthy_events: Vec<&String> = texts.iter().filter(|t| t.contains("is now unhealthy")).collect();
    assert_eq!(
        unhealthy_events.len(),
        1,
        "hitting the bound must land the assignment in exactly one visible unhealthy event; got: {texts:?}"
    );
    assert!(
        unhealthy_events[0].contains("missing-required-field"),
        "the unhealthy event must carry the real reason; got: {}",
        unhealthy_events[0]
    );

    // Confirm it actually terminates rather than merely pausing: drive
    // one more full cycle's worth of missing-field polls and make sure
    // nothing clears again and the terminal event does not repeat.
    for _ in 0..REQUIRED_FIELD_FAILURE_AMENDMENT_THRESHOLD {
        let scratchpad =
            persistence.assignment_scratchpads.get("watch-cycle-1").await.unwrap().unwrap_or_default();
        run_contract_bound_tick(
            &persistence,
            &dispatcher,
            &event_bus,
            &assignment,
            None,
            &contract,
            scratchpad,
            false,
            false,
            vec![missing_name()],
            ExtractionPath::Llm,
            None,
        )
        .await;
    }
    let more_texts = drain_system_message_texts(&mut rx);
    assert!(
        !more_texts.iter().any(|t| t.contains("re-authoring its contract") || t.contains("is now unhealthy")),
        "past the ceiling, later polls must neither amend again nor repeat the terminal event; got: {more_texts:?}"
    );
}

#[test]
fn record_seen_evicts_oldest_entries_past_the_cap() {
    let mut scratchpad = AssignmentScratchpad::default();
    let ids: Vec<String> = (0..SEEN_IDS_CAP + 10).map(|i| format!("item-{i}")).collect();
    record_seen(&mut scratchpad, ids.clone());

    assert_eq!(scratchpad.seen_ids.len(), SEEN_IDS_CAP);
    // The oldest 10 were dropped; the most recent SEEN_IDS_CAP remain.
    assert_eq!(scratchpad.seen_ids.first().unwrap(), &ids[10]);
    assert_eq!(scratchpad.seen_ids.last().unwrap(), &ids[ids.len() - 1]);
}

#[test]
fn build_event_context_summarizes_a_single_candidate_directly() {
    let c = candidate("a");
    let ctx = build_event_context(&[&c]);
    assert_eq!(ctx.summary, "New item a");
}

#[test]
fn build_event_context_bundles_multiple_candidates_into_one_summary() {
    let a = candidate("a");
    let b = candidate("b");
    let ctx = build_event_context(&[&a, &b]);
    assert!(ctx.summary.contains("2 new items"), "got: {}", ctx.summary);
    assert!(ctx.summary.contains("New item a"));
    assert!(ctx.summary.contains("New item b"));
    assert_eq!(ctx.payload["items"].as_array().unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// `parse_candidates` — authoring/legacy mode (`contract: None`)
// ---------------------------------------------------------------------------

/// Unwraps a [`parse_candidates`] result that's expected to be a
/// successful [`WatchObservation::Observed`], panicking with the actual
/// variant otherwise — every test in this file predating
/// [`WatchObservation`] asserted directly on a `Vec`, so this keeps them
/// exactly as readable while the failure variants get their own
/// dedicated tests below.
fn expect_observed(observation: Option<WatchObservation>) -> Vec<AgentWatchCandidate> {
    match observation.expect("must parse") {
        WatchObservation::Observed(candidates) => candidates,
        other => panic!("expected WatchObservation::Observed, got {other:?}"),
    }
}

#[test]
fn parse_candidates_reads_plain_json_array() {
    let raw = r#"[{"id":"a","summary":"New email from finance","payload":{"sender":"finance@co.com"}}]"#;
    let got = expect_observed(parse_candidates(raw, None));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, "a");
    assert_eq!(got[0].summary, "New email from finance");
    assert_eq!(got[0].payload["sender"], "finance@co.com");
}

#[test]
fn parse_candidates_reads_fenced_json_array() {
    let raw = "Here's what I found:\n```json\n[{\"id\":\"x\",\"summary\":\"item x\",\"payload\":{}}]\n```";
    let got = expect_observed(parse_candidates(raw, None));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, "x");
}

#[test]
fn parse_candidates_empty_array_is_a_valid_no_findings_reply() {
    let got = expect_observed(parse_candidates("[]", None));
    assert!(got.is_empty());
}

#[test]
fn parse_candidates_garbage_reply_returns_none() {
    assert!(parse_candidates("I couldn't check that right now, sorry.", None).is_none());
}

#[test]
fn parse_candidates_drops_entries_missing_a_stable_id() {
    let raw = r#"[{"summary":"no id here","payload":{}},{"id":"b","summary":"has id","payload":{}}]"#;
    let got = expect_observed(parse_candidates(raw, None));
    assert_eq!(got.len(), 1, "the id-less entry must be dropped, not kept or fail the batch");
    assert_eq!(got[0].id, "b");
}

#[test]
fn parse_candidates_defaults_summary_to_id_and_payload_to_null() {
    let raw = r#"[{"id":"only-id"}]"#;
    let got = expect_observed(parse_candidates(raw, None));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].summary, "only-id");
    assert_eq!(got[0].payload, serde_json::Value::Null);
}

// ---------------------------------------------------------------------------
// `parse_candidates` — the reported-failure channel (`WatchObservation::
// ToolError` / `::ObservationFailed`) — this is the fix for the bind/
// authoring reply contract's missing error channel: previously a child
// whose tool call failed mid-turn could only reply `[]` (indistinguishable
// from a source that genuinely had nothing) or fabricate items.
// ---------------------------------------------------------------------------

#[test]
fn parse_candidates_tool_error_reply_is_not_conflated_with_an_empty_success() {
    let raw = r#"{"status":"tool_error","tool":"notion-search","detail":"429 Too Many Requests"}"#;
    let got = parse_candidates(raw, None).expect("must parse");
    assert_eq!(
        got,
        WatchObservation::ToolError {
            tool: Some("notion-search".to_string()),
            detail: "429 Too Many Requests".to_string()
        }
    );
}

#[test]
fn parse_candidates_tool_error_reply_tolerates_a_missing_tool_name() {
    let raw = r#"{"status":"tool_error","detail":"connection reset"}"#;
    let got = parse_candidates(raw, None).expect("must parse");
    assert_eq!(got, WatchObservation::ToolError { tool: None, detail: "connection reset".to_string() });
}

#[test]
fn parse_candidates_observation_failed_reply_carries_the_stated_reason() {
    let raw = r#"{"status":"failed","reason":"the page never finished loading after 3 retries"}"#;
    let got = parse_candidates(raw, None).expect("must parse");
    assert_eq!(
        got,
        WatchObservation::ObservationFailed { reason: "the page never finished loading after 3 retries".to_string() }
    );
}

#[test]
fn parse_candidates_bind_mode_tool_error_is_not_treated_as_a_candidate() {
    // A tool-error reply must never be run through `parse_bind_candidate`
    // as if it were payload data — that would quarantine it (blaming the
    // observation) instead of surfacing the real reported failure.
    let contract =
        bind_mode_contract(IdentityStrategy::NativeId, Some("message_id"), vec![], vec!["subject", "status"]);
    let raw = r#"{"status":"tool_error","tool":"inbox-search","detail":"timed out after 30s"}"#;

    let got = parse_candidates(raw, Some(&contract)).expect("must parse");

    assert_eq!(
        got,
        WatchObservation::ToolError { tool: Some("inbox-search".to_string()), detail: "timed out after 30s".to_string() }
    );
}

#[test]
fn parse_candidates_new_tagged_success_shape_with_items_still_parses() {
    let raw = r#"{"status":"ok","candidates":[{"id":"a","summary":"New email","payload":{}}]}"#;
    let got = expect_observed(parse_candidates(raw, None));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, "a");
}

#[test]
fn parse_candidates_new_tagged_success_shape_with_zero_items_is_a_quiet_tick() {
    let raw = r#"{"status":"ok","candidates":[]}"#;
    let got = expect_observed(parse_candidates(raw, None));
    assert!(got.is_empty());
}

#[test]
fn parse_candidates_legacy_bare_array_shape_still_parses_as_a_successful_observation() {
    // The exact backward-compatibility case this fix must never break: a
    // model that hasn't seen (or ignores) the tagged-object contract and
    // replies in the old bare-array shape must still be parsed as a
    // successful observation, not dropped or misread as a failure — a
    // prior incident in this subsystem was caused by exactly this kind of
    // reply-shape change quarantining an otherwise-correct reply.
    let raw = r#"[{"id":"a","summary":"New email from finance","payload":{"sender":"finance@co.com"}}]"#;
    let got = expect_observed(parse_candidates(raw, None));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, "a");
}

#[test]
fn parse_candidates_legacy_authoring_envelope_without_a_status_key_still_parses() {
    // Pre-fix authoring replies were `{"candidates": [...], "contract":
    // {...}}` with no `status` key at all. That must keep parsing as a
    // success too.
    let raw = r#"{"candidates":[{"id":"a","summary":"s","payload":{}}],"contract":{"source":{}}}"#;
    let got = expect_observed(parse_candidates(raw, None));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, "a");
}

// ---------------------------------------------------------------------------
// `parse_candidates` — bind mode (`contract: Some(_)`)
//
// Regression coverage for the bind-mode parser bug: `build_bind_prompt`
// tells the model never to include a top-level `id`, but the parser used
// to require exactly that field and silently dropped every entry that
// omitted it — so a contract-bound watch with `identity.strategy ==
// composite_native` or `content_hash` (which have no top-level `id` to
// coincidentally supply) parsed every poll down to zero candidates,
// forever. Each test below drives literal bind-mode reply text (no
// `id`/`summary`/`payload` wrapper — the item IS the payload) through
// the real parser for one `IdentityStrategy` and asserts every entry
// survives with the same identity key `identity_key` itself would
// compute.
// ---------------------------------------------------------------------------

/// Minimal bind-mode `WatchContract` fixture: the caller picks the
/// identity ladder rung and its inputs; `change.material_fields` is
/// fixed to a field disjoint from every strategy's identity inputs used
/// below, and `fields` is populated from every field name the fixture's
/// reply text reports (mirroring what `author_contract` would have
/// declared from the same observation).
fn bind_mode_contract(
    strategy: IdentityStrategy,
    source_field: Option<&str>,
    identity_fields: Vec<&str>,
    declared_fields: Vec<&str>,
) -> WatchContract {
    WatchContract {
        contract_version: 1,
        authored_at: "2026-07-27T09:00:00Z".to_string(),
        authored_by_run: "run-1".to_string(),
        source: WatchSource { kind: "test".to_string(), ref_: "test".to_string() },
        identity: IdentitySpec {
            strategy,
            source_field: source_field.map(str::to_string),
            format: None,
            fields: identity_fields.into_iter().map(str::to_string).collect(),
            rationale: "test fixture".to_string(),
        },
        change: ChangeSpec { material_fields: vec!["status".to_string()], version_hint_field: None },
        predicate: PredicateSpec {
            natural_language: String::new(),
            fields: vec![],
            predicate: ao_protocol::watch_contract::legacy_expr::parse("not_empty(status)")
                .expect("fixture predicate must parse"),
        },
        mode: WatchMode::NewOnly,
        fields: declared_fields
            .into_iter()
            .map(|f| (f.to_string(), FieldSpec { field_type: "string".to_string(), required: false }))
            .collect(),
    }
}

#[test]
fn parse_candidates_bind_mode_native_id_survives_with_no_top_level_id() {
    let contract =
        bind_mode_contract(IdentityStrategy::NativeId, Some("message_id"), vec![], vec!["subject", "status"]);
    let raw = r#"[{"message_id":"msg-1","subject":"Q3 numbers","status":"unread"}]"#;

    let got = expect_observed(parse_candidates(raw, Some(&contract)));

    assert_eq!(got.len(), 1, "a native_id item with no top-level `id` key must not be dropped");
    let expected_id = identity_key(&contract, &got[0].payload).expect("identity must be derivable");
    assert_eq!(got[0].id, expected_id, "candidate id must be the same identity_key run_contract_bound_tick derives");
    assert_eq!(got[0].payload["message_id"], "msg-1");
    assert_eq!(got[0].payload["subject"], "Q3 numbers");
}

#[test]
fn parse_candidates_bind_mode_composite_native_survives_with_no_top_level_id() {
    // Mirrors the reported incident's contract shape exactly:
    // identity.fields = ["first_name", "last_name"], contract.fields =
    // {company, first_name, last_name}.
    let contract = bind_mode_contract(
        IdentityStrategy::CompositeNative,
        None,
        vec!["first_name", "last_name"],
        vec!["company", "first_name", "last_name", "status"],
    );
    let raw = r#"[{"company":"Acme","first_name":"Jane","last_name":"Doe","status":"new"}]"#;

    let got = expect_observed(parse_candidates(raw, Some(&contract)));

    assert_eq!(got.len(), 1, "a composite_native item with no top-level `id` key must not be dropped");
    let expected_id = identity_key(&contract, &got[0].payload).expect("identity must be derivable");
    assert_eq!(got[0].id, expected_id, "candidate id must be the same identity_key run_contract_bound_tick derives");
    assert_eq!(got[0].payload["first_name"], "Jane");
    assert_eq!(got[0].payload["last_name"], "Doe");
}

#[test]
fn parse_candidates_bind_mode_content_hash_survives_with_no_top_level_id() {
    let contract = bind_mode_contract(
        IdentityStrategy::ContentHash,
        None,
        vec!["subject", "sender"],
        vec!["subject", "sender", "status"],
    );
    let raw = r#"[{"subject":"Q3 numbers","sender":"finance@co.com","status":"unread"}]"#;

    let got = expect_observed(parse_candidates(raw, Some(&contract)));

    assert_eq!(got.len(), 1, "a content_hash item with no top-level `id` key must not be dropped");
    let expected_id = identity_key(&contract, &got[0].payload).expect("identity must be derivable");
    assert_eq!(got[0].id, expected_id, "candidate id must be the same identity_key run_contract_bound_tick derives");
}

#[test]
fn parse_candidates_bind_mode_never_drops_even_when_identity_is_undrivable() {
    // A composite_native item missing one of its two declared identity
    // fields: `identity_key` fails closed (`ContractError::MissingField`),
    // but the item must still survive parsing — dropping it here would
    // silently reintroduce a 100%-drop-shaped bug, just gated on a
    // different condition. The caller's diff loop is what quarantines
    // this (and surfaces a health event for it), not the parser.
    let contract = bind_mode_contract(
        IdentityStrategy::CompositeNative,
        None,
        vec!["first_name", "last_name"],
        vec!["first_name", "last_name", "status"],
    );
    let raw = r#"[{"first_name":"Jane","status":"new"}]"#;

    let got = expect_observed(parse_candidates(raw, Some(&contract)));

    assert_eq!(got.len(), 1, "an item this contract can't derive an identity for must still reach the caller");
    assert!(
        identity_key(&contract, &got[0].payload).is_err(),
        "sanity check: this fixture must actually be identity-undrivable"
    );
}

// ---------------------------------------------------------------------------
// `scoped_mcp_registry`
// ---------------------------------------------------------------------------

/// Minimal named stub tool, registered under whatever name the test
/// needs — mirrors the `mcp__{server}__{tool}` names `McpToolAdapter`
/// registers at runtime, without pulling in a live MCP connection.
struct NamedStubTool(&'static str);

#[async_trait_attr]
impl ao_engine_tools_core::IoTool for NamedStubTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "stub tool for connector_scope filtering tests"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn invoke(
        &self,
        _input: serde_json::Value,
        _ctx: &RunnerContext,
    ) -> Result<ao_engine_tools_core::ToolOutput, AoError> {
        Ok(ao_engine_tools_core::ToolOutput::text("ok"))
    }
}

fn registry_with_tools(names: &[&'static str]) -> Registry {
    let mut registry = Registry::new();
    for name in names {
        registry.register_io(Arc::new(NamedStubTool(name)));
    }
    registry
}

#[test]
fn scoped_mcp_registry_restricts_to_the_named_connector_when_scoped() {
    let registry = registry_with_tools(&["mcp__notion__search", "mcp__github__search", "read_file"]);

    let scoped = scoped_mcp_registry(&registry, Some("notion"));
    let names = scoped.list();

    assert_eq!(names, vec!["mcp__notion__search".to_string()]);
    assert!(scoped.lookup("mcp__github__search").is_none(), "must exclude the other connector's tools");
    assert!(scoped.lookup("read_file").is_none(), "must exclude non-MCP tools");
}

#[test]
fn scoped_mcp_registry_keeps_every_mcp_tool_when_unscoped() {
    let registry = registry_with_tools(&["mcp__notion__search", "mcp__github__search", "read_file"]);

    let scoped = scoped_mcp_registry(&registry, None);
    let mut names = scoped.list();
    names.sort();

    assert_eq!(names, vec!["mcp__github__search".to_string(), "mcp__notion__search".to_string()]);
    assert!(scoped.lookup("read_file").is_none(), "must still exclude non-MCP tools");
}

// ---------------------------------------------------------------------------
// `LiveAgentWatchDetector`
// ---------------------------------------------------------------------------

fn scripted_provider_resolver(
    provider: Arc<ao_engine_tools_runner::provider::MockProviderClient>,
) -> ProviderResolver {
    Arc::new(move |_profile: &AgentProfile| Some(provider.clone() as Arc<dyn ProviderClient>))
}

/// `make_agent` defaults to `Cli` (see its own `runner_mode: Default::default()`).
/// The `live_detector_*` tests below exercise `observe_via_native_session`
/// specifically (the `Api`-mode path, driven directly by an injected
/// `resolve_provider` — see module doc for why `Cli`-mode can no longer be
/// tested that way), so they need an `Api`-mode profile.
fn make_api_agent(id: &str) -> AgentProfile {
    AgentProfile { runner_mode: AgentRunnerMode::Api, ..make_agent(id) }
}

/// A `RunnerDispatcher` that panics if ever asked to pick a runner.
/// Correct for every `live_detector_*` test below: they all use an
/// `Api`-mode profile, so `observe()` never touches `self.dispatcher` at
/// all — if one of these tests started reaching this stub, that would
/// itself be a bug (the wrong runner_mode branch firing).
fn dispatcher_that_must_not_be_used() -> Arc<RunnerDispatcher> {
    struct UnreachableRunner;
    #[async_trait]
    impl AgentRunner for UnreachableRunner {
        async fn run(&self, _request: AgentRunRequest) -> Result<RunComplete, AoError> {
            panic!("dispatcher_that_must_not_be_used: an Api-mode profile must never dispatch through RunnerDispatcher");
        }
        fn mode(&self) -> AgentRunnerMode {
            AgentRunnerMode::Cli
        }
    }
    Arc::new(RunnerDispatcher::with_runners(
        Arc::new(UnreachableRunner),
        Arc::new(UnreachableRunner),
    ))
}

#[tokio::test]
async fn live_detector_runs_the_agent_and_returns_structured_findings() {
    use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};

    let (_tmp, persistence) = make_persistence().await;
    persistence.agents.create(&make_api_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-live-1", "agent-1");

    let findings = concat!(
        "```json\n",
        r#"[{"id":"email-42","summary":"New email from finance","payload":{"subject":"Q3 numbers"}}]"#,
        "\n```"
    );
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText(findings.to_string()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let detector = LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        Arc::new(Registry::new()),
        scripted_provider_resolver(provider),
        dispatcher_that_must_not_be_used(),
        Arc::new(EventBus::new(64)),
    );

    let candidates = detector
        .observe(&assignment, "Check my inbox for a new email from finance")
        .await
        .expect("observation must succeed");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id, "email-42");
    assert_eq!(candidates[0].summary, "New email from finance");
    assert_eq!(candidates[0].payload["subject"], "Q3 numbers");
}

#[tokio::test]
async fn live_detector_empty_findings_is_a_valid_quiet_observation() {
    use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};

    let (_tmp, persistence) = make_persistence().await;
    persistence.agents.create(&make_api_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-live-2", "agent-1");

    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("[]".to_string()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let detector = LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        Arc::new(Registry::new()),
        scripted_provider_resolver(provider),
        dispatcher_that_must_not_be_used(),
        Arc::new(EventBus::new(64)),
    );

    let candidates = detector.observe(&assignment, "watch").await.expect("must succeed");
    assert!(candidates.is_empty());
}

#[tokio::test]
async fn live_detector_total_parse_drop_emits_a_health_event_not_a_quiet_tick() {
    use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};

    let (_tmp, persistence) = make_persistence().await;
    persistence.agents.create(&make_api_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-live-total-drop", "agent-1");

    // Two items, neither carrying the `id` field authoring/legacy mode
    // requires — every one of them is dropped while parsing. That must
    // not be indistinguishable from the model legitimately reporting an
    // empty `[]` (see `live_detector_empty_findings_is_a_valid_quiet_observation`).
    let findings = r#"[{"summary":"a","payload":{}},{"summary":"b","payload":{}}]"#;
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText(findings.to_string()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let event_bus = Arc::new(EventBus::new(64));
    let mut health_rx = event_bus.subscribe();

    let detector = LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        Arc::new(Registry::new()),
        scripted_provider_resolver(provider),
        dispatcher_that_must_not_be_used(),
        event_bus,
    );

    let candidates =
        detector.observe(&assignment, "watch").await.expect("a total-drop parse is not itself an observation error");
    assert!(candidates.is_empty());

    let texts = drain_system_message_texts(&mut health_rx);
    assert_eq!(texts.len(), 1, "a reply whose items all got dropped must emit exactly one health event");
    assert!(texts[0].contains('2'), "message must state how many items the reply reported: {}", texts[0]);
}

#[tokio::test]
async fn live_detector_tool_error_reply_emits_a_health_event_carrying_the_real_reason_not_a_quiet_tick() {
    use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};

    let (_tmp, persistence) = make_persistence().await;
    persistence.agents.create(&make_api_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-live-tool-error", "agent-1");

    // The child made a tool call mid-turn and it failed — before this
    // fix, the only way to express that was `[]`, indistinguishable from
    // a source that genuinely had nothing this poll.
    let reply = r#"{"status":"tool_error","tool":"notion-search","detail":"429 Too Many Requests"}"#;
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText(reply.to_string()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let event_bus = Arc::new(EventBus::new(64));
    let mut health_rx = event_bus.subscribe();

    let detector = LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        Arc::new(Registry::new()),
        scripted_provider_resolver(provider),
        dispatcher_that_must_not_be_used(),
        event_bus,
    );

    let candidates = detector
        .observe(&assignment, "watch")
        .await
        .expect("a reported tool error is not itself an observation error — it must not be conflated with a hard parse failure");
    assert!(candidates.is_empty(), "a reported tool error must never be reported as, or fabricated into, a candidate");

    let texts = drain_system_message_texts(&mut health_rx);
    assert_eq!(texts.len(), 1, "a reported tool error must surface as exactly one health event, not a quiet tick");
    assert!(
        texts[0].contains("notion-search") && texts[0].contains("429 Too Many Requests"),
        "the health event must carry the real reported reason (which tool, and what it returned), not a generic \
         message; got: {}",
        texts[0]
    );
}

#[tokio::test]
async fn live_detector_authoring_tool_error_reply_emits_a_health_event_and_proposes_no_contract() {
    use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};

    let (_tmp, persistence) = make_persistence().await;
    persistence.agents.create(&make_api_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-live-authoring-tool-error", "agent-1");

    let reply = r#"{"status":"failed","reason":"the source's API was unreachable for the whole session"}"#;
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText(reply.to_string()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let event_bus = Arc::new(EventBus::new(64));
    let mut health_rx = event_bus.subscribe();

    let detector = LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        Arc::new(Registry::new()),
        scripted_provider_resolver(provider),
        dispatcher_that_must_not_be_used(),
        event_bus,
    );

    let authoring_reply = detector
        .observe_for_authoring(&assignment, "watch", None)
        .await
        .expect("a reported observation failure is not itself an observation error");
    assert!(authoring_reply.candidates.is_empty());
    assert!(
        authoring_reply.proposed_contract.is_none(),
        "a poll that could not observe anything must not fabricate a contract proposal"
    );

    let texts = drain_system_message_texts(&mut health_rx);
    assert_eq!(texts.len(), 1, "a reported observation failure must surface as exactly one health event");
    assert!(
        texts[0].contains("the source's API was unreachable for the whole session"),
        "the health event must carry the real stated reason verbatim; got: {}",
        texts[0]
    );
}

#[tokio::test]
async fn live_detector_unparseable_reply_fails_the_observation() {
    use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};

    let (_tmp, persistence) = make_persistence().await;
    persistence.agents.create(&make_api_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-live-3", "agent-1");

    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("I looked but I'm not sure how to summarize this.".to_string()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let detector = LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        Arc::new(Registry::new()),
        scripted_provider_resolver(provider),
        dispatcher_that_must_not_be_used(),
        Arc::new(EventBus::new(64)),
    );

    let err = detector.observe(&assignment, "watch").await.unwrap_err();
    assert!(matches!(err, AgentWatchDetectError::Failed(ref msg) if msg.contains("could not parse")), "got: {err}");
}

#[tokio::test]
async fn live_detector_no_provider_configured_fails_the_observation() {
    let (_tmp, persistence) = make_persistence().await;
    persistence.agents.create(&make_api_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-live-4", "agent-1");

    let detector = LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        Arc::new(Registry::new()),
        Arc::new(|_profile: &AgentProfile| None),
        dispatcher_that_must_not_be_used(),
        Arc::new(EventBus::new(64)),
    );

    let err = detector.observe(&assignment, "watch").await.unwrap_err();
    assert!(
        matches!(err, AgentWatchDetectError::Failed(ref msg) if msg.contains("no provider configured")),
        "got: {err}"
    );
}

#[tokio::test]
async fn live_detector_unknown_agent_fails_the_observation() {
    let (_tmp, persistence) = make_persistence().await;
    // Deliberately no agent created — the assignment references an
    // agent id that doesn't exist in this persistence layer.
    let assignment = agent_watch_assignment("watch-live-5", "no-such-agent");

    let detector = LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        Arc::new(Registry::new()),
        Arc::new(|_profile: &AgentProfile| None),
        dispatcher_that_must_not_be_used(),
        Arc::new(EventBus::new(64)),
    );

    let err = detector.observe(&assignment, "watch").await.unwrap_err();
    assert!(
        matches!(err, AgentWatchDetectError::Failed(ref msg) if msg.contains("does not exist")),
        "got: {err}"
    );
}

// ---------------------------------------------------------------------------
// `LiveAgentWatchDetector` — CLI mode (`observe_via_profile_runner`)
// ---------------------------------------------------------------------------

/// Captures the last `AgentRunRequest` it was asked to run and replies
/// with a scripted `RunComplete`. Stands in for `CliAgentRunner` so these
/// tests can prove *what* the detector dispatches (full profile, no MCP
/// scoping, isolation fields, `bypass_instance_cap`) without spawning a
/// real CLI process — the actual process-spawn plumbing
/// (`--mcp-config`, `InstanceRegistry` bypass) is covered directly in
/// `agent_runner::cli`'s own tests.
struct RequestCapturingRunner {
    captured: std::sync::Mutex<Option<AgentRunRequest>>,
    reply: std::sync::Mutex<Option<Result<RunComplete, String>>>,
}

impl RequestCapturingRunner {
    fn new(reply: Result<String, String>) -> Arc<Self> {
        Self::new_with_end_reason(reply.map(|output_text| {
            (output_text, ao_protocol::event::RunEndReason::Completed)
        }))
    }

    fn new_with_end_reason(
        reply: Result<(String, ao_protocol::event::RunEndReason), String>,
    ) -> Arc<Self> {
        let reply = reply.map(|(output_text, end_reason)| RunComplete {
            run_id: "test-run".to_string(),
            output_text,
            workflow_followups: vec![],
            end_reason,
        });
        Arc::new(Self {
            captured: std::sync::Mutex::new(None),
            reply: std::sync::Mutex::new(Some(reply)),
        })
    }
}

#[async_trait]
impl AgentRunner for RequestCapturingRunner {
    async fn run(&self, request: AgentRunRequest) -> Result<RunComplete, AoError> {
        let reply = self.reply.lock().unwrap().take().expect("run() called more than once");
        *self.captured.lock().unwrap() = Some(request);
        reply.map_err(AoError::Internal)
    }
    fn mode(&self) -> AgentRunnerMode {
        AgentRunnerMode::Cli
    }
}

fn make_cli_agent(id: &str) -> AgentProfile {
    AgentProfile { runner_mode: AgentRunnerMode::Cli, ..make_agent(id) }
}

#[tokio::test]
async fn cli_mode_dispatches_through_the_profile_runner_with_full_tool_surface() {
    let (_tmp, persistence) = make_persistence().await;
    persistence.agents.create(&make_cli_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-cli-1", "agent-1");

    let findings = r#"[{"id":"issue-9","summary":"New GitHub issue","payload":{"repo":"acme/widgets"}}]"#;
    let runner = RequestCapturingRunner::new(Ok(findings.to_string()));
    let dispatcher = Arc::new(RunnerDispatcher::with_runners(
        runner.clone() as Arc<dyn AgentRunner>,
        runner.clone() as Arc<dyn AgentRunner>,
    ));

    let detector = LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        Arc::new(Registry::new()),
        Arc::new(|_profile: &AgentProfile| None),
        dispatcher,
        Arc::new(EventBus::new(64)),
    );

    let candidates = detector
        .observe(&assignment, "Check my GitHub notifications")
        .await
        .expect("observation must succeed");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id, "issue-9");

    let captured = runner.captured.lock().unwrap().take().expect("runner.run must have been called");
    assert_eq!(
        captured.agent.id, "agent-1",
        "cli mode must dispatch through the profile runner (real AgentRunner::run), \
         not the tool-less one-shot CliProviderClient path"
    );
    assert!(
        captured.prompt.contains("Check my GitHub notifications"),
        "the watch condition must reach the dispatched request's prompt; got: {}",
        captured.prompt
    );
    assert!(
        captured.isolate_history,
        "a watch poll must never resume or bleed into the agent's real personal history"
    );
    assert!(
        captured.transcript_override.is_some(),
        "a watch poll must write to its own sidechain transcript file, not the agent's real one"
    );
    assert!(
        captured.event_channel.as_deref().is_some_and(|c| c.starts_with("agent-watch:")),
        "a watch poll must stream on its own event channel, not the agent's live chat feed; got: {:?}",
        captured.event_channel
    );
    assert!(
        captured.bypass_instance_cap,
        "a watch poll must bypass the agent's max_instances slot (see agent_runner::cli's \
         bypass_instance_cap_tests for proof this is actually honored)"
    );
}

/// Regression test (no per-surface model selection): the
/// watch tick dispatches the assignment's own configured `AgentProfile`
/// verbatim — this contract rewrite must not introduce a hardcoded model
/// or provider anywhere on this path. `RequestCapturingRunner` captures
/// the exact `AgentRunRequest` the CLI-mode detector dispatches, so this
/// proves `profile.model` reaches the child unmodified rather than
/// asserting a negative about code that isn't there.
#[tokio::test]
async fn cli_mode_dispatch_preserves_the_profiles_own_model_with_no_override() {
    let (_tmp, persistence) = make_persistence().await;
    let profile = AgentProfile { model: Some("claude-sonnet-5".to_string()), ..make_cli_agent("agent-1") };
    persistence.agents.create(&profile).await.unwrap();
    let assignment = agent_watch_assignment("watch-cli-model", "agent-1");

    let runner = RequestCapturingRunner::new(Ok("[]".to_string()));
    let dispatcher = Arc::new(RunnerDispatcher::with_runners(
        runner.clone() as Arc<dyn AgentRunner>,
        runner.clone() as Arc<dyn AgentRunner>,
    ));

    let detector = LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        Arc::new(Registry::new()),
        Arc::new(|_profile: &AgentProfile| None),
        dispatcher,
        Arc::new(EventBus::new(64)),
    );

    detector.observe(&assignment, "watch").await.expect("observation must succeed");

    let captured = runner.captured.lock().unwrap().take().expect("runner.run must have been called");
    assert_eq!(
        captured.agent.model.as_deref(),
        Some("claude-sonnet-5"),
        "the watch path must dispatch the assignment's own configured agent profile verbatim, \
         never overriding or stripping its model — no per-surface model selection"
    );
}

#[tokio::test]
async fn cli_mode_session_error_fails_the_observation() {
    let (_tmp, persistence) = make_persistence().await;
    persistence.agents.create(&make_cli_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-cli-2", "agent-1");

    let runner = RequestCapturingRunner::new(Err("mock cli process crashed".to_string()));
    let dispatcher = Arc::new(RunnerDispatcher::with_runners(
        runner.clone() as Arc<dyn AgentRunner>,
        runner as Arc<dyn AgentRunner>,
    ));

    let detector = LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        Arc::new(Registry::new()),
        Arc::new(|_profile: &AgentProfile| None),
        dispatcher,
        Arc::new(EventBus::new(64)),
    );

    let err = detector.observe(&assignment, "watch").await.unwrap_err();
    assert!(
        matches!(err, AgentWatchDetectError::Failed(ref msg) if msg.contains("mock cli process crashed")),
        "got: {err}"
    );
}

#[tokio::test]
async fn cli_mode_non_completed_end_reason_fails_the_observation_clearly() {
    // `runner.run` can return `Ok` even when the CLI process didn't
    // finish normally — e.g. the agent's own `no_output_timeout_ms`
    // watchdog tripping on a hung child. This must surface a clear
    // reason rather than a generic "could not parse" error.
    let (_tmp, persistence) = make_persistence().await;
    persistence.agents.create(&make_cli_agent("agent-1")).await.unwrap();
    let assignment = agent_watch_assignment("watch-cli-3", "agent-1");

    let runner = RequestCapturingRunner::new_with_end_reason(Ok((
        String::new(),
        ao_protocol::event::RunEndReason::NoOutputTimeout,
    )));
    let dispatcher = Arc::new(RunnerDispatcher::with_runners(
        runner.clone() as Arc<dyn AgentRunner>,
        runner as Arc<dyn AgentRunner>,
    ));

    let detector = LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        Arc::new(Registry::new()),
        Arc::new(|_profile: &AgentProfile| None),
        dispatcher,
        Arc::new(EventBus::new(64)),
    );

    let err = detector.observe(&assignment, "watch").await.unwrap_err();
    assert!(
        matches!(err, AgentWatchDetectError::Failed(ref msg) if msg.contains("ended without completing normally")),
        "got: {err}"
    );
}

// ---------------------------------------------------------------------------
// `build_watch_prompt` / `build_full_profile_watch_prompt` — contract injection
// ---------------------------------------------------------------------------

fn sample_contract(strategy: IdentityStrategy) -> WatchContract {
    let mut fields = HashMap::new();
    fields.insert(
        "client_name".to_string(),
        FieldSpec { field_type: "string".to_string(), required: true },
    );
    fields.insert(
        "tag".to_string(),
        FieldSpec { field_type: "string".to_string(), required: false },
    );
    WatchContract {
        contract_version: 1,
        authored_at: "2026-07-27T09:00:00Z".to_string(),
        authored_by_run: "run-1".to_string(),
        source: WatchSource { kind: "database".to_string(), ref_: "clients".to_string() },
        identity: IdentitySpec {
            strategy,
            source_field: matches!(strategy, IdentityStrategy::NativeId)
                .then(|| "unique_identifier".to_string()),
            format: None,
            fields: vec!["client_name".to_string()],
            rationale: "the source exposes a stable per-row key".to_string(),
        },
        change: ChangeSpec { material_fields: vec!["tag".to_string()], version_hint_field: None },
        predicate: PredicateSpec {
            natural_language: "tag contains 'Very Important'".to_string(),
            fields: vec!["tag".to_string()],
            predicate: ao_protocol::watch_contract::legacy_expr::parse("contains(tag, 'Very Important')")
                .expect("valid fixture expr"),
        },
        mode: WatchMode::PredicateTransition,
        fields,
    }
}

#[test]
fn authoring_mode_has_no_contract_block_and_asks_for_a_proposal() {
    let prompt = build_watch_prompt("Check my inbox for a new email from finance", None, None);

    assert!(
        !prompt.contains("already has a contract from a previous run"),
        "authoring mode must not inject an existing contract; got: {prompt}"
    );
    assert!(
        prompt.to_lowercase().contains("propose a contract"),
        "authoring mode must ask the model to propose a contract; got: {prompt}"
    );
    assert!(
        prompt.contains("rationale"),
        "authoring mode must ask the model to report its identity choice and why; got: {prompt}"
    );
}

#[test]
fn bind_mode_contains_the_declared_source_field_and_verbatim_instruction() {
    let contract = sample_contract(IdentityStrategy::NativeId);
    let prompt = build_watch_prompt("Check my inbox for a new email from finance", Some(&contract), None);

    assert!(
        prompt.contains("unique_identifier"),
        "bind mode must name the contract's declared source_field; got: {prompt}"
    );
    assert!(
        prompt.to_lowercase().contains("verbatim"),
        "bind mode must instruct verbatim transcription of the native id; got: {prompt}"
    );
    assert!(
        !prompt.to_lowercase().contains("propose a contract"),
        "bind mode must not ask the model to author a new contract; got: {prompt}"
    );
}

#[test]
fn bind_mode_lists_the_extraction_fields_without_composite_or_content_hash_leaking_source_field_talk() {
    let contract = sample_contract(IdentityStrategy::CompositeNative);
    let prompt = build_watch_prompt("watch", Some(&contract), None);

    assert!(prompt.contains("client_name"), "must list the contract's declared fields; got: {prompt}");
    assert!(prompt.contains("tag"), "must list the contract's declared fields; got: {prompt}");
    assert!(
        !prompt.to_lowercase().contains("verbatim"),
        "composite_native has no single native id to relay verbatim; got: {prompt}"
    );
}

#[test]
fn old_stable_id_sentence_is_gone_from_both_modes() {
    let authoring_prompt = build_full_profile_watch_prompt("watch", None, None);
    let contract = sample_contract(IdentityStrategy::NativeId);
    let bind_prompt = build_full_profile_watch_prompt("watch", Some(&contract), None);

    for prompt in [&authoring_prompt, &bind_prompt] {
        let lower = prompt.to_lowercase();
        assert!(
            !lower.contains("must return the exact same"),
            "the old model-decided-stable-id instruction must be gone; got: {prompt}"
        );
        assert!(
            !lower.contains("this is the only field the dedup"),
            "the old dedup-reads-this-field framing must be gone; got: {prompt}"
        );
    }
}

#[test]
fn full_profile_prompt_contains_the_watch_prompt_as_a_substring_in_both_modes() {
    let authoring_watch_prompt = build_watch_prompt("watch", None, None);
    let authoring_full_prompt = build_full_profile_watch_prompt("watch", None, None);
    assert!(
        authoring_full_prompt.contains(&authoring_watch_prompt),
        "build_full_profile_watch_prompt must still delegate to build_watch_prompt in authoring mode"
    );

    let contract = sample_contract(IdentityStrategy::NativeId);
    let bind_watch_prompt = build_watch_prompt("watch", Some(&contract), None);
    let bind_full_prompt = build_full_profile_watch_prompt("watch", Some(&contract), None);
    assert!(
        bind_full_prompt.contains(&bind_watch_prompt),
        "build_full_profile_watch_prompt must still delegate to build_watch_prompt in bind mode"
    );
}

#[test]
fn authoring_prompt_teaches_all_six_predicate_functions() {
    let authoring_prompt = build_authoring_prompt("watch", None);

    for function_name in ["contains", "equals", "not_empty", "and", "or", "not"] {
        assert!(
            authoring_prompt.contains(function_name),
            "authoring prompt must name the `{function_name}` predicate function so the \
             model never has to guess `predicate.expr` syntax; got: {authoring_prompt}"
        );
    }
}

#[test]
fn contract_proposal_shape_and_authoring_prompt_teach_mode() {
    // The defect this guards against: `WatchMode::NewOnly` was already
    // fully implemented at the runtime-tick layer, but `mode` was never
    // shown to the authoring model at all, so it always serde-defaulted
    // to `PredicateTransition` — an "appearance" watch could never
    // actually be authored, no matter how well the runtime supported it.
    assert!(
        CONTRACT_PROPOSAL_SHAPE.contains("\"mode\""),
        "the proposal shape must expose `mode` as a field the model can set; got: {CONTRACT_PROPOSAL_SHAPE}"
    );
    assert!(
        CONTRACT_PROPOSAL_SHAPE.contains("new_only"),
        "the proposal shape must name the new_only variant, not just say `mode` exists; got: \
         {CONTRACT_PROPOSAL_SHAPE}"
    );

    let authoring_prompt = build_authoring_prompt("watch", None);
    assert!(
        authoring_prompt.contains("new_only") && authoring_prompt.contains("predicate_transition"),
        "the authoring prompt must explain when to choose new_only vs. predicate_transition, not just list \
         them in the bare shape; got: {authoring_prompt}"
    );
    assert!(
        authoring_prompt.to_lowercase().contains("material_fields may be") || {
            let lower = authoring_prompt.to_lowercase();
            lower.contains("new_only") && lower.contains("empty")
        },
        "the authoring prompt must say material_fields may be left empty under new_only; got: {authoring_prompt}"
    );
}

#[test]
fn authoring_prompt_biases_identity_toward_a_native_id_when_one_is_present() {
    let authoring_prompt = build_authoring_prompt("watch", None);
    let lower = authoring_prompt.to_lowercase();
    assert!(
        lower.contains("native id") || lower.contains("native identifier"),
        "the authoring prompt must explicitly steer toward a native id when the source exposes one; got: \
         {authoring_prompt}"
    );
    assert!(
        lower.contains("fall back") || lower.contains("fallback"),
        "the authoring prompt must frame a composite of semantic fields as the fallback, not the default; \
         got: {authoring_prompt}"
    );
}

// ---------------------------------------------------------------------------
// Tests — deterministic extraction (`select_agent_watch_candidates`)
//
// `ScriptedDetector::new(vec![])` doubles as the "must never be called"
// double throughout this section: its `observe` pops from an empty
// queue and panics (see its own doc comment) the instant it's invoked,
// so any test using it that reaches a passing `assert` has already
// proven the model detector was never touched.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deterministic_extraction_first_poll_seeds_baseline_without_firing() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction(
        "watch-det-seed",
        "agent-1",
        contract,
        "det_seed_srv",
        "det_seed_tool",
        items_by_id_extraction_plan(),
        true,
    );
    stash_structured_payload(
        "det_seed_srv",
        "det_seed_tool",
        serde_json::json!({ "items": [{ "id": "a" }, { "id": "b" }] }),
    );

    // Never scripted with a response: any call panics the test.
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![]));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()),
        &assignment,
        "unused — the deterministic path never reaches the model",
        None,
    )
    .await;

    assert!(!fired, "a deterministic watch's first poll must seed a baseline, not fire");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched on the seeding poll");

    let scratchpad = persistence.assignment_scratchpads.get("watch-det-seed").await.unwrap().unwrap();
    assert_eq!(scratchpad.snapshots.len(), 2, "both resolved items must be seeded into the snapshot store");
    assert_eq!(
        scratchpad.last_extraction_path,
        ExtractionPath::Deterministic,
        "the scratchpad must record which extraction path ran"
    );
    assert_eq!(scratchpad.last_inferred_tier, Some(Tier::Deterministic));
}

#[tokio::test]
async fn deterministic_extraction_full_tick_fires_with_zero_model_calls() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction(
        "watch-det-fire",
        "agent-1",
        contract,
        "det_fire_srv",
        "det_fire_tool",
        items_by_id_extraction_plan(),
        true,
    );

    // Constructed with an empty response queue and never given one:
    // `AgentWatchDetector::observe`/`observe_for_authoring` panic the
    // instant either is called, on either poll below.
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![]));

    stash_structured_payload(
        "det_fire_srv",
        "det_fire_tool",
        serde_json::json!({ "items": [{ "id": "a" }, { "id": "b" }] }),
    );
    let seeding_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()),
        &assignment,
        "unused — the deterministic path never reaches the model",
        None,
    )
    .await;
    assert!(!seeding_fired, "the first deterministic-tier poll must seed a baseline, not fire");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched on the seeding poll");

    // A genuinely new row (`c`) appears in the source on the next poll.
    stash_structured_payload(
        "det_fire_srv",
        "det_fire_tool",
        serde_json::json!({ "items": [{ "id": "a" }, { "id": "b" }, { "id": "c" }] }),
    );
    let second_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()),
        &assignment,
        "unused — the deterministic path never reaches the model",
        None,
    )
    .await;
    assert!(second_fired, "a genuinely new row on the next poll must fire — with zero detector calls so far");

    let (_agent_id, _message) = rx.try_recv().expect("exactly one message must have been dispatched");
    assert!(rx.try_recv().is_err(), "the new row must fire exactly once");

    let scratchpad = persistence.assignment_scratchpads.get("watch-det-fire").await.unwrap().unwrap();
    assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Deterministic);
    assert_eq!(scratchpad.last_inferred_tier, Some(Tier::Deterministic));
}

#[tokio::test]
async fn extraction_none_still_routes_through_the_llm_detector() {
    // Byte-for-byte the pre-extraction-plan behavior: `extraction` is
    // `None` (the default `agent_watch_assignment_with_contract` leaves
    // it), so every poll must still ask `detector.observe` — a
    // `ScriptedDetector` scripted with real responses (not an empty,
    // panic-on-call queue) proves the model path is actually reachable,
    // and the scratchpad's recorded path/tier prove it's the path that
    // actually ran, not merely available.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-llm-fallback", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate("a")]),
        Ok(vec![candidate("a"), candidate("b")]),
    ]));

    let seeding_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None,
    )
    .await;
    assert!(!seeding_fired, "the first poll must still seed a baseline, not fire");

    let seeded = persistence.assignment_scratchpads.get("watch-llm-fallback").await.unwrap().unwrap();
    assert_eq!(seeded.last_extraction_path, ExtractionPath::Llm, "no extraction plan is configured");
    assert_eq!(seeded.last_inferred_tier, None, "no plan means no tier was ever inferred");

    let second_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None,
    )
    .await;
    assert!(second_fired, "a genuinely new candidate from the detector must still fire");
    assert!(rx.try_recv().is_ok(), "the fire must have actually dispatched a message");

    let scratchpad = persistence.assignment_scratchpads.get("watch-llm-fallback").await.unwrap().unwrap();
    assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Llm);
    assert_eq!(scratchpad.last_inferred_tier, None);
}

#[tokio::test]
async fn model_call_counter_only_increments_when_the_llm_detector_actually_spawns() {
    // The deterministic extraction path never calls `detector.observe`
    // at all, so it must record zero model calls; a poll with no
    // extraction plan configured always falls back to the LLM detector,
    // so it must record exactly one. Proves the counter is keyed on
    // whether a model session actually spawned for the poll, not on the
    // poll itself.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let det_contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let det_assignment = agent_watch_assignment_with_extraction(
        "watch-model-call-det",
        "agent-1",
        det_contract,
        "model_call_det_srv",
        "model_call_det_tool",
        items_by_id_extraction_plan(),
        true,
    );
    stash_structured_payload(
        "model_call_det_srv",
        "model_call_det_tool",
        serde_json::json!({ "items": [{ "id": "a" }] }),
    );
    // Never scripted with a response: any call panics the test — the
    // whole point of this path is that it must never reach the detector.
    let det_detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![]));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &det_detector,
        &Arc::new(Registry::new()), &det_assignment, "unused", None)
        .await;

    let det_scratchpad =
        persistence.assignment_scratchpads.get("watch-model-call-det").await.unwrap().unwrap();
    assert!(
        det_scratchpad.model_calls_by_day.is_empty(),
        "a deterministic-tier poll spawns no model session and must not increment the counter"
    );

    let llm_contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let llm_assignment = agent_watch_assignment_with_contract("watch-model-call-llm", "agent-1", llm_contract);
    let llm_detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a")])]));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &llm_detector,
        &Arc::new(Registry::new()), &llm_assignment, "watch", None)
        .await;

    let llm_scratchpad =
        persistence.assignment_scratchpads.get("watch-model-call-llm").await.unwrap().unwrap();
    let total_calls: u32 = llm_scratchpad.model_calls_by_day.values().sum();
    assert_eq!(total_calls, 1, "a poll with no extraction plan must fall back to the model exactly once");
}

// -- extraction-plan authoring, structural-failure fallback, and health (this phase) --

#[tokio::test]
async fn deterministic_extraction_legitimately_empty_resolution_is_a_quiet_tick_not_an_error() {
    // resolve() finding zero items because the source genuinely has none
    // right now must be indistinguishable from any other quiet poll: no
    // health event, no degraded flag, still zero model calls.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let mut health_rx = event_bus.subscribe();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction(
        "watch-det-empty",
        "agent-1",
        contract,
        "det_empty_srv",
        "det_empty_tool",
        items_by_id_extraction_plan(),
        true,
    );
    stash_structured_payload("det_empty_srv", "det_empty_tool", serde_json::json!({ "items": [] }));

    // Never scripted with a response: any call panics the test.
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![]));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;

    assert!(!fired, "an empty resolution has nothing to fire on");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched");

    let scratchpad = persistence.assignment_scratchpads.get("watch-det-empty").await.unwrap().unwrap();
    assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Deterministic, "an empty Ok is still the deterministic path, not a fallback");
    assert!(!scratchpad.extraction_plan_degraded, "a legitimately empty result must never be treated as degraded");
    assert_eq!(scratchpad.extraction_plan_degraded_reason, None);

    let health_texts = drain_system_message_texts(&mut health_rx);
    assert!(
        !health_texts.iter().any(|t| t.contains("extraction plan")),
        "an empty-but-valid resolution must not emit any extraction-plan health event; got: {health_texts:?}"
    );
}

#[tokio::test]
async fn deterministic_extraction_structural_failure_falls_back_marks_degraded_and_emits_health_event() {
    // The payload shape no longer has the "items" array the plan's
    // selector expects — a genuine BindError, not an empty result — must
    // fall back to the model for this one poll, be visibly unhealthy
    // with the real structured-error detail attached, and never be
    // mistaken for "nothing new."
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let mut health_rx = event_bus.subscribe();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction(
        "watch-det-broken",
        "agent-1",
        contract,
        "det_broken_srv",
        "det_broken_tool",
        items_by_id_extraction_plan(),
        true,
    );
    // The tool's response shape moved — no "items" key at all.
    stash_structured_payload(
        "det_broken_srv",
        "det_broken_tool",
        serde_json::json!({ "records": [{ "id": "a" }] }),
    );

    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a")])]));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;

    assert!(!fired, "a structural-failure poll must never fire");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched on a degraded poll");

    let scratchpad = persistence.assignment_scratchpads.get("watch-det-broken").await.unwrap().unwrap();
    assert_eq!(
        scratchpad.last_extraction_path,
        ExtractionPath::Llm,
        "a structural BindError must fall back to the model, not silently resolve to zero candidates"
    );
    assert_eq!(
        scratchpad.last_inferred_tier, None,
        "the model handled this poll, not the deterministic plan — recording the attempted tier here would \
         let a fallback poll misreport itself as a healthy deterministic one"
    );
    assert!(scratchpad.extraction_plan_degraded, "a structural failure must set the degraded latch");
    let reason = scratchpad.extraction_plan_degraded_reason.expect("a degraded plan must carry the real reason");
    assert!(
        reason.contains("items") && reason.contains("did not resolve"),
        "the degraded reason must carry the structured BindError's own detail, not a generic message; got: {reason}"
    );

    let health_texts = drain_system_message_texts(&mut health_rx);
    assert!(
        health_texts.iter().any(|t| t.contains("no longer matches") && t.contains(&reason)),
        "the user must see a health event naming the real cause; got: {health_texts:?}"
    );
}

#[tokio::test]
async fn deterministic_extraction_fallback_after_structural_failure_does_not_fire_even_on_a_new_candidate() {
    // Isolates `force_seed_only` from `is_first_poll`: the plan breaks on
    // a poll *after* a clean baseline already exists, and the model
    // fallback reports a genuinely new, matching candidate. Even so,
    // nothing may fire — an extraction mechanism hiccup must never be
    // trusted to mass-fire on identity keys it may have gotten wrong.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction(
        "watch-det-fallback-quiet",
        "agent-1",
        contract,
        "det_fbq_srv",
        "det_fbq_tool",
        items_by_id_extraction_plan(),
        true,
    );

    // Poll 1: clean deterministic seed — establishes a real baseline so
    // `is_first_poll` is false on poll 2.
    stash_structured_payload("det_fbq_srv", "det_fbq_tool", serde_json::json!({ "items": [{ "id": "a" }] }));
    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a"), candidate("new-item")])]));
    let seeding_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;
    assert!(!seeding_fired, "poll 1 must seed, not fire");

    // Poll 2: the shape breaks — falls back to the model, which reports
    // a brand-new, already-matching item that would normally fire.
    stash_structured_payload("det_fbq_srv", "det_fbq_tool", serde_json::json!({ "records": [{ "id": "a" }] }));
    let second_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;

    assert!(!second_fired, "a genuinely new candidate must still not fire while the extraction plan is degraded");
    assert!(rx.try_recv().is_err(), "nothing must have been dispatched");

    let scratchpad = persistence.assignment_scratchpads.get("watch-det-fallback-quiet").await.unwrap().unwrap();
    assert!(scratchpad.extraction_plan_degraded);
    assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Llm);
}

#[tokio::test]
async fn extraction_plan_invalidated_when_contract_is_amended_and_is_re_authored() {
    // A scratchpad-authored plan (decision: lives on the scratchpad, not
    // the trigger's `contract`) tagged with a fingerprint that no longer
    // matches the live contract must be treated as absent — never used,
    // even though its selector would otherwise resolve fine against the
    // current stash content — and a fresh one authored in its place.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let live_fingerprint = contract.fingerprint();
    let mut assignment = agent_watch_assignment_with_contract("watch-plan-stale", "agent-1", contract);
    if let AssignmentTrigger::AgentWatch { connector_scope, extraction_tool, extraction_output_schema_declared, .. } =
        &mut assignment.trigger
    {
        *connector_scope = Some("stale_srv".to_string());
        *extraction_tool = Some("stale_tool".to_string());
        *extraction_output_schema_declared = true;
    }

    // Seed the scratchpad as if a plan had been authored against some
    // *other* (now-amended) contract — a fingerprint that cannot match
    // the live contract computed above.
    persistence
        .assignment_scratchpads
        .set(
            "watch-plan-stale",
            &AssignmentScratchpad {
                extraction_plan: Some(items_by_id_extraction_plan()),
                extraction_plan_fingerprint: Some("stale-fingerprint-from-a-prior-contract".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Stash content that the stale plan's own selector would resolve
    // just fine, if it were (wrongly) trusted — proving the mismatch
    // check, not a coincidentally-broken selector, is what's under test.
    stash_structured_payload("stale_srv", "stale_tool", serde_json::json!({ "items": [{ "id": "a" }] }));

    // This poll must fall back to the model regardless (a freshly
    // authored plan only takes effect starting the *next* poll).
    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a")])]));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;
    assert!(!fired, "first poll under the new contract must seed, not fire");

    let scratchpad = persistence.assignment_scratchpads.get("watch-plan-stale").await.unwrap().unwrap();
    assert_eq!(
        scratchpad.last_extraction_path,
        ExtractionPath::Llm,
        "the stale plan must never be used, even though its selector would have resolved fine"
    );
    assert_eq!(
        scratchpad.extraction_plan_fingerprint.as_deref(),
        Some(live_fingerprint.as_str()),
        "a fresh plan must be authored and tagged against the live contract's fingerprint"
    );
    assert!(scratchpad.extraction_plan.is_some(), "a fresh plan must have been authored from the current stash sample");
}

#[tokio::test]
async fn extraction_plan_is_authored_from_stash_and_used_deterministically_on_the_next_poll() {
    // End-to-end proof of the actual goal: no manual `extraction` is
    // ever configured on the trigger — the plan is authored purely from
    // a payload sample and, once authored, the very next poll resolves
    // deterministically with zero model calls.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let mut assignment = agent_watch_assignment_with_contract("watch-plan-authored", "agent-1", contract);
    if let AssignmentTrigger::AgentWatch { connector_scope, extraction_tool, extraction_output_schema_declared, .. } =
        &mut assignment.trigger
    {
        *connector_scope = Some("auth_srv".to_string());
        *extraction_tool = Some("auth_tool".to_string());
        *extraction_output_schema_declared = true;
    }

    // Poll 1: no plan exists yet, so this poll must still ask the model
    // — but a plan is authored as a side effect, from this same stash
    // sample, for the *next* poll to use.
    stash_structured_payload("auth_srv", "auth_tool", serde_json::json!({ "items": [{ "id": "a" }, { "id": "b" }] }));
    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a"), candidate("b")])]));
    let seeding_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;
    assert!(!seeding_fired, "poll 1 must seed, not fire");

    let seeded = persistence.assignment_scratchpads.get("watch-plan-authored").await.unwrap().unwrap();
    assert_eq!(seeded.last_extraction_path, ExtractionPath::Llm, "poll 1 has no plan yet, so it must still ask the model");
    assert!(seeded.extraction_plan.is_some(), "a plan must have been authored from poll 1's own stash sample");

    // Poll 2: a genuinely new row appears. The detector is never
    // scripted with a second response — any call panics the test — so a
    // fire here can only have come from the freshly authored plan.
    stash_structured_payload(
        "auth_srv",
        "auth_tool",
        serde_json::json!({ "items": [{ "id": "a" }, { "id": "b" }, { "id": "c" }] }),
    );
    let second_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;

    assert!(second_fired, "a genuinely new row must fire — with zero further model calls");
    assert!(rx.try_recv().is_ok(), "the fire must have actually dispatched a message");

    let scratchpad = persistence.assignment_scratchpads.get("watch-plan-authored").await.unwrap().unwrap();
    assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Deterministic);
}

// -- text-only (no `structuredContent`) rescue via `StashedPayload::json_body` --

#[test]
fn author_extraction_plan_authors_from_a_text_only_json_array_payload() {
    // Mirrors `extraction_plan_is_authored_from_stash_and_used_deterministically_on_the_next_poll`'s
    // authoring step, but the stash here holds ONLY a text block — no
    // `structuredContent` at all — as many real MCP servers actually
    // return. `author_extraction_plan` must still author a plan by
    // parsing the text as JSON via `StashedPayload::json_body`.
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let stashed = payload_stash::StashedPayload {
        server: "srv".to_string(),
        tool: "tool".to_string(),
        args: serde_json::json!({}),
        args_hash: "hash".to_string(),
        captured_at: Utc::now(),
        structured: None,
        text: Some(r#"[{"id":"a"},{"id":"b"}]"#.to_string()),
    };

    let plan = author_extraction_plan(&contract, &stashed, None, &[])
        .plan
        .expect("a text-only JSON array body must still author a plan");
    assert_eq!(
        plan.selector.kind,
        extractor_contract::ExtractorKind::JsonPath { path: String::new() },
        "a root-array body selects the whole document, same as a structured root array would"
    );
    assert_eq!(plan.identity, extractor_contract::ExtractorKind::JsonPath { path: "id".to_string() });
}

#[test]
fn author_extraction_plan_selects_the_row_shaped_array_over_a_metadata_sibling_array() {
    // Regression coverage for the exact shape a live Notion
    // `notion-query-data-sources` call returns: a top-level `results`
    // array of row objects sits alongside `data_source_ids`, itself an
    // array too (of plain id strings) — and, unqualified, `data_source_ids`
    // sorts alphabetically ahead of `results`. `author_extraction_plan`
    // must still select `results`, never the metadata sibling, or every
    // future poll would try to read Company/First name/Last Name out of
    // a bare id string and quarantine every row forever.
    let mut contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(url)", vec![]);
    contract.identity.source_field = Some("url".to_string());

    let stashed = payload_stash::StashedPayload {
        server: "srv".to_string(),
        tool: "tool".to_string(),
        args: serde_json::json!({}),
        args_hash: "hash".to_string(),
        captured_at: Utc::now(),
        structured: Some(serde_json::json!({
            "results": [
                {"url": "https://app.notion.com/a1b2c3d4e5f60718293a4b5c6d7e8f90", "Company": "Rose's Roses"},
                {"url": "https://app.notion.com/0f9e8d7c6b5a49382716f5e4d3c2b1a0", "Company": "Second Co"}
            ],
            "has_more": false,
            "data_source_ids": ["collection://11112222-3333-4444-5555-666677778888"]
        })),
        text: None,
    };

    let plan = author_extraction_plan(&contract, &stashed, None, &[])
        .plan
        .expect("an object body with a row-shaped array field must author a plan");
    assert_eq!(
        plan.selector.expr, "results",
        "must select the row-shaped `results` array, not the alphabetically-earlier `data_source_ids`"
    );
    assert_eq!(plan.selector.kind, extractor_contract::ExtractorKind::JsonPath { path: "results".to_string() });
}

#[test]
fn author_extraction_plan_returns_none_for_a_text_only_html_payload() {
    // The rescue is JSON-in-text only — prose/markup that doesn't parse
    // as JSON at all must leave authoring exactly where it was before
    // this change: no plan, keep falling back to the model.
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let stashed = payload_stash::StashedPayload {
        server: "srv".to_string(),
        tool: "tool".to_string(),
        args: serde_json::json!({}),
        args_hash: "hash".to_string(),
        captured_at: Utc::now(),
        structured: None,
        text: Some("<html><body>not json</body></html>".to_string()),
    };

    assert!(author_extraction_plan(&contract, &stashed, None, &[]).plan.is_none());
}

// -- Tier 2: tabular markup embedded in a string field --

/// Builds a [`payload_stash::StashedPayload`] whose `structured` body is
/// `structured` — the shape every Tier 2 test below starts from.
fn stashed_structured(structured: serde_json::Value) -> payload_stash::StashedPayload {
    payload_stash::StashedPayload {
        server: "srv".to_string(),
        tool: "tool".to_string(),
        args: serde_json::json!({}),
        args_hash: "hash".to_string(),
        captured_at: Utc::now(),
        structured: Some(structured),
        text: None,
    }
}

/// A `WatchContract` fixture for the Tier 2 tests: `NativeId` keyed on
/// `name`, with `score` as the one material field — matching the
/// `Name`/`Score` header columns every HTML/markdown fixture table below
/// uses.
fn name_score_contract() -> WatchContract {
    let mut contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(name)", vec!["score"]);
    contract.identity.source_field = Some("name".to_string());
    contract
}

#[test]
fn author_extraction_plan_authors_a_tier_2_plan_from_an_html_table_with_a_th_header() {
    let stashed = stashed_structured(serde_json::json!({
        "text": "<table><tr><th>Name</th><th>Score</th></tr><tr><td>Alice</td><td>10</td></tr></table>",
    }));
    let model_candidates =
        vec![candidate_with_payload("alice", serde_json::json!({"name": "Alice", "score": "10"}))];

    let attempt = author_extraction_plan(&name_score_contract(), &stashed, None, &model_candidates);
    assert!(attempt.degraded_reason.is_none());
    let plan = attempt.plan.expect("a single <th>-headed table must author and freeze a Tier 2 plan");
    assert_eq!(
        plan.selector.kind,
        extractor_contract::ExtractorKind::Table {
            field_path: "text".to_string(),
            columns: vec!["name".to_string(), "score".to_string()],
            identity_columns: vec!["name".to_string()],
        }
    );
    assert_eq!(plan.selector.expr, "text");
}

#[test]
fn author_extraction_plan_authors_a_tier_2_plan_from_an_html_table_with_no_th_header() {
    let stashed = stashed_structured(serde_json::json!({
        "text": "<table><tr><td>Name</td><td>Score</td></tr><tr><td>Bob</td><td>20</td></tr></table>",
    }));
    let model_candidates = vec![candidate_with_payload("bob", serde_json::json!({"name": "Bob", "score": "20"}))];

    let attempt = author_extraction_plan(&name_score_contract(), &stashed, None, &model_candidates);
    let plan =
        attempt.plan.expect("a table with no <th> at all must still use its first <tr> as the header row");
    assert_eq!(
        plan.selector.kind,
        extractor_contract::ExtractorKind::Table {
            field_path: "text".to_string(),
            columns: vec!["name".to_string(), "score".to_string()],
            identity_columns: vec!["name".to_string()],
        }
    );
}

#[test]
fn author_extraction_plan_authors_a_tier_2_plan_from_a_markdown_pipe_table() {
    let stashed = stashed_structured(serde_json::json!({
        "text": "| Name | Score |\n|---|---|\n| Dana | 40 |\n| Eve | 50 |",
    }));
    let model_candidates = vec![
        candidate_with_payload("dana", serde_json::json!({"name": "Dana", "score": "40"})),
        candidate_with_payload("eve", serde_json::json!({"name": "Eve", "score": "50"})),
    ];

    let attempt = author_extraction_plan(&name_score_contract(), &stashed, None, &model_candidates);
    let plan = attempt.plan.expect("a markdown pipe table must author and freeze a Tier 2 plan");
    assert_eq!(
        plan.selector.kind,
        extractor_contract::ExtractorKind::Table {
            field_path: "text".to_string(),
            columns: vec!["name".to_string(), "score".to_string()],
            identity_columns: vec!["name".to_string()],
        }
    );
}

#[test]
fn author_extraction_plan_returns_none_when_zero_tables_are_found() {
    let stashed = stashed_structured(serde_json::json!({ "text": "just some prose, no tables here" }));
    let attempt = author_extraction_plan(&name_score_contract(), &stashed, None, &[]);
    assert!(attempt.plan.is_none());
    assert!(attempt.degraded_reason.is_none(), "zero candidates is the ordinary 'nothing to author yet' case");
}

#[test]
fn author_extraction_plan_returns_none_when_two_tables_are_found() {
    let stashed = stashed_structured(serde_json::json!({
        "text": "<table><tr><th>A</th></tr><tr><td>1</td></tr></table> and \
                  <table><tr><th>B</th></tr><tr><td>2</td></tr></table>",
    }));
    let attempt = author_extraction_plan(&name_score_contract(), &stashed, None, &[]);
    assert!(attempt.plan.is_none(), "an ambiguous multi-table payload must never be guessed at");
    assert!(attempt.degraded_reason.is_none());
}

#[test]
fn author_extraction_plan_does_not_freeze_a_tier_2_plan_on_a_replay_mismatch() {
    let stashed = stashed_structured(serde_json::json!({
        "text": "<table><tr><th>Name</th><th>Score</th></tr><tr><td>Alice</td><td>10</td></tr></table>",
    }));
    // The model reports a different score for the same row — a
    // disagreement the table parser's own replay must catch before ever
    // trusting this candidate plan enough to freeze it.
    let model_candidates =
        vec![candidate_with_payload("alice", serde_json::json!({"name": "Alice", "score": "999"}))];

    let attempt = author_extraction_plan(&name_score_contract(), &stashed, None, &model_candidates);
    assert!(attempt.plan.is_none(), "a Tier 2 candidate must never freeze when it disagrees with the model");
    assert!(
        attempt.degraded_reason.is_some(),
        "a replay mismatch must leave a diagnosable reason, unlike the ordinary 'nothing found' case"
    );
}

#[test]
fn author_extraction_plan_freezes_a_tier_2_plan_once_blank_template_rows_are_filtered_out() {
    // The exact ground-truth bug this fix targets: a table with 1 real
    // data row and 4 blank template rows. Pre-fix, the parser counted
    // all 5 as data rows and the gate refused to freeze forever because
    // "5 != 1" — `author_extraction_plan_does_not_freeze_a_tier_2_plan_on_a_replay_mismatch`
    // covers the gate correctly refusing a genuine disagreement; this
    // covers blank-row filtering making the two sides agree instead.
    let stashed = stashed_structured(serde_json::json!({
        "text": "<table><tr><th>Name</th><th>Score</th></tr>\
            <tr><td>Alice</td><td>10</td></tr>\
            <tr><td></td><td></td></tr>\
            <tr><td></td><td></td></tr>\
            <tr><td></td><td></td></tr>\
            <tr><td></td><td></td></tr></table>",
    }));
    let model_candidates =
        vec![candidate_with_payload("alice", serde_json::json!({"name": "Alice", "score": "10"}))];

    let attempt = author_extraction_plan(&name_score_contract(), &stashed, None, &model_candidates);
    assert!(
        attempt.degraded_reason.is_none(),
        "blank template rows must never be counted as a parser/model disagreement"
    );
    let plan = attempt.plan.expect("filtering blank rows must let the Tier 2 plan freeze");
    assert_eq!(
        plan.selector.kind,
        extractor_contract::ExtractorKind::Table {
            field_path: "text".to_string(),
            columns: vec!["name".to_string(), "score".to_string()],
            identity_columns: vec!["name".to_string()],
        }
    );
}

#[test]
fn author_extraction_plan_freezes_a_tier_2_plan_when_identities_match_out_of_order() {
    let stashed = stashed_structured(serde_json::json!({
        "text": "| Name | Score |\n|---|---|\n| Dana | 40 |\n| Eve | 50 |",
    }));
    // Same two rows the parser found, but the model reports them in the
    // OPPOSITE order — the agreement gate must compare identities as a
    // set, never positionally.
    let model_candidates = vec![
        candidate_with_payload("eve", serde_json::json!({"name": "Eve", "score": "50"})),
        candidate_with_payload("dana", serde_json::json!({"name": "Dana", "score": "40"})),
    ];

    let attempt = author_extraction_plan(&name_score_contract(), &stashed, None, &model_candidates);
    assert!(
        attempt.degraded_reason.is_none(),
        "matching identity sets reported in different orders must not be treated as a mismatch"
    );
    assert!(attempt.plan.is_some(), "an order-insensitive identity match must still freeze a Tier 2 plan");
}

#[test]
fn author_extraction_plan_does_not_freeze_when_row_counts_match_but_identities_differ() {
    let stashed = stashed_structured(serde_json::json!({
        "text": "<table><tr><th>Name</th><th>Score</th></tr><tr><td>Alice</td><td>10</td></tr></table>",
    }));
    // Same row COUNT (1 == 1), but the model extracted an entirely
    // different row's identity from the same payload — a count-only
    // gate would have let this promote a parser producing the wrong
    // rows entirely.
    let model_candidates = vec![candidate_with_payload("bob", serde_json::json!({"name": "Bob", "score": "10"}))];

    let attempt = author_extraction_plan(&name_score_contract(), &stashed, None, &model_candidates);
    assert!(attempt.plan.is_none(), "matching row counts must never substitute for matching identities");
    let reason = attempt.degraded_reason.expect("an identity mismatch must leave a diagnosable reason");
    assert!(reason.contains("Alice"), "the mismatch reason must name what the parser actually produced: {reason}");
    assert!(reason.contains("Bob"), "the mismatch reason must name what the model actually produced: {reason}");
}

#[tokio::test]
async fn select_agent_watch_candidates_marks_degraded_on_a_fresh_tier_2_replay_mismatch() {
    // Regression for a bug where the fresh-authoring Tier 2 replay-
    // mismatch branch set `extraction_plan_degraded_reason` WITHOUT
    // setting `extraction_plan_degraded`, violating that field's own
    // documented invariant ("Some only while extraction_plan_degraded is
    // true") and making `derive_extraction_health` report the generic
    // `ModelAssisted` reason instead of this specific, diagnosable one.
    let event_bus = Arc::new(EventBus::new(64));
    let registry = Arc::new(Registry::new());
    let contract = name_score_contract();
    let mut assignment = agent_watch_assignment_with_contract("watch-tier2-fresh-mismatch", "agent-1", contract.clone());
    if let AssignmentTrigger::AgentWatch { connector_scope, extraction_tool, .. } = &mut assignment.trigger {
        *connector_scope = Some("tier2_fresh_srv".to_string());
        *extraction_tool = Some("tier2_fresh_tool".to_string());
    }
    stash_structured_payload(
        "tier2_fresh_srv",
        "tier2_fresh_tool",
        serde_json::json!({
            "text": "<table><tr><th>Name</th><th>Score</th></tr><tr><td>Alice</td><td>10</td></tr></table>",
        }),
    );
    // The model reports a different score for the same row — the same
    // disagreement `author_extraction_plan_does_not_freeze_a_tier_2_plan_on_a_replay_mismatch`
    // exercises directly, but here driven through the tick-level
    // candidate selection this fix actually lives in.
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![
        candidate_with_payload("alice", serde_json::json!({"name": "Alice", "score": "999"})),
    ])]));
    let mut scratchpad = AssignmentScratchpad::default();

    let result = select_agent_watch_candidates(
        &detector,
        &registry,
        &event_bus,
        &assignment,
        "unused",
        Some("tier2_fresh_srv"),
        &contract,
        &mut scratchpad,
        None,
        Some("tier2_fresh_tool"),
        None,
        false,
    )
    .await;

    assert!(result.is_ok());
    assert!(scratchpad.extraction_plan.is_none(), "a mismatched Tier 2 candidate must never freeze");
    assert!(
        scratchpad.extraction_plan_degraded,
        "a Tier 2 replay mismatch must mark the watch degraded, matching its own recorded reason"
    );
    let reason = scratchpad.extraction_plan_degraded_reason.clone().expect("a replay mismatch must carry a reason");
    assert!(!reason.is_empty());

    // The bug's user-visible symptom: `derive_extraction_health` must
    // report this specific cause, not fall through to the generic
    // "no deterministic extraction plan is bound" copy.
    let (health, health_reason) = derive_extraction_health(Some(&scratchpad), Some("tier2_fresh_tool"), false);
    assert_eq!(health, ExtractionHealth::Degraded);
    assert_eq!(health_reason.as_deref(), Some(reason.as_str()));
}

#[test]
fn tabular_extraction_plan_row_ids_are_stable_regardless_of_row_order() {
    // Deterministic-id regression coverage for the row-id-drift bug: a
    // row's identity must be a pure function of its own field values,
    // never its position, so the same two rows parsed in swapped order
    // must still yield the same id per name.
    let columns = vec!["name".to_string(), "score".to_string()];
    let plan = ExtractionPlan {
        selector: extractor_contract::Selector {
            kind: extractor_contract::ExtractorKind::Table {
                field_path: "text".to_string(),
                columns,
                identity_columns: vec!["name".to_string()],
            },
            expr: "text".to_string(),
        },
        identity: extractor_contract::ExtractorKind::Hash,
        predicate: extractor_contract::Predicate::NotEmpty { path: "name".to_string() },
    };

    let forward = serde_json::json!({
        "text": "<table><tr><th>Name</th><th>Score</th></tr><tr><td>Alice</td><td>10</td></tr><tr><td>Bob</td><td>20</td></tr></table>",
    });
    let reversed = serde_json::json!({
        "text": "<table><tr><th>Name</th><th>Score</th></tr><tr><td>Bob</td><td>20</td></tr><tr><td>Alice</td><td>10</td></tr></table>",
    });

    let forward_res = extractor_contract::resolve(&plan, Some(&forward), None).expect("must resolve");
    let reversed_res = extractor_contract::resolve(&plan, Some(&reversed), None).expect("must resolve");

    let id_for = |res: &extractor_contract::Resolution, name: &str| {
        res.items
            .iter()
            .find(|item| item.value.get("name").and_then(Value::as_str) == Some(name))
            .unwrap_or_else(|| panic!("row {name} must be present"))
            .id
            .clone()
    };

    assert_eq!(
        id_for(&forward_res, "Alice"),
        id_for(&reversed_res, "Alice"),
        "the same row's id must not depend on its position in the source"
    );
    assert_eq!(id_for(&forward_res, "Bob"), id_for(&reversed_res, "Bob"));
    assert_ne!(
        id_for(&forward_res, "Alice"),
        id_for(&forward_res, "Bob"),
        "two different rows must never collide onto the same id"
    );
}

#[test]
fn author_extraction_plan_authors_and_freezes_a_tier_2_plan_for_the_notion_free_form_list_shape() {
    // Reproduces the actual failing shape a live `notion-fetch` MCP call
    // returned: a `{metadata, title, url, text}` envelope whose `text`
    // field is an HTML table of client rows — the exact payload that,
    // pre-fix, never froze a plan and paid for a model call on every
    // single poll forever.
    let mut contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(first)", vec!["company"]);
    contract.identity = IdentitySpec {
        strategy: IdentityStrategy::CompositeNative,
        source_field: None,
        format: None,
        fields: vec!["first".to_string(), "last".to_string()],
        rationale: "test fixture: composite identity on first+last".to_string(),
    };

    let html = "<table><tr><th>First</th><th>Last</th><th>Company</th></tr>\
        <tr><td>Peter</td><td>Grace</td><td>Peter's Pool Construction</td></tr>\
        <tr><td>Martha</td><td>Johns</td><td>Martha's Bakery</td></tr>\
        <tr><td>David</td><td>Button</td><td>Button Consulting</td></tr>\
        <tr><td>John</td><td>Stones</td><td>Stones &amp; Co</td></tr></table>";
    let stashed = stashed_structured(serde_json::json!({
        "metadata": {"source": "notion"},
        "title": "Clients",
        "url": "https://notion.so/abc",
        "text": html,
    }));

    let model_candidates = vec![
        candidate_with_payload(
            "peter-grace",
            serde_json::json!({"first": "Peter", "last": "Grace", "company": "Peter's Pool Construction"}),
        ),
        candidate_with_payload(
            "martha-johns",
            serde_json::json!({"first": "Martha", "last": "Johns", "company": "Martha's Bakery"}),
        ),
        candidate_with_payload(
            "david-button",
            serde_json::json!({"first": "David", "last": "Button", "company": "Button Consulting"}),
        ),
        candidate_with_payload(
            "john-stones",
            serde_json::json!({"first": "John", "last": "Stones", "company": "Stones & Co"}),
        ),
    ];

    let attempt = author_extraction_plan(&contract, &stashed, None, &model_candidates);
    assert!(attempt.degraded_reason.is_none());
    let plan = attempt.plan.expect(
        "a table embedded in the `text` field, agreeing with the model's own extraction, must author and \
         freeze a Tier 2 plan",
    );
    assert_eq!(
        plan.selector.kind,
        extractor_contract::ExtractorKind::Table {
            field_path: "text".to_string(),
            columns: vec!["first".to_string(), "last".to_string(), "company".to_string()],
            identity_columns: vec!["first".to_string(), "last".to_string()],
        }
    );
}

#[tokio::test]
async fn extraction_plan_is_authored_from_text_only_stash_and_resolves_probabilistically_on_the_next_poll() {
    // End-to-end sibling of `extraction_plan_is_authored_from_stash_and_used_deterministically_on_the_next_poll`:
    // same shape, except the server here never populates
    // `structuredContent` — only a text block holding a JSON-stringified
    // array. Proves three things in one pass: (1) authoring succeeds off
    // a text-only sample, (2) the very next poll resolves with zero
    // model calls, and (3) — the tier-honesty requirement — that poll's
    // tier is `Probabilistic`, never `Deterministic`, even though
    // `extraction_output_schema_declared` is `true`: a text-parsed body
    // never carries a real server promise about its shape.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let mut assignment = agent_watch_assignment_with_contract("watch-text-rescue-authored", "agent-1", contract);
    if let AssignmentTrigger::AgentWatch { connector_scope, extraction_tool, extraction_output_schema_declared, .. } =
        &mut assignment.trigger
    {
        *connector_scope = Some("text_auth_srv".to_string());
        *extraction_tool = Some("text_auth_tool".to_string());
        *extraction_output_schema_declared = true;
    }

    // Poll 1: no plan exists yet, so this poll must still ask the model
    // — but a plan is authored as a side effect, from this same
    // text-only stash sample, for the *next* poll to use.
    stash_text_payload("text_auth_srv", "text_auth_tool", r#"[{"id":"a"},{"id":"b"}]"#);
    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a"), candidate("b")])]));
    let seeding_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;
    assert!(!seeding_fired, "poll 1 must seed, not fire");

    let seeded = persistence.assignment_scratchpads.get("watch-text-rescue-authored").await.unwrap().unwrap();
    assert_eq!(seeded.last_extraction_path, ExtractionPath::Llm, "poll 1 has no plan yet, so it must still ask the model");
    assert!(seeded.extraction_plan.is_some(), "a plan must have been authored from poll 1's text-only stash sample");

    // Poll 2: a genuinely new row appears. The detector is never
    // scripted with a second response — any call panics the test — so a
    // fire here can only have come from the freshly authored, text-rescued plan.
    stash_text_payload("text_auth_srv", "text_auth_tool", r#"[{"id":"a"},{"id":"b"},{"id":"c"}]"#);
    let second_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;

    assert!(second_fired, "a genuinely new row extracted from the text-rescued body must fire");
    assert!(rx.try_recv().is_ok(), "the fire must have actually dispatched a message");

    let scratchpad = persistence.assignment_scratchpads.get("watch-text-rescue-authored").await.unwrap().unwrap();
    assert_eq!(
        scratchpad.last_extraction_path,
        ExtractionPath::Probabilistic,
        "a text-parsed body must cap at Probabilistic, never claim Deterministic, regardless of \
         extraction_output_schema_declared"
    );
    assert_eq!(
        scratchpad.model_calls_by_day.values().sum::<u32>(),
        1,
        "only poll 1 (before a plan existed) should have touched the model; poll 2 must resolve for free"
    );
}

#[tokio::test]
async fn end_to_end_notion_table_authoring_unlocks_a_tabular_extraction_plan_with_zero_model_calls_on_the_next_poll(
) {
    // The end-to-end regression this whole fix exists for: a live watch
    // over a Notion-table-shaped response — an HTML `<table>` embedded
    // inside a tool response's own `text` field, with no native page id
    // anywhere in the row data — used to be genuinely unauthorable
    // (`mode` was never shown to the model, and even a `new_only`
    // proposal was rejected for having empty `change.material_fields`).
    // Drives the REAL entry point (`run_agent_watch_tick`, never an
    // internal helper directly) through three polls: (1) authoring off
    // the table payload, (2) the first contract-bound poll, which
    // authors a Tier 2 tabular extraction plan from that same payload as
    // a side effect, and (3) the next poll, which must resolve entirely
    // off that frozen plan — zero calls to the model detector at all.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let mut assignment = agent_watch_assignment("watch-e2e-notion-table", "agent-1");
    if let AssignmentTrigger::AgentWatch { connector_scope, .. } = &mut assignment.trigger {
        *connector_scope = Some("notion_e2e".to_string());
    }
    persistence.assignments.add(assignment.clone()).await.unwrap();

    fn table_payload(rows: &[(&str, &str, &str)]) -> serde_json::Value {
        let mut html = String::from("<table><tr><th>Name</th><th>Company</th><th>Status</th></tr>");
        for (name, company, status) in rows {
            html.push_str(&format!("<tr><td>{name}</td><td>{company}</td><td>{status}</td></tr>"));
        }
        html.push_str("</table>");
        serde_json::json!({ "metadata": { "source": "notion" }, "text": html })
    }
    fn row_candidate(name: &str, company: &str, status: &str) -> AgentWatchCandidate {
        candidate_with_payload(name, serde_json::json!({ "name": name, "company": company, "status": status }))
    }

    let two_rows = [("Peter", "Pete's Pool Construction", "New"), ("Grace", "Acme Corp", "New")];
    stash_structured_payload("notion_e2e", "notion-query-database", table_payload(&two_rows));

    // No native page id anywhere in this row shape. The FIRST attempt
    // below reproduces the actual reported live failure (finding 4): with
    // no native id to lean on, the model piles every semantic field into
    // `identity.fields`, leaving nothing for `change.material_fields` —
    // genuinely unsatisfiable as authored, since `mode` here is
    // `predicate_transition`, not `new_only`. That rejection
    // (`EmptyMaterialFields`) is now same-tick repairable (fix 4), so
    // the SECOND attempt — moving "status" out of identity and into
    // material_fields — must converge within the same poll's attempt
    // budget instead of needing a whole extra poll.
    let over_wide_identity_proposal = serde_json::json!({
        "source": { "kind": "notion_database", "ref": "clients-db" },
        "identity": {
            "strategy": "composite_native",
            "fields": ["name", "company", "status"],
            "rationale": "no native page id was present in this table; used every field observed"
        },
        "mode": "predicate_transition",
        "change": { "material_fields": [] },
        "predicate": { "natural_language": "status is set", "fields": ["status"], "expr": "not_empty(status)" },
        "tool_used": "notion-query-database"
    });
    let corrected_proposal = serde_json::json!({
        "source": { "kind": "notion_database", "ref": "clients-db" },
        "identity": {
            "strategy": "composite_native",
            "fields": ["name", "company"],
            "rationale": "no native page id was present in this table; name+company together identify a row"
        },
        "mode": "predicate_transition",
        "change": { "material_fields": ["status"] },
        "predicate": { "natural_language": "status is set", "fields": ["status"], "expr": "not_empty(status)" },
        "tool_used": "notion-query-database"
    });

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![
            Ok(AuthoringReply {
                candidates: vec![row_candidate("Peter", "Pete's Pool Construction", "New")],
                proposed_contract: Some(over_wide_identity_proposal),
            }),
            Ok(AuthoringReply {
                candidates: vec![row_candidate("Peter", "Pete's Pool Construction", "New")],
                proposed_contract: Some(corrected_proposal),
            }),
        ],
        vec![Ok(vec![
            row_candidate("Peter", "Pete's Pool Construction", "New"),
            row_candidate("Grace", "Acme Corp", "New"),
        ])],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    // --- Poll 1: authoring off the table payload. ---
    let fired_1 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;
    assert!(!fired_1, "an authoring poll must never fire");

    // (a) authoring succeeded within the attempt budget — via a
    // same-tick repair, not a from-scratch guess: the first attempt's
    // over-wide identity was genuinely unsatisfiable as authored, and
    // the second attempt only converged because fix 4 handed the model
    // a targeted correction within this same poll.
    let repairs = detector.observed_repairs();
    assert_eq!(repairs.len(), 2, "the over-wide-identity rejection must spend its same-tick repair attempt");
    assert!(repairs[0].is_none(), "the very first attempt ever has no repair context to seed");
    assert!(
        matches!(repairs[1], Some(RepairContext::EmptyMaterialFields)),
        "the second attempt must carry the EmptyMaterialFields repair context; got: {:?}",
        repairs[1]
    );

    let contract = stored_contract(&persistence, "watch-e2e-notion-table")
        .await
        .expect("authoring off a Notion-table-shaped response with no native id must succeed");
    assert_eq!(contract.identity.strategy, IdentityStrategy::CompositeNative);

    // (b) `set_assignment_contract` actually froze extraction_tool.
    let (extraction_tool, extraction_args) = stored_extraction(&persistence, "watch-e2e-notion-table").await;
    assert_eq!(extraction_tool.as_deref(), Some("notion-query-database"));
    assert_eq!(extraction_args, None);

    // --- Poll 2: first contract-bound poll — authors the extraction
    // plan from this same stashed payload as a side effect. ---
    let assignment = persistence.assignments.get("watch-e2e-notion-table").await.expect("assignment must exist");
    let fired_2 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;
    assert!(!fired_2, "the first contract-bound poll must re-seed under the new contract, not fire");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched while seeding");

    // (c) the tabular extraction plan was actually authored on the live
    // path — not merely reachable in isolation.
    let scratchpad_after_poll2 =
        persistence.assignment_scratchpads.get("watch-e2e-notion-table").await.unwrap().unwrap();
    let plan = scratchpad_after_poll2
        .extraction_plan
        .as_ref()
        .expect("a Tier 2 tabular extraction plan must have been authored from the live payload sample");
    assert!(
        matches!(plan.selector.kind, extractor_contract::ExtractorKind::Table { .. }),
        "the authored plan must be the Tier 2 table selector, not some other kind; got: {:?}",
        plan.selector.kind
    );
    assert_eq!(
        scratchpad_after_poll2.extraction_plan_fingerprint.as_deref(),
        Some(contract.fingerprint().as_str()),
        "the authored plan must be fingerprinted against the exact contract that will judge future polls"
    );
    assert_eq!(
        scratchpad_after_poll2.model_calls_by_day.values().sum::<u32>(),
        3,
        "poll 1 made two model calls (its same-tick repair) and poll 2 made one"
    );

    // --- Poll 3: a genuinely new row appears. The detector's queues are
    // now fully drained — any further call to either panics the test —
    // so a fire here can only have come from the frozen, deterministic
    // Tier 2 plan. ---
    let three_rows = [
        ("Peter", "Pete's Pool Construction", "New"),
        ("Grace", "Acme Corp", "New"),
        ("Alex", "Beta LLC", "New"),
    ];
    stash_structured_payload("notion_e2e", "notion-query-database", table_payload(&three_rows));

    let fired_3 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;
    assert!(fired_3, "the genuinely new row must fire, off the deterministic plan alone");
    assert!(rx.try_recv().is_ok(), "the fire must have actually dispatched a message");

    // (d) THE HEADLINE RESULT: poll 3 made zero model calls — the
    // counter is unchanged from right after poll 2.
    let scratchpad_after_poll3 =
        persistence.assignment_scratchpads.get("watch-e2e-notion-table").await.unwrap().unwrap();
    assert_eq!(
        scratchpad_after_poll3.model_calls_by_day.values().sum::<u32>(),
        3,
        "poll 3 must resolve entirely off the frozen plan — zero additional model calls since poll 2"
    );
    assert_eq!(
        scratchpad_after_poll3.last_extraction_path,
        ExtractionPath::Probabilistic,
        "a Table selector is always Probabilistic tier (no server-declared schema covers markup shape)"
    );
}

#[tokio::test]
async fn end_to_end_notion_table_new_only_watch_fires_exactly_once_with_deterministic_identity_across_polls() {
    // Regression test for the specific production failure this whole
    // fix exists for: an agent-driven identity re-decided every poll
    // minted a fresh id each time it looked (`row-1..row-4` on the
    // seeding poll, `peter-grace`/`martha-johns` on later polls — 8 ids
    // for 4 rows), so every already-seen row looked "new" again forever.
    // A `WatchContract`'s whole point is that `identity_key` is
    // recomputed the same way, from the same declared fields, on every
    // poll — this drives the REAL entry point (`run_agent_watch_tick`,
    // never an internal helper) through authoring, the first
    // contract-bound poll (which authors a Tier 2 tabular extraction
    // plan from the same Notion-table-shaped payload as a side effect),
    // and a third poll that appends exactly one new row — asserting the
    // fire is exact (one message, naming only the new row) AND that the
    // two already-seen rows' `identity_key` is byte-identical to what
    // the prior poll already recorded, not merely that they didn't
    // happen to fire this time.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let mut assignment = agent_watch_assignment("watch-e2e-table-determinism", "agent-1");
    if let AssignmentTrigger::AgentWatch { connector_scope, .. } = &mut assignment.trigger {
        *connector_scope = Some("notion_det_e2e".to_string());
    }
    persistence.assignments.add(assignment.clone()).await.unwrap();

    fn table_payload(rows: &[(&str, &str, &str)]) -> serde_json::Value {
        let mut html = String::from("<table><tr><th>Name</th><th>Company</th><th>Status</th></tr>");
        for (name, company, status) in rows {
            html.push_str(&format!("<tr><td>{name}</td><td>{company}</td><td>{status}</td></tr>"));
        }
        html.push_str("</table>");
        serde_json::json!({ "metadata": { "source": "notion" }, "text": html })
    }
    fn row_candidate(name: &str, company: &str, status: &str) -> AgentWatchCandidate {
        candidate_with_payload(name, serde_json::json!({ "name": name, "company": company, "status": status }))
    }

    let two_rows = [("Peter", "Pete's Pool Construction", "New"), ("Grace", "Acme Corp", "New")];
    stash_structured_payload("notion_det_e2e", "notion-query-clients", table_payload(&two_rows));

    // No native page id anywhere in this row shape, and this watch's own
    // condition really is "tell me when a new client row appears" — the
    // exact shape `mode: new_only` exists for, so `change.material_fields`
    // is omitted entirely rather than naming a field
    // nothing downstream needs.
    let proposal = serde_json::json!({
        "source": { "kind": "notion_database", "ref": "clients-db" },
        "identity": {
            "strategy": "composite_native",
            "fields": ["name", "company"],
            "rationale": "no native page id was present in this table; name+company together identify a row"
        },
        "mode": "new_only",
        "predicate": { "natural_language": "a new client row appeared", "fields": [], "expr": "not_empty(name)" },
        "tool_used": "notion-query-clients"
    });

    let detector = Arc::new(ScriptedAuthoringDetector::new(
        vec![Ok(AuthoringReply {
            candidates: vec![row_candidate("Peter", "Pete's Pool Construction", "New")],
            proposed_contract: Some(proposal),
        })],
        vec![Ok(vec![
            row_candidate("Peter", "Pete's Pool Construction", "New"),
            row_candidate("Grace", "Acme Corp", "New"),
        ])],
    ));
    let detector_dyn: Arc<dyn AgentWatchDetector> = detector.clone();

    // --- Poll 1: authoring off the table payload. ---
    let fired_1 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;
    assert!(!fired_1, "an authoring poll must never fire");

    let repairs = detector.observed_repairs();
    assert_eq!(repairs.len(), 1, "a new_only proposal with a clean composite identity must author in one attempt");
    assert!(repairs[0].is_none(), "the only attempt has no repair context to seed");

    let contract = stored_contract(&persistence, "watch-e2e-table-determinism")
        .await
        .expect("authoring a new_only proposal off a Notion-table-shaped response must succeed");
    assert_eq!(
        contract.mode,
        WatchMode::NewOnly,
        "assertion 2: the bound contract must actually be able to fire on a new row appearing"
    );
    assert!(contract.change.material_fields.is_empty(), "new_only needs no material_fields to fire");

    // (b) `set_assignment_contract` actually froze extraction_tool.
    let (extraction_tool, _extraction_args) = stored_extraction(&persistence, "watch-e2e-table-determinism").await;
    assert_eq!(
        extraction_tool.as_deref(),
        Some("notion-query-clients"),
        "assertion 3: set_assignment_contract must freeze the self-reported tool"
    );

    // --- Poll 2: first contract-bound poll — seeds under the new
    // contract and authors the Tier 2 tabular extraction plan as a side
    // effect. ---
    let assignment =
        persistence.assignments.get("watch-e2e-table-determinism").await.expect("assignment must exist");
    let fired_2 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;
    assert!(!fired_2, "the first contract-bound poll must re-seed under the new contract, not fire");
    assert!(rx.try_recv().is_err(), "no message should have been dispatched while seeding");

    let scratchpad_after_poll2 =
        persistence.assignment_scratchpads.get("watch-e2e-table-determinism").await.unwrap().unwrap();
    let plan = scratchpad_after_poll2
        .extraction_plan
        .as_ref()
        .expect("assertion 3: a Tier 2 tabular extraction plan must have been authored and bound after the live tick");
    assert!(
        matches!(plan.selector.kind, extractor_contract::ExtractorKind::Table { .. }),
        "the authored plan must be the Tier 2 table selector, not some other kind; got: {:?}",
        plan.selector.kind
    );
    assert_eq!(scratchpad_after_poll2.snapshots.len(), 2, "both existing rows must be seeded");
    let poll2_identity_keys: std::collections::HashSet<String> =
        scratchpad_after_poll2.snapshots.iter().map(|s| s.identity_key.clone()).collect();
    assert_eq!(poll2_identity_keys.len(), 2, "Peter and Grace must key to two DISTINCT identities");

    // --- Poll 3: exactly one new row (Alex) appears alongside the two
    // already-seen rows. The detector's queues are now fully drained —
    // any further call to either panics the test — so this can only
    // resolve off the frozen, deterministic Tier 2 plan. ---
    let three_rows = [
        ("Peter", "Pete's Pool Construction", "New"),
        ("Grace", "Acme Corp", "New"),
        ("Alex", "Beta LLC", "New"),
    ];
    stash_structured_payload("notion_det_e2e", "notion-query-clients", table_payload(&three_rows));

    let fired_3 = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector_dyn,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;
    assert!(fired_3, "assertion 4: a genuinely new row on the next poll must fire, off the deterministic plan alone");

    // Exactly one message, naming exactly the new row — `build_event_context`
    // only ever emits the "N new items" summary framing for more than one
    // fired candidate, so its absence here is itself proof only one candidate
    // fired, not merely that the message happens to mention "Alex".
    let (_agent_id, message) = rx.try_recv().expect("exactly one message must have been dispatched");
    assert!(
        rx.try_recv().is_err(),
        "the new row must fire exactly once — no phantom refires on the already-seen rows"
    );
    assert!(
        message.content.contains("\"name\": \"Alex\""),
        "the one fired item's payload must be the genuinely new row; got: {}",
        message.content
    );
    assert!(
        !message.content.contains("Agent watch found"),
        "assertion 4: this framing only appears for a burst of more than one fired candidate — its presence \
         would mean Peter and/or Grace phantom-refired alongside Alex; got: {}",
        message.content
    );
    assert!(!message.content.contains("Peter"), "Peter must not be reported as new again; got: {}", message.content);
    assert!(!message.content.contains("Grace"), "Grace must not be reported as new again; got: {}", message.content);

    // Direct check on the underlying mechanism: Peter's and Grace's
    // `identity_key` must be byte-identical to what poll 2 already
    // recorded — not merely "a fire didn't happen for them," but the
    // actual keys held steady, which a re-minted-id bug would violate
    // even before it got anywhere near the fire decision.
    let scratchpad_after_poll3 =
        persistence.assignment_scratchpads.get("watch-e2e-table-determinism").await.unwrap().unwrap();
    assert_eq!(scratchpad_after_poll3.snapshots.len(), 3, "Alex's snapshot must be added alongside the two existing ones");
    let poll3_identity_keys: std::collections::HashSet<String> =
        scratchpad_after_poll3.snapshots.iter().map(|s| s.identity_key.clone()).collect();
    assert!(
        poll2_identity_keys.is_subset(&poll3_identity_keys),
        "assertion 4: Peter's and Grace's identity_key must be unchanged across polls — a re-minted key would \
         drop them from this set and silently replace them with new, different-looking ones instead"
    );
    assert_eq!(poll3_identity_keys.len(), 3, "exactly one new identity_key (Alex's) must have been added");

    // Zero model calls on this poll — the steady state is genuinely
    // reachable, not just "it fires," but for free.
    assert_eq!(
        scratchpad_after_poll3.model_calls_by_day.values().sum::<u32>(),
        2,
        "poll 1 (authoring) made one model call and poll 2 made one; poll 3 must add none"
    );
}

// -- structural expectation check (Probabilistic-tier plans only) ------
//
// A text-rescued plan has no server-declared schema behind it, so
// `extractor_contract::resolve` can keep "succeeding" — selector still
// matches, an `Ok(Resolution)` still comes back — even after the
// source quietly renamed or dropped the exact field the plan's
// `identity` reads, silently mis-keying every item as `id: "null"`
// instead of erroring. These tests prove the baseline recorded at
// authoring time (`extraction_plan_expected_item_count`/
// `extraction_plan_expected_fields`) catches that drift where a bare
// `resolve()` call cannot.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn structural_field_set_drift_spends_exactly_one_reauthor_call_and_marks_the_watch_unhealthy() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let mut health_rx = event_bus.subscribe();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let mut assignment = agent_watch_assignment_with_contract("watch-structural-drift", "agent-1", contract);
    if let AssignmentTrigger::AgentWatch { connector_scope, extraction_tool, extraction_output_schema_declared, .. } =
        &mut assignment.trigger
    {
        *connector_scope = Some("struct_drift_srv".to_string());
        *extraction_tool = Some("struct_drift_tool".to_string());
        *extraction_output_schema_declared = true;
    }

    // Poll 1: no plan exists yet, so this poll asks the model — but a
    // plan is authored as a side effect from this same text-only
    // sample, recording {"id"} as the expected field set for the next
    // poll to compare against.
    stash_text_payload("struct_drift_srv", "struct_drift_tool", r#"[{"id":"a"},{"id":"b"}]"#);
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate("a"), candidate("b")]), // poll 1's authoring-mode call
        Ok(vec![candidate("a")]),                 // poll 2's one re-author call
    ]));
    let seeding_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;
    assert!(!seeding_fired, "poll 1 must seed, not fire");

    let seeded = persistence.assignment_scratchpads.get("watch-structural-drift").await.unwrap().unwrap();
    assert!(seeded.extraction_plan.is_some(), "a plan must have been authored from poll 1's stash sample");
    assert_eq!(
        seeded.extraction_plan_expected_fields.as_ref().map(|f| f.iter().cloned().collect::<Vec<_>>()),
        Some(vec!["id".to_string()]),
        "the baseline must be the field set actually observed in poll 1's sample"
    );

    // Poll 2: the source renamed "id" to "identifier" — the selector
    // still matches (it selects the whole array), so `resolve()` would
    // otherwise silently succeed with every item's identity computed as
    // "null". The structural check must catch this instead.
    stash_text_payload(
        "struct_drift_srv",
        "struct_drift_tool",
        r#"[{"identifier":"a"},{"identifier":"b"},{"identifier":"c"}]"#,
    );
    let second_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;
    assert!(!second_fired, "a poll whose structural expectation just broke must never fire");
    assert!(rx.try_recv().is_err(), "nothing should have been dispatched on a degraded poll");

    let scratchpad = persistence.assignment_scratchpads.get("watch-structural-drift").await.unwrap().unwrap();
    assert_eq!(
        scratchpad.model_calls_by_day.values().sum::<u32>(),
        2,
        "poll 1's authoring call plus exactly one re-author call for poll 2's mismatch — never a retry loop"
    );
    assert_eq!(
        scratchpad.last_extraction_path,
        ExtractionPath::Llm,
        "the one re-author call's candidates, not the (mismatched) plan's, must be what this poll recorded"
    );
    assert!(scratchpad.extraction_plan_degraded, "a structural expectation mismatch must mark the watch unhealthy");
    let reason = scratchpad
        .extraction_plan_degraded_reason
        .expect("a degraded plan must carry a non-empty reason naming what changed");
    assert!(!reason.is_empty());
    assert!(
        reason.contains("gained") && reason.contains("\"identifier\"") && reason.contains("lost") && reason.contains("\"id\""),
        "the reason must name exactly which fields were gained/lost, not a generic message; got: {reason}"
    );
    assert!(scratchpad.extraction_plan.is_none(), "the mismatched plan must be invalidated for re-authoring");
    assert!(
        scratchpad.extraction_plan_expected_fields.is_none() && scratchpad.extraction_plan_expected_item_count.is_none(),
        "the stale baseline must be cleared alongside the invalidated plan"
    );

    let health_texts = drain_system_message_texts(&mut health_rx);
    assert!(
        health_texts.iter().any(|t| t.contains(&reason)),
        "the user must see a health event naming the real cause; got: {health_texts:?}"
    );
}

#[tokio::test]
async fn structural_field_set_match_spends_zero_model_calls() {
    // Sibling to the drift test above with the field set held constant
    // across polls (only the row count grows, the normal outcome of a
    // watch finding new items) — proves the baseline comparison itself
    // never fires a false positive on a watch's ordinary job.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let mut assignment = agent_watch_assignment_with_contract("watch-structural-match", "agent-1", contract);
    if let AssignmentTrigger::AgentWatch { connector_scope, extraction_tool, extraction_output_schema_declared, .. } =
        &mut assignment.trigger
    {
        *connector_scope = Some("struct_match_srv".to_string());
        *extraction_tool = Some("struct_match_tool".to_string());
        *extraction_output_schema_declared = true;
    }

    stash_text_payload("struct_match_srv", "struct_match_tool", r#"[{"id":"a"},{"id":"b"}]"#);
    // Never scripted with a second response: any call on poll 2 panics
    // the test — the whole point is that a field-set match must not
    // touch the model at all.
    let detector: Arc<dyn AgentWatchDetector> =
        Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a"), candidate("b")])]));
    let seeding_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;
    assert!(!seeding_fired, "poll 1 must seed, not fire");

    stash_text_payload("struct_match_srv", "struct_match_tool", r#"[{"id":"a"},{"id":"b"},{"id":"c"}]"#);
    let second_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;
    assert!(second_fired, "a genuinely new row with a matching field set must fire normally");
    assert!(rx.try_recv().is_ok(), "the fire must have actually dispatched a message");

    let scratchpad = persistence.assignment_scratchpads.get("watch-structural-match").await.unwrap().unwrap();
    assert_eq!(
        scratchpad.model_calls_by_day.values().sum::<u32>(),
        1,
        "only poll 1's authoring call should have touched the model; a field-set match resolves for free"
    );
    assert!(!scratchpad.extraction_plan_degraded, "a field-set match must never be treated as degraded");
    assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Probabilistic);
}

// -- model-call counter / quiet-watch drift signal ---------------------

#[tokio::test]
async fn model_call_counter_increments_once_per_llm_detector_spawn() {
    // Same fixture as `new_candidate_after_seed_fires_and_persists_scratchpad`
    // (no `ExtractionPlan` bound, so every poll falls back to the model):
    // two polls, two real detector spawns, so the day bucket must sum to 2.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-model-calls", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate("a")]),
        Ok(vec![candidate("a"), candidate("b")]),
    ]));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;

    let scratchpad = persistence.assignment_scratchpads.get("watch-model-calls").await.unwrap().unwrap();
    let total_calls: u32 = scratchpad.model_calls_by_day.values().sum();
    assert_eq!(total_calls, 2, "each poll spawned exactly one LLM detector session");
}

/// Regression test for the counter's real undercount: `ScriptedDetector`
/// (used by every other model-call test above) is a canned single-shot
/// fake with no notion of "turns," so it can never exercise the bug a
/// real session hits. This drives an actual `LiveAgentWatchDetector`
/// session through a `MockProviderClient` scripted to call a tool before
/// its final reply — two real provider turns for one detector spawn —
/// and asserts the day bucket reflects both, not just one.
#[tokio::test]
async fn model_call_counter_counts_every_provider_turn_a_session_actually_spent() {
    use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};

    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_api_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-model-call-turns", "agent-1", contract);

    let script = vec![
        // Turn 1: the model calls a tool before it has anything to report.
        vec![
            CompletionEvent::AssistantText("checking the source".into()),
            CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "mcp__testconnector__lookup".into(),
                input: serde_json::json!({}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 2: final reply, no tool_use → exit.
        vec![
            CompletionEvent::AssistantText("[]".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));
    let live_registry = Arc::new(registry_with_tools(&["mcp__testconnector__lookup"]));
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(LiveAgentWatchDetector::with_provider_resolver(
        Arc::clone(&persistence),
        live_registry,
        scripted_provider_resolver(provider),
        dispatcher_that_must_not_be_used(),
        Arc::clone(&event_bus),
    ));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()),
        &assignment,
        "watch",
        None,
    )
    .await;

    let scratchpad = persistence.assignment_scratchpads.get("watch-model-call-turns").await.unwrap().unwrap();
    let total_calls: u32 = scratchpad.model_calls_by_day.values().sum();
    assert_eq!(
        total_calls, 2,
        "one detector spawn that took 2 real provider turns must count as 2 model calls, not 1"
    );
}

#[tokio::test]
async fn model_call_counter_stays_empty_when_deterministic_extraction_skips_the_model() {
    // Mirrors `deterministic_extraction_full_tick_fires_with_zero_model_calls`
    // (the detector is scripted with zero responses and panics if ever
    // called): a poll whose candidates came from `extractor_contract::resolve`
    // must never touch the model-call counter.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction(
        "watch-det-no-model-calls",
        "agent-1",
        contract,
        "det_nocall_srv",
        "det_nocall_tool",
        items_by_id_extraction_plan(),
        true,
    );
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![]));

    stash_structured_payload(
        "det_nocall_srv",
        "det_nocall_tool",
        serde_json::json!({ "items": [{ "id": "a" }, { "id": "b" }] }),
    );
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None).await;

    stash_structured_payload(
        "det_nocall_srv",
        "det_nocall_tool",
        serde_json::json!({ "items": [{ "id": "a" }, { "id": "b" }, { "id": "c" }] }),
    );
    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None).await;

    let scratchpad = persistence.assignment_scratchpads.get("watch-det-no-model-calls").await.unwrap().unwrap();
    assert!(
        scratchpad.model_calls_by_day.is_empty(),
        "a fully deterministic tick must never increment the model-call counter"
    );
}

#[tokio::test]
async fn resolve_with_plan_produces_candidates_from_a_text_only_json_payload_with_zero_model_calls() {
    // Same shape as `model_call_counter_stays_empty_when_deterministic_extraction_skips_the_model`
    // (a frozen plan, `ScriptedDetector::new(vec![])` so any `observe`
    // call panics), except the stash here holds ONLY a text block — no
    // `structuredContent` — exactly the shape `StashedPayload::json_body`'s
    // text-rescue exists for. Before this feature, a server that never
    // sets `structuredContent` would fail `BindError::NoContentSupplied`
    // on every poll and fall back to a full model session forever.
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction(
        "watch-text-no-model-calls",
        "agent-1",
        contract,
        "text_nocall_srv",
        "text_nocall_tool",
        items_at_root_extraction_plan(),
        true,
    );
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![]));

    stash_text_payload("text_nocall_srv", "text_nocall_tool", r#"[{"id":"a"},{"id":"b"}]"#);
    let seed_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;
    assert!(!seed_fired, "the first poll must seed a baseline, not fire");

    stash_text_payload("text_nocall_srv", "text_nocall_tool", r#"[{"id":"a"},{"id":"b"},{"id":"c"}]"#);
    let second_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "unused", None,
    )
    .await;
    assert!(second_fired, "a genuinely new row extracted from the text-rescued body must fire");
    assert!(rx.try_recv().is_ok(), "the fire must have actually dispatched a message");

    let scratchpad = persistence.assignment_scratchpads.get("watch-text-no-model-calls").await.unwrap().unwrap();
    assert!(
        scratchpad.model_calls_by_day.is_empty(),
        "a text-rescued resolve must never touch the model-call counter (the scripted detector \
         would have panicked had it been called at all)"
    );
    assert_eq!(
        scratchpad.last_extraction_path,
        ExtractionPath::Probabilistic,
        "a text-parsed body must cap at Probabilistic, never claim Deterministic, even with \
         extraction_output_schema_declared: true"
    );
}

#[tokio::test]
async fn consecutive_polls_without_new_items_increments_and_resets_on_a_fire() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_contract("watch-drift", "agent-1", contract);

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![
        Ok(vec![candidate("a")]), // poll 1: seeds baseline, no fire
        Ok(vec![candidate("a")]), // poll 2: nothing new, no fire
        Ok(vec![candidate("a"), candidate("b")]), // poll 3: "b" is new, fires
    ]));

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let after_seed = persistence.assignment_scratchpads.get("watch-drift").await.unwrap().unwrap();
    assert_eq!(after_seed.consecutive_polls_without_new_items, 1, "the seeding poll itself fires nothing");
    assert_eq!(after_seed.last_new_item_at, None);

    run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    let after_quiet = persistence.assignment_scratchpads.get("watch-drift").await.unwrap().unwrap();
    assert_eq!(after_quiet.consecutive_polls_without_new_items, 2, "a second item-less poll must increment the streak");
    assert_eq!(after_quiet.last_new_item_at, None);

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()), &assignment, "watch", None).await;
    assert!(fired, "the third poll's new candidate must fire");
    let after_fire = persistence.assignment_scratchpads.get("watch-drift").await.unwrap().unwrap();
    assert_eq!(after_fire.consecutive_polls_without_new_items, 0, "a fire must reset the streak");
    assert!(after_fire.last_new_item_at.is_some(), "a fire must stamp when it happened");
}

// ---------------------------------------------------------------------------
// Tests — steady-state direct-invoke (`resolve_with_plan`'s frozen
// `extraction_tool`/`extraction_args` branch)
//
// Every test below uses `ScriptedDetector::new(vec![])` unless it
// specifically exercises the LLM fallback — an empty queue panics the
// instant `observe` is called, so a passing assertion already proves
// the model was never touched on the direct-invoke path.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn direct_invoke_calls_the_tool_directly_and_spawns_no_model_session() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction_and_args(
        "watch-direct-invoke-seed",
        "agent-1",
        contract,
        "di_seed_srv",
        "di_seed_tool",
        serde_json::json!({ "query": "status" }),
        items_by_id_extraction_plan(),
        true,
    );

    let tool = Arc::new(FakeConnectorTool::new(
        "di_seed_srv",
        "di_seed_tool",
        vec![FakeConnectorOutcome::Stash(serde_json::json!({ "items": [{ "id": "a" }, { "id": "b" }] }))],
    ));
    let registry = registry_with_tool(tool.clone());
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![]));

    let fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &registry,
        &assignment,
        "unused — the direct-invoke path never reaches the model",
        None,
    )
    .await;

    assert!(!fired, "the first direct-invoke poll must seed a baseline, not fire");
    assert_eq!(tool.call_count(), 1, "the connector must be called exactly once for this poll");

    let scratchpad = persistence.assignment_scratchpads.get("watch-direct-invoke-seed").await.unwrap().unwrap();
    assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Deterministic);
    assert!(scratchpad.model_calls_by_day.is_empty(), "a successful direct-invoke poll must not record a model call");
    assert!(!scratchpad.extraction_plan_degraded);
}

/// The test that distinguishes this feature from a cache: the connector
/// double is scripted with two DIFFERENT responses, and its own call
/// count is asserted after each poll. A regression that read
/// `latest_for` without ever invoking the tool again — replaying poll
/// 1's stash entry forever — would fail both the call-count assertion
/// and the second poll's fire assertion below.
#[tokio::test]
async fn direct_invoke_fetches_fresh_upstream_data_each_poll_not_a_cached_stash_replay() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction_and_args(
        "watch-direct-invoke-fresh",
        "agent-1",
        contract,
        "di_fresh_srv",
        "di_fresh_tool",
        serde_json::json!({ "query": "status" }),
        items_by_id_extraction_plan(),
        true,
    );

    let tool = Arc::new(FakeConnectorTool::new(
        "di_fresh_srv",
        "di_fresh_tool",
        vec![
            FakeConnectorOutcome::Stash(serde_json::json!({ "items": [{ "id": "a" }, { "id": "b" }] })),
            FakeConnectorOutcome::Stash(serde_json::json!({ "items": [{ "id": "a" }, { "id": "b" }, { "id": "c" }] })),
        ],
    ));
    let registry = registry_with_tool(tool.clone());
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![]));

    let seeding_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &registry,
        &assignment,
        "unused — the direct-invoke path never reaches the model",
        None,
    )
    .await;
    assert!(!seeding_fired, "the first poll must seed a baseline, not fire");
    assert_eq!(tool.call_count(), 1, "the connector must have been called once after the first poll");

    let second_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &registry,
        &assignment,
        "unused — the direct-invoke path never reaches the model",
        None,
    )
    .await;
    assert!(second_fired, "a genuinely new row returned by the SECOND connector call must fire");
    assert_eq!(
        tool.call_count(),
        2,
        "the connector must have been called again on the second poll — proof this is a live fetch, not a stash replay"
    );
    assert!(rx.try_recv().is_ok(), "the fire must have actually dispatched a message");

    let scratchpad = persistence.assignment_scratchpads.get("watch-direct-invoke-fresh").await.unwrap().unwrap();
    assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Deterministic);
    assert!(scratchpad.model_calls_by_day.is_empty(), "zero model calls across both direct-invoke polls");
}

#[tokio::test]
async fn direct_invoke_failure_falls_back_to_the_model_and_sets_degraded() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction_and_args(
        "watch-direct-invoke-fail",
        "agent-1",
        contract,
        "di_fail_srv",
        "di_fail_tool",
        serde_json::json!({ "query": "status" }),
        items_by_id_extraction_plan(),
        true,
    );

    let tool = Arc::new(FakeConnectorTool::new(
        "di_fail_srv",
        "di_fail_tool",
        vec![FakeConnectorOutcome::ToolError("simulated auth failure".to_string())],
    ));
    let registry = registry_with_tool(tool.clone());
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a")])]));

    let fired =
        run_agent_watch_tick(&persistence, &dispatcher, &event_bus, &detector, &registry, &assignment, "watch", None)
            .await;
    assert!(!fired, "force_seed_only must suppress firing on the poll that fell back");
    assert_eq!(tool.call_count(), 1);

    let scratchpad = persistence.assignment_scratchpads.get("watch-direct-invoke-fail").await.unwrap().unwrap();
    assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Llm, "the fallback poll's candidates came from the model");
    assert!(scratchpad.extraction_plan_degraded, "a direct-invoke failure must set the degraded flag");
    let reason = scratchpad.extraction_plan_degraded_reason.expect("a specific reason must be recorded");
    assert!(reason.contains("simulated auth failure"), "the reason must name the actual cause, got: {reason}");
    let total_calls: u32 = scratchpad.model_calls_by_day.values().sum();
    assert_eq!(total_calls, 1, "the LLM fallback must record exactly one model call");
}

/// The mandatory cross-contamination guard: `direct_invoke_payload` must
/// read back the connector's own response by its EXACT
/// `(server, tool, args_hash)` key, never `PayloadStash::latest_for`,
/// which ignores args entirely. Seeds a stash entry for the SAME
/// `(server, tool)` under DIFFERENT args — simulating a concurrent
/// assignment or unrelated session that happened to call the same
/// connector tool — then scripts this poll's own connector call to
/// succeed but leave nothing extractable behind. If the readback used
/// `latest_for`, it would silently resolve against the "leaked" entry
/// below instead of falling back to the model.
#[tokio::test]
async fn direct_invoke_stash_readback_ignores_a_payload_recorded_for_different_args() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let correct_args = serde_json::json!({ "query": "correct" });
    let assignment = agent_watch_assignment_with_extraction_and_args(
        "watch-direct-invoke-xcontam",
        "agent-1",
        contract,
        "di_xcontam_srv",
        "di_xcontam_tool",
        correct_args,
        items_by_id_extraction_plan(),
        true,
    );

    let wrong_args = serde_json::json!({ "query": "wrong" });
    payload_stash::global().record(payload_stash::StashedPayload {
        server: "di_xcontam_srv".to_string(),
        tool: "di_xcontam_tool".to_string(),
        args: wrong_args.clone(),
        args_hash: payload_stash::hash_args(&wrong_args),
        captured_at: Utc::now(),
        structured: Some(serde_json::json!({ "items": [{ "id": "leaked-from-another-session" }] })),
        text: None,
    });

    let tool =
        Arc::new(FakeConnectorTool::new("di_xcontam_srv", "di_xcontam_tool", vec![FakeConnectorOutcome::NoStash]));
    let registry = registry_with_tool(tool.clone());
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![])]));

    let fired =
        run_agent_watch_tick(&persistence, &dispatcher, &event_bus, &detector, &registry, &assignment, "watch", None)
            .await;
    assert!(!fired);
    assert_eq!(tool.call_count(), 1);

    let scratchpad = persistence.assignment_scratchpads.get("watch-direct-invoke-xcontam").await.unwrap().unwrap();
    assert_eq!(
        scratchpad.last_extraction_path,
        ExtractionPath::Llm,
        "an exact-key miss must fall back to the model, not silently resolve against the wrong-args entry"
    );
    assert!(scratchpad.extraction_plan_degraded);
    assert!(
        scratchpad.snapshots.is_empty(),
        "the wrong-args payload must never enter this watch's snapshot state: {:?}",
        scratchpad.snapshots
    );
}

#[tokio::test]
async fn direct_invoke_poll_with_no_new_items_still_advances_the_empty_poll_streak() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction_and_args(
        "watch-direct-invoke-empty-streak",
        "agent-1",
        contract,
        "di_empty_srv",
        "di_empty_tool",
        serde_json::json!({ "query": "status" }),
        items_by_id_extraction_plan(),
        true,
    );

    let same_content = serde_json::json!({ "items": [{ "id": "a" }] });
    let tool = Arc::new(FakeConnectorTool::new(
        "di_empty_srv",
        "di_empty_tool",
        vec![FakeConnectorOutcome::Stash(same_content.clone()), FakeConnectorOutcome::Stash(same_content)],
    ));
    let registry = registry_with_tool(tool.clone());
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![]));

    run_agent_watch_tick(&persistence, &dispatcher, &event_bus, &detector, &registry, &assignment, "watch", None)
        .await;
    let after_seed =
        persistence.assignment_scratchpads.get("watch-direct-invoke-empty-streak").await.unwrap().unwrap();
    assert_eq!(after_seed.consecutive_polls_without_new_items, 1, "the seeding poll itself fires nothing");

    run_agent_watch_tick(&persistence, &dispatcher, &event_bus, &detector, &registry, &assignment, "watch", None)
        .await;
    let after_quiet =
        persistence.assignment_scratchpads.get("watch-direct-invoke-empty-streak").await.unwrap().unwrap();
    assert_eq!(
        after_quiet.consecutive_polls_without_new_items, 2,
        "a second direct-invoke poll with unchanged upstream content must still advance the streak"
    );
    assert_eq!(tool.call_count(), 2, "both polls must have called the connector directly");
}

/// A row persisted before `extraction_args` existed (or one whose
/// authoring pass never froze args) must behave exactly as it did
/// before this feature — reading whatever the stash cache already
/// holds via `latest_for`, never attempting a direct invoke. No
/// `FakeConnectorTool`/registry entry is registered at all here, so a
/// regression that tried to call through the registry regardless of
/// `extraction_args` would fail the registry lookup (or silently
/// degrade) instead of reproducing the old cache-read result.
#[tokio::test]
async fn no_frozen_extraction_args_keeps_using_the_stash_cache_read_unchanged() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, mut rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction(
        "watch-legacy-no-args",
        "agent-1",
        contract,
        "legacy_srv",
        "legacy_tool",
        items_by_id_extraction_plan(),
        true,
    );

    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![]));

    stash_structured_payload("legacy_srv", "legacy_tool", serde_json::json!({ "items": [{ "id": "a" }, { "id": "b" }] }));
    let seeding_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()),
        &assignment,
        "unused — the deterministic path never reaches the model",
        None,
    )
    .await;
    assert!(!seeding_fired);

    stash_structured_payload(
        "legacy_srv",
        "legacy_tool",
        serde_json::json!({ "items": [{ "id": "a" }, { "id": "b" }, { "id": "c" }] }),
    );
    let second_fired = run_agent_watch_tick(
        &persistence,
        &dispatcher,
        &event_bus,
        &detector,
        &Arc::new(Registry::new()),
        &assignment,
        "unused — the deterministic path never reaches the model",
        None,
    )
    .await;
    assert!(second_fired, "the stash cache-read path must still fire on a new row exactly like before this feature");
    assert!(rx.try_recv().is_ok());

    let scratchpad = persistence.assignment_scratchpads.get("watch-legacy-no-args").await.unwrap().unwrap();
    assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Deterministic);
    assert!(!scratchpad.extraction_plan_degraded);
}

/// A `Hash`-kind selector always infers `Tier::ChangeDetectionOnly`
/// regardless of what content is fetched (`infer_tier` never even
/// consults `has_structured_content` for that variant) — a poll bound
/// to such a plan must go straight to the model without ever invoking
/// the connector, since any fetched content would just be discarded by
/// the `ChangeDetectionOnly` branch. `FakeConnectorTool::new(.., vec![])`
/// panics the instant `invoke` is called, so a passing assertion here
/// already proves the connector was never touched.
#[tokio::test]
async fn change_detection_only_tier_never_calls_the_connector_even_with_frozen_args() {
    let (_tmp, persistence) = make_persistence().await;
    let event_bus = Arc::new(EventBus::new(64));
    let (dispatcher, _rx) = make_recording_dispatcher();
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let assignment = agent_watch_assignment_with_extraction_and_args(
        "watch-direct-invoke-change-detection-only",
        "agent-1",
        contract,
        "di_cdo_srv",
        "di_cdo_tool",
        serde_json::json!({ "query": "status" }),
        change_detection_only_extraction_plan(),
        true,
    );

    let tool = Arc::new(FakeConnectorTool::new("di_cdo_srv", "di_cdo_tool", vec![]));
    let registry = registry_with_tool(tool.clone());
    let detector: Arc<dyn AgentWatchDetector> = Arc::new(ScriptedDetector::new(vec![Ok(vec![candidate("a")])]));

    let fired =
        run_agent_watch_tick(&persistence, &dispatcher, &event_bus, &detector, &registry, &assignment, "watch", None)
            .await;
    assert!(!fired, "the first poll must seed a baseline, not fire");
    assert_eq!(
        tool.call_count(),
        0,
        "a ChangeDetectionOnly-tier plan must never invoke the connector — its result would be discarded anyway"
    );

    let scratchpad =
        persistence.assignment_scratchpads.get("watch-direct-invoke-change-detection-only").await.unwrap().unwrap();
    assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Llm);
    assert_eq!(scratchpad.last_inferred_tier, Some(Tier::ChangeDetectionOnly));
    let total_calls: u32 = scratchpad.model_calls_by_day.values().sum();
    assert_eq!(total_calls, 1, "the model must still be called exactly once");
}

// -- derive_extraction_health -------------------------------------------

#[test]
fn derive_extraction_health_is_pending_before_any_poll_has_completed() {
    let (health, reason) = derive_extraction_health(None, None, false);
    assert_eq!(health, ExtractionHealth::Pending);
    assert_eq!(reason, None);
}

#[test]
fn derive_extraction_health_is_pending_even_with_a_frozen_tool_if_no_poll_has_completed() {
    // A frozen `extraction_tool` alone (e.g. carried forward from a prior
    // contract) proves nothing about whether this watch has ever polled —
    // `scratchpad` being `None` is the only signal that matters here.
    let (health, _reason) = derive_extraction_health(None, Some("notion-fetch"), false);
    assert_eq!(health, ExtractionHealth::Pending);
}

#[test]
fn derive_extraction_health_is_model_assisted_for_a_frozen_tool_with_no_persisted_plan_after_a_completed_poll() {
    // The exact live-bug shape: a frozen extraction tool, `extraction:
    // None` (no trigger-level override), `scratchpad.extraction_plan:
    // None` (author_extraction_plan never produced one), and at least
    // one completed poll (`scratchpad` is `Some`). This must read as
    // `ModelAssisted`, never as healthy/`Deterministic` — the entire
    // point of this enum.
    let scratchpad = AssignmentScratchpad { extraction_plan: None, ..Default::default() };
    let (health, reason) = derive_extraction_health(Some(&scratchpad), Some("notion-fetch"), false);
    assert_eq!(health, ExtractionHealth::ModelAssisted);
    assert_ne!(health, ExtractionHealth::Deterministic);
    let reason = reason.expect("ModelAssisted must carry a human-readable reason, never a bare state");
    assert!(reason.contains("notion-fetch"), "reason should name the frozen tool: {reason}");
}

#[test]
fn derive_extraction_health_is_model_assisted_with_no_frozen_tool_at_all_after_a_completed_poll() {
    // Still mid-authoring (or an authoring reply that never
    // self-reported a tool): no plan, no tool, but a poll has completed.
    // Every poll still runs the model, so this must not read as
    // `Pending` (that would imply nothing is known yet).
    let scratchpad = AssignmentScratchpad::default();
    let (health, reason) = derive_extraction_health(Some(&scratchpad), None, false);
    assert_eq!(health, ExtractionHealth::ModelAssisted);
    assert!(reason.is_some());
}

#[test]
fn derive_extraction_health_is_deterministic_when_a_plan_is_persisted_on_the_scratchpad() {
    let scratchpad =
        AssignmentScratchpad { extraction_plan: Some(items_by_id_extraction_plan()), ..Default::default() };
    let (health, reason) = derive_extraction_health(Some(&scratchpad), Some("notion-fetch"), false);
    assert_eq!(health, ExtractionHealth::Deterministic);
    assert_eq!(reason, None);
}

#[test]
fn derive_extraction_health_is_deterministic_when_the_trigger_carries_a_manual_extraction_override() {
    // `select_agent_watch_candidates` checks the trigger's own
    // `extraction` override before ever consulting
    // `scratchpad.extraction_plan` — this must read as `Deterministic`
    // too, even with no plan persisted on the scratchpad, or the health
    // badge would call a genuinely deterministic watch model-assisted.
    let scratchpad = AssignmentScratchpad { extraction_plan: None, ..Default::default() };
    let (health, _reason) = derive_extraction_health(Some(&scratchpad), Some("notion-fetch"), true);
    assert_eq!(health, ExtractionHealth::Deterministic);
}

#[test]
fn derive_extraction_health_is_degraded_when_the_plan_degraded_flag_is_set_even_with_a_plan_persisted() {
    // Degraded must win over Deterministic: a plan can be persisted on
    // the scratchpad (not yet invalidated) while the watch is degraded
    // for this poll — `resolve_with_plan`'s structural-failure branch
    // clears the plan, but the direct-invoke-failure branch does not, so
    // both fields can legitimately be set at once. The fail-open
    // fallback must stay visible either way.
    let scratchpad = AssignmentScratchpad {
        extraction_plan: Some(items_by_id_extraction_plan()),
        extraction_plan_degraded: true,
        extraction_plan_degraded_reason: Some("tool \"notion-fetch\" returned an error: rate limited".to_string()),
        ..Default::default()
    };
    let (health, reason) = derive_extraction_health(Some(&scratchpad), Some("notion-fetch"), false);
    assert_eq!(health, ExtractionHealth::Degraded);
    assert_ne!(health, ExtractionHealth::Deterministic);
    assert_eq!(reason.as_deref(), Some("tool \"notion-fetch\" returned an error: rate limited"));
}

#[test]
fn derive_extraction_health_degraded_reason_is_none_when_none_was_recorded() {
    let scratchpad = AssignmentScratchpad { extraction_plan_degraded: true, ..Default::default() };
    let (health, reason) = derive_extraction_health(Some(&scratchpad), None, false);
    assert_eq!(health, ExtractionHealth::Degraded);
    assert_eq!(reason, None);
}

// -- derive_watch_contract_status -----------------------------------------

#[test]
fn derive_watch_contract_status_is_not_yet_attempted_with_no_scratchpad_and_no_contract() {
    assert_eq!(derive_watch_contract_status(None, None), WatchContractStatus::NotYetAttempted);
}

#[test]
fn derive_watch_contract_status_is_not_yet_attempted_right_after_a_contract_invalidating_edit() {
    // `invalidate_watch_contract_state` zeroes `authoring_failure_streak`
    // — the scratchpad row still exists (other telemetry survives), but
    // from this function's perspective nothing has been attempted yet
    // for the watch's current instruction.
    let mut scratchpad = AssignmentScratchpad {
        authoring_failure_streak: 4,
        last_authoring_rejection_reason: Some("proposal failed validation".to_string()),
        ..Default::default()
    };
    scratchpad.invalidate_watch_contract_state();
    assert_eq!(derive_watch_contract_status(None, Some(&scratchpad)), WatchContractStatus::NotYetAttempted);
}

#[test]
fn derive_watch_contract_status_is_authoring_rejected_below_the_ceiling() {
    let scratchpad = AssignmentScratchpad {
        authoring_failure_streak: 2,
        last_authoring_rejection_reason: Some(
            "proposal failed validation: no material fields declared".to_string(),
        ),
        ..Default::default()
    };
    assert_eq!(
        derive_watch_contract_status(None, Some(&scratchpad)),
        WatchContractStatus::AuthoringRejected {
            attempts: 2,
            ceiling_hit: false,
            last_rejection_reason: Some(
                "proposal failed validation: no material fields declared".to_string()
            ),
        }
    );
}

#[test]
fn derive_watch_contract_status_reports_ceiling_hit_once_the_streak_reaches_it() {
    let scratchpad = AssignmentScratchpad {
        authoring_failure_streak: AUTHORING_FAILURE_CEILING,
        last_authoring_rejection_reason: Some("still broken".to_string()),
        ..Default::default()
    };
    assert_eq!(
        derive_watch_contract_status(None, Some(&scratchpad)),
        WatchContractStatus::AuthoringRejected {
            attempts: AUTHORING_FAILURE_CEILING,
            ceiling_hit: true,
            last_rejection_reason: Some("still broken".to_string()),
        }
    );
}

#[test]
fn derive_watch_contract_status_is_bound_once_a_contract_is_present_regardless_of_scratchpad() {
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    assert_eq!(
        derive_watch_contract_status(Some(&contract), None),
        WatchContractStatus::Bound { bound_after_repairs: None },
        "a contract can be bound with no scratchpad yet persisted at all"
    );
}

#[test]
fn derive_watch_contract_status_bound_surfaces_the_repair_count_when_present() {
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let scratchpad = AssignmentScratchpad { contract_bound_after_failed_attempts: Some(3), ..Default::default() };
    assert_eq!(
        derive_watch_contract_status(Some(&contract), Some(&scratchpad)),
        WatchContractStatus::Bound { bound_after_repairs: Some(3) }
    );
}

#[test]
fn derive_watch_contract_status_bound_never_reports_authoring_rejected_even_with_a_leftover_streak() {
    // Defense in depth: even if some future code path left a nonzero
    // `authoring_failure_streak` lying around on a scratchpad alongside a
    // now-bound contract, `contract.is_some()` must always win — the
    // three states are mutually exclusive by construction, not by which
    // field happens to be checked first.
    let contract = dedup_contract(WatchMode::PredicateTransition, "not_empty(id)", vec![]);
    let scratchpad = AssignmentScratchpad { authoring_failure_streak: 5, ..Default::default() };
    assert_eq!(
        derive_watch_contract_status(Some(&contract), Some(&scratchpad)),
        WatchContractStatus::Bound { bound_after_repairs: None }
    );
}

// -- model_calls_today ---------------------------------------------------

#[test]
fn model_calls_today_reflects_the_current_date_bucket() {
    let mut scratchpad = AssignmentScratchpad::default();
    let today = today_utc();
    scratchpad.record_model_call(&today);
    scratchpad.record_model_call(&today);
    // A different (past) day's bucket must never leak into today's count.
    scratchpad.model_calls_by_day.insert("2020-01-01".to_string(), 99);

    assert_eq!(model_calls_today(&scratchpad), 2);
}

#[test]
fn model_calls_today_is_zero_when_todays_bucket_is_absent() {
    let mut scratchpad = AssignmentScratchpad::default();
    scratchpad.model_calls_by_day.insert("2020-01-01".to_string(), 5);
    assert_eq!(model_calls_today(&scratchpad), 0);
}
