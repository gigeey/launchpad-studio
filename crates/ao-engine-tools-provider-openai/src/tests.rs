use std::fs;
use std::sync::Mutex;

use ao_engine_tools_provider_config::OpenAIConfig;
use ao_protocol::data_root::DATA_DIR_ENV_VAR;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use ao_engine_tools_runner::{
    message::{ContentBlock, Message},
    provider::{CompletionEvent, CompletionRequest, ProviderClient, ProviderError, StopReason},
};

use crate::response::OpenAIEvent;
use crate::{run_translator_for_test, ClientCreateError, OpenAIClient};

static DATA_DIR_TEST_MUTEX: Mutex<()> = Mutex::new(());

struct EnvGuard {
    key: &'static str,
    prior: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, val);
        Self { key, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

// ── Config / constructor tests ─────────────────────────────────────────────

#[test]
fn from_loaded_config_success() {
    let _lock = DATA_DIR_TEST_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    fs::write(
        &path,
        r#"
[openai]
api_key = "sk-openai-test"
model = "gpt-4o"
"#,
    )
    .expect("write fixture");

    let _guard = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
    let _client = OpenAIClient::from_loaded_config().expect("client constructed");
}

#[test]
fn from_loaded_config_missing_provider() {
    let _lock = DATA_DIR_TEST_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    fs::write(
        &path,
        r#"
[anthropic]
api_key = "sk-ant-test"
"#,
    )
    .expect("write fixture");

    let _guard = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());

    let err = OpenAIClient::from_loaded_config().expect_err("should fail");
    assert!(
        matches!(err, ClientCreateError::MissingProvider(name) if name == "openai"),
        "expected MissingProvider(\"openai\"), got: {err:?}"
    );
}

#[test]
fn from_loaded_config_openrouter_success() {
    let _lock = DATA_DIR_TEST_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    fs::write(
        &path,
        r#"
[openrouter]
api_key = "sk-or-test"
model = "anthropic/claude-opus-4.7"
"#,
    )
    .expect("write fixture");

    let _guard = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
    let _client = OpenAIClient::from_loaded_config_openrouter().expect("client constructed");
}

#[test]
fn from_loaded_config_openrouter_missing_provider() {
    let _lock = DATA_DIR_TEST_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("providers.toml");
    fs::write(
        &path,
        r#"
[anthropic]
api_key = "sk-ant-test"
"#,
    )
    .expect("write fixture");

    let _guard = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());

    let err = OpenAIClient::from_loaded_config_openrouter().expect_err("should fail");
    assert!(
        matches!(err, ClientCreateError::MissingProvider(name) if name == "openrouter"),
        "expected MissingProvider(\"openrouter\"), got: {err:?}"
    );
}

// ── Translator state-machine order-invariant tests ─────────────────────────

/// Text-only turn: [AssistantText*, Usage, TurnComplete{Natural}]
#[tokio::test]
async fn text_only_turn_order_invariant() {
    let events: Vec<Result<OpenAIEvent, ProviderError>> = vec![
        Ok(OpenAIEvent::TextDelta {
            content: "Hello".into(),
        }),
        Ok(OpenAIEvent::TextDelta {
            content: " world".into(),
        }),
        Ok(OpenAIEvent::FinishReason {
            reason: "stop".into(),
        }),
        Ok(OpenAIEvent::Usage {
            value: json!({ "prompt_tokens": 10, "completion_tokens": 5 }),
        }),
        Ok(OpenAIEvent::Done),
    ];

    let results = run_translator_for_test(events, CancellationToken::new()).await;

    for r in &results {
        assert!(r.is_ok(), "unexpected error: {r:?}");
    }

    let text_positions: Vec<usize> = results
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            if matches!(r, Ok(CompletionEvent::AssistantText(_))) {
                Some(i)
            } else {
                None
            }
        })
        .collect();
    let usage_pos = results
        .iter()
        .position(|r| matches!(r, Ok(CompletionEvent::Usage(_))))
        .expect("Usage not found");
    let tc_pos = results
        .iter()
        .position(|r| matches!(r, Ok(CompletionEvent::TurnComplete { .. })))
        .expect("TurnComplete not found");

    assert!(
        text_positions.iter().all(|&p| p < usage_pos),
        "all AssistantText must precede Usage"
    );
    assert!(usage_pos < tc_pos, "Usage must precede TurnComplete");
    assert_eq!(tc_pos, results.len() - 1, "TurnComplete must be last");

    // StopReason is Natural for "stop" finish_reason.
    assert!(
        matches!(
            &results[tc_pos],
            Ok(CompletionEvent::TurnComplete {
                stop_reason: StopReason::Natural
            })
        ),
        "expected TurnComplete{{Natural}}, got: {:?}",
        results[tc_pos]
    );

    // No ToolUse events in a text-only turn.
    assert!(
        !results
            .iter()
            .any(|r| matches!(r, Ok(CompletionEvent::ToolUse { .. }))),
        "unexpected ToolUse in text-only turn"
    );
}

