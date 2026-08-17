//! Integration tests for Deferred Tools and ToolSearch.
//!
//! Exercises the full two-tier tool loading system end-to-end:
//! ToolSearch keyword search, select: activation, tools-array filtering,
//! telemetry emission, override behavior, rotation, and cross-session
//! isolation.
//!
//! Tests 1, 3, 4: use run_session + CapturingProvider to verify the tools
//!   array sent to the provider at each turn.
//! Tests 2, 5, 6: call ToolSearch::invoke directly against a constructed
//!   RunnerContext (no provider needed).
//! Test 7: exercises JsonlTelemetryWriter rotation directly.
//! Test 8: verifies cross-session isolation of activated_tools.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use ao_engine_tools_core::{
    DenialTracker, EngineTool, EventKind, IoTool, LoadPolicy, LoadPolicyOverride,
    NoopDenialTracker, NoopTelemetryWriter, PermissionMode, Registry, RunnerContext, SessionKind,
    TelemetryWriter, ToolOutput, ToolUsageEvent,
};
use ao_engine_tools_engine::ToolSearch;
use ao_engine_tools_runner::hooks::config::RunnerSettings;
use ao_engine_tools_runner::prompt_bridge::StubBridge;
use ao_engine_tools_runner::provider::{
    CompletionEvent, CompletionRequest, CompletionStream, MockProviderClient, ProviderClient,
    ProviderError, StopReason,
};
use ao_engine_tools_runner::message::{ContentBlock, Message};
use ao_engine_tools_runner::query_loop::{run_session, RunnerConfig};
use ao_engine_tools_runner::tool_usage_log::JsonlTelemetryWriter;
use ao_protocol::error::AoError;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

// ============================================================
//  Mock IO tools
// ============================================================

struct MockIoDeferred {
    name: String,
    desc: String,
}

#[async_trait]
impl IoTool for MockIoDeferred {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.desc
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }
    async fn invoke(&self, _: Value, _: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("deferred-ok"))
    }
}

struct MockIoAlways {
    name: String,
}

#[async_trait]
impl IoTool for MockIoAlways {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "Always-load stub tool"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn invoke(&self, _: Value, _: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("always-ok"))
    }
}

// ============================================================
//  SpyTelemetry — captures emitted events for assertions
// ============================================================

struct SpyTelemetry {
    events: Arc<Mutex<Vec<ToolUsageEvent>>>,
}

impl SpyTelemetry {
    fn new() -> (Arc<Self>, Arc<Mutex<Vec<ToolUsageEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (Arc::new(Self { events: events.clone() }), events)
    }
}

impl TelemetryWriter for SpyTelemetry {
    fn emit(&self, e: ToolUsageEvent) {
        self.events.lock().unwrap().push(e);
    }
}

// ============================================================
//  CapturingProvider — records tool names per provider call
// ============================================================

struct CapturingProvider {
    captured: Arc<Mutex<Vec<Vec<String>>>>,
    captured_deferred: Arc<Mutex<Vec<Vec<String>>>>,
    inner: MockProviderClient,
}

impl CapturingProvider {
    /// Returns `(provider, captured_tools, captured_deferred)`. The second
    /// vector records `req.tools` names per turn; the third records the
    /// `req.deferred_tools` flag set per turn. Under the current deferred-loading
    /// contract, deferred tools are present in `req.tools` and flagged via
    /// `req.deferred_tools` — the per-dialect builder is what omits or
    /// `defer_loading`-tags them downstream.
    fn new(
        script: Vec<Vec<CompletionEvent>>,
    ) -> (
        Arc<Self>,
        Arc<Mutex<Vec<Vec<String>>>>,
        Arc<Mutex<Vec<Vec<String>>>>,
    ) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_deferred = Arc::new(Mutex::new(Vec::new()));
        let p = Arc::new(Self {
            captured: captured.clone(),
            captured_deferred: captured_deferred.clone(),
            inner: MockProviderClient::new(script),
        });
        (p, captured, captured_deferred)
    }
}

#[async_trait]
impl ProviderClient for CapturingProvider {
    async fn complete(
        &self,
        req: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let names: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
        self.captured.lock().unwrap().push(names);
        let deferred: Vec<String> = req.deferred_tools.iter().cloned().collect();
        self.captured_deferred.lock().unwrap().push(deferred);
        self.inner.complete(req, cancel).await
    }

    fn message_normalizer(&self) -> &dyn ao_engine_tools_runner::MessageNormalizer {
        self.inner.message_normalizer()
    }
}

