//! Unit tests for the query loop entry point. Declared from `mod.rs`
//! as `#[cfg(test)] mod tests;` so private items remain in scope.
//!
//! These tests cover loop-exit conditions, error propagation, and the
//! cancellation invariant (one `tool_result` per `tool_use` even when
//! the cancel token fires mid-batch). The full Read → Edit → Bash
//! crate-level integration test lives in `tests/end_to_end.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::{
    DenialTracker, IoTool, NoopDenialTracker, PermissionMode, Registry, RunnerContext, SessionKind,
    ToolOutput,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use ao_engine_tools_core::{NoopEventSink, QuestionBridge, QuestionRequest};

use crate::hooks::config::RunnerSettings;
use crate::message::{ContentBlock, Message};
use crate::prompt_bridge::{AskQuestionError, ChoiceId, LiveBridge, StubBridge, UserPromptBridge};
use crate::provider::{
    CompletionEvent, CompletionRequest, CompletionStream, MockProviderClient, ProviderClient,
    ProviderError, StopReason, Usage,
};
use crate::query_loop::{run_session, RunnerConfig, RunnerError, SessionEvent, SessionEventSink};

// ---------- fixtures ----------

fn ctx() -> RunnerContext {
    RunnerContext::new("session-test", "agent-test").unwrap()
}

fn ctx_with_registry(registry: Arc<Registry>) -> RunnerContext {
    RunnerContext::new("session-test", "agent-test").unwrap().with_registry(registry)
}

