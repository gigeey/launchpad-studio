//! Unit tests for the provider seam and the scripted mock client.
//! Declared from `mod.rs` as `#[cfg(test)] mod tests;` so private and
//! `cfg(test)`-gated items remain in scope.

use std::time::Duration;

use ao_engine_tools_core::PermissionMode;
use serde_json::json;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::{
    CompletionEvent, CompletionRequest, MockProviderClient, ProviderClient, ProviderError,
    StopReason, ToolSpec, Usage,
};

fn empty_request() -> CompletionRequest {
    CompletionRequest {
        messages: vec![],
        system_prompt: None,
        tools: vec![],
        mode: PermissionMode::Default,
        ..Default::default()
    }
}

#[tokio::test]
async fn single_turn_yields_scripted_events_in_order() {
    let script = vec![vec![
        CompletionEvent::AssistantText("hello".into()),
        CompletionEvent::ToolUse {
            id: "call_1".into(),
            name: "Read".into(),
            input: json!({"file_path": "/tmp/x"}),
        },
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]];
    let client = MockProviderClient::new(script);

    let mut stream = client
        .complete(empty_request(), CancellationToken::new())
        .await
        .expect("complete");

    let e1 = stream.recv().await.unwrap().unwrap();
    assert_eq!(e1, CompletionEvent::AssistantText("hello".into()));

    let e2 = stream.recv().await.unwrap().unwrap();
    assert_eq!(
        e2,
        CompletionEvent::ToolUse {
            id: "call_1".into(),
            name: "Read".into(),
            input: json!({"file_path": "/tmp/x"}),
        }
    );

    let e3 = stream.recv().await.unwrap().unwrap();
    assert_eq!(e3, CompletionEvent::TurnComplete { stop_reason: StopReason::Natural });

    // Channel closes after the producer's for-loop ends.
    assert!(stream.recv().await.is_none());
    assert_eq!(client.remaining_turns(), 0);
}

