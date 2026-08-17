//! Runtime smoke — deferred-tool resolution + invocation across all three providers.
//!
//! Exercises the full ToolSearch → tool-resolution → tool-invocation path end-to-end
//! against a scripted mock provider (no real API calls) and a real echo_mcp_server
//! subprocess. Covers three provider "flavors" (Anthropic, OpenAI, Gemini) in separate
//! test functions; at the runner level all three share the same CompletionRequest
//! mechanics — provider-specific wire-format differences (Anthropic `defer_loading`
//! flag vs OpenAI/Gemini omission) are covered in the provider-crate unit tests.
//!
//! Test scenario per provider:
//!   Turn 1: model calls ToolSearch with `name=mcp__benchmark__weather`
//!           → runner resolves it (inserts into `loaded_deferred_tools`)
//!   Turn 2: model calls `mcp__benchmark__weather` with `{"city":"SF"}`
//!           → runner invokes McpToolAdapter → echo_mcp_server responds
//!   Turn 3: model emits final text → session ends (not cancelled)
//!
//! Runner-level assertions for each flavor:
//!   (a) Turn-1 `CompletionRequest.deferred_tools` contains `mcp__benchmark__weather`
//!       (Anthropic adds `defer_loading: true`; OpenAI/Gemini omit from `tools[]`)
//!   (b) Turn-2 `CompletionRequest.deferred_tools` does NOT contain `mcp__benchmark__weather`
//!       (all providers include it fully after ToolSearch resolved it in turn 1)
//!   (c) `ToolResult` for `t2` (the weather call) exists in the final transcript
//!       and `is_error = false`
//!   (d) Session ends normally: `outcome.cancelled = false`, `turns = 3`
//!
//! These tests run by default (`cargo test`) — no env-var gate, no network calls.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ao_engine_tools_core::{
    policy::LoadPolicy, DenialTracker, NoopDenialTracker, PermissionMode, Registry, RunnerContext, SessionKind,
};
use ao_engine_tools_engine::ToolSearch;
use ao_engine_tools_runner::{
    hooks::config::RunnerSettings,
    mcp::{McpClientHandle, McpToolAdapter},
    message::Message,
    prompt_bridge::StubBridge,
    provider::{
        CompletionEvent, CompletionRequest, CompletionStream, MockProviderClient, ProviderClient,
        ProviderError, StopReason,
    },
    query_loop::{run_session, RunnerConfig},
    MessageNormalizer,
};
use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

// ── Binary locator ────────────────────────────────────────────────────────────

/// Locate the `echo_mcp_server` fixture binary.
///
/// This is an integration test, so cargo sets `CARGO_BIN_EXE_echo_mcp_server`
/// to the absolute path of the fixture `[[bin]]` target and guarantees it is
/// built before this test runs. Using the env var (rather than searching
/// `target/debug/` by hand) is what creates that dependency edge: edit
/// `tests/fixtures/echo_mcp_server.rs` and cargo rebuilds it for us.
///
/// The unit tests under `src/mcp/` cannot do this — cargo only sets
/// `CARGO_BIN_EXE_*` for integration tests — so they fall back to a disk
/// search. See `crate::mcp::test_support` for that workaround and its caveats.
fn echo_server_bin() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_echo_mcp_server").into()
}

// ── Capturing provider ────────────────────────────────────────────────────────

/// Wraps `MockProviderClient` and captures `deferred_tools` from each
/// `complete()` call so assertions can verify how the deferral set changes
/// across turns.
struct CapturingProvider {
    captured_deferred: Arc<Mutex<Vec<HashSet<String>>>>,
    inner: MockProviderClient,
    #[allow(dead_code)]
    first_call_done: AtomicBool,
}

#[async_trait]
impl ProviderClient for CapturingProvider {
    async fn complete(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        self.first_call_done.swap(true, Ordering::SeqCst);
        self.captured_deferred
            .lock()
            .unwrap()
            .push(request.deferred_tools.clone());
        self.inner.complete(request, cancel).await
    }

    fn message_normalizer(&self) -> &dyn MessageNormalizer {
        self.inner.message_normalizer()
    }
}

// ── Test fixtures ─────────────────────────────────────────────────────────────