fn config(provider: Arc<dyn ProviderClient>) -> RunnerConfig {
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

fn config_with_bridge(
    provider: Arc<dyn ProviderClient>,
    bridge: Arc<dyn UserPromptBridge>,
) -> RunnerConfig {
    let mut c = config(provider);
    c.bridge = bridge;
    c
}

// ---------- stub tools used for loop exercise ----------

/// Echo tool that records inputs it has been invoked with. Concurrency-
/// safe so tests can use it inside a `Concurrent` batch.
struct EchoTool {
    invocations: Arc<AtomicUsize>,
}

#[async_trait]
impl IoTool for EchoTool {
    fn name(&self) -> &str {
        "Echo"
    }
    fn description(&self) -> &str {
        "Echo tool used by query-loop unit tests"
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"msg": {"type": "string"}},
            "required": ["msg"],
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn invoke(&self, input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        let msg = input
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok(ToolOutput::text(format!("echo: {msg}")))
    }
}

/// Tool that fires the supplied cancel token on first invocation, then
/// returns a normal result. Used to exercise the mid-batch cancellation
/// invariant — once the first concurrent slot fires, remaining slots
/// observe the cancelled token and short-circuit to `cancelled`
/// `tool_result`s without running their tool body.
struct CancelOnFirstCallTool {
    fired: Arc<AtomicUsize>,
    cancel: CancellationToken,
}

#[async_trait]
impl IoTool for CancelOnFirstCallTool {
    fn name(&self) -> &str {
        "CancelStub"
    }
    fn description(&self) -> &str {
        "Fires the supplied cancel token on first invocation"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn is_concurrency_safe(&self) -> bool {
        // Concurrency-safe so the partitioner emits a single Concurrent
        // batch big enough that not-yet-started slots can still observe
        // the cancel token after the first slot fires it.
        true
    }
    async fn invoke(&self, _input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let prior = self.fired.fetch_add(1, Ordering::SeqCst);
        if prior == 0 {
            self.cancel.cancel();
            // Sleep briefly so subsequent slots have time to observe
            // the cancelled token before this future resolves.
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(ToolOutput::text("done"))
    }
}

// ---------- provider stubs ----------

/// Provider that always returns a hard transport error. Used to verify
/// error propagation from `complete()`.
struct FailingProvider;

#[async_trait]
impl ProviderClient for FailingProvider {
    async fn complete(
        &self,
        _request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        Err(ProviderError::Transport("simulated network failure".into()))
    }

    fn message_normalizer(&self) -> &dyn crate::message::MessageNormalizer {
        static NORMALIZER: std::sync::OnceLock<crate::message::normalizer::MockNormalizer> =
            std::sync::OnceLock::new();
        NORMALIZER.get_or_init(|| crate::message::normalizer::MockNormalizer)
    }
}

// ---------- tests ----------

#[tokio::test]
async fn returns_outcome_when_first_turn_has_no_tool_use() {
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("hello world".into()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let outcome = run_session(Vec::new(), ctx(), config(provider))
        .await
        .expect("session ok");

    assert_eq!(outcome.turns, 1);
    assert!(!outcome.cancelled);
    assert_eq!(outcome.final_assistant_text, "hello world");
    // One assistant message captured, no tool_result blocks.
    assert_eq!(outcome.messages.len(), 1);
    assert!(matches!(&outcome.messages[0], Message::Assistant { .. }));
}

#[tokio::test]
async fn includes_caller_supplied_initial_messages_in_transcript() {
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("ack".into()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let initial = vec![Message::User {
        content: vec![ContentBlock::Text { text: "hi".into() }],
    }];
    let outcome = run_session(initial, ctx(), config(provider))
        .await
        .expect("session ok");

    assert_eq!(outcome.messages.len(), 2, "user + assistant");
    assert!(matches!(&outcome.messages[0], Message::User { .. }));
    assert!(matches!(&outcome.messages[1], Message::Assistant { .. }));
}

#[tokio::test]
async fn loops_through_multiple_turns_and_appends_tool_results() {
    let mut registry = Registry::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool {
        invocations: invocations.clone(),
    }));
    let runner_ctx = ctx_with_registry(Arc::new(registry));

    let script = vec![
        // Turn 1: tool_use Echo("ping")
        vec![
            CompletionEvent::AssistantText("calling echo".into()),
            CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "Echo".into(),
                input: json!({"msg": "ping"}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 2: final assistant text, no tool_use → exit.
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");

    assert_eq!(outcome.turns, 2);
    assert!(!outcome.cancelled);
    assert_eq!(outcome.final_assistant_text, "done");
    // Tool was invoked exactly once.
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    // Transcript: assistant turn 1 + tool_result + assistant turn 2.
    assert_eq!(outcome.messages.len(), 3);
    assert!(matches!(&outcome.messages[0], Message::Assistant { .. }));
    assert!(matches!(&outcome.messages[1], Message::ToolResult { .. }));
    assert!(matches!(&outcome.messages[2], Message::Assistant { .. }));

    if let Message::ToolResult { tool_use_id, content, is_error } = &outcome.messages[1] {
        assert_eq!(tool_use_id.as_str(), "call_1");
        assert!(!is_error);
        assert!(matches!(&content[0], ContentBlock::Text { text } if text == "echo: ping"));
    } else {
        panic!("expected ToolResult");
    }
}

#[tokio::test]
async fn unknown_tool_yields_error_tool_result_without_aborting() {
    let registry = Registry::new();
    let runner_ctx = ctx_with_registry(Arc::new(registry));

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "DoesNotExist".into(),
                input: json!({}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("recovered".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");

    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.messages.len(), 3);
    if let Message::ToolResult { tool_use_id, content, is_error } = &outcome.messages[1] {
        assert_eq!(tool_use_id.as_str(), "call_1");
        assert!(is_error);
        let text = if let ContentBlock::Text { text } = &content[0] { text.as_str() } else { panic!("expected text block") };
        assert!(text.contains("unknown tool"), "got: {text}");
    } else {
        panic!("expected ToolResult");
    }
}

#[tokio::test]
async fn unknown_tool_near_miss_includes_did_you_mean_suggestion() {
    // Same shape as `unknown_tool_yields_error_tool_result_without_aborting`,
    // but with a real Echo tool registered. A typo'd name ("Ech" — 1 char off
    // "Echo") triggers `Registry::nearest_name`, which surfaces in the API
    // dispatcher's error message as a "Did you mean ...?" hint.
    let mut registry = Registry::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool {
        invocations: invocations.clone(),
    }));
    let runner_ctx = ctx_with_registry(Arc::new(registry));

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "call_typo".into(),
                name: "Ech".into(),
                input: json!({}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("recovered".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");

    // The bad call must not actually invoke EchoTool.
    assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 0);

    if let Message::ToolResult { tool_use_id, content, is_error } = &outcome.messages[1] {
        assert_eq!(tool_use_id.as_str(), "call_typo");
        assert!(is_error);
        let text = if let ContentBlock::Text { text } = &content[0] { text.as_str() } else { panic!("expected text block") };
        assert!(text.contains("unknown tool 'Ech'"), "got: {text}");
        assert!(text.contains("Did you mean 'Echo'?"), "got: {text}");
    } else {
        panic!("expected ToolResult");
    }
}

#[tokio::test]
async fn schema_violation_yields_error_tool_result() {
    let mut registry = Registry::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool {
        invocations: invocations.clone(),
    }));
    let runner_ctx = ctx_with_registry(Arc::new(registry));

    // EchoTool's schema requires `msg`; feed an empty object.
    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "bad".into(),
                name: "Echo".into(),
                input: json!({}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("noted".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");

    // Tool body never ran.
    assert_eq!(invocations.load(Ordering::SeqCst), 0);
    if let Message::ToolResult { content, is_error, .. } = &outcome.messages[1] {
        assert!(is_error);
        let text = if let ContentBlock::Text { text } = &content[0] { text.as_str() } else { panic!("expected text block") };
        assert!(
            text.contains("InputValidationError"),
            "got: {text}"
        );
    } else {
        panic!("expected ToolResult");
    }
}

#[tokio::test]
async fn provider_transport_error_propagates_as_runner_error() {
    let provider = Arc::new(FailingProvider);
    let err = run_session(Vec::new(), ctx(), config(provider))
        .await
        .expect_err("expected provider error");
    match err {
        RunnerError::Provider(msg) => {
            assert!(
                msg.contains("transport") || msg.contains("network failure"),
                "got: {msg}"
            );
        }
    }
}

#[tokio::test]
async fn provider_script_exhaustion_propagates_as_runner_error() {
    // Empty script → first complete() returns ScriptExhausted.
    let provider = Arc::new(MockProviderClient::new(vec![]));
    let err = run_session(Vec::new(), ctx(), config(provider))
        .await
        .expect_err("expected runner error");
    match err {
        RunnerError::Provider(msg) => {
            assert!(msg.contains("exhausted"), "got: {msg}");
        }
    }
}

#[tokio::test]
async fn pre_cancelled_token_returns_cancelled_outcome_immediately() {
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("never reached".into()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let runner_ctx = ctx();
    runner_ctx.cancel.cancel();

    let outcome = timeout(
        Duration::from_millis(100),
        run_session(Vec::new(), runner_ctx, config(provider)),
    )
    .await
    .expect("did not return promptly")
    .expect("session ok");

    assert!(outcome.cancelled);
    assert_eq!(outcome.turns, 0);
    assert!(outcome.messages.is_empty());
}

#[tokio::test]
async fn mid_batch_cancellation_emits_cancelled_results_for_pending_slots() {
    let cancel = CancellationToken::new();
    let mut registry = Registry::new();
    let fired = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(CancelOnFirstCallTool {
        fired: fired.clone(),
        cancel: cancel.clone(),
    }));

    let runner_ctx = RunnerContext::new("session-test", "agent-test")
        .unwrap()
        .with_registry(Arc::new(registry));
    // Wire the runner's cancel token to the same handle the tool will
    // fire — the concurrent batch must observe the cancel mid-flight.
    let runner_ctx = RunnerContext {
        cancel: cancel.clone(),
        ..runner_ctx
    };

    let mut script = Vec::new();
    let mut events = Vec::new();
    for i in 0..6 {
        events.push(CompletionEvent::ToolUse {
            id: format!("call_{i}"),
            name: "CancelStub".into(),
            input: json!({}),
        });
    }
    events.push(CompletionEvent::TurnComplete { stop_reason: StopReason::Natural });
    script.push(events);
    let provider = Arc::new(MockProviderClient::new(script));

    // Cap concurrency to 1 so subsequent slots queue behind the
    // cancel-firing slot — they must observe the cancelled token from
    // the semaphore-acquire branch and short-circuit without ever
    // calling `invoke`. With cap > 1 the not-yet-started slots produce
    // cancelled results too, but cap=1 is the tightest test of the
    // invariant.
    let mut cfg = config(provider);
    cfg.settings.permissions.concurrent_tool_cap = 1;

    let outcome = timeout(
        Duration::from_millis(500),
        run_session(Vec::new(), runner_ctx, cfg),
    )
    .await
    .expect("session did not return promptly")
    .expect("session ok");

    assert!(outcome.cancelled);
    // Six tool_result blocks emitted — one per tool_use, in original
    // order, regardless of which actually executed before cancel fired.
    let results: Vec<(&str, &[ContentBlock], bool)> = outcome
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { tool_use_id, content, is_error } => {
                Some((tool_use_id.as_str(), content.as_slice(), *is_error))
            }
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 6, "one tool_result per tool_use");
    for (i, (id, _, _)) in results.iter().enumerate() {
        assert_eq!(*id, format!("call_{i}"), "ordering");
    }

    // Exactly one `invoke` ran (the slot that fired the cancel) — every
    // other slot short-circuited via the executor's cancel branch.
    assert_eq!(fired.load(Ordering::SeqCst), 1);

    // The first slot's result is `done`; the rest are `cancelled`.
    let (_, content0, _) = results[0];
    assert!(matches!(&content0[0], ContentBlock::Text { text } if text == "done"));
    for (_, content, is_error) in results.iter().skip(1) {
        assert!(is_error);
        assert!(matches!(&content[0], ContentBlock::Text { text } if text == "cancelled"));
    }
}

#[tokio::test]
async fn assistant_turn_with_only_tool_use_is_recorded_in_transcript() {
    let mut registry = Registry::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool {
        invocations: invocations.clone(),
    }));
    let runner_ctx = ctx_with_registry(Arc::new(registry));

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "c1".into(),
                name: "Echo".into(),
                input: json!({"msg": "x"}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("bye".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");

    // Even though the first assistant turn had no text, an Assistant
    // message is still appended so the transcript records the
    // tool_use blocks the model emitted.
    assert!(matches!(&outcome.messages[0], Message::Assistant { .. }));
    if let Message::Assistant { content } = &outcome.messages[0] {
        // No Text block when assistant_text is empty — only the ToolUse block.
        assert_eq!(content.len(), 1);
        assert!(matches!(&content[0], ContentBlock::ToolUse { id, .. } if id == "c1"));
    }
}

#[tokio::test]
async fn reasoning_blocks_are_replayed_on_the_assistant_turn_in_stream_order() {
    // The actual multi-turn-legality fix: when a turn emits reasoning blocks
    // (signed thinking and/or safety-redacted) before a tool_use, the
    // assistant message the runner appends must echo every reasoning block
    // back, in the order they streamed, ahead of the tool_use. Dropping a
    // redacted block — or reordering it relative to a signed block — makes
    // the next request illegal and Anthropic rejects it.
    let mut registry = Registry::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool {
        invocations: invocations.clone(),
    }));
    let runner_ctx = ctx_with_registry(Arc::new(registry));

    let script = vec![
        vec![
            CompletionEvent::ThinkingBlock {
                text: Some("Plan: echo it.".into()),
                signature: Some("sig_qa==".into()),
            },
            CompletionEvent::RedactedThinkingBlock {
                data: "EncryptedBlob==".into(),
            },
            CompletionEvent::ToolUse {
                id: "c1".into(),
                name: "Echo".into(),
                input: json!({"msg": "x"}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("bye".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");

    let Message::Assistant { content } = &outcome.messages[0] else {
        panic!("expected first message to be the assistant turn");
    };
    // Order: signed thinking → redacted → tool_use. No text block (empty).
    assert_eq!(content.len(), 3, "expected three blocks, got {content:?}");
    assert!(
        matches!(
            &content[0],
            ContentBlock::Thinking { signature, .. } if signature.as_deref() == Some("sig_qa==")
        ),
        "block 0 should be the signed thinking block, got {:?}",
        content[0]
    );
    assert!(
        matches!(
            &content[1],
            ContentBlock::RedactedThinking { data } if data == "EncryptedBlob=="
        ),
        "block 1 should be the redacted block carrying its payload, got {:?}",
        content[1]
    );
    assert!(
        matches!(&content[2], ContentBlock::ToolUse { id, .. } if id == "c1"),
        "block 2 should be the tool_use, got {:?}",
        content[2]
    );
}

#[tokio::test]
async fn end_of_stream_without_turn_complete_still_finalizes_turn() {
    // A provider that closes its channel without emitting TurnComplete
    // is treated as the same boundary — the runner finalizes the turn
    // and either loops (if tool_use blocks were collected) or returns.
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("partial".into()),
        // No TurnComplete; the mock's spawned task ends and the
        // channel closes after the last event drains.
    ]]));

    let outcome = run_session(Vec::new(), ctx(), config(provider))
        .await
        .expect("session ok");

    assert_eq!(outcome.turns, 1);
    assert_eq!(outcome.final_assistant_text, "partial");
}

#[tokio::test]
async fn config_with_explicit_bridge_is_threaded_through() {
    // Smoke-test that swapping the bridge produces a different observable
    // outcome — here we route through StubBridge (which always denies).
    // Without permission rules forcing an Ask, the bridge isn't called,
    // so this is just a wiring smoke test.
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("ok".into()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));
    let cfg = config_with_bridge(provider, Arc::new(StubBridge));
    let outcome = run_session(Vec::new(), ctx(), cfg)
        .await
        .expect("session ok");
    assert!(!outcome.cancelled);
    assert_eq!(outcome.final_assistant_text, "ok");
}

#[tokio::test]
async fn cancel_restores_cwd_to_bottom_of_worktree_stack() {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use ao_engine_tools_core::WorktreeEntry;

    // Simulate two EnterWorktree calls: stack[0] = session-start, stack[1] = worktree-a.
    let session_start = PathBuf::from("/session-start");
    let worktree_a = PathBuf::from("/worktree-a");
    let stack = Arc::new(Mutex::new(vec![
        WorktreeEntry {
            restore_cwd: session_start.clone(),
            worktree_path: PathBuf::from("/repo/.launchpad_studio/worktrees/a"),
            branch: "worktree/a".to_string(),
            base_commit: "abc123".to_string(),
        },
        WorktreeEntry {
            restore_cwd: worktree_a,
            worktree_path: PathBuf::from("/repo/.launchpad_studio/worktrees/b"),
            branch: "worktree/b".to_string(),
            base_commit: "def456".to_string(),
        },
    ]));

    // cwd is currently "inside" a second worktree switch.
    let current_cwd = PathBuf::from("/worktree-b");
    let runner_ctx = RunnerContext::new_with_cwd("sess", "agent", current_cwd)
        .with_worktree_stack(stack);

    // Keep an Arc clone so we can inspect cwd after run_session takes ownership.
    let cwd_arc = runner_ctx.cwd.clone();

    // Pre-cancel so the loop exits immediately at the first check.
    runner_ctx.cancel.cancel();

    let provider = Arc::new(MockProviderClient::new(vec![]));
    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");

    assert!(outcome.cancelled);
    // Cwd must be restored to the bottom-of-stack (session-start).
    assert_eq!(*cwd_arc.read().unwrap(), session_start);
}

#[tokio::test]
async fn cancel_with_empty_worktree_stack_is_noop() {
    use std::path::PathBuf;

    let cwd = PathBuf::from("/some-dir");
    let runner_ctx = RunnerContext::new_with_cwd("sess", "agent", cwd.clone());
    let cwd_arc = runner_ctx.cwd.clone();

    runner_ctx.cancel.cancel();

    let provider = Arc::new(MockProviderClient::new(vec![]));
    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");

    assert!(outcome.cancelled);
    // cwd unchanged — stack was empty, so no-op.
    assert_eq!(*cwd_arc.read().unwrap(), cwd);
}

#[tokio::test]
async fn cancel_calls_live_bridge_cancel_pending_resolves_pending_ask_question() {
    use std::path::PathBuf;

    // Build a LiveBridge backed by a noop event sink.
    let sink: Arc<dyn ao_engine_tools_core::EventSink + Send + Sync> = Arc::new(NoopEventSink);
    let bridge = Arc::new(LiveBridge::new(sink));

    // Create a context with the bridge wired in.
    let runner_ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_prompt_bridge(bridge.clone() as Arc<dyn QuestionBridge + Send + Sync>);

    // Spawn a task that suspends inside ask_question.
    let bridge_for_task = bridge.clone();
    let ask_handle: tokio::task::JoinHandle<Result<ChoiceId, AskQuestionError>> =
        tokio::spawn(async move {
            bridge_for_task
                .ask_question(QuestionRequest {
                    question: "Continue?".to_string(),
                    choices: vec![],
                    agent_id: "agent".to_string(),
                    session_id: "sess".to_string(),
                })
                .await
        });

    // Yield so the spawned task registers its oneshot sender.
    tokio::task::yield_now().await;
    assert_eq!(bridge.pending_count(), 1);

    // Pre-cancel the context — on_session_end fires immediately at the
    // loop's first check, calling cancel_pending on the bridge.
    runner_ctx.cancel.cancel();

    let provider = Arc::new(MockProviderClient::new(vec![]));
    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");

    assert!(outcome.cancelled);

    // The pending ask_question must now resolve with Cancelled.
    let result = timeout(Duration::from_millis(200), ask_handle)
        .await
        .expect("task did not hang")
        .expect("task joined");
    assert!(
        matches!(result, Err(AskQuestionError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert_eq!(bridge.pending_count(), 0, "channel map must be empty after cancel_pending");
}

// ---------- skill tool filter enforcement tests ----------

/// IoTool that always succeeds — used to test filter allow/deny paths.
struct AllowedTool;

#[async_trait]
impl IoTool for AllowedTool {
    fn name(&self) -> &str { "Read" }
    fn description(&self) -> &str { "Stub Read tool for filter tests" }
    fn input_schema(&self) -> Value { json!({"type": "object"}) }
    fn is_concurrency_safe(&self) -> bool { true }
    async fn invoke(&self, _input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("read-ok"))
    }
}

struct DeniedTool;

#[async_trait]
impl IoTool for DeniedTool {
    fn name(&self) -> &str { "Bash" }
    fn description(&self) -> &str { "Stub Bash tool for filter tests" }
    fn input_schema(&self) -> Value { json!({"type": "object"}) }
    fn is_concurrency_safe(&self) -> bool { true }
    async fn invoke(&self, _input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("bash-ok"))
    }
}

#[tokio::test]
async fn skill_tool_filter_denies_unlisted_tool() {
    let mut registry = Registry::new();
    registry.register_io(Arc::new(DeniedTool));

    let runner_ctx = ctx_with_registry(Arc::new(registry));
    // Manually set the filter to only allow "Read" — simulates what SkillTool does.
    runner_ctx.set_skill_tool_filter(["Read".to_string()].into());

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "Bash".into(),
                input: json!({}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("noted".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");

    if let Message::ToolResult { content, is_error, .. } = &outcome.messages[1] {
        assert!(is_error);
        let text = if let ContentBlock::Text { text } = &content[0] { text.as_str() } else { panic!("expected text block") };
        assert!(
            text.contains("not permitted") && text.contains("Bash"),
            "got: {text}"
        );
    } else {
        panic!("expected ToolResult at index 1");
    }
}

#[tokio::test]
async fn skill_tool_filter_allows_listed_tool() {
    let mut registry = Registry::new();
    registry.register_io(Arc::new(AllowedTool));

    let runner_ctx = ctx_with_registry(Arc::new(registry));
    // Set filter: only "Read" is allowed.
    runner_ctx.set_skill_tool_filter(["Read".to_string()].into());

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "call_r".into(),
                name: "Read".into(),
                input: json!({}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");

    if let Message::ToolResult { content, is_error, .. } = &outcome.messages[1] {
        assert!(!is_error, "Read should be allowed through filter");
        assert!(matches!(&content[0], ContentBlock::Text { text } if text == "read-ok"));
    } else {
        panic!("expected ToolResult at index 1");
    }
}

#[tokio::test]
async fn skill_tool_filter_clears_at_turn_boundary() {
    let mut registry = Registry::new();
    registry.register_io(Arc::new(AllowedTool));

    let runner_ctx = ctx_with_registry(Arc::new(registry));
    // Set the filter active at the start.
    runner_ctx.set_skill_tool_filter(["Read".to_string()].into());

    // Keep an Arc reference to check the filter after the turn boundary.
    let filter_arc = runner_ctx.skill_tool_filter.clone();

    let script = vec![
        // Turn 1: Read allowed (filter active)
        vec![
            CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "Read".into(),
                input: json!({}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 2: no tool use → exit. By this point filter should be cleared.
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");

    assert_eq!(outcome.turns, 2);
    // After the first turn's tool results are appended, the filter is cleared.
    assert!(
        filter_arc.read().unwrap().is_none(),
        "filter must be cleared after turn boundary"
    );
}

// ---------- invoked telemetry hook ----------

use ao_engine_tools_core::{EventKind, TelemetryWriter, ToolUsageEvent};
use std::sync::Mutex;

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
    fn emit(&self, event: ToolUsageEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// Tool that always returns ToolOutput::Error.
struct ErrorTool;

#[async_trait]
impl IoTool for ErrorTool {
    fn name(&self) -> &str { "ErrorTool" }
    fn description(&self) -> &str { "Always returns an error" }
    fn input_schema(&self) -> Value { json!({"type": "object"}) }
    fn is_concurrency_safe(&self) -> bool { true }
    async fn invoke(&self, _input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::error("tool said no", false))
    }
}

#[tokio::test]
async fn invoked_event_emitted_after_successful_deferred_tool_execution() {
    let mut registry = Registry::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool { invocations: invocations.clone() }));
    let registry = Arc::new(registry);

    let (spy, events) = SpyTelemetry::new();
    let activated: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(["Echo".to_string()].into()));

    let runner_ctx = RunnerContext::new("session-test", "agent-test")
        .unwrap()
        .with_registry(registry)
        .with_activated_tools(activated)
        .with_telemetry(spy);

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "Echo".into(),
                input: json!({"msg": "hello"}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));
    run_session(Vec::new(), runner_ctx, config(provider)).await.unwrap();

    let captured = events.lock().unwrap();
    let invoked: Vec<_> = captured.iter().filter(|e| matches!(e.kind, EventKind::Invoked)).collect();
    assert_eq!(invoked.len(), 1, "exactly one Invoked event");
    assert_eq!(invoked[0].tool_name, "Echo");
    assert_eq!(invoked[0].agent_id, "agent-test");
    assert_eq!(invoked[0].session_id, "session-test");
}

#[tokio::test]
async fn no_invoked_event_for_always_loaded_tool() {
    let mut registry = Registry::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool { invocations: invocations.clone() }));

    let (spy, events) = SpyTelemetry::new();
    // activated_tools is empty — Echo is treated as always-loaded
    let activated: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));

    let runner_ctx = RunnerContext::new("session-test", "agent-test")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_activated_tools(activated)
        .with_telemetry(spy);

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "Echo".into(),
                input: json!({"msg": "hi"}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));
    run_session(Vec::new(), runner_ctx, config(provider)).await.unwrap();

    let captured = events.lock().unwrap();
    let invoked: Vec<_> = captured.iter().filter(|e| matches!(e.kind, EventKind::Invoked)).collect();
    assert_eq!(invoked.len(), 0, "no Invoked event for always-loaded tool");
}

#[tokio::test]
async fn no_invoked_event_when_tool_returns_error_variant() {
    let mut registry = Registry::new();
    registry.register_io(Arc::new(ErrorTool));

    let (spy, events) = SpyTelemetry::new();
    let activated: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(["ErrorTool".to_string()].into()));

    let runner_ctx = RunnerContext::new("session-test", "agent-test")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_activated_tools(activated)
        .with_telemetry(spy);

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "ErrorTool".into(),
                input: json!({}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));
    run_session(Vec::new(), runner_ctx, config(provider)).await.unwrap();

    let captured = events.lock().unwrap();
    let invoked: Vec<_> = captured.iter().filter(|e| matches!(e.kind, EventKind::Invoked)).collect();
    assert_eq!(invoked.len(), 0, "no Invoked event when tool returns Error variant");
}