/// Tool-using turn: [ToolUse*, Usage, TurnComplete{ToolUse}]
#[tokio::test]
async fn tool_using_turn_order_invariant() {
    let events: Vec<Result<OpenAIEvent, ProviderError>> = vec![
        Ok(OpenAIEvent::ToolCallDelta {
            index: 0,
            id: Some("call_abc".into()),
            name: Some("read_file".into()),
            arguments_chunk: None,
        }),
        Ok(OpenAIEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments_chunk: Some("{\"path\":".into()),
        }),
        Ok(OpenAIEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments_chunk: Some("\"src/main.rs\"}".into()),
        }),
        Ok(OpenAIEvent::FinishReason {
            reason: "tool_calls".into(),
        }),
        Ok(OpenAIEvent::Usage {
            value: json!({ "prompt_tokens": 50, "completion_tokens": 20 }),
        }),
        Ok(OpenAIEvent::Done),
    ];

    let results = run_translator_for_test(events, CancellationToken::new()).await;

    for r in &results {
        assert!(r.is_ok(), "unexpected error: {r:?}");
    }

    let tool_pos = results
        .iter()
        .position(|r| matches!(r, Ok(CompletionEvent::ToolUse { .. })))
        .expect("ToolUse not found");
    let usage_pos = results
        .iter()
        .position(|r| matches!(r, Ok(CompletionEvent::Usage(_))))
        .expect("Usage not found");
    let tc_pos = results
        .iter()
        .position(|r| matches!(r, Ok(CompletionEvent::TurnComplete { .. })))
        .expect("TurnComplete not found");

    assert!(tool_pos < usage_pos, "ToolUse must precede Usage");
    assert!(usage_pos < tc_pos, "Usage must precede TurnComplete");
    assert_eq!(tc_pos, results.len() - 1, "TurnComplete must be last");

    // Verify the ToolUse content.
    assert!(
        matches!(&results[tool_pos], Ok(CompletionEvent::ToolUse { id, name, input })
            if id == "call_abc"
            && name == "read_file"
            && input == &json!({"path": "src/main.rs"})),
        "ToolUse content mismatch: {:?}",
        results[tool_pos]
    );

    // StopReason is ToolUse.
    assert!(
        matches!(
            &results[tc_pos],
            Ok(CompletionEvent::TurnComplete {
                stop_reason: StopReason::ToolUse
            })
        ),
        "expected TurnComplete{{ToolUse}}, got: {:?}",
        results[tc_pos]
    );
}