// ============================================================
//  RunnerConfig helpers
// ============================================================

fn make_config(provider: Arc<dyn ProviderClient>) -> RunnerConfig {
    RunnerConfig {
        provider,
        bridge: Arc::new(StubBridge),
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings: RunnerSettings::default(),
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    }
}

fn make_config_with_overrides(
    provider: Arc<dyn ProviderClient>,
    overrides: HashMap<String, LoadPolicyOverride>,
) -> RunnerConfig {
    RunnerConfig {
        provider,
        bridge: Arc::new(StubBridge),
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings: RunnerSettings {
            tool_load_overrides: overrides,
            ..RunnerSettings::default()
        },
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    }
}

// ============================================================
//  Output / message parsing helpers
// ============================================================

fn result_names(out: &ToolOutput) -> Vec<String> {
    match out {
        ToolOutput::Structured(v) => v["results"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|r| r["name"].as_str().unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    }
}

fn activated_names(out: &ToolOutput) -> Vec<String> {
    match out {
        ToolOutput::Structured(v) => v["activated"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|r| r["name"].as_str().unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    }
}

fn unresolved_names(out: &ToolOutput) -> Vec<String> {
    match out {
        ToolOutput::Structured(v) => v["unresolved"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|r| r.as_str().unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    }
}

/// Extract result names from a ToolResult message's content text (serialized JSON).
/// For keyword-search responses, the JSON has {"results": [...]}.
fn msg_result_names(content_str: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(content_str).unwrap_or(Value::Null);
    v["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| r["name"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Extract activated names from a ToolResult message's content text (serialized JSON).
/// For select: responses, the JSON has {"activated": [...], "unresolved": [...]}.
fn msg_activated_names(content_str: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(content_str).unwrap_or(Value::Null);
    v["activated"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| r["name"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn tool_result_content_text(msg: &Message) -> &str {
    if let Message::ToolResult { content, .. } = msg {
        content.iter().find_map(|b| {
            if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None }
        }).unwrap_or("")
    } else {
        ""
    }
}

// ============================================================
//  Test 1 — Full round-trip
// ============================================================

/// Build a minimal registry with Echo (AlwaysLoad), PlanTool (Deferred),
/// and ToolSearch (AlwaysLoad engine tool).  After init_session_context:
///   always_load_tools = {Echo, ToolSearch}
///
/// Script:
///   Turn 1: model calls ToolSearch keyword "plan"  → PlanTool in results
///   Turn 2: model calls ToolSearch select:PlanTool → activates PlanTool
///   Turn 3: model calls ToolSearch keyword "plan"  → PlanTool absent (activated)
///   Turn 4: model returns "done" (no tool use)
///
/// Assertions:
///   - Turn 1 tools array has Echo + ToolSearch + PlanTool, with PlanTool
///     flagged in deferred_tools (present-but-deferred contract)
///   - Turn 3 tools array still has PlanTool, but it is no longer flagged in
///     deferred_tools (activated in turn 2)
///   - Turn 1 search result contains PlanTool
///   - Turn 3 search result does NOT contain PlanTool
#[tokio::test]
async fn test1_full_round_trip() {
    let mut registry = Registry::new();
    registry.register_io(Arc::new(MockIoAlways { name: "Echo".into() }));
    registry.register_io(Arc::new(MockIoDeferred {
        name: "PlanTool".into(),
        desc: "A planning tool for organizing and planning tasks.".into(),
    }));
    registry.register_engine(Arc::new(ToolSearch));
    registry.build_deferred_index();

    let (spy, _) = SpyTelemetry::new();

    let (provider, captured, captured_deferred) = CapturingProvider::new(vec![
        // Turn 1: keyword search
        vec![
            CompletionEvent::ToolUse {
                id: "c1".into(),
                name: "ToolSearch".into(),
                input: json!({"query": "plan", "max_results": 10}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 2: activate PlanTool
        vec![
            CompletionEvent::ToolUse {
                id: "c2".into(),
                name: "ToolSearch".into(),
                input: json!({"query": "select:PlanTool"}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 3: keyword search again — PlanTool now activated
        vec![
            CompletionEvent::ToolUse {
                id: "c3".into(),
                name: "ToolSearch".into(),
                input: json!({"query": "plan", "max_results": 10}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 4: no tool use → session ends
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ]);

    let ctx = RunnerContext::new("sess-1", "agent-1")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_telemetry(spy);

    let outcome = run_session(Vec::new(), ctx, make_config(provider))
        .await
        .expect("session ok");

    let turns = captured.lock().unwrap();
    let deferred = captured_deferred.lock().unwrap();
    assert_eq!(turns.len(), 4);
    assert_eq!(deferred.len(), 4);

    // Turn 1: PlanTool is present in the tools array but flagged deferred.
    // Providers consume the flag to advertise it lazily (Anthropic
    // defer_loading) or omit it (OpenAI/Gemini) — it is never silently dropped
    // upstream of the dialect builder.
    assert!(turns[0].contains(&"Echo".to_string()), "Echo in turn 1");
    assert!(turns[0].contains(&"ToolSearch".to_string()), "ToolSearch in turn 1");
    assert!(
        turns[0].contains(&"PlanTool".to_string()),
        "PlanTool must be present in turn-1 tools array (flagged deferred)"
    );
    assert!(
        deferred[0].contains(&"PlanTool".to_string()),
        "PlanTool must be flagged in deferred_tools on turn 1 (not yet activated)"
    );

    // Turn 3: PlanTool was activated in turn 2 — still present, and no longer
    // flagged deferred.
    assert!(
        turns[2].contains(&"PlanTool".to_string()),
        "PlanTool must appear in turn-3 tools array"
    );
    assert!(
        !deferred[2].contains(&"PlanTool".to_string()),
        "PlanTool must NOT be flagged deferred in turn 3 (activated in turn 2)"
    );

    // messages: [Asst(t1), TR(t1), Asst(t2), TR(t2), Asst(t3), TR(t3), Asst(t4)]
    assert_eq!(outcome.messages.len(), 7);

    // Turn 1 ToolSearch result: PlanTool in results (deferred, unloaded)
    assert!(
        matches!(&outcome.messages[1], Message::ToolResult { .. }),
        "expected ToolResult at messages[1]"
    );
    assert!(
        msg_result_names(tool_result_content_text(&outcome.messages[1]))
            .contains(&"PlanTool".to_string()),
        "turn-1 search must include PlanTool"
    );

    // Turn 3 ToolSearch result: PlanTool absent (now in activated_tools)
    assert!(
        matches!(&outcome.messages[5], Message::ToolResult { .. }),
        "expected ToolResult at messages[5]"
    );
    assert!(
        !msg_result_names(tool_result_content_text(&outcome.messages[5]))
            .contains(&"PlanTool".to_string()),
        "turn-3 search must not return PlanTool (it is activated)"
    );
}

// ============================================================
//  Test 2 — Filter persistence across multiple ToolSearch calls
// ============================================================

/// After activating a deferred tool, all subsequent keyword searches must
/// exclude it — the filter persists for the life of the session.
#[tokio::test]
async fn test2_filter_persistence() {
    let mut registry = Registry::new();
    registry.register_io(Arc::new(MockIoDeferred {
        name: "PlanTool".into(),
        desc: "A planning tool for organizing tasks.".into(),
    }));
    registry.register_engine(Arc::new(ToolSearch));
    registry.build_deferred_index();

    let always_load: Arc<HashSet<String>> =
        Arc::new(["ToolSearch".to_string()].into());
    let ctx = RunnerContext::new("sess-2", "agent-2")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_always_load_tools(always_load)
        .with_activated_tools(Arc::new(Mutex::new(HashSet::new())))
        .with_telemetry(Arc::new(NoopTelemetryWriter));

    let tool = ToolSearch;

    // Before activation: PlanTool appears in keyword search
    let out1 = tool.invoke(json!({"query": "plan", "max_results": 10}), &ctx).await.unwrap();
    assert!(
        result_names(&out1).contains(&"PlanTool".to_string()),
        "PlanTool must appear in search before activation"
    );

    // Activate PlanTool via select:
    let sel = tool.invoke(json!({"query": "select:PlanTool"}), &ctx).await.unwrap();
    assert!(activated_names(&sel).contains(&"PlanTool".to_string()));

    // After activation: PlanTool absent from all subsequent searches
    let out2 = tool.invoke(json!({"query": "plan", "max_results": 10}), &ctx).await.unwrap();
    assert!(
        !result_names(&out2).contains(&"PlanTool".to_string()),
        "PlanTool must not appear after activation (first check)"
    );

    let out3 = tool.invoke(json!({"query": "plan", "max_results": 10}), &ctx).await.unwrap();
    assert!(
        !result_names(&out3).contains(&"PlanTool".to_string()),
        "PlanTool must not appear after activation (second check — persists)"
    );
}

// ============================================================
//  Test 3 — ForceAlwaysLoad override
// ============================================================

/// A tool configured ForceAlwaysLoad is promoted into always_load_tools:
///   - It appears in the turn-1 tools array without any select: call.
///   - ToolSearch keyword search never returns it (it is in loaded_set).
#[tokio::test]
async fn test3_force_always_load_override() {
    let mut registry = Registry::new();
    registry.register_io(Arc::new(MockIoDeferred {
        name: "DeferredStub".into(),
        desc: "A deferred stub tool for override testing.".into(),
    }));
    registry.register_engine(Arc::new(ToolSearch));
    registry.build_deferred_index();

    let mut overrides = HashMap::new();
    overrides.insert("DeferredStub".to_string(), LoadPolicyOverride::ForceAlwaysLoad);

    let (spy, _) = SpyTelemetry::new();

    let (provider, captured, _captured_deferred) = CapturingProvider::new(vec![
        // Turn 1: empty query lists all unloaded deferred tools
        vec![
            CompletionEvent::ToolUse {
                id: "c1".into(),
                name: "ToolSearch".into(),
                input: json!({"query": "", "max_results": 100}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 2: done
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ]);

    let ctx = RunnerContext::new("sess-3", "agent-3")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_telemetry(spy);

    let outcome = run_session(
        Vec::new(),
        ctx,
        make_config_with_overrides(provider, overrides),
    )
    .await
    .expect("session ok");

    let turns = captured.lock().unwrap();
    assert_eq!(turns.len(), 2);

    // DeferredStub is now always-loaded — must appear in turn-1 tools array
    assert!(
        turns[0].contains(&"DeferredStub".to_string()),
        "ForceAlwaysLoad tool must appear in turn-1 tools array"
    );
    assert!(turns[0].contains(&"ToolSearch".to_string()));

    // ToolSearch search result: DeferredStub absent (it is in loaded_set)
    // messages: [Asst(t1), TR(t1), Asst(t2)]
    assert_eq!(outcome.messages.len(), 3);
    assert!(
        matches!(&outcome.messages[1], Message::ToolResult { .. }),
        "expected ToolResult at messages[1]"
    );
    assert!(
        !msg_result_names(tool_result_content_text(&outcome.messages[1]))
            .contains(&"DeferredStub".to_string()),
        "ForceAlwaysLoad tool must not appear in ToolSearch keyword results"
    );
}

// ============================================================
//  Test 4 — ForceDeferred override
// ============================================================

/// A tool configured ForceDeferred is removed from always_load_tools:
///   - On turn 1 it is present in the tools array but flagged in deferred_tools
///     (not in always_load_tools, not yet activated).
///   - select:<name> returns its schema and adds it to activated_tools.
///   - On turn 2 it is still present and no longer flagged in deferred_tools.
#[tokio::test]
async fn test4_force_deferred_override() {
    let mut registry = Registry::new();
    registry.register_io(Arc::new(MockIoAlways { name: "AlwaysStub".into() }));
    registry.register_engine(Arc::new(ToolSearch));
    registry.build_deferred_index();

    let mut overrides = HashMap::new();
    overrides.insert("AlwaysStub".to_string(), LoadPolicyOverride::ForceDeferred);

    let (spy, _) = SpyTelemetry::new();

    let (provider, captured, captured_deferred) = CapturingProvider::new(vec![
        // Turn 1: model tries to activate AlwaysStub via select:
        vec![
            CompletionEvent::ToolUse {
                id: "c1".into(),
                name: "ToolSearch".into(),
                input: json!({"query": "select:AlwaysStub"}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 2: done
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ]);

    let ctx = RunnerContext::new("sess-4", "agent-4")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_telemetry(spy);

    let outcome = run_session(
        Vec::new(),
        ctx,
        make_config_with_overrides(provider, overrides),
    )
    .await
    .expect("session ok");

    let turns = captured.lock().unwrap();
    let deferred = captured_deferred.lock().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(deferred.len(), 2);

    // Turn 1: AlwaysStub is force-deferred — present in the tools array but
    // flagged in deferred_tools (not in always_load_tools, not yet activated).
    assert!(
        turns[0].contains(&"AlwaysStub".to_string()),
        "ForceDeferred tool must be present in turn-1 tools array (flagged deferred)"
    );
    assert!(
        deferred[0].contains(&"AlwaysStub".to_string()),
        "ForceDeferred tool must be flagged in deferred_tools on turn 1"
    );
    assert!(turns[0].contains(&"ToolSearch".to_string()));

    // Turn 2: AlwaysStub was activated via select: in turn 1 — still present,
    // and no longer flagged deferred.
    assert!(
        turns[1].contains(&"AlwaysStub".to_string()),
        "tool must remain in tools array after select: activation"
    );
    assert!(
        !deferred[1].contains(&"AlwaysStub".to_string()),
        "tool must NOT be flagged deferred in turn 2 (activated in turn 1)"
    );

    // Turn 1 ToolSearch result: select: returned AlwaysStub with schema
    // messages: [Asst(t1), TR(t1), Asst(t2)]
    assert_eq!(outcome.messages.len(), 3);
    assert!(
        matches!(&outcome.messages[1], Message::ToolResult { .. }),
        "expected ToolResult at messages[1]"
    );
    {
        let content_str = tool_result_content_text(&outcome.messages[1]);
        let parsed: Value = serde_json::from_str(content_str)
            .expect("ToolSearch result must be valid JSON");
        let names = msg_activated_names(content_str);
        assert!(
            names.contains(&"AlwaysStub".to_string()),
            "select: must return AlwaysStub in activated[] with schema"
        );
        // Schema field must be present
        let activated_arr = parsed["activated"].as_array().unwrap();
        for entry in activated_arr {
            assert!(
                entry.get("schema").is_some(),
                "activated entry must have a schema field"
            );
        }
    }
}

// ============================================================
//  Test 5 — Soft error on unknown name
// ============================================================

/// select: with a mix of a known tool and an unknown name returns Ok:
///   - known tool lands in `activated` with its schema
///   - unknown name lands in `unresolved`
///   - the call itself never returns Err
#[tokio::test]
async fn test5_soft_error_on_unknown_name() {
    let mut registry = Registry::new();
    registry.register_io(Arc::new(MockIoAlways { name: "Read".into() }));
    registry.register_engine(Arc::new(ToolSearch));
    registry.build_deferred_index();

    let always_load: Arc<HashSet<String>> =
        Arc::new(["ToolSearch".to_string(), "Read".to_string()].into());
    let ctx = RunnerContext::new("sess-5", "agent-5")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_always_load_tools(always_load)
        .with_activated_tools(Arc::new(Mutex::new(HashSet::new())))
        .with_telemetry(Arc::new(NoopTelemetryWriter));

    let result = ToolSearch
        .invoke(json!({"query": "select:NoSuchTool,Read"}), &ctx)
        .await;
    assert!(result.is_ok(), "select: with unknown name must return Ok");

    let out = result.unwrap();
    assert!(
        activated_names(&out).contains(&"Read".to_string()),
        "Read (registered, always-loaded) must be in activated"
    );
    assert_eq!(
        unresolved_names(&out),
        vec!["NoSuchTool".to_string()],
        "NoSuchTool must be in unresolved"
    );
}

// ============================================================
//  Test 6 — Idempotent re-select
// ============================================================

/// Calling select:X twice in the same session must:
///   - succeed both times (Ok)
///   - return the schema both times
///   - emit a Selected telemetry event each time (not deduplicated)
#[tokio::test]
async fn test6_idempotent_reselect() {
    let mut registry = Registry::new();
    registry.register_io(Arc::new(MockIoDeferred {
        name: "PlanTool".into(),
        desc: "Planning tool.".into(),
    }));
    registry.register_engine(Arc::new(ToolSearch));
    registry.build_deferred_index();

    let always_load: Arc<HashSet<String>> =
        Arc::new(["ToolSearch".to_string()].into());
    let (spy, events) = SpyTelemetry::new();
    let ctx = RunnerContext::new("sess-6", "agent-6")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_always_load_tools(always_load)
        .with_activated_tools(Arc::new(Mutex::new(HashSet::new())))
        .with_telemetry(spy);

    // First select
    let out1 = ToolSearch
        .invoke(json!({"query": "select:PlanTool"}), &ctx)
        .await
        .unwrap();
    assert_eq!(unresolved_names(&out1).len(), 0);
    assert!(activated_names(&out1).contains(&"PlanTool".to_string()));
    // Schema must be present
    let v1 = match &out1 {
        ToolOutput::Structured(v) => v,
        _ => panic!("expected Structured output"),
    };
    assert!(
        v1["activated"].as_array().unwrap()[0].get("schema").is_some(),
        "schema must be returned on first select"
    );

    // Second select (idempotent)
    let out2 = ToolSearch
        .invoke(json!({"query": "select:PlanTool"}), &ctx)
        .await
        .unwrap();
    assert_eq!(unresolved_names(&out2).len(), 0);
    assert!(activated_names(&out2).contains(&"PlanTool".to_string()));
    let v2 = match &out2 {
        ToolOutput::Structured(v) => v,
        _ => panic!("expected Structured output"),
    };
    assert!(
        v2["activated"].as_array().unwrap()[0].get("schema").is_some(),
        "schema must be returned on second select"
    );

    // Both calls must have emitted a Selected telemetry event
    let ev = events.lock().unwrap();
    let selected: Vec<_> = ev
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Selected))
        .collect();
    assert_eq!(selected.len(), 2, "both select calls must emit Selected telemetry");
    assert!(
        selected.iter().all(|e| e.tool_name == "PlanTool"),
        "both events must name PlanTool"
    );
}

// ============================================================
//  Test 7 — Telemetry rotation
// ============================================================

async fn count_file_lines(path: &std::path::Path) -> usize {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => content.lines().count(),
        Err(_) => 0,
    }
}

fn make_test_event() -> ToolUsageEvent {
    ToolUsageEvent {
        agent_id: "test-agent".to_string(),
        session_id: "test-session".to_string(),
        tool_name: "TestTool".to_string(),
        kind: EventKind::Invoked,
        ts: Utc::now(),
        metadata: Value::Object(Default::default()),
    }
}

/// Emit 10 001 events → rotation: .jsonl.1 = 10 000, .jsonl = 1.
/// Then emit 10 000 more → second rotation overwrites prior .1.
///
/// Uses new_with_capacity with room for all events so none are dropped
/// before flush() drains the channel.
#[tokio::test]
async fn test7_telemetry_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tool_usage.jsonl");
    let rotated = dir.path().join("tool_usage.jsonl.1");

    // --- First rotation: emit 10 001 events ---
    // Capacity large enough that try_send never drops before flush drains.
    let writer1 = JsonlTelemetryWriter::new_with_capacity(path.clone(), 11_000);
    for _ in 0..10_001 {
        writer1.emit(make_test_event());
    }
    writer1.flush().await;

    assert_eq!(
        count_file_lines(&path).await,
        1,
        "main file must have 1 line after first rotation"
    );
    assert_eq!(
        count_file_lines(&rotated).await,
        10_000,
        ".1 must have 10 000 lines after first rotation"
    );

    // --- Second rotation: seed from disk (1 line), emit 10 000 more ---
    // The background task seeds line_count = 1 from disk.
    // Events 0..9998 write, bringing line_count to 10 000.
    // Event 9999 triggers the rotation: .jsonl → .1 (overwriting prior .1),
    // fresh .jsonl created, event 9999 written there (line_count = 1).
    let writer2 = JsonlTelemetryWriter::new_with_capacity(path.clone(), 11_000);
    for _ in 0..10_000 {
        writer2.emit(make_test_event());
    }
    writer2.flush().await;

    assert_eq!(
        count_file_lines(&path).await,
        1,
        "main file must have 1 line after second rotation"
    );
    assert_eq!(
        count_file_lines(&rotated).await,
        10_000,
        ".1 must be overwritten by second rotation (10 000 lines)"
    );
}

// ============================================================
//  Test 8 — Cross-session isolation
// ============================================================

/// Two RunnerContexts share no activated_tools state.
/// Activating a tool in session A must not affect session B.
#[test]
fn test8_cross_session_isolation() {
    let activated_a: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let activated_b: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    let ctx_a = RunnerContext::new("sess-a", "agent-a")
        .unwrap()
        .with_activated_tools(activated_a.clone());
    let ctx_b = RunnerContext::new("sess-b", "agent-b")
        .unwrap()
        .with_activated_tools(activated_b.clone());

    // Activate ToolX in session A
    ctx_a
        .activated_tools
        .lock()
        .unwrap()
        .insert("ToolX".to_string());

    // Session B must not see ToolX
    assert!(
        !ctx_b.activated_tools.lock().unwrap().contains("ToolX"),
        "session B must not share activated_tools with session A"
    );

    // Session A still has ToolX
    assert!(
        ctx_a.activated_tools.lock().unwrap().contains("ToolX"),
        "session A activated_tools must be unaffected"
    );
}