// ---------- session initialization ----------

use ao_engine_tools_core::{LoadPolicy, LoadPolicyOverride};
use super::init_session_context;

/// A deferred IoTool stub used only by session-init tests.
struct DeferredStubTool;

#[async_trait]
impl IoTool for DeferredStubTool {
    fn name(&self) -> &str {
        "DeferredStub"
    }
    fn description(&self) -> &str {
        "Deferred stub tool for session-init tests"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }
    async fn invoke(&self, _input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("deferred"))
    }
}

#[test]
fn session_init_activated_tools_is_empty() {
    let (spy, _) = SpyTelemetry::new();
    let ctx = RunnerContext::new("sess", "agent-init").unwrap().with_telemetry(spy);
    let cfg = config(Arc::new(MockProviderClient::new(vec![])));
    let ctx = init_session_context(ctx, &cfg);
    assert!(ctx.activated_tools.lock().unwrap().is_empty());
}

#[test]
fn session_init_always_load_tools_matches_default_policies() {
    let mut registry = Registry::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool { invocations: invocations.clone() }));
    registry.register_io(Arc::new(DeferredStubTool));

    let (spy, _) = SpyTelemetry::new();
    let ctx = RunnerContext::new("sess", "agent-init").unwrap()
        .with_registry(Arc::new(registry))
        .with_telemetry(spy);
    let cfg = config(Arc::new(MockProviderClient::new(vec![])));
    let ctx = init_session_context(ctx, &cfg);

    assert!(ctx.always_load_tools.contains("Echo"), "Echo should be always-loaded by default");
    assert!(!ctx.always_load_tools.contains("DeferredStub"), "DeferredStub should not be always-loaded");
}