/// Parallel tool turn: two indices emitted in ascending index order, both with
/// distinct tool_call_id strings, followed by Usage + TurnComplete{ToolUse}.
#[tokio::test]
async fn parallel_tool_turn_emits_both_in_index_order() {
    let events: Vec<Result<OpenAIEvent, ProviderError>> = vec![
        Ok(OpenAIEvent::ToolCallDelta {
            index: 0,
            id: Some("call_first".into()),
            name: Some("tool_a".into()),
            arguments_chunk: Some("{\"x\":1}".into()),
        }),
        Ok(OpenAIEvent::ToolCallDelta {
            index: 1,
            id: Some("call_second".into()),
            name: Some("tool_b".into()),
            arguments_chunk: Some("{\"y\":2}".into()),
        }),
        Ok(OpenAIEvent::FinishReason {
            reason: "tool_calls".into(),
        }),
        Ok(OpenAIEvent::Usage {
            value: json!({ "prompt_tokens": 30, "completion_tokens": 15 }),
        }),
        Ok(OpenAIEvent::Done),
    ];

    let results = run_translator_for_test(events, CancellationToken::new()).await;

    for r in &results {
        assert!(r.is_ok(), "unexpected error: {r:?}");
    }

    let tool_events: Vec<&Result<CompletionEvent, ProviderError>> = results
        .iter()
        .filter(|r| matches!(r, Ok(CompletionEvent::ToolUse { .. })))
        .collect();
    assert_eq!(tool_events.len(), 2, "expected exactly 2 ToolUse events");

    // Index 0 → tool_a with call_first id.
    assert!(
        matches!(tool_events[0], Ok(CompletionEvent::ToolUse { id, name, input })
            if id == "call_first"
            && name == "tool_a"
            && input == &json!({"x": 1})),
        "first ToolUse mismatch: {:?}",
        tool_events[0]
    );

    // Index 1 → tool_b with call_second id.
    assert!(
        matches!(tool_events[1], Ok(CompletionEvent::ToolUse { id, name, input })
            if id == "call_second"
            && name == "tool_b"
            && input == &json!({"y": 2})),
        "second ToolUse mismatch: {:?}",
        tool_events[1]
    );

    // Usage and TurnComplete follow both tool uses.
    let last_tool_pos = results
        .iter()
        .rposition(|r| matches!(r, Ok(CompletionEvent::ToolUse { .. })))
        .unwrap();
    let usage_pos = results
        .iter()
        .position(|r| matches!(r, Ok(CompletionEvent::Usage(_))))
        .expect("Usage not found");
    let tc_pos = results
        .iter()
        .position(|r| matches!(r, Ok(CompletionEvent::TurnComplete { .. })))
        .expect("TurnComplete not found");

    assert!(
        last_tool_pos < usage_pos,
        "last ToolUse must precede Usage"
    );
    assert!(usage_pos < tc_pos, "Usage must precede TurnComplete");
}

/// Malformed JSON in tool call arguments emits Error (not ToolUse) for that
/// index; the turn still completes with TurnComplete.
#[tokio::test]
async fn malformed_tool_call_arguments_emits_error_not_tool_use() {
    let events: Vec<Result<OpenAIEvent, ProviderError>> = vec![
        Ok(OpenAIEvent::ToolCallDelta {
            index: 0,
            id: Some("call_bad".into()),
            name: Some("bad_tool".into()),
            arguments_chunk: Some("{not valid json".into()),
        }),
        Ok(OpenAIEvent::FinishReason {
            reason: "tool_calls".into(),
        }),
        Ok(OpenAIEvent::Usage {
            value: json!({ "prompt_tokens": 10, "completion_tokens": 5 }),
        }),
        Ok(OpenAIEvent::Done),
    ];

    let results = run_translator_for_test(events, CancellationToken::new()).await;

    // No ToolUse for the malformed index.
    assert!(
        !results
            .iter()
            .any(|r| matches!(r, Ok(CompletionEvent::ToolUse { .. }))),
        "expected no ToolUse for malformed arguments"
    );

    // Error event is present and mentions the index.
    let has_error = results.iter().any(|r| {
        if let Ok(CompletionEvent::Error(msg)) = r {
            msg.contains("malformed tool_call arguments for index 0")
        } else {
            false
        }
    });
    assert!(has_error, "expected Error event for malformed arguments");

    // TurnComplete is still emitted.
    assert!(
        results
            .iter()
            .any(|r| matches!(r, Ok(CompletionEvent::TurnComplete { .. }))),
        "expected TurnComplete even after malformed arguments"
    );
}