/// Build a `Registry` containing:
/// - `ToolSearch` (AlwaysLoad engine tool)
/// - `mcp__benchmark__weather` (Deferred IoTool backed by echo_mcp_server)
///
/// Returns the registry and the live `McpClientHandle` (caller must shut it down).
async fn build_registry() -> (Registry, McpClientHandle) {
    let bin = echo_server_bin();
    let handle = McpClientHandle::spawn(
        "benchmark",
        bin.to_str().expect("echo_mcp_server path is valid UTF-8"),
        &[],
        &HashMap::new(),
    )
    .await
    .expect("echo_mcp_server should spawn for runtime smoke test");

    let adapter = McpToolAdapter::new(
        "benchmark",
        "weather",
        "Get current weather for a city",
        json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"]
        }),
        handle.clone(),
        LoadPolicy::Deferred,
        Default::default(),
        None,
    );

    let mut registry = Registry::new();
    registry.register_engine(Arc::new(ToolSearch));
    registry.register_io(Arc::new(adapter));
    registry.build_deferred_index();

    (registry, handle)
}

/// 3-turn scripted provider script:
///   Turn 1 — model calls ToolSearch to resolve the weather tool
///   Turn 2 — model calls the resolved weather tool
///   Turn 3 — model emits a final text response (no tool_use → loop exits)
fn make_script() -> Vec<Vec<CompletionEvent>> {
    vec![
        vec![
            CompletionEvent::ToolUse {
                id: "t1".into(),
                name: "ToolSearch".into(),
                input: json!({ "name": "mcp__benchmark__weather" }),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::ToolUse {
                id: "t2".into(),
                name: "mcp__benchmark__weather".into(),
                input: json!({ "city": "SF" }),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("Weather retrieved successfully.".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ]
}

// ── Shared smoke runner ───────────────────────────────────────────────────────

async fn run_smoke(label: &str) {
    let (registry, mcp_client) = build_registry().await;

    let captured_deferred: Arc<Mutex<Vec<HashSet<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = CapturingProvider {
        captured_deferred: captured_deferred.clone(),
        inner: MockProviderClient::new(make_script()),
        first_call_done: AtomicBool::new(false),
    };

    let runner_ctx = RunnerContext::new("smoke-session", &format!("smoke-{label}"))
        .expect("RunnerContext::new should succeed")
        .with_registry(Arc::new(registry));

    let runner_config = RunnerConfig {
        provider: Arc::new(provider),
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
    };

    let outcome = run_session(Vec::new(), runner_ctx, runner_config)
        .await
        .expect("run_session should complete without a hard provider error");

    // (d) Session ends normally — not cancelled, 3 turns completed.
    assert!(
        !outcome.cancelled,
        "[{label}] session must end normally (not cancelled)"
    );
    assert_eq!(
        outcome.turns, 3,
        "[{label}] session must complete in exactly 3 turns"
    );

    let turns_deferred = captured_deferred.lock().unwrap();

    // (a) Turn 1: weather must be in deferred_tools (not yet resolved by ToolSearch).
    //     Anthropic will add `defer_loading: true`; OpenAI/Gemini will omit the tool.
    assert!(
        turns_deferred[0].contains("mcp__benchmark__weather"),
        "[{label}] turn-1 CompletionRequest.deferred_tools must contain mcp__benchmark__weather"
    );

    // (b) Turn 2: weather must NOT be in deferred_tools after ToolSearch resolved it.
    //     All providers include it fully in their respective wire formats.
    assert!(
        !turns_deferred[1].contains("mcp__benchmark__weather"),
        "[{label}] turn-2 CompletionRequest.deferred_tools must NOT contain mcp__benchmark__weather"
    );

    // (c) The weather tool was actually invoked: a non-error ToolResult for t2 exists.
    let weather_result = outcome.messages.iter().find(|m| {
        matches!(m, Message::ToolResult { tool_use_id, .. } if tool_use_id == "t2")
    });
    assert!(
        weather_result.is_some(),
        "[{label}] ToolResult for t2 (mcp__benchmark__weather) must be present in the transcript"
    );
    if let Some(Message::ToolResult { is_error, .. }) = weather_result {
        assert!(
            !is_error,
            "[{label}] mcp__benchmark__weather invocation must succeed (is_error = false)"
        );
    }

    mcp_client.shutdown().await;
}

// ── Per-provider smoke tests ──────────────────────────────────────────────────

/// Anthropic flavor: deferred tools are advertised with `defer_loading: true`
/// by the Anthropic request builder; after ToolSearch resolves a tool the next
/// request omits the flag.
#[tokio::test]
async fn smoke_anthropic() {
    run_smoke("anthropic").await;
}

/// OpenAI flavor: deferred tools are omitted from `tools[]` entirely; after
/// ToolSearch resolves a tool it appears in the subsequent request.
#[tokio::test]
async fn smoke_openai() {
    run_smoke("openai").await;
}

/// Gemini flavor: deferred tools are omitted from `functionDeclarations`; after
/// ToolSearch resolves a tool its declaration appears in the subsequent request.
#[tokio::test]
async fn smoke_gemini() {
    run_smoke("gemini").await;
}
