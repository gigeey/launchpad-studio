//! SSE parser for Gemini's `streamGenerateContent?alt=sse` response stream.
//!
//! Gemini emits one complete JSON object per `data:` SSE event — distinct
//! from OpenAI's line-delimited JSONL style. This module handles framing and
//! JSON dispatch into typed [`GeminiStreamEvent`] values. Translation to
//! canonical [`CompletionEvent`]s lives in `lib.rs`.

use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::collections::VecDeque;
use thiserror::Error;

/// Error variants that can arise during SSE parsing or HTTP transport.
#[derive(Debug, Error)]
pub enum GeminiError {
    /// Malformed JSON or invalid UTF-8 in an SSE `data:` block.
    #[error("gemini decode error: {context}")]
    Decode { context: String },

    /// HTTP 429 — server asked us to back off.
    #[error("gemini rate limit: {message}")]
    RateLimit {
        message: String,
        retry_after: Option<std::time::Duration>,
    },

    /// HTTP 401 or 403 — credentials rejected.
    #[error("gemini auth error: {message}")]
    Auth { message: String },

    /// Other HTTP 4xx / 5xx.
    #[error("gemini provider error {status}: {body}")]
    Provider { status: u16, body: String },

    /// Network / connection failure.
    #[error("gemini transport error: {0}")]
    Transport(reqwest::Error),
}

/// One typed event parsed from the Gemini `streamGenerateContent?alt=sse` stream.
///
/// Mirrors the structure of a Gemini `GenerateContentResponse` chunk:
/// - `parts` carries the `candidates[0].content.parts[]` array (as raw JSON)
/// - `finish_reason` is `candidates[0].finishReason` when non-null/non-empty
/// - `usage` is the top-level `usageMetadata` object when present
#[derive(Debug, Clone, PartialEq)]
pub struct GeminiStreamEvent {
    /// The `candidates[0].content.parts[]` array, in source order.
    pub parts: Vec<Value>,
    /// `candidates[0].finishReason`, present only when the turn ends.
    pub finish_reason: Option<String>,
    /// The top-level `usageMetadata` block, present in the terminal event.
    pub usage: Option<Value>,
}

/// Adapt a chunked byte stream into a stream of typed [`GeminiStreamEvent`] values.
///
/// Frames on `\n\n` (or `\r\n\r\n`) SSE boundaries. Handles:
/// - `event:` / `id:` / `:` comment lines (ignored per SSE spec)
/// - blank-line event separators
/// - multi-line `data:` continuations (joined with `\n` per SSE spec)
///
/// Malformed JSON surfaces as [`GeminiError::Decode`]. The stream terminates
/// cleanly on upstream EOF — Gemini has no `[DONE]` sentinel.
///
/// Buffer is capped at 64 KiB; exceeding this without a boundary closes the
/// stream with [`GeminiError::Decode`].
pub fn parse_sse_stream<S, B, E>(
    byte_stream: S,
) -> impl Stream<Item = Result<GeminiStreamEvent, GeminiError>>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    struct SseState<S> {
        inner: S,
        buf: Vec<u8>,
        pending: VecDeque<Result<GeminiStreamEvent, GeminiError>>,
        done: bool,
    }

    futures_util::stream::unfold(
        SseState {
            inner: byte_stream,
            buf: Vec::new(),
            pending: VecDeque::new(),
            done: false,
        },
        |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((event, state));
                }

                if state.done {
                    return None;
                }

                if let Some(drain_end) = find_sse_boundary(&state.buf) {
                    let block = state.buf[..drain_end].to_vec();
                    state.buf.drain(..drain_end);

                    match extract_event(&block) {
                        Ok(None) => continue,
                        Ok(Some(event)) => {
                            state.pending.push_back(Ok(event));
                            continue;
                        }
                        Err(e) => {
                            return Some((Err(e), SseState { done: true, ..state }));
                        }
                    }
                }

                if state.buf.len() > 64 * 1024 {
                    return Some((
                        Err(GeminiError::Decode {
                            context: "SSE parse error: buffer exceeded 64 KiB without event boundary".into(),
                        }),
                        SseState { done: true, ..state },
                    ));
                }

                match state.inner.next().await {
                    Some(Ok(chunk)) => state.buf.extend_from_slice(chunk.as_ref()),
                    Some(Err(e)) => {
                        return Some((
                            Err(GeminiError::Decode {
                                context: format!("SSE stream error: {e}"),
                            }),
                            SseState { done: true, ..state },
                        ));
                    }
                    None => return None,
                }
            }
        },
    )
}