// ─── cancellation + transport error integration tests ─────────────────────────

/// Build a minimal CompletionRequest with one user text message for HTTP-level tests.
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

/// Build an OpenAIConfig pointed at a local port (no TLS, no org/project).
fn local_config(port: u16) -> OpenAIConfig {
    OpenAIConfig {
        api_key: "test-key".into(),
        base_url: format!("http://127.0.0.1:{port}"),
        model: "gpt-4o".into(),
        organization: None,
        project: None,
            max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
}
}

/// Minimal HTTP/1.1 server that accepts one connection, sends HTTP 200 with SSE
/// headers and one TextDelta event, then holds the connection open indefinitely.
///
/// Simulates an in-flight SSE stream so the cancel test can fire mid-stream.
async fn spawn_drip_sse_server() -> (tokio::task::JoinHandle<()>, u16) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind failed");
    let port = listener.local_addr().expect("local_addr").port();

    let handle = tokio::spawn(async move {
        let Ok((mut conn, _)) = listener.accept().await else {
            return;
        };

        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 512];
        loop {
            match conn.read(&mut tmp).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }

        if conn
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: text/event-stream\r\n\
                  Cache-Control: no-cache\r\n\r\n",
            )
            .await
            .is_err()
        {
            return;
        }

        // One TextDelta event — confirms the reader task is alive before cancel fires.
        let event = b"data: {\"id\":\"ctest\",\"choices\":[{\"index\":0,\
                       \"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n";
        if conn.write_all(event).await.is_err() {
            return;
        }

        // Hold the connection open; test cancels before more data arrives.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    });

    (handle, port)
}

