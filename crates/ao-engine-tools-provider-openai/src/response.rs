//! SSE response parser — frames the OpenAI Chat Completions streaming response
//! into typed [`OpenAIEvent`] values.
//!
//! Frames on `\n\n` (or `\r\n\r\n`) SSE boundaries, strips `data: ` prefixes,
//! special-cases `data: [DONE]`, and dispatches on the OpenAI chunk JSON shape.
//! The pending-byte buffer is capped at 64 KiB; exceeding this limit closes the
//! stream with [`ProviderError::Transport`].
//!
//! The translator state machine (`complete()` in lib.rs) sits one layer above
//! and maps typed events onto `CompletionEvent` values; this module handles
//! only framing and JSON dispatch.

use ao_engine_tools_runner::provider::ProviderError;
use futures_util::{Stream, StreamExt};
use std::collections::VecDeque;

/// Typed events emitted by OpenAI's Chat Completions SSE stream.
#[derive(Debug, PartialEq)]
pub enum OpenAIEvent {
    /// A text delta from `choices[0].delta.content`.
    TextDelta { content: String },
    /// A tool-call delta from one entry in `choices[0].delta.tool_calls`.
    ///
    /// The first delta for a given index carries `id` and `name`; subsequent
    /// deltas carry only `arguments_chunk`. One [`OpenAIEvent`] is emitted per
    /// entry in the `tool_calls` array, so parallel tool calls at distinct
    /// indices each get their own sequence of events.
    ToolCallDelta {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments_chunk: Option<String>,
    },
    /// The non-null `finish_reason` from `choices[0].finish_reason`.
    FinishReason { reason: String },
    /// The final usage block emitted when `stream_options.include_usage: true`.
    ///
    /// Arrives in a separate chunk with `choices: []` after the finish chunk.
    Usage { value: serde_json::Value },
    /// The `[DONE]` sentinel; the stream yields no further items after this.
    Done,
}

/// Adapt a chunked byte stream into a stream of typed [`OpenAIEvent`] values.
///
/// Frames on `\n\n` SSE boundaries, strips `data: ` prefixes, and dispatches
/// on the OpenAI Chat Completions JSON shape. `data: [DONE]` emits
/// [`OpenAIEvent::Done`] and closes the stream. One SSE frame may yield
/// multiple events (e.g. a `tool_calls` array with several indices). Malformed
/// JSON or a buffer exceeding 64 KiB without a boundary closes the stream with
/// [`ProviderError::Transport`].
pub fn parse_sse_stream<S, B, E>(
    byte_stream: S,
) -> impl Stream<Item = Result<OpenAIEvent, ProviderError>>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    struct SseState<S> {
        inner: S,
        buf: Vec<u8>,
        pending: VecDeque<Result<OpenAIEvent, ProviderError>>,
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
                // Drain any events buffered from the last parsed SSE frame.
                if let Some(event) = state.pending.pop_front() {
                    return Some((event, state));
                }

                if state.done {
                    return None;
                }

                // Parse a complete SSE frame if the buffer contains one.
                if let Some(drain_end) = find_sse_boundary(&state.buf) {
                    let event_block = state.buf[..drain_end].to_vec();
                    state.buf.drain(..drain_end);

                    match extract_events(&event_block) {
                        Ok(events) if events.is_empty() => continue,
                        Ok(events) => {
                            let is_terminal =
                                events.iter().any(|e| matches!(e, OpenAIEvent::Done));
                            state.pending.extend(events.into_iter().map(Ok));
                            if is_terminal {
                                state.done = true;
                            }
                            continue;
                        }
                        Err(e) => {
                            state.done = true;
                            return Some((Err(e), state));
                        }
                    }
                }

                // Guard: refuse to buffer more than 64 KiB without a boundary.
                if state.buf.len() > 64 * 1024 {
                    state.done = true;
                    return Some((
                        Err(ProviderError::Transport(
                            "SSE parse error: buffer exceeded 64 KiB without event boundary"
                                .into(),
                        )),
                        state,
                    ));
                }

                // Pull the next byte chunk from the underlying stream.
                match state.inner.next().await {
                    Some(Ok(chunk)) => state.buf.extend_from_slice(chunk.as_ref()),
                    Some(Err(e)) => {
                        state.done = true;
                        return Some((
                            Err(ProviderError::Transport(format!("SSE stream error: {e}"))),
                            state,
                        ));
                    }
                    None => {
                        return None;
                    }
                }
            }
        },
    )
}

/// Return the byte offset just past the first `\n\n` or `\r\n\r\n` boundary,
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