#[test]
fn session_init_force_always_load_adds_deferred_tool() {
    let mut registry = Registry::new();
    registry.register_io(Arc::new(DeferredStubTool));

    let mut overrides = std::collections::HashMap::new();
    overrides.insert("DeferredStub".to_string(), LoadPolicyOverride::ForceAlwaysLoad);
    let settings = RunnerSettings { tool_load_overrides: overrides, ..RunnerSettings::default() };

    let (spy, _) = SpyTelemetry::new();
    let ctx = RunnerContext::new("sess", "agent-init").unwrap()
        .with_registry(Arc::new(registry))
        .with_telemetry(spy);
    let cfg = RunnerConfig {
        provider: Arc::new(MockProviderClient::new(vec![])),
        bridge: Arc::new(StubBridge),
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings,
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    };
    let ctx = init_session_context(ctx, &cfg);
    assert!(ctx.always_load_tools.contains("DeferredStub"), "ForceAlwaysLoad should promote a deferred tool");
}

#[test]
fn session_init_force_deferred_removes_always_load_tool() {
    let mut registry = Registry::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool { invocations: invocations.clone() }));

    let mut overrides = std::collections::HashMap::new();
    overrides.insert("Echo".to_string(), LoadPolicyOverride::ForceDeferred);
    let settings = RunnerSettings { tool_load_overrides: overrides, ..RunnerSettings::default() };

    let (spy, _) = SpyTelemetry::new();
    let ctx = RunnerContext::new("sess", "agent-init").unwrap()
        .with_registry(Arc::new(registry))
        .with_telemetry(spy);
    let cfg = RunnerConfig {
        provider: Arc::new(MockProviderClient::new(vec![])),
        bridge: Arc::new(StubBridge),
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings,
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    };
    let ctx = init_session_context(ctx, &cfg);
    assert!(!ctx.always_load_tools.contains("Echo"), "ForceDeferred should demote an always-load tool");
}

#[test]
fn session_init_two_sessions_have_separate_activated_tools() {
    let (spy1, _) = SpyTelemetry::new();
    let (spy2, _) = SpyTelemetry::new();
    let cfg = config(Arc::new(MockProviderClient::new(vec![])));

    let ctx1 = init_session_context(
        RunnerContext::new("sess-1", "agent-1").unwrap().with_telemetry(spy1),
        &cfg,
    );
    let ctx2 = init_session_context(
        RunnerContext::new("sess-2", "agent-2").unwrap().with_telemetry(spy2),
        &cfg,
    );

    ctx1.activated_tools.lock().unwrap().insert("SomeTool".to_string());

    assert!(
        !ctx2.activated_tools.lock().unwrap().contains("SomeTool"),
        "session 2 must not share activated_tools with session 1"
    );
}

// ---------- dialect adapter — filter tools array ----------

use std::sync::atomic::AtomicBool;

/// Provider that records the tool names passed in each `complete()` call
/// and optionally inserts a tool name into `activated_tools` after the
/// first call (simulating a ToolSearch select: activation mid-session).
/// Delegates stream construction to an inner MockProviderClient.
struct CapturingProvider {
    captured: Arc<Mutex<Vec<Vec<String>>>>,
    captured_deferred: Arc<Mutex<Vec<Vec<String>>>>,
    inner: MockProviderClient,
    /// After the first complete() call, insert this name into activated_tools.
    activate_after_turn1: Option<(Arc<Mutex<std::collections::HashSet<String>>>, String)>,
    first_call_done: AtomicBool,
}

impl CapturingProvider {
    fn new(
        turns: Vec<Vec<CompletionEvent>>,
        activate_after: Option<(Arc<Mutex<std::collections::HashSet<String>>>, String)>,
    ) -> (Arc<Self>, Arc<Mutex<Vec<Vec<String>>>>, Arc<Mutex<Vec<Vec<String>>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_deferred = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Self {
            captured: captured.clone(),
            captured_deferred: captured_deferred.clone(),
            inner: MockProviderClient::new(turns),
            activate_after_turn1: activate_after,
            first_call_done: AtomicBool::new(false),
        });
        (provider, captured, captured_deferred)
    }
}