/// Minimal HTTP/1.1 server that sends one SSE event via chunked transfer
/// encoding, then closes the connection without the final `0\r\n\r\n` chunk.
///
/// reqwest's chunked decoder reports an error on premature EOF, which
/// propagates through the SSE parser as ProviderError::Transport.
async fn spawn_abrupt_close_server() -> (tokio::task::JoinHandle<()>, u16) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind failed");
    let port = listener.local_addr().expect("local_addr").port();

    let handle = tokio::spawn(async move {
        let Ok((mut conn, _)) = listener.accept().await else {
            return;
        };

        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 512];
        loop {
            match conn.read(&mut tmp).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }

        if conn
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: text/event-stream\r\n\
                  Transfer-Encoding: chunked\r\n\
                  Cache-Control: no-cache\r\n\r\n",
            )
            .await
            .is_err()
        {
            return;
        }

        // One valid SSE event as a proper chunk.
        let event: &[u8] = b"data: {\"id\":\"ctest\",\"choices\":[{\"index\":0,\
                              \"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n";
        let chunk_header = format!("{:X}\r\n", event.len());
        let _ = conn.write_all(chunk_header.as_bytes()).await;
        let _ = conn.write_all(event).await;
        let _ = conn.write_all(b"\r\n").await;

        // Drop without sending the terminal "0\r\n\r\n" chunk.
        // reqwest detects premature EOF and returns an error from the byte stream.
    });

    (handle, port)
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_mid_stream_closes_channel_within_100ms_no_turn_complete() {
    use std::time::{Duration, Instant};

    let (_server_handle, port) = spawn_drip_sse_server().await;
    let client = OpenAIClient::from_config(local_config(port));
    let cancel = CancellationToken::new();

    let mut stream = client
        .complete(minimal_request(), cancel.clone())
        .await
        .expect("complete should succeed against mock server");

    // Wait for the first AssistantText to confirm the reader task is alive.
    let first = stream.recv().await;
    assert!(
        matches!(first, Some(Ok(CompletionEvent::AssistantText(_)))),
        "first event should be AssistantText, got: {first:?}"
    );

    let cancel_at = Instant::now();
    cancel.cancel();

    let mut post_cancel_events = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(200), stream.recv()).await {
            Ok(Some(ev)) => post_cancel_events.push(ev),
            Ok(None) => break,
            Err(_) => panic!(
                "channel did not close within 200ms after cancel; events: {post_cancel_events:?}"
            ),
        }
    }

    assert!(
        cancel_at.elapsed() < Duration::from_millis(100),
        "channel should close within 100ms of cancel, elapsed: {:?}",
        cancel_at.elapsed()
    );

    for ev in post_cancel_events {
        assert!(
            !matches!(ev, Ok(CompletionEvent::TurnComplete { .. })),
            "TurnComplete must not be emitted post-cancel"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn http_401_returns_transport_error_with_status_and_body() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("{\"error\":{\"message\":\"Invalid API key\"}}"),
        )
        .mount(&mock_server)
        .await;

    let config = OpenAIConfig {
        api_key: "sk-bad".into(),
        base_url: mock_server.uri(),
        model: "gpt-4o".into(),
        organization: None,
        project: None,
            max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
};
    let result = OpenAIClient::from_config(config)
        .complete(minimal_request(), CancellationToken::new())
        .await;

    match result {
        Err(ProviderError::Transport(msg)) => {
            assert!(msg.starts_with("401"), "expected '401 ...', got: {msg}");
        }
        other => panic!("expected Transport error, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn http_503_returns_transport_error_with_status_and_body() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
        .mount(&mock_server)
        .await;

    let config = OpenAIConfig {
        api_key: "test-key".into(),
        base_url: mock_server.uri(),
        model: "gpt-4o".into(),
        organization: None,
        project: None,
            max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
};
    let result = OpenAIClient::from_config(config)
        .complete(minimal_request(), CancellationToken::new())
        .await;

    match result {
        Err(ProviderError::Transport(msg)) => {
            assert!(msg.starts_with("503"), "expected '503 ...', got: {msg}");
        }
        other => panic!("expected Transport error, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn closed_connection_mid_stream_yields_transport_error_then_closes() {
    let (_server_handle, port) = spawn_abrupt_close_server().await;
    let client = OpenAIClient::from_config(local_config(port));

    let mut stream = client
        .complete(minimal_request(), CancellationToken::new())
        .await
        .expect("complete should succeed for HTTP 200");

    let mut events = Vec::new();
    while let Some(ev) = stream.recv().await {
        events.push(ev);
    }

    let has_transport_err = events
        .iter()
        .any(|e| matches!(e, Err(ProviderError::Transport(_))));
    assert!(
        has_transport_err,
        "expected Transport error from abrupt connection close, got: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_sse_data_mid_stream_yields_transport_error_and_closes() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Valid first event followed by a malformed JSON data line.
    let body = concat!(
        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,",
        "\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
        "data: {not valid json}\n\n",
    );

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&mock_server)
        .await;

    let config = OpenAIConfig {
        api_key: "test-key".into(),
        base_url: mock_server.uri(),
        model: "gpt-4o".into(),
        organization: None,
        project: None,
            max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
};
    let mut stream = OpenAIClient::from_config(config)
        .complete(minimal_request(), CancellationToken::new())
        .await
        .expect("complete should succeed for HTTP 200");

    let mut events = Vec::new();
    while let Some(ev) = stream.recv().await {
        events.push(ev);
    }

    let has_transport_err = events.iter().any(|e| {
        matches!(e, Err(ProviderError::Transport(msg)) if msg.contains("SSE parse error"))
    });
    assert!(
        has_transport_err,
        "expected Transport error containing 'SSE parse error', got: {events:?}"
    );

    // Channel is closed — stream.recv() already returned None above.
}