/// Return the byte offset just past the first `\n\n` or `\r\n\r\n` SSE boundary,
/// or `None` if no complete boundary is present yet.
fn find_sse_boundary(buf: &[u8]) -> Option<usize> {
    let len = buf.len();
    let mut i = 0;
    while i < len {
        match buf[i] {
            b'\n' if i + 1 < len && buf[i + 1] == b'\n' => return Some(i + 2),
            b'\r'
                if i + 3 < len
                    && buf[i + 1] == b'\n'
                    && buf[i + 2] == b'\r'
                    && buf[i + 3] == b'\n' =>
            {
                return Some(i + 4)
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Parse one SSE event block into a typed [`GeminiStreamEvent`], or `None`
/// for blocks with no `data:` line (comment-only or `event:`-only blocks).
///
/// Multi-line `data:` continuations are joined with `\n` per SSE spec.
/// Returns `Err(GeminiError::Decode)` on malformed JSON or invalid UTF-8.
fn extract_event(block: &[u8]) -> Result<Option<GeminiStreamEvent>, GeminiError> {
    let text = std::str::from_utf8(block).map_err(|e| GeminiError::Decode {
        context: format!("SSE parse error: invalid UTF-8: {e}"),
    })?;

    let mut data_lines: Vec<&str> = Vec::new();

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            let value = rest.strip_prefix(' ').unwrap_or(rest);
            data_lines.push(value);
        }
        // `event:`, `id:`, `:` (comment), and unknown fields are ignored per SSE spec.
    }

    if data_lines.is_empty() {
        return Ok(None);
    }

    let json_str = data_lines.join("\n");

    let value: Value = serde_json::from_str(&json_str).map_err(|e| GeminiError::Decode {
        context: format!("SSE JSON parse error: {e}"),
    })?;

    parse_response_event(&value).map(Some)
}

/// Extract a typed [`GeminiStreamEvent`] from a parsed Gemini SSE JSON chunk.
fn parse_response_event(value: &Value) -> Result<GeminiStreamEvent, GeminiError> {
    let first_candidate = value
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|cs| cs.first());

    let parts = first_candidate
        .and_then(|c| c.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    let finish_reason = first_candidate
        .and_then(|c| c.get("finishReason"))
        .and_then(|r| r.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    let usage = value.get("usageMetadata").filter(|v| !v.is_null()).cloned();

    Ok(GeminiStreamEvent {
        parts,
        finish_reason,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::json;

    fn bytes_stream(
        chunks: Vec<Vec<u8>>,
    ) -> impl Stream<Item = Result<Vec<u8>, std::convert::Infallible>> {
        futures_util::stream::iter(chunks.into_iter().map(Ok))
    }

    fn text_event_sse(text: &str) -> Vec<u8> {
        let json = serde_json::to_string(&json!({
            "candidates": [
                {
                    "content": {
                        "role": "model",
                        "parts": [{ "text": text }]
                    }
                }
            ]
        }))
        .unwrap();
        format!("data: {json}\n\n").into_bytes()
    }

    fn tool_call_event_sse(name: &str, args: serde_json::Value) -> Vec<u8> {
        let json = serde_json::to_string(&json!({
            "candidates": [
                {
                    "content": {
                        "role": "model",
                        "parts": [{ "functionCall": { "name": name, "args": args } }]
                    }
                }
            ]
        }))
        .unwrap();
        format!("data: {json}\n\n").into_bytes()
    }

    fn terminal_event_sse(finish_reason: &str) -> Vec<u8> {
        let json = serde_json::to_string(&json!({
            "candidates": [
                {
                    "content": { "role": "model", "parts": [] },
                    "finishReason": finish_reason
                }
            ],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        }))
        .unwrap();
        format!("data: {json}\n\n").into_bytes()
    }

    // (a) Single-event stream yields one event and then ends.
    #[tokio::test]
    async fn single_event_stream() {
        let data = text_event_sse("Hello, world!");
        let stream = parse_sse_stream(bytes_stream(vec![data]));
        let events: Vec<_> = stream.collect().await;

        assert_eq!(events.len(), 1);
        let ev = events[0].as_ref().expect("expected Ok event");
        assert_eq!(ev.parts.len(), 1);
        assert_eq!(ev.parts[0]["text"], "Hello, world!");
        assert!(ev.finish_reason.is_none());
        assert!(ev.usage.is_none());
    }

    // (b) Multi-event stream yields all events in order.
    #[tokio::test]
    async fn multi_event_stream() {
        let mut data = text_event_sse("Hello ");
        data.extend(text_event_sse("world"));
        data.extend(terminal_event_sse("STOP"));

        let stream = parse_sse_stream(bytes_stream(vec![data]));
        let events: Vec<_> = stream.collect().await;

        assert_eq!(events.len(), 3, "expected 3 events");

        let e0 = events[0].as_ref().unwrap();
        assert_eq!(e0.parts[0]["text"], "Hello ");
        assert!(e0.finish_reason.is_none());

        let e1 = events[1].as_ref().unwrap();
        assert_eq!(e1.parts[0]["text"], "world");
        assert!(e1.finish_reason.is_none());

        let e2 = events[2].as_ref().unwrap();
        assert!(e2.parts.is_empty());
        assert_eq!(e2.finish_reason.as_deref(), Some("STOP"));
        let usage = e2.usage.as_ref().unwrap();
        assert_eq!(usage["promptTokenCount"], 10);
        assert_eq!(usage["candidatesTokenCount"], 5);
        assert_eq!(usage["totalTokenCount"], 15);
    }

    // (c) Multi-line data continuation is joined and parsed correctly.
    #[tokio::test]
    async fn multi_line_data_continuation() {
        // Split a JSON object across two data: lines at a token boundary.
        // Per SSE spec, multiple data: lines in one event are joined with \n.
        // JSON allows whitespace (including \n) between tokens, so this is valid.
        let json_part1 = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"#;
        let json_part2 = r#""finishReason":"STOP"}]}"#;

        let sse = format!("data: {json_part1}\ndata: {json_part2}\n\n");
        let stream = parse_sse_stream(bytes_stream(vec![sse.into_bytes()]));
        let events: Vec<_> = stream.collect().await;

        assert_eq!(events.len(), 1);
        let ev = events[0].as_ref().expect("expected Ok event");
        assert_eq!(ev.parts[0]["text"], "hi");
        assert_eq!(ev.finish_reason.as_deref(), Some("STOP"));
    }

    // (d) Malformed JSON in a data: block surfaces as Decode error.
    #[tokio::test]
    async fn malformed_json_yields_decode_error() {
        let malformed = b"data: {not valid json}\n\n".to_vec();
        let stream = parse_sse_stream(bytes_stream(vec![malformed]));
        let events: Vec<_> = stream.collect().await;

        assert_eq!(events.len(), 1, "expected exactly one error event");
        match &events[0] {
            Err(GeminiError::Decode { context }) => {
                assert!(context.contains("SSE JSON parse error"), "unexpected context: {context}");
            }
            other => panic!("expected Decode error, got: {other:?}"),
        }
    }

    // event: lines are ignored; only data: lines matter.
    #[tokio::test]
    async fn event_lines_are_ignored() {
        let json = serde_json::to_string(&json!({
            "candidates": [
                {
                    "content": { "role": "model", "parts": [{ "text": "ok" }] }
                }
            ]
        }))
        .unwrap();
        let sse = format!("event: candidate\ndata: {json}\n\n");
        let stream = parse_sse_stream(bytes_stream(vec![sse.into_bytes()]));
        let events: Vec<_> = stream.collect().await;

        assert_eq!(events.len(), 1);
        let ev = events[0].as_ref().unwrap();
        assert_eq!(ev.parts[0]["text"], "ok");
    }

    // Stream split across TCP frame boundaries still parses correctly.
    #[tokio::test]
    async fn stream_split_across_chunk_boundary() {
        let data = text_event_sse("split");
        let mid = data.len() / 2;
        let chunk1 = data[..mid].to_vec();
        let chunk2 = data[mid..].to_vec();

        let stream = parse_sse_stream(bytes_stream(vec![chunk1, chunk2]));
        let events: Vec<_> = stream.collect().await;

        assert_eq!(events.len(), 1);
        let ev = events[0].as_ref().unwrap();
        assert_eq!(ev.parts[0]["text"], "split");
    }

    // functionCall parts are preserved verbatim in the parts vec.
    #[tokio::test]
    async fn function_call_parts_preserved() {
        let args = json!({ "file_path": "/tmp/test.txt" });
        let data = tool_call_event_sse("Read", args.clone());
        let stream = parse_sse_stream(bytes_stream(vec![data]));
        let events: Vec<_> = stream.collect().await;

        assert_eq!(events.len(), 1);
        let ev = events[0].as_ref().unwrap();
        assert_eq!(ev.parts.len(), 1);
        let fc = &ev.parts[0]["functionCall"];
        assert_eq!(fc["name"], "Read");
        assert_eq!(fc["args"]["file_path"], "/tmp/test.txt");
    }

    // EOF with no remaining data yields no extra events.
    #[tokio::test]
    async fn eof_terminates_cleanly() {
        let stream = parse_sse_stream(bytes_stream(vec![]));
        let events: Vec<_> = stream.collect().await;
        assert!(events.is_empty(), "empty stream should yield no events");
    }
}
