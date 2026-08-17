//! Integration tests for the Gemini provider.
//!
//! End-to-end and positional-ordering regression tests appear below.

use std::sync::{Arc, Mutex};

use crate::{
    ordering::ToolCallOrderTracker,
    response::{GeminiError, GeminiStreamEvent},
    run_translator_for_test, GeminiClient,
};
use ao_engine_tools_provider_config::GeminiConfig;
use ao_engine_tools_runner::{
    message::{ContentBlock, Message},
    provider::{CompletionEvent, CompletionRequest, ProviderClient, ProviderError},
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fresh_tracker() -> Arc<Mutex<ToolCallOrderTracker>> {
    Arc::new(Mutex::new(ToolCallOrderTracker::new()))
}

fn text_event(text: &str) -> Result<GeminiStreamEvent, GeminiError> {
    Ok(GeminiStreamEvent {
        parts: vec![json!({ "text": text })],
        finish_reason: None,
        usage: None,
    })
}

fn tool_call_event(name: &str, args: Value) -> Result<GeminiStreamEvent, GeminiError> {
    Ok(GeminiStreamEvent {
        parts: vec![json!({ "functionCall": { "name": name, "args": args } })],
        finish_reason: None,
        usage: None,
    })
}

fn mixed_event(text: &str, tool_name: &str, args: Value) -> Result<GeminiStreamEvent, GeminiError> {
    Ok(GeminiStreamEvent {
        parts: vec![
            json!({ "text": text }),
            json!({ "functionCall": { "name": tool_name, "args": args } }),
        ],
        finish_reason: None,
        usage: None,
    })
}

fn assert_assistant_text(result: &Result<CompletionEvent, impl std::fmt::Debug>, expected: &str) {
    match result {
        Ok(CompletionEvent::AssistantText(text)) => {
            assert_eq!(text, expected, "AssistantText content mismatch");
        }
        other => panic!("expected AssistantText({expected:?}), got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (a) Text-only stream — no functionCalls, ids irrelevant
// ---------------------------------------------------------------------------

#[tokio::test]
async fn translator_text_only_stream() {
    let events = vec![text_event("Hello, "), text_event("world!")];
    let cancel = CancellationToken::new();
    let results = run_translator_for_test(events, cancel, fresh_tracker(), 0).await;

    assert_eq!(results.len(), 2);
    assert_assistant_text(&results[0], "Hello, ");
    assert_assistant_text(&results[1], "world!");
}

// ---------------------------------------------------------------------------
// (b) FunctionCall-only stream — single part at absolute index 0
// ---------------------------------------------------------------------------

#[tokio::test]
async fn translator_function_call_only_stream() {
    let events = vec![tool_call_event("Read", json!({ "file_path": "/tmp/a.txt" }))];
    let cancel = CancellationToken::new();
    let tracker = fresh_tracker();
    let results = run_translator_for_test(events, cancel, Arc::clone(&tracker), 0).await;

    assert_eq!(results.len(), 1);
    match &results[0] {
        Ok(CompletionEvent::ToolUse { id, name, input }) => {
            // functionCall is at absolute parts index 0 in this single-part event
            assert_eq!(id, "gemini-call-0-0");
            assert_eq!(name, "Read");
            assert_eq!(input["file_path"], "/tmp/a.txt");
        }
        other => panic!("expected ToolUse, got: {other:?}"),
    }

    // Verify the tracker was populated
    let t = tracker.lock().unwrap();
    assert_eq!(t.lookup_name(0, 0), Some("Read"));
}

// ---------------------------------------------------------------------------
// (c) Mixed text + functionCall in one event — text at index 0, call at index 1
// ---------------------------------------------------------------------------

#[tokio::test]
async fn translator_mixed_text_and_function_call_in_one_event() {
    let events = vec![mixed_event(
        "I will read the file.",
        "Read",
        json!({ "file_path": "/etc/hosts" }),
    )];
    let cancel = CancellationToken::new();
    let results = run_translator_for_test(events, cancel, fresh_tracker(), 0).await;

    assert_eq!(results.len(), 2, "expected text then tool-use");
    assert_assistant_text(&results[0], "I will read the file.");

    match &results[1] {
        Ok(CompletionEvent::ToolUse { id, name, input }) => {
            // text part occupies index 0, functionCall is at absolute index 1
            assert_eq!(id, "gemini-call-0-1");
            assert_eq!(name, "Read");
            assert_eq!(input["file_path"], "/etc/hosts");
        }
        other => panic!("expected ToolUse, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (d) Multiple sequential events — global_part_index accumulates across events
//     Event 0: [text]          → text at abs index 0
//     Event 1: [functionCall]  → call at abs index 1  → "gemini-call-0-1"
//     Event 2: [text]          → text at abs index 2
//     Event 3: [functionCall]  → call at abs index 3  → "gemini-call-0-3"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn translator_multiple_sequential_events() {
    let events = vec![
        text_event("First chunk. "),
        tool_call_event("Bash", json!({ "command": "ls" })),
        text_event("Second chunk."),
        tool_call_event("Read", json!({ "file_path": "/tmp/b.txt" })),
    ];
    let cancel = CancellationToken::new();
    let results = run_translator_for_test(events, cancel, fresh_tracker(), 0).await;

    assert_eq!(results.len(), 4);
    assert_assistant_text(&results[0], "First chunk. ");

    match &results[1] {
        Ok(CompletionEvent::ToolUse { id, name, .. }) => {
            assert_eq!(id, "gemini-call-0-1");  // text(0) then Bash(1)
            assert_eq!(name, "Bash");
        }
        other => panic!("expected ToolUse[0], got: {other:?}"),
    }

    assert_assistant_text(&results[2], "Second chunk.");

    match &results[3] {
        Ok(CompletionEvent::ToolUse { id, name, .. }) => {
            assert_eq!(id, "gemini-call-0-3");  // text(0), Bash(1), text(2), Read(3)
            assert_eq!(name, "Read");
        }
        other => panic!("expected ToolUse[1], got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Terminal events: Usage + TurnComplete
// ---------------------------------------------------------------------------

fn terminal_event(finish_reason: &str, usage: Option<serde_json::Value>) -> Result<GeminiStreamEvent, GeminiError> {
    Ok(GeminiStreamEvent {
        parts: vec![],
        finish_reason: Some(finish_reason.into()),
        usage,
    })
}

fn usage_metadata(prompt: u64, candidates: u64, total: u64) -> serde_json::Value {
    json!({
        "promptTokenCount": prompt,
        "candidatesTokenCount": candidates,
        "totalTokenCount": total,
    })
}

#[tokio::test]
async fn translator_text_stream_with_stop_emits_turn_complete() {
    use ao_engine_tools_runner::provider::StopReason;

    let events = vec![
        text_event("Hello"),
        terminal_event("STOP", None),
    ];
    let cancel = CancellationToken::new();
    let results = run_translator_for_test(events, cancel, fresh_tracker(), 0).await;

    assert_eq!(results.len(), 2, "expected AssistantText then TurnComplete");
    assert_assistant_text(&results[0], "Hello");
    match &results[1] {
        Ok(CompletionEvent::TurnComplete { stop_reason }) => {
            assert_eq!(*stop_reason, StopReason::Natural);
        }
        other => panic!("expected TurnComplete, got: {other:?}"),
    }
}

#[tokio::test]
async fn translator_function_call_turn_emits_tool_use_stop_reason() {
    use ao_engine_tools_runner::provider::StopReason;

    let events = vec![
        tool_call_event("Read", json!({ "file_path": "/tmp/x" })),
        terminal_event("STOP", None),
    ];
    let cancel = CancellationToken::new();
    let results = run_translator_for_test(events, cancel, fresh_tracker(), 0).await;

    assert_eq!(results.len(), 2, "expected ToolUse then TurnComplete");
    match &results[0] {
        Ok(CompletionEvent::ToolUse { .. }) => {}
        other => panic!("expected ToolUse, got: {other:?}"),
    }
    // has_function_call override: STOP → ToolUse
    match &results[1] {
        Ok(CompletionEvent::TurnComplete { stop_reason }) => {
            assert_eq!(*stop_reason, StopReason::ToolUse);
        }
        other => panic!("expected TurnComplete with ToolUse, got: {other:?}"),
    }
}

#[tokio::test]
async fn translator_terminal_event_with_usage_emits_usage_before_turn_complete() {
    use ao_engine_tools_runner::provider::{StopReason, Usage};

    let events = vec![
        text_event("Done"),
        terminal_event("STOP", Some(usage_metadata(100, 50, 150))),
    ];
    let cancel = CancellationToken::new();
    let results = run_translator_for_test(events, cancel, fresh_tracker(), 0).await;

    assert_eq!(results.len(), 3, "expected AssistantText, Usage, TurnComplete");
    assert_assistant_text(&results[0], "Done");
    match &results[1] {
        Ok(CompletionEvent::Usage(u)) => {
            assert_eq!(*u, Usage { input_tokens: 100, output_tokens: 50, cache_read: None, cache_creation: None });
        }
        other => panic!("expected Usage, got: {other:?}"),
    }
    match &results[2] {
        Ok(CompletionEvent::TurnComplete { stop_reason }) => {
            assert_eq!(*stop_reason, StopReason::Natural);
        }
        other => panic!("expected TurnComplete, got: {other:?}"),
    }
}

#[tokio::test]
async fn translator_max_tokens_stop_reason_mapped() {
    use ao_engine_tools_runner::provider::StopReason;

    let events = vec![
        text_event("truncated"),
        terminal_event("MAX_TOKENS", None),
    ];
    let cancel = CancellationToken::new();
    let results = run_translator_for_test(events, cancel, fresh_tracker(), 0).await;

    assert_eq!(results.len(), 2);
    match &results[1] {
        Ok(CompletionEvent::TurnComplete { stop_reason }) => {
            assert_eq!(*stop_reason, StopReason::MaxTokens);
        }
        other => panic!("expected TurnComplete(MaxTokens), got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// SSE decode error surfaces as ProviderError::Transport
// ---------------------------------------------------------------------------

#[tokio::test]
async fn translator_sse_error_surfaces_as_transport_error() {
    let events = vec![
        text_event("ok"),
        Err(GeminiError::Decode { context: "bad json".into() }),
    ];
    let cancel = CancellationToken::new();
    let results = run_translator_for_test(events, cancel, fresh_tracker(), 0).await;

    assert_eq!(results.len(), 2);
    assert_assistant_text(&results[0], "ok");
    match &results[1] {
        Err(e) => {
            let msg = e.to_string();
            assert!(msg.contains("bad json"), "unexpected error message: {msg}");
        }
        other => panic!("expected Transport error, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// HTTP-level tests: cancellation, 429, 401, transport error
// ---------------------------------------------------------------------------

fn minimal_request() -> CompletionRequest {
    CompletionRequest {
        messages: vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }],
        system_prompt: None,
        tools: vec![],
        mode: Default::default(),
        ..Default::default()
    }
}

fn test_config(base_url: String) -> GeminiConfig {
    GeminiConfig {
        api_key: "AIza-TEST-KEY".into(),
        base_url,
        model: "gemini-1.5-pro".into(),
    }
}

// (a) Pre-cancelling the token terminates the stream cleanly without panic.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_token_pre_fired_terminates_stream_cleanly() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let sse_body = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hello\"}]}}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[]},\"finishReason\":\"STOP\"}],",
        "\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":1,\"totalTokenCount\":6}}\n\n",
    );

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(".*streamGenerateContent.*"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&mock_server)
        .await;

    let cancel = CancellationToken::new();
    // Pre-cancel before complete() is called.
    cancel.cancel();

    let client = GeminiClient::from_config(test_config(mock_server.uri())).unwrap();
    let mut stream = client
        .complete(minimal_request(), cancel)
        .await
        .expect("complete should succeed for HTTP 200");

    // Drain the stream; it should terminate without panicking.
    let mut events = Vec::new();
    while let Some(ev) = stream.recv().await {
        events.push(ev);
    }
    // Biased select checks cancel first → translator immediately breaks → 0 events.
    assert!(
        events.is_empty(),
        "expected no events with pre-cancelled token, got: {events:?}"
    );
}

// (b) HTTP 429 with Retry-After header surfaces as a Transport error containing "429".
#[tokio::test(flavor = "multi_thread")]
async fn http_429_with_retry_after_returns_transport_error() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(".*streamGenerateContent.*"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .set_body_string(r#"{"error":{"message":"Resource exhausted"}}"#),
        )
        .mount(&mock_server)
        .await;

    let client = GeminiClient::from_config(test_config(mock_server.uri())).unwrap();
    let result = client
        .complete(minimal_request(), CancellationToken::new())
        .await;

    match result {
        Err(ProviderError::Transport(msg)) => {
            assert!(msg.starts_with("429:"), "expected '429: ...', got: {msg}");
            assert!(msg.contains("Resource exhausted"), "expected error message in body, got: {msg}");
        }
        other => panic!("expected Transport error, got: {other:?}"),
    }
}

// (c) HTTP 401 surfaces error.message from response body.
#[tokio::test(flavor = "multi_thread")]
async fn http_401_surfaces_error_message_from_body() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(".*streamGenerateContent.*"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(r#"{"error":{"message":"API key not valid. Please pass a valid API key."}}"#),
        )
        .mount(&mock_server)
        .await;

    let client = GeminiClient::from_config(test_config(mock_server.uri())).unwrap();
    let result = client
        .complete(minimal_request(), CancellationToken::new())
        .await;

    match result {
        Err(ProviderError::Transport(msg)) => {
            assert!(
                msg.contains("API key not valid"),
                "expected error.message surfaced verbatim, got: {msg}"
            );
        }
        other => panic!("expected Transport error, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Positional-ordering regression test
//
// Scripts two parallel Read functionCall parts in a single assistant turn,
// supplies the tool results in REVERSED completion order, and asserts the
// next-turn functionResponse parts are in the original parts-array order.
//
// Uses the REAL messages.rs denormalizer and REAL ordering.rs parser — not a
// unit mock of either.  If the synthesised-id format ever drifts out of sync
// with the parser this test will fail to re-pair the responses and the
// position assertions will catch it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn positional_ordering_two_parallel_reads_complete_in_reverse_order() {
    use crate::messages::GeminiMessageNormalizer;
    use ao_engine_tools_runner::message::MessageNormalizer;

    // --- Part 1: run the translator against a scripted assistant turn ---------
    //
    // The assistant turn contains two parallel functionCall parts in a single
    // SSE event:
    //   parts[0]  →  Read { file_path: "/path/a" }   →  synthesised id gemini-call-0-0
    //   parts[1]  →  Read { file_path: "/path/b" }   →  synthesised id gemini-call-0-1
    //
    // A follow-up terminal event with finishReason=STOP closes the turn.

    let tracker = fresh_tracker();
    let cancel = CancellationToken::new();

    let parallel_call_event = Ok(GeminiStreamEvent {
        parts: vec![
            json!({ "functionCall": { "name": "Read", "args": { "file_path": "/path/a" } } }),
            json!({ "functionCall": { "name": "Read", "args": { "file_path": "/path/b" } } }),
        ],
        finish_reason: None,
        usage: None,
    });
    let terminal_event = Ok(GeminiStreamEvent {
        parts: vec![],
        finish_reason: Some("STOP".into()),
        usage: None,
    });

    let results =
        run_translator_for_test(vec![parallel_call_event, terminal_event], cancel, Arc::clone(&tracker), 0)
            .await;

    // Expect: ToolUse(0), ToolUse(1), TurnComplete — has_function_call overrides STOP → ToolUse.
    assert_eq!(results.len(), 3, "expected ToolUse×2 + TurnComplete, got: {results:?}");

    // Capture the synthesised ids emitted by the translator.
    let id_0 = match &results[0] {
        Ok(CompletionEvent::ToolUse { id, name, input }) => {
            assert_eq!(id, "gemini-call-0-0", "first call must be at absolute index 0");
            assert_eq!(name, "Read");
            assert_eq!(input["file_path"], "/path/a");
            id.clone()
        }
        other => panic!("expected ToolUse[0], got: {other:?}"),
    };
    let id_1 = match &results[1] {
        Ok(CompletionEvent::ToolUse { id, name, input }) => {
            assert_eq!(id, "gemini-call-0-1", "second call must be at absolute index 1");
            assert_eq!(name, "Read");
            assert_eq!(input["file_path"], "/path/b");
            id.clone()
        }
        other => panic!("expected ToolUse[1], got: {other:?}"),
    };

    // --- Part 2: supply tool results in REVERSED completion order ------------
    //
    // The executor finishes parts_index=1 first, then parts_index=0.
    // The ToolResult messages therefore arrive with id_1 before id_0.

    let tool_results_reversed_order = vec![
        Message::ToolResult {
            tool_use_id: id_1.clone(), // gemini-call-0-1 (parts_index=1) arrived first
            content: vec![ContentBlock::Text { text: "contents of /path/b".into() }],
            is_error: false,
        },
        Message::ToolResult {
            tool_use_id: id_0.clone(), // gemini-call-0-0 (parts_index=0) arrived second
            content: vec![ContentBlock::Text { text: "contents of /path/a".into() }],
            is_error: false,
        },
    ];

    // --- Part 3: run the REAL denormalizer with the shared tracker -----------
    //
    // The tracker was populated by run_translator_for_test above; the
    // GeminiMessageNormalizer shares the same Arc<Mutex<...>>.

    let norm = GeminiMessageNormalizer::with_tracker(Arc::clone(&tracker));
    let value = norm
        .to_provider(&tool_results_reversed_order)
        .expect("to_provider must not fail for valid ToolResult messages");

    // --- Part 4: assert parts[] is in original parts-array order -------------
    //
    // Even though the results arrived in reverse (1 then 0), the denormalizer
    // must emit functionResponse parts sorted by parts_index:
    //   parts[0]  →  Read response for /path/a  (parts_index=0)
    //   parts[1]  →  Read response for /path/b  (parts_index=1)

    let contents = value.as_array().expect("to_provider must return an array");
    assert_eq!(contents.len(), 1, "consecutive ToolResult messages must collapse into one user-role message");
    assert_eq!(contents[0]["role"], "user");

    let parts = contents[0]["parts"].as_array().expect("user message must have a parts array");
    assert_eq!(parts.len(), 2, "expected exactly 2 functionResponse parts");

    // parts[0] must correspond to the call at parts_index=0 (Read "/path/a").
    assert_eq!(
        parts[0]["functionResponse"]["name"], "Read",
        "parts[0] must be the Read call at original parts_index=0"
    );
    assert_eq!(
        parts[0]["functionResponse"]["response"]["output"], "contents of /path/a",
        "parts[0] response must be the /path/a contents (original order restored)"
    );

    // parts[1] must correspond to the call at parts_index=1 (Read "/path/b").
    assert_eq!(
        parts[1]["functionResponse"]["name"], "Read",
        "parts[1] must be the Read call at original parts_index=1"
    );
    assert_eq!(
        parts[1]["functionResponse"]["response"]["output"], "contents of /path/b",
        "parts[1] response must be the /path/b contents (original order restored)"
    );
}

// (d) Connection-refused surfaces as Transport error (reqwest::Error wrapping).
#[tokio::test(flavor = "multi_thread")]
async fn connection_refused_surfaces_as_transport_error() {
    // Port 1 is reserved and will always refuse connections.
    let client = GeminiClient::from_config(GeminiConfig {
        api_key: "AIza-TEST-KEY".into(),
        base_url: "http://127.0.0.1:1".into(),
        model: "gemini-1.5-pro".into(),
    })
    .unwrap();

    let result = client
        .complete(minimal_request(), CancellationToken::new())
        .await;

    match result {
        Err(ProviderError::Transport(msg)) => {
            assert!(!msg.is_empty(), "expected non-empty transport error message");
        }
        other => panic!("expected Transport error for connection refused, got: {other:?}"),
    }
}