#[async_trait]
impl ProviderClient for CapturingProvider {
    async fn complete(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let mut names: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();
        names.sort();
        let mut deferred: Vec<String> = request.deferred_tools.iter().cloned().collect();
        deferred.sort();
        let is_first = !self.first_call_done.swap(true, Ordering::SeqCst);
        self.captured.lock().unwrap().push(names);
        self.captured_deferred.lock().unwrap().push(deferred);

        // After capturing turn-1 tools, activate a deferred tool so turn-2 sees it.
        if is_first {
            if let Some((activated, name)) = &self.activate_after_turn1 {
                activated.lock().unwrap().insert(name.clone());
            }
        }

        self.inner.complete(request, cancel).await
    }

    fn message_normalizer(&self) -> &dyn crate::message::MessageNormalizer {
        self.inner.message_normalizer()
    }
}

/// A deferred IO tool stub (returns LoadPolicy::Deferred) for filter tests.
struct DeferredIoTool {
    tool_name: &'static str,
}

#[async_trait]
impl IoTool for DeferredIoTool {
    fn name(&self) -> &str { self.tool_name }
    fn description(&self) -> &str { "Deferred IO stub for filter tests" }
    fn input_schema(&self) -> Value { json!({"type": "object"}) }
    fn load_policy(&self) -> ao_engine_tools_core::LoadPolicy { ao_engine_tools_core::LoadPolicy::Deferred }
    async fn invoke(&self, _: Value, _: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("deferred-ok"))
    }
}

/// Verify that a deferred tool appears in `request.tools` but is flagged in
/// `request.deferred_tools` on turn 1 (before ToolSearch resolves it).
/// With the new deferral semantics ALL tools appear in request.tools; the
/// deferred_tools set is what Anthropic uses to add defer_loading: true.
#[tokio::test]
async fn dialect_filter_turn1_contains_only_always_load_tools() {
    let mut registry = Registry::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool { invocations: invocations.clone() }));
    registry.register_io(Arc::new(DeferredIoTool { tool_name: "DeferredStub" }));

    let always_load: std::collections::HashSet<String> = ["Echo".to_string()].into();
    let activated: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));

    let (provider, captured, captured_deferred) = CapturingProvider::new(
        vec![vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ]],
        None,
    );

    let runner_ctx = RunnerContext::new("sess", "agent").unwrap()
        .with_registry(Arc::new(registry))
        .with_always_load_tools(Arc::new(always_load))
        .with_activated_tools(activated);

    run_session(Vec::new(), runner_ctx, config(provider)).await.unwrap();

    let turns = captured.lock().unwrap();
    let deferred_turns = captured_deferred.lock().unwrap();
    assert_eq!(turns.len(), 1);
    // All tools appear in request.tools (sorted by CapturingProvider)
    assert_eq!(
        turns[0],
        vec!["DeferredStub".to_string(), "Echo".to_string()],
        "turn 1 must contain all tools (always-load + deferred)"
    );
    // DeferredStub is in deferred_tools set — the provider sees it as defer_loading
    assert_eq!(
        deferred_turns[0],
        vec!["DeferredStub".to_string()],
        "DeferredStub must be in deferred_tools before activation"
    );
}

/// Verify that before activation a deferred tool is flagged in `deferred_tools`
/// (not absent from `request.tools` — it's present so Anthropic can advertise it).
#[tokio::test]
async fn dialect_filter_deferred_tool_absent_before_activation() {
    let mut registry = Registry::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool { invocations: invocations.clone() }));
    registry.register_io(Arc::new(DeferredIoTool { tool_name: "DeferredStub" }));

    let always_load: std::collections::HashSet<String> = ["Echo".to_string()].into();
    let activated: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));

    let (provider, captured, captured_deferred) = CapturingProvider::new(
        vec![vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ]],
        None,
    );

    let runner_ctx = RunnerContext::new("sess", "agent").unwrap()
        .with_registry(Arc::new(registry))
        .with_always_load_tools(Arc::new(always_load))
        .with_activated_tools(activated);

    run_session(Vec::new(), runner_ctx, config(provider)).await.unwrap();

    let turns = captured.lock().unwrap();
    let deferred_turns = captured_deferred.lock().unwrap();
    // DeferredStub IS present in request.tools (for Anthropic defer_loading advertise)
    assert!(
        turns[0].contains(&"DeferredStub".to_string()),
        "deferred tool is present in request.tools so providers can advertise it"
    );
    // But it IS in deferred_tools (Anthropic adds defer_loading: true; OpenAI/Gemini omit it)
    assert!(
        deferred_turns[0].contains(&"DeferredStub".to_string()),
        "deferred tool must be in deferred_tools before activation"
    );
}

#[tokio::test]
async fn dialect_filter_activated_tool_appears_in_next_turn() {
    let mut registry = Registry::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool { invocations: invocations.clone() }));
    registry.register_io(Arc::new(DeferredIoTool { tool_name: "DeferredStub" }));

    let always_load: std::collections::HashSet<String> = ["Echo".to_string()].into();
    let activated: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));

    // After turn 1, activate DeferredStub so turn 2 sees it NOT in deferred_tools.
    let (provider, captured, captured_deferred) = CapturingProvider::new(
        vec![
            // Turn 1: Echo invocation
            vec![
                CompletionEvent::AssistantText("turn1".into()),
                CompletionEvent::ToolUse {
                    id: "c1".into(),
                    name: "Echo".into(),
                    input: json!({"msg": "hi"}),
                },
                CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
            ],
            // Turn 2: exit
            vec![
                CompletionEvent::AssistantText("done".into()),
                CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
            ],
        ],
        Some((activated.clone(), "DeferredStub".to_string())),
    );

    let runner_ctx = RunnerContext::new("sess", "agent").unwrap()
        .with_registry(Arc::new(registry))
        .with_always_load_tools(Arc::new(always_load))
        .with_activated_tools(activated);

    run_session(Vec::new(), runner_ctx, config(provider)).await.unwrap();

    let turns = captured.lock().unwrap();
    let deferred_turns = captured_deferred.lock().unwrap();
    assert_eq!(turns.len(), 2, "expected 2 turns");

    // Turn 1: DeferredStub in tools, but also in deferred_tools (not yet activated)
    assert_eq!(
        turns[0],
        vec!["DeferredStub".to_string(), "Echo".to_string()],
        "turn 1: all tools present"
    );
    assert_eq!(
        deferred_turns[0],
        vec!["DeferredStub".to_string()],
        "turn 1: DeferredStub is in deferred_tools before activation"
    );

    // Turn 2: DeferredStub still in tools, but NOT in deferred_tools (activated after turn 1)
    assert_eq!(
        turns[1],
        vec!["DeferredStub".to_string(), "Echo".to_string()],
        "turn 2: all tools still present"
    );
    assert!(
        deferred_turns[1].is_empty(),
        "turn 2: DeferredStub must not be in deferred_tools after activation"
    );
}

// ---------- drift-guard — Usage ordering ----------

// Placed in query_loop/tests.rs because it exercises run_session's drain loop
// directly, keeping the ordering invariant co-located with the loop under test.

#[tokio::test]
async fn usage_event_arrives_before_turn_complete_when_scripted() {
    // A turn that emits Usage between the assistant text and TurnComplete.
    // The loop must pass through the Usage event without deadlocking or
    // short-circuiting, and exit normally with one assistant turn.
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("ok".into()),
        CompletionEvent::Usage(Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read: None,
            cache_creation: None,
        }),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let outcome = run_session(Vec::new(), ctx(), config(provider))
        .await
        .expect("session ok");

    assert_eq!(outcome.turns, 1);
    assert!(!outcome.cancelled);
    assert_eq!(outcome.final_assistant_text, "ok");
    assert_eq!(outcome.messages.len(), 1);
    assert!(matches!(&outcome.messages[0], Message::Assistant { .. }));
}