/// Parse one SSE event block into zero or more [`OpenAIEvent`] values.
///
/// Returns `Ok(vec![])` for blocks with no `data:` line (comment-only blocks).
/// Returns `Err` on malformed JSON; the caller closes the stream on error.
fn extract_events(block: &[u8]) -> Result<Vec<OpenAIEvent>, ProviderError> {
    let text = std::str::from_utf8(block)
        .map_err(|e| ProviderError::Transport(format!("SSE parse error: invalid UTF-8: {e}")))?;

    let mut data_line: Option<&str> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data: ") {
            data_line = Some(rest);
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_line = Some(rest);
        }
    }

    let json_str = match data_line {
        Some(s) => s,
        None => return Ok(vec![]),
    };

    if json_str == "[DONE]" {
        return Ok(vec![OpenAIEvent::Done]);
    }

    let value: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| ProviderError::Transport(format!("SSE parse error: {e}")))?;

    parse_chunk_events(&value)
}

/// Extract all [`OpenAIEvent`] values from a parsed OpenAI SSE chunk JSON.
fn parse_chunk_events(value: &serde_json::Value) -> Result<Vec<OpenAIEvent>, ProviderError> {
    let mut events = Vec::new();

    if let Some(choices) = value.get("choices").and_then(|v| v.as_array()) {
        if let Some(choice) = choices.first() {
            let delta = choice.get("delta");

            // Text delta: choices[0].delta.content (skip empty strings).
            if let Some(content) = delta
                .and_then(|d| d.get("content"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                events.push(OpenAIEvent::TextDelta {
                    content: content.to_string(),
                });
            }

            // Tool-call deltas: one event per entry in choices[0].delta.tool_calls.
            if let Some(tool_calls) = delta
                .and_then(|d| d.get("tool_calls"))
                .and_then(|v| v.as_array())
            {
                for tc in tool_calls {
                    let index = tc
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| {
                            ProviderError::Transport(
                                "SSE parse error: tool_call delta missing index".into(),
                            )
                        })? as u32;

                    let id = tc.get("id").and_then(|v| v.as_str()).map(String::from);
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let arguments_chunk = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    events.push(OpenAIEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments_chunk,
                    });
                }
            }

            // Finish reason: choices[0].finish_reason (non-null, non-empty).
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                events.push(OpenAIEvent::FinishReason {
                    reason: reason.to_string(),
                });
            }
        }
    }

    // Usage block: present in the final chunk when stream_options.include_usage=true.
    // This chunk typically has choices: [].
    if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
        events.push(OpenAIEvent::Usage {
            value: usage.clone(),
        });
    }

    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn bytes_stream(
        chunks: Vec<Vec<u8>>,
    ) -> impl Stream<Item = Result<Vec<u8>, std::convert::Infallible>> {
        futures_util::stream::iter(chunks.into_iter().map(Ok))
    }

    #[tokio::test]
    async fn text_only_fixture_yields_correct_event_sequence() {
        let fixture: Vec<u8> = include_bytes!("../tests/fixtures/sse_text_only.txt").to_vec();
        let stream = parse_sse_stream(bytes_stream(vec![fixture]));
        let events: Vec<_> = stream.collect().await;

        // All events should be Ok.
        for e in &events {
            assert!(e.is_ok(), "unexpected error: {e:?}");
        }

        // TextDelta events carry the expected content fragments.
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| {
                if let Ok(OpenAIEvent::TextDelta { content }) = e {
                    Some(content.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert!(texts.contains(&"Hello"), "missing 'Hello' delta");
        assert!(texts.contains(&"世界"), "missing '世界' delta (Unicode)");

        // FinishReason("stop") appears before Usage and Done.
        let finish_pos = events
            .iter()
            .position(|e| matches!(e, Ok(OpenAIEvent::FinishReason { reason }) if reason == "stop"))
            .expect("FinishReason(stop) not found");
        let usage_pos = events
            .iter()
            .position(|e| matches!(e, Ok(OpenAIEvent::Usage { .. })))
            .expect("Usage not found");
        let done_pos = events
            .iter()
            .position(|e| matches!(e, Ok(OpenAIEvent::Done)))
            .expect("Done not found");

        assert!(finish_pos < usage_pos, "FinishReason must precede Usage");
        assert!(usage_pos < done_pos, "Usage must precede Done");
        assert_eq!(done_pos, events.len() - 1, "Done must be last");
    }

    #[tokio::test]
    async fn tool_call_fixture_demonstrates_delta_accumulation() {
        let fixture: Vec<u8> = include_bytes!("../tests/fixtures/sse_tool_call.txt").to_vec();
        let stream = parse_sse_stream(bytes_stream(vec![fixture]));
        let events: Vec<_> = stream.collect().await;

        for e in &events {
            assert!(e.is_ok(), "unexpected error: {e:?}");
        }

        // First ToolCallDelta for index 0 carries id and name.
        let first_tc = events.iter().find(|e| {
            matches!(e, Ok(OpenAIEvent::ToolCallDelta { index: 0, id: Some(_), .. }))
        });
        assert!(first_tc.is_some(), "expected first ToolCallDelta with id");
        if let Some(Ok(OpenAIEvent::ToolCallDelta { id, name, .. })) = first_tc {
            assert_eq!(id.as_deref(), Some("call_abc123"));
            assert_eq!(name.as_deref(), Some("read_file"));
        }

        // Multiple ToolCallDelta events for index 0 (demonstrating accumulation).
        let tc_count = events
            .iter()
            .filter(|e| matches!(e, Ok(OpenAIEvent::ToolCallDelta { index: 0, .. })))
            .count();
        assert!(tc_count >= 2, "expected multiple ToolCallDelta events for index 0");

        // FinishReason is "tool_calls".
        assert!(
            events.iter().any(|e| matches!(e, Ok(OpenAIEvent::FinishReason { reason }) if reason == "tool_calls")),
            "expected FinishReason(tool_calls)"
        );

        // Usage precedes Done.
        let usage_pos = events
            .iter()
            .position(|e| matches!(e, Ok(OpenAIEvent::Usage { .. })))
            .expect("Usage not found");
        let done_pos = events
            .iter()
            .position(|e| matches!(e, Ok(OpenAIEvent::Done)))
            .expect("Done not found");
        assert!(usage_pos < done_pos, "Usage must precede Done");
        assert_eq!(done_pos, events.len() - 1, "Done must be last");
    }

    #[tokio::test]
    async fn malformed_data_line_closes_stream_with_error() {
        let malformed = b"data: {not valid json}\n\n".to_vec();
        let stream = parse_sse_stream(bytes_stream(vec![malformed]));
        let events: Vec<_> = stream.collect().await;

        assert_eq!(events.len(), 1, "expected exactly one error event");
        assert!(
            matches!(&events[0], Err(ProviderError::Transport(msg)) if msg.contains("SSE parse error")),
            "expected SSE parse error, got: {:?}",
            events[0]
        );
    }

    #[tokio::test]
    async fn done_sentinel_closes_stream() {
        let data = b"data: [DONE]\n\n".to_vec();
        let stream = parse_sse_stream(bytes_stream(vec![data]));
        let events: Vec<_> = stream.collect().await;

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Ok(OpenAIEvent::Done)));
    }

    #[tokio::test]
    async fn unicode_split_across_tcp_frame_boundary() {
        // "世界" in UTF-8: 世=0xE4 0xB8 0x96, 界=0xE7 0x95 0x8C.
        // Build the full SSE byte stream then split mid-codepoint to verify
        // the parser buffers correctly and does not corrupt the string.
        let event = format!(
            "data: {{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\
             \"model\":\"gpt-4o\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"世界\"}},\
             \"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n"
        );
        let bytes: Vec<u8> = event.into_bytes();

        // Find the first byte of 世 (0xE4) and cut after it, mid-codepoint.
        let cut = bytes.iter().position(|&b| b == 0xE4).unwrap() + 1;
        let chunk1 = bytes[..cut].to_vec();
        let chunk2 = bytes[cut..].to_vec();

        let stream = parse_sse_stream(bytes_stream(vec![chunk1, chunk2]));
        let events: Vec<_> = stream.collect().await;

        let text_event = events
            .iter()
            .find(|e| matches!(e, Ok(OpenAIEvent::TextDelta { .. })))
            .expect("TextDelta not found");
        if let Ok(OpenAIEvent::TextDelta { content }) = text_event {
            assert_eq!(content, "世界", "Unicode content corrupted by frame split");
        }
    }

    #[tokio::test]
    async fn buffer_overflow_closes_stream_with_error() {
        // A data line that never completes (no \n\n boundary) exceeding 64 KiB.
        let big_chunk = vec![b'x'; 65 * 1024];
        let stream = parse_sse_stream(bytes_stream(vec![big_chunk]));
        let events: Vec<_> = stream.collect().await;

        assert_eq!(events.len(), 1, "expected exactly one error event");
        assert!(
            matches!(&events[0], Err(ProviderError::Transport(msg)) if msg.contains("64 KiB")),
            "expected 64 KiB error, got: {:?}",
            events[0]
        );
    }
}