#[tokio::test]
async fn two_turn_flow_drains_each_turn_in_order() {
    let script = vec![
        vec![
            CompletionEvent::AssistantText("turn one".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("turn two".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let client = MockProviderClient::new(script);
    assert_eq!(client.remaining_turns(), 2);

    let mut s1 = client
        .complete(empty_request(), CancellationToken::new())
        .await
        .expect("turn 1");
    assert_eq!(
        s1.recv().await.unwrap().unwrap(),
        CompletionEvent::AssistantText("turn one".into())
    );
    assert_eq!(
        s1.recv().await.unwrap().unwrap(),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural }
    );
    assert!(s1.recv().await.is_none());
    assert_eq!(client.remaining_turns(), 1);

    let mut s2 = client
        .complete(empty_request(), CancellationToken::new())
        .await
        .expect("turn 2");
    assert_eq!(
        s2.recv().await.unwrap().unwrap(),
        CompletionEvent::AssistantText("turn two".into())
    );
    assert_eq!(
        s2.recv().await.unwrap().unwrap(),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural }
    );
    assert!(s2.recv().await.is_none());
    assert_eq!(client.remaining_turns(), 0);
}

#[tokio::test]
async fn calling_past_script_returns_script_exhausted() {
    let client = MockProviderClient::new(vec![vec![CompletionEvent::TurnComplete { stop_reason: StopReason::Natural }]]);

    // Drain the one scripted turn.
    let mut s = client
        .complete(empty_request(), CancellationToken::new())
        .await
        .expect("first turn");
    let _ = s.recv().await;
    let _ = s.recv().await;

    // Second call has no turn left to play.
    let err = client
        .complete(empty_request(), CancellationToken::new())
        .await
        .expect_err("expected ScriptExhausted");
    assert!(matches!(err, ProviderError::ScriptExhausted));
}

#[tokio::test]
async fn cancellation_mid_stream_stops_emission_promptly() {
    // Build a long script so the producer must block on backpressure
    // after the channel buffer (8) fills. Cancellation needs to
    // pre-empt that send.
    let mut events = Vec::with_capacity(1024);
    for i in 0..1024 {
        events.push(CompletionEvent::AssistantText(format!("event {i}")));
    }
    events.push(CompletionEvent::TurnComplete { stop_reason: StopReason::Natural });

    let client = MockProviderClient::new(vec![events]);
    let cancel = CancellationToken::new();
    let mut stream = client
        .complete(empty_request(), cancel.clone())
        .await
        .expect("complete");

    // Drain a couple events to confirm the stream is live.
    let _ = stream.recv().await.unwrap().unwrap();
    let _ = stream.recv().await.unwrap().unwrap();

    cancel.cancel();

    // After cancel, the producer drops its sender; whatever is already
    // buffered drains and then we get None. The whole drain must
    // complete well within 100ms — there are at most a buffer's worth
    // of events left in flight.
    let drained = timeout(Duration::from_millis(100), async {
        let mut count = 0usize;
        while stream.recv().await.is_some() {
            count += 1;
        }
        count
    })
    .await
    .expect("stream did not close within 100ms after cancel");

    // Hard upper bound: we expect at most `MOCK_CHANNEL_BUFFER` (8)
    // already-buffered events plus one in-flight send to drain. Allow
    // some headroom (32) without losing the assertion's signal: the
    // scripted turn had >1000 events and we should NOT see anything
    // close to that.
    assert!(
        drained < 32,
        "expected cancellation to halt emission promptly, drained {drained} events"
    );
}

#[test]
fn tool_spec_is_serializable() {
    let spec = ToolSpec {
        name: "Read".into(),
        description: "read a file".into(),
        input_schema: json!({"type": "object"}),
    };
    let s = serde_json::to_string(&spec).unwrap();
    let back: ToolSpec = serde_json::from_str(&s).unwrap();
    assert_eq!(back.name, "Read");
    assert_eq!(back.description, "read a file");
    assert_eq!(back.input_schema, json!({"type": "object"}));
}

#[tokio::test]
async fn usage_event_replays_without_deadlock() {
    let usage = Usage {
        input_tokens: 200,
        output_tokens: 80,
        cache_read: Some(50),
        cache_creation: None,
    };
    let script = vec![vec![
        CompletionEvent::AssistantText("hello".into()),
        CompletionEvent::Usage(usage.clone()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]];
    let client = MockProviderClient::new(script);

    let mut stream = client
        .complete(empty_request(), CancellationToken::new())
        .await
        .expect("complete");

    let e1 = stream.recv().await.unwrap().unwrap();
    assert_eq!(e1, CompletionEvent::AssistantText("hello".into()));

    let e2 = stream.recv().await.unwrap().unwrap();
    assert_eq!(e2, CompletionEvent::Usage(usage));

    let e3 = stream.recv().await.unwrap().unwrap();
    assert_eq!(e3, CompletionEvent::TurnComplete { stop_reason: StopReason::Natural });

    assert!(stream.recv().await.is_none());
}

// ─── resolve_model / resolve_max_output_tokens / resolve_max_context_tokens /
//     resolve_reasoning_effort precedence ────────────────────────────────────

/// A `ProviderClient` stub that only exists to hand the `resolve_*` family a
/// scripted set of provider defaults. `complete`/`message_normalizer` are
/// never exercised by these tests.
#[derive(Default)]
struct StubProvider {
    default_model: Option<String>,
    default_max_output_tokens: Option<u32>,
    default_max_context_tokens: Option<u32>,
    default_reasoning_effort: Option<ao_protocol::agent::ReasoningEffort>,
}

#[async_trait::async_trait]
impl ProviderClient for StubProvider {
    async fn complete(
        &self,
        _request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<super::CompletionStream, ProviderError> {
        unreachable!("resolve_* tests never call complete()")
    }

    fn message_normalizer(&self) -> &dyn crate::message::MessageNormalizer {
        unreachable!("resolve_* tests never call message_normalizer()")
    }

    fn default_model(&self) -> Option<String> {
        self.default_model.clone()
    }

    fn default_max_output_tokens(&self) -> Option<u32> {
        self.default_max_output_tokens
    }

    fn default_max_context_tokens(&self) -> Option<u32> {
        self.default_max_context_tokens
    }

    fn default_reasoning_effort(&self) -> Option<ao_protocol::agent::ReasoningEffort> {
        self.default_reasoning_effort
    }
}

/// Branch 1: an explicit per-agent override wins even when the provider
/// advertises its own default — this is the regression that matters most,
/// since it's what makes `AgentProfile.model` do anything at all.
#[test]
fn resolve_model_prefers_agent_override() {
    let provider = StubProvider { default_model: Some("provider-default".into()), ..Default::default() };
    let resolved = super::resolve_model(Some("agent-override".into()), &provider);
    assert_eq!(resolved.as_deref(), Some("agent-override"));
}

/// Branch 2: no agent override, but the provider has a model to fall back
/// on — simulates `providers.toml` carrying a persisted `model` value, which
/// providers surface through `default_model()`.
#[test]
fn resolve_model_falls_back_to_provider_persisted_default() {
    let provider = StubProvider { default_model: Some("persisted-in-providers-toml".into()), ..Default::default() };
    let resolved = super::resolve_model(None, &provider);
    assert_eq!(resolved.as_deref(), Some("persisted-in-providers-toml"));
}

/// Branch 3: neither an agent override nor a provider default exists —
/// resolution bottoms out at `None` and the caller's own hardcoded fallback
/// (if any) applies. Exercises the same `default_model()` a provider
/// implementation returns when its config was loaded with nothing but the
/// crate's built-in default, i.e. the "hardcoded default" tier collapses
/// into this branch when the provider has no explicit opinion.
#[test]
fn resolve_model_returns_none_when_nothing_is_configured() {
    let provider = StubProvider { default_model: None, ..Default::default() };
    let resolved = super::resolve_model(None, &provider);
    assert_eq!(resolved, None);
}

// ─── resolve_max_output_tokens / resolve_max_context_tokens /
//     resolve_reasoning_effort — same three branches as resolve_model,
//     since all four route through the same generic `resolve` core. ──────────

#[test]
fn resolve_max_output_tokens_prefers_agent_override() {
    let provider = StubProvider { default_max_output_tokens: Some(4096), ..Default::default() };
    assert_eq!(super::resolve_max_output_tokens(Some(16000), &provider), Some(16000));
}

#[test]
fn resolve_max_output_tokens_falls_back_to_provider_persisted_default() {
    let provider = StubProvider { default_max_output_tokens: Some(4096), ..Default::default() };
    assert_eq!(super::resolve_max_output_tokens(None, &provider), Some(4096));
}

#[test]
fn resolve_max_output_tokens_returns_none_when_nothing_is_configured() {
    let provider = StubProvider::default();
    assert_eq!(super::resolve_max_output_tokens(None, &provider), None);
}

#[test]
fn resolve_max_context_tokens_prefers_agent_override() {
    let provider = StubProvider { default_max_context_tokens: Some(50_000), ..Default::default() };
    assert_eq!(super::resolve_max_context_tokens(Some(8_000), &provider), Some(8_000));
}

#[test]
fn resolve_max_context_tokens_falls_back_to_provider_persisted_default() {
    let provider = StubProvider { default_max_context_tokens: Some(50_000), ..Default::default() };
    assert_eq!(super::resolve_max_context_tokens(None, &provider), Some(50_000));
}

#[test]
fn resolve_max_context_tokens_returns_none_when_nothing_is_configured() {
    let provider = StubProvider::default();
    assert_eq!(super::resolve_max_context_tokens(None, &provider), None);
}

#[test]
fn resolve_reasoning_effort_prefers_agent_override() {
    let provider = StubProvider {
        default_reasoning_effort: Some(ao_protocol::agent::ReasoningEffort::Low),
        ..Default::default()
    };
    assert_eq!(
        super::resolve_reasoning_effort(Some(ao_protocol::agent::ReasoningEffort::High), &provider),
        Some(ao_protocol::agent::ReasoningEffort::High)
    );
}

#[test]
fn resolve_reasoning_effort_falls_back_to_provider_persisted_default() {
    let provider = StubProvider {
        default_reasoning_effort: Some(ao_protocol::agent::ReasoningEffort::Medium),
        ..Default::default()
    };
    assert_eq!(
        super::resolve_reasoning_effort(None, &provider),
        Some(ao_protocol::agent::ReasoningEffort::Medium)
    );
}

#[test]
fn resolve_reasoning_effort_returns_none_when_nothing_is_configured() {
    let provider = StubProvider::default();
    assert_eq!(super::resolve_reasoning_effort(None, &provider), None);
}