#[tokio::test]
async fn turn_with_no_usage_event_exits_cleanly() {
    // Providers that do not expose token counts emit no Usage event.
    // The loop must exit normally without waiting for one.
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("done".into()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let outcome = run_session(Vec::new(), ctx(), config(provider))
        .await
        .expect("session ok");

    assert_eq!(outcome.turns, 1);
    assert!(!outcome.cancelled);
    assert_eq!(outcome.final_assistant_text, "done");
}

#[tokio::test]
async fn dialect_filter_force_always_load_tool_appears_in_all_turns() {
    let mut registry = Registry::new();
    registry.register_io(Arc::new(DeferredIoTool { tool_name: "Promoted" }));
    let invocations = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(EchoTool { invocations: invocations.clone() }));

    let (provider, captured, _captured_deferred) = CapturingProvider::new(
        vec![
            vec![
                CompletionEvent::ToolUse {
                    id: "c1".into(),
                    name: "Echo".into(),
                    input: json!({"msg": "x"}),
                },
                CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
            ],
            vec![
                CompletionEvent::AssistantText("done".into()),
                CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
            ],
        ],
        None,
    );

    // Use ForceAlwaysLoad override so init_session_context promotes "Promoted" into always_load_tools.
    let mut overrides = std::collections::HashMap::new();
    overrides.insert("Promoted".to_string(), LoadPolicyOverride::ForceAlwaysLoad);
    let cfg = RunnerConfig {
        provider,
        bridge: Arc::new(StubBridge),
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings: RunnerSettings { tool_load_overrides: overrides, ..RunnerSettings::default() },
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    };

    let runner_ctx = RunnerContext::new("sess", "agent").unwrap()
        .with_registry(Arc::new(registry));

    run_session(Vec::new(), runner_ctx, cfg).await.unwrap();

    let turns = captured.lock().unwrap();
    assert_eq!(turns.len(), 2);
    for (i, turn_tools) in turns.iter().enumerate() {
        assert!(
            turn_tools.contains(&"Promoted".to_string()),
            "ForceAlwaysLoad tool must appear in turn {}", i + 1
        );
    }
}

// ---------- pending_user_messages drain ----------

/// Tool that pushes a synthetic user-role message onto the runner
/// context's `pending_user_messages` queue. Models the side-channel
/// behavior of the inline `RunSkill` IoTool, which enqueues the
/// substituted skill body for replay as a fresh user turn after the
/// tool_result lands.
struct EnqueueingTool {
    payload: String,
}

#[async_trait]
impl IoTool for EnqueueingTool {
    fn name(&self) -> &str {
        "Enqueue"
    }
    fn description(&self) -> &str {
        "Test tool that pushes a synthetic user message onto pending_user_messages."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn invoke(&self, _input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        ctx.enqueue_user_message(self.payload.clone());
        Ok(ToolOutput::text("enqueued"))
    }
}

/// Regression — API path: after a tool dispatches and the
/// tool_result lands in the transcript, any message the tool enqueued
/// onto `pending_user_messages` MUST (a) be appended as a `Message::User`
/// in the canonical transcript so the next provider turn sees it AND
/// (b) be surfaced to the live event sink as
/// `SessionEvent::HiddenUserMessage` so the UI can render a coalesced
/// chip. Previously the queue was a write-only sink — IoTools wrote and
/// nothing read, so inline-skill bodies never reached the model.
#[tokio::test]
async fn pending_user_messages_drained_into_transcript_and_sink() {
    let mut registry = Registry::new();
    let body = "[skill \"demo\" loaded]\nDo a thing.".to_string();
    registry.register_io(Arc::new(EnqueueingTool {
        payload: body.clone(),
    }));
    let runner_ctx = ctx_with_registry(Arc::new(registry));

    // Turn 1 fires the enqueueing tool; turn 2 has no tool_use so the
    // loop exits naturally.
    let script = vec![
        vec![
            CompletionEvent::AssistantText("priming the skill".into()),
            CompletionEvent::ToolUse {
                id: "call-skill".into(),
                name: "Enqueue".into(),
                input: json!({}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("acting on skill".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let captured: Arc<std::sync::Mutex<Vec<SessionEvent>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::new(CapturingSink {
        captured: Arc::clone(&captured),
    });
    let cfg = RunnerConfig {
        provider,
        bridge: Arc::new(StubBridge),
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings: RunnerSettings::default(),
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: Some(sink),
        thinking: None,
        max_turns: None,
    };

    let outcome = run_session(Vec::new(), runner_ctx, cfg)
        .await
        .expect("session ok");

    // (a) Transcript shape: assistant turn 1 + tool_result + drained
    //     hidden user message + assistant turn 2.
    assert_eq!(
        outcome.messages.len(),
        4,
        "expected 4 messages, got {:?}",
        outcome.messages.iter().map(|m| std::mem::discriminant(m)).collect::<Vec<_>>()
    );
    assert!(matches!(&outcome.messages[0], Message::Assistant { .. }));
    assert!(matches!(&outcome.messages[1], Message::ToolResult { .. }));
    if let Message::User { content } = &outcome.messages[2] {
        assert_eq!(content.len(), 1, "drained message must be a single text block");
        if let ContentBlock::Text { text } = &content[0] {
            assert_eq!(text, &body, "drained message body must match enqueued payload");
        } else {
            panic!("drained message content must be text");
        }
    } else {
        panic!(
            "expected drained user message at index 2; got {:?}",
            std::mem::discriminant(&outcome.messages[2])
        );
    }
    assert!(matches!(&outcome.messages[3], Message::Assistant { .. }));

    // (b) Sink saw a HiddenUserMessage event AFTER the tool_result and
    //     BEFORE the next turn's first AssistantText.
    let events = captured.lock().unwrap();
    let tool_result_idx = events
        .iter()
        .position(|e| matches!(e, SessionEvent::ToolResult { .. }))
        .expect("ToolResult must be emitted");
    let hidden_idx = events
        .iter()
        .position(|e| matches!(e, SessionEvent::HiddenUserMessage { .. }))
        .expect("HiddenUserMessage must be emitted");
    assert!(
        hidden_idx > tool_result_idx,
        "HiddenUserMessage must arrive after the triggering ToolResult \
         (tool_result idx={tool_result_idx}, hidden idx={hidden_idx})"
    );
    if let SessionEvent::HiddenUserMessage { content } = &events[hidden_idx] {
        assert_eq!(content, &body, "sink payload must match enqueued body");
    }

    // The next-turn AssistantText, if any, must arrive AFTER the hidden
    // event so the chip can land between bubbles in render order.
    let next_text_idx = events
        .iter()
        .enumerate()
        .skip(hidden_idx + 1)
        .find_map(|(i, e)| matches!(e, SessionEvent::AssistantText(_)).then_some(i));
    if let Some(idx) = next_text_idx {
        assert!(
            idx > hidden_idx,
            "next assistant text must arrive after the hidden user injection"
        );
    }
}

// ---------- live event sink ----------

/// Sink that captures every emitted `SessionEvent` into a shared `Vec`
/// in arrival order so tests can assert ordering and per-chunk delivery.
struct CapturingSink {
    captured: Arc<std::sync::Mutex<Vec<SessionEvent>>>,
}

impl SessionEventSink for CapturingSink {
    fn emit(&self, event: SessionEvent) {
        self.captured.lock().unwrap().push(event);
    }
}

/// The sink must receive every assistant text fragment as a separate
/// `AssistantText` event — concatenation only happens inside the loop's
/// transcript buffer, never on the live stream. Without this the
/// terminal REPL renders multi-chunk turns as a single end-of-turn block.
#[tokio::test]
async fn event_sink_receives_per_chunk_assistant_text_in_order() {
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("Hi ".into()),
        CompletionEvent::AssistantText("there ".into()),
        CompletionEvent::AssistantText("Axew!".into()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let captured: Arc<std::sync::Mutex<Vec<SessionEvent>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::new(CapturingSink { captured: captured.clone() });

    let mut cfg = config(provider);
    cfg.event_sink = Some(sink as Arc<dyn SessionEventSink>);

    let outcome = run_session(Vec::new(), ctx(), cfg).await.unwrap();
    assert!(!outcome.cancelled);

    let events = captured.lock().unwrap();
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::AssistantText(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec!["Hi ", "there ", "Axew!"],
        "sink must receive each chunk as a separate event in order"
    );
}

/// A turn that emits a `tool_use` block must surface to the sink BEFORE
/// the corresponding `tool_result`. The `[tool_use]` line in the
/// dogfood loop relies on this ordering — without it, results would
/// print before the model's request that produced them.
#[tokio::test]
async fn event_sink_emits_tool_use_before_tool_result() {
    // Turn 1 has a tool_use; turn 2 closes naturally so the loop exits.
    let provider = Arc::new(MockProviderClient::new(vec![
        vec![
            CompletionEvent::AssistantText("calling echo… ".into()),
            CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "Echo".into(),
                input: json!({"msg": "ping"}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::ToolUse },
        ],
        vec![
            CompletionEvent::AssistantText("done.".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ]));

    let echo_invocations = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::new();
    registry.register_io(Arc::new(EchoTool {
        invocations: echo_invocations.clone(),
    }));
    registry.build_deferred_index();

    let captured: Arc<std::sync::Mutex<Vec<SessionEvent>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::new(CapturingSink { captured: captured.clone() });

    let mut cfg = config(provider);
    cfg.event_sink = Some(sink as Arc<dyn SessionEventSink>);

    let outcome = run_session(Vec::new(), ctx_with_registry(Arc::new(registry)), cfg)
        .await
        .unwrap();
    assert_eq!(outcome.turns, 2);
    assert_eq!(echo_invocations.load(Ordering::SeqCst), 1);

    let events = captured.lock().unwrap();

    // Find positions of tool_use and tool_result for the same id.
    let tool_use_idx = events.iter().position(|e| matches!(
        e,
        SessionEvent::ToolUse { id, .. } if id == "call_1"
    ));
    let tool_result_idx = events.iter().position(|e| matches!(
        e,
        SessionEvent::ToolResult { tool_use_id, .. } if tool_use_id == "call_1"
    ));

    let tu = tool_use_idx.expect("ToolUse for call_1 missing");
    let tr = tool_result_idx.expect("ToolResult for call_1 missing");
    assert!(tu < tr, "ToolUse must precede ToolResult (got {tu} then {tr})");

    // The text chunks bracket the tool round-trip in the obvious order.
    let kinds: Vec<&'static str> = events
        .iter()
        .map(|e| match e {
            SessionEvent::AssistantText(_) => "text",
            SessionEvent::ToolUse { .. } => "tool_use",
            SessionEvent::ToolResult { .. } => "tool_result",
            SessionEvent::Usage(_) => "usage",
            SessionEvent::ThinkingStart => "thinking_start",
            SessionEvent::ThinkingDelta { .. } => "thinking_delta",
            SessionEvent::ThinkingEnd { .. } => "thinking_end",
            SessionEvent::ThinkingBlock { .. } => "thinking_block",
            SessionEvent::RedactedThinkingBlock { .. } => "redacted_thinking_block",
            SessionEvent::HiddenUserMessage { .. } => "hidden_user_message",
            SessionEvent::FormPosted { .. } => "form_posted",
        })
        .collect();
    // Guaranteed sequence on the wire: text → tool_use → tool_result → text.
    assert_eq!(kinds, vec!["text", "tool_use", "tool_result", "text"]);
}

// ---------- SessionEvent::Usage variant ----------

#[tokio::test]
async fn mock_provider_usage_event_reaches_sink() {
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("hi".into()),
        CompletionEvent::Usage(Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read: Some(2),
            cache_creation: None,
        }),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let captured: Arc<std::sync::Mutex<Vec<SessionEvent>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::new(CapturingSink { captured: captured.clone() });

    let mut cfg = config(provider);
    cfg.event_sink = Some(sink as Arc<dyn SessionEventSink>);

    let outcome = run_session(Vec::new(), ctx(), cfg).await.unwrap();
    assert!(!outcome.cancelled);

    let events = captured.lock().unwrap();
    let usage_events: Vec<&SessionEvent> = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::Usage(_)))
        .collect();

    assert_eq!(usage_events.len(), 1, "exactly one Usage event must reach the sink");
    if let SessionEvent::Usage(u) = usage_events[0] {
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 5);
        assert_eq!(u.cache_read, Some(2));
    } else {
        panic!("expected Usage event");
    }
}

#[tokio::test]
async fn usage_event_emit_only_does_not_affect_loop() {
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("done".into()),
        CompletionEvent::Usage(Usage {
            input_tokens: 20,
            output_tokens: 10,
            cache_read: None,
            cache_creation: None,
        }),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let captured: Arc<std::sync::Mutex<Vec<SessionEvent>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::new(CapturingSink { captured: captured.clone() });

    let mut cfg = config(provider);
    cfg.event_sink = Some(sink as Arc<dyn SessionEventSink>);

    let outcome = run_session(Vec::new(), ctx(), cfg).await.unwrap();
    assert!(!outcome.cancelled, "Usage event must not affect loop termination");
    assert_eq!(outcome.turns, 1, "session completes in one turn");
    assert_eq!(outcome.final_assistant_text, "done");
}

// ---------- async form snapshot tests ----------

/// A stub tool that masquerades as AskUserQuestionWithForm and immediately
/// returns the async-posted result without touching any bridge.
struct FakeAsyncFormTool {
    form_id: String,
}

#[async_trait]
impl IoTool for FakeAsyncFormTool {
    fn name(&self) -> &str {
        "AskUserQuestionWithForm"
    }
    fn description(&self) -> &str {
        "stub"
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn invoke(
        &self,
        _input: Value,
        _ctx: &RunnerContext,
    ) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::Structured(json!({
            "posted": true,
            "form_id": self.form_id,
            "spec": { "title": "Test form", "intro": null, "fields": [], "form_id": self.form_id }
        })))
    }
}

#[tokio::test]
async fn async_form_intercept_records_pending_form_on_default_thread() {
    use ao_persistence::paths::DataRoot;
    use ao_persistence::snapshot::SnapshotStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let data_root = DataRoot::new(dir.path());
    data_root.ensure_directories().await.unwrap();
    let snap_store = Arc::new(SnapshotStore::load(data_root).await.unwrap());

    let mut registry = Registry::new();
    registry.register_io(Arc::new(FakeAsyncFormTool {
        form_id: "form-test-123".to_string(),
    }));

    let runner_ctx = RunnerContext::new("sess", "agent-snap")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_snapshot_store(Arc::clone(&snap_store));

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "call_form".into(),
                name: "AskUserQuestionWithForm".into(),
                input: json!({ "mode": "async", "title": "Test", "fields": [] }),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");
    assert!(!outcome.cancelled);

    let snap = snap_store.get().await;
    let agent = snap.agents.get("agent-snap").expect("agent entry must exist");
    assert_eq!(agent.pending_forms.len(), 1);
    assert_eq!(
        agent.pending_forms[0].form_id, "form-test-123",
        "pending form must be recorded after async form is posted"
    );
    assert_eq!(
        agent.pending_forms[0].thread_id, None,
        "run carried no thread_id, so the pending form lands on the default thread"
    );
}

/// Regression guard for the thread-scoping bug: a run on a non-default thread
/// must record its pending form under that thread_id, not the default slot —
/// otherwise the form silently surfaces on the wrong tab.
#[tokio::test]
async fn async_form_intercept_scopes_pending_form_to_run_thread_id() {
    use ao_persistence::paths::DataRoot;
    use ao_persistence::snapshot::SnapshotStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let data_root = DataRoot::new(dir.path());
    data_root.ensure_directories().await.unwrap();
    let snap_store = Arc::new(SnapshotStore::load(data_root).await.unwrap());

    let mut registry = Registry::new();
    registry.register_io(Arc::new(FakeAsyncFormTool {
        form_id: "form-thread-b".to_string(),
    }));

    let runner_ctx = RunnerContext::new("sess", "agent-snap")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_snapshot_store(Arc::clone(&snap_store))
        .with_thread("thread-b".to_string());

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "call_form".into(),
                name: "AskUserQuestionWithForm".into(),
                input: json!({ "mode": "async", "title": "Test", "fields": [] }),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");
    assert!(!outcome.cancelled);

    let snap = snap_store.get().await;
    let agent = snap.agents.get("agent-snap").expect("agent entry must exist");
    assert_eq!(agent.pending_forms.len(), 1);
    assert_eq!(agent.pending_forms[0].form_id, "form-thread-b");
    assert_eq!(
        agent.pending_forms[0].thread_id.as_deref(),
        Some("thread-b"),
        "pending form must be scoped to the run's own thread_id, not the default thread"
    );
}

// ---------- tool_result_message: is_error propagation from Structured payloads ----------

#[test]
fn tool_result_message_structured_with_is_error_true_maps_to_is_error() {
    let payload = ToolOutput::Structured(json!({"is_error": true, "stdout": "", "stderr": "oops"}));
    let msg = super::tool_result_message("id-1", &payload);
    match msg {
        Message::ToolResult { tool_use_id, is_error, .. } => {
            assert_eq!(tool_use_id, "id-1");
            assert!(is_error, "Structured payload with is_error=true must yield is_error on the message");
        }
        other => panic!("expected ToolResult, got: {other:?}"),
    }
}

#[test]
fn tool_result_message_structured_without_is_error_field_maps_to_not_is_error() {
    let payload = ToolOutput::Structured(json!({"stdout": "hi", "stderr": ""}));
    let msg = super::tool_result_message("id-2", &payload);
    match msg {
        Message::ToolResult { tool_use_id, is_error, .. } => {
            assert_eq!(tool_use_id, "id-2");
            assert!(!is_error, "Structured payload without is_error field must yield is_error=false");
        }
        other => panic!("expected ToolResult, got: {other:?}"),
    }
}

#[test]
fn tool_result_message_structured_with_is_error_false_maps_to_not_is_error() {
    let payload = ToolOutput::Structured(json!({"is_error": false, "exit_status": 3}));
    let msg = super::tool_result_message("id-3", &payload);
    match msg {
        Message::ToolResult { is_error, .. } => {
            assert!(!is_error, "Structured payload with is_error=false must yield is_error=false");
        }
        other => panic!("expected ToolResult, got: {other:?}"),
    }
}

// ── session-kind regression surface ─────────────────────────────────────────

#[tokio::test]
async fn interactive_session_does_not_include_sleep_tool() {
    let mut base_registry = ao_engine_tools_core::Registry::default();
    crate::register_all(&mut base_registry);
    let runner_ctx = RunnerContext::new("s", "a")
        .unwrap()
        .with_registry(Arc::new(base_registry));
    let cfg = RunnerConfig {
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        ..config(Arc::new(MockProviderClient::new(vec![])))
    };
    let runner_ctx = crate::query_loop::init_session_context(runner_ctx, &cfg);
    assert!(
        runner_ctx.registry.lookup_engine("Sleep").is_none(),
        "Interactive session must not have Sleep in the registry"
    );
}

#[tokio::test]
async fn autonomous_session_includes_sleep_tool() {
    let mut base_registry = ao_engine_tools_core::Registry::default();
    crate::register_all(&mut base_registry);
    let runner_ctx = RunnerContext::new("s", "a")
        .unwrap()
        .with_registry(Arc::new(base_registry));
    let cfg = RunnerConfig {
        kind: SessionKind::Autonomous,
        auto_approve: vec![],
        ..config(Arc::new(MockProviderClient::new(vec![])))
    };
    let runner_ctx = crate::query_loop::init_session_context(runner_ctx, &cfg);
    assert!(
        runner_ctx.registry.lookup_engine("Sleep").is_some(),
        "Autonomous session must have Sleep in the registry"
    );
}

// ── OutcomeRecord persistence ────────────────────────────────

/// Stand-in for the real `RunSkill` engine tool (`ao-engine-tools-engine`,
/// not a dependency of this crate). The query loop only cares about the
/// tool name and the `skill` input field, so a minimal `IoTool` with the
/// same name and input shape exercises the same code path.
struct FakeRunSkillTool;

#[async_trait]
impl IoTool for FakeRunSkillTool {
    fn name(&self) -> &str {
        "RunSkill"
    }
    fn description(&self) -> &str {
        "stand-in RunSkill tool used by query-loop unit tests"
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"skill": {"type": "string"}},
            "required": ["skill"],
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn invoke(&self, _input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("skill loaded"))
    }
}

#[tokio::test]
async fn natural_exit_persists_outcome_record_with_pre_surfaced_memory_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    let outcome_store = Arc::new(ao_persistence::outcome::OutcomeStore::new(data_root));

    let runner_ctx = ctx().with_outcome_store(Arc::clone(&outcome_store));
    runner_ctx.record_artifact_used(ao_protocol::outcome::ArtifactRef::memory("mem-1"));
    runner_ctx.record_artifact_used(ao_protocol::outcome::ArtifactRef::memory("mem-2"));

    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("hello".into()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");
    assert!(!outcome.cancelled);

    let records = outcome_store.read_all("agent-test").await.unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.session_id, "session-test");
    assert!(!record.turn_id.is_empty());
    assert_eq!(
        record.artifacts_used,
        vec![
            ao_protocol::outcome::ArtifactRef::memory("mem-1"),
            ao_protocol::outcome::ArtifactRef::memory("mem-2"),
        ]
    );
    assert_eq!(record.signal, ao_protocol::outcome::OutcomeSignal::Implicit);
}

#[tokio::test]
async fn run_skill_invocation_is_recorded_as_artifact_used_in_outcome_record() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    let outcome_store = Arc::new(ao_persistence::outcome::OutcomeStore::new(data_root));

    let mut registry = Registry::new();
    registry.register_io(Arc::new(FakeRunSkillTool));
    let runner_ctx = ctx_with_registry(Arc::new(registry)).with_outcome_store(Arc::clone(&outcome_store));

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "call_1".into(),
                name: "RunSkill".into(),
                input: json!({"skill": "/review-pr"}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");
    assert!(!outcome.cancelled);

    let records = outcome_store.read_all("agent-test").await.unwrap();
    assert_eq!(records.len(), 1);
    // Leading `/` stripped, matching the resolution rule `RunSkill` itself uses.
    assert_eq!(
        records[0].artifacts_used,
        vec![ao_protocol::outcome::ArtifactRef::skill("review-pr")]
    );
}

#[tokio::test]
async fn cancelled_session_does_not_persist_an_outcome_record() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    let outcome_store = Arc::new(ao_persistence::outcome::OutcomeStore::new(data_root));

    let runner_ctx = ctx().with_outcome_store(Arc::clone(&outcome_store));
    runner_ctx.cancel.cancel();

    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("unreachable".into()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let outcome = run_session(Vec::new(), runner_ctx, config(provider))
        .await
        .expect("session ok");
    assert!(outcome.cancelled);

    let records = outcome_store.read_all("agent-test").await.unwrap();
    assert!(records.is_empty(), "a cancelled turn must not persist an outcome record");
}

#[tokio::test]
async fn missing_outcome_store_is_a_silent_no_op() {
    // No `.with_outcome_store(...)` — the default `RunnerContext` carries
    // `outcome_store: None`. The turn must still complete normally.
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("hello".into()),
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let outcome = run_session(Vec::new(), ctx(), config(provider))
        .await
        .expect("session ok");
    assert!(!outcome.cancelled);
}

#[tokio::test]
async fn turn_cap_exceeded_does_not_persist_an_outcome_record() {
    // Distinct from `cancelled_session_does_not_persist_an_outcome_record`:
    // that test exercises the loop-top `runner_ctx.cancel.is_cancelled()`
    // check. This one exercises the separate `config.max_turns` early
    // return (`query_loop::mod.rs` turn-cap branch), which is its own
    // `return` ahead of `finalize_turn_outcome` and was previously
    // untested. Inspection verifiers rely on this cap to bound a runaway
    // child; this asserts that path also skips outcome persistence, same
    // as true cancellation, since a capped run never "completed".
    let tmp = tempfile::tempdir().unwrap();
    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    let outcome_store = Arc::new(ao_persistence::outcome::OutcomeStore::new(data_root));

    let runner_ctx = ctx().with_outcome_store(Arc::clone(&outcome_store));

    // A single scripted turn that carries a tool_use is enough: with
    // max_turns=1, the cap check fires immediately after this first turn
    // completes, before the tool_use is ever dispatched and before the
    // loop could reach a natural (empty-tool_uses) exit.
    let provider = Arc::new(MockProviderClient::new(vec![vec![
        CompletionEvent::AssistantText("still working".into()),
        CompletionEvent::ToolUse {
            id: "call_1".into(),
            name: "DoesNotExist".into(),
            input: json!({}),
        },
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]]));

    let mut cfg = config(provider);
    cfg.max_turns = Some(1);

    let outcome = run_session(Vec::new(), runner_ctx, cfg)
        .await
        .expect("session ok");
    assert!(outcome.cancelled, "turn-cap exhaustion must report cancelled=true");
    assert_eq!(outcome.turns, 1);

    let records = outcome_store.read_all("agent-test").await.unwrap();
    assert!(
        records.is_empty(),
        "a turn-cap-exhausted run must not persist an outcome record"
    );
}

// --- preview_text char-boundary safety -------------------------------

/// Regression: `preview_text` used to slice its truncated output by raw
/// byte index (`&one_line[..max]`), which panics ("byte index N is not a
/// char boundary") whenever `max` falls inside a multi-byte character.
/// Tool input/output logged through this path routinely carries em-dashes,
/// emoji, and other multi-byte text, so this fired on real tool traffic,
/// not just adversarial input. This builds text with a 3-byte em-dash
/// ('—') straddling the cap exactly.
#[test]
fn preview_text_truncation_does_not_panic_on_multibyte_boundary() {
    let prefix = "x".repeat(199);
    let text = format!("{prefix}—{}", "y".repeat(50));
    assert!(
        !text.is_char_boundary(200),
        "test setup must place a multi-byte char straddling the cap"
    );

    let preview = super::preview_text(&text, 200);

    assert!(preview.contains("bytes>"), "must note the byte count of the original");
}

#[test]
fn preview_text_leaves_short_text_untouched() {
    assert_eq!(super::preview_text("hello", 200), "hello");
}
