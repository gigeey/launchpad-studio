//! SSE response parser — frames the Anthropic streaming byte stream into typed
//! [`AnthropicEvent`] values.
//!
//! The parser frames on `\n\n` boundaries (SSE blank-line separator), strips
//! `data: ` prefixes, and dispatches on the JSON `type` field. A single event
//! may not exceed 16 KiB; if the buffer grows past that limit without a
//! boundary, the stream closes with [`ProviderError::Transport`].
//!
//! The translator state machine (`complete()`) sits one layer above this
//! module and maps typed events onto `CompletionEvent` values; this module is
//! responsible only for framing and JSON dispatch.

use ao_engine_tools_runner::provider::ProviderError;
use futures_util::{Stream, StreamExt};

/// The six SSE event families emitted by the Anthropic Messages streaming API.
#[derive(Debug, PartialEq)]
pub enum AnthropicEvent {
    /// Carries the per-turn input usage from `message_start`.
    MessageStart { usage: serde_json::Value },
    /// Opens a content block at `index` with the given kind.
    ContentBlockStart {
        index: u32,
        content_block: ContentBlockKind,
    },
    /// Appends a delta to the content block at `index`.
    ContentBlockDelta { index: u32, delta: DeltaKind },
    /// Closes the content block at `index`.
    ContentBlockStop { index: u32 },
    /// Carries the stop reason and final output usage.
    MessageDelta {
        stop_reason: Option<String>,
        usage: Option<serde_json::Value>,
    },
    /// Signals that the message is complete.
    MessageStop,
}

/// The content-block kinds that can appear in a `content_block_start` event.
///
/// `Thinking` opens an extended-thinking reasoning block. The complete()
/// loop tracks the open block by `index` so the matching `content_block_stop`
/// can emit a `ThinkingEnd` event with the elapsed duration. When the model
/// runs with `display = "omitted"` the open will arrive but no
/// [`DeltaKind::ThinkingDelta`] follows — only a [`DeltaKind::SignatureDelta`]
/// before the close — which is itself a meaningful "thinking happened" signal.
#[derive(Debug, PartialEq)]
pub enum ContentBlockKind {
    Text,
    ToolUse { id: String, name: String },
    Thinking,
    /// A safety-redacted reasoning block. Unlike `Thinking`, the entire
    /// payload arrives inline on the `content_block_start` event as an
    /// opaque `data` string — no `thinking_delta`/`signature_delta` chunks
    /// follow, and the matching `content_block_stop` closes it immediately.
    /// The `data` blob is captured here so the consumer loop can replay it
    /// verbatim on the next turn.
    RedactedThinking { data: String },
}

/// The delta kinds that can appear in a `content_block_delta` event.
///
/// `ThinkingDelta` carries a chunk of reasoning text from an open thinking
/// block. `SignatureDelta` carries the cryptographic attestation Anthropic
/// emits at the end of a thinking block. The signature has no UI surface,
/// but the consumer loop captures it onto the in-flight thinking block so
/// the assistant turn message echoed on the next iteration carries it
/// verbatim — Anthropic rejects a follow-up turn whose transcript echoes a
/// `thinking` block without the original signature when the prior turn
/// also emitted `tool_use`.
#[derive(Debug, PartialEq)]
pub enum DeltaKind {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { text: String },
    SignatureDelta { signature: String },
}

/// Adapt a chunked byte stream into a stream of typed [`AnthropicEvent`] values.
///
/// Frames on `\n\n` (or `\r\n\r\n`) SSE boundaries, strips `data: ` prefixes,
/// and dispatches on the JSON `type` field. `ping` events are silently skipped.
/// Any other unknown event type, malformed JSON, or stream error yields
/// `Err(ProviderError::Transport(...))` and closes the stream.
///
/// `byte_stream` must implement [`Unpin`]; callers with a non-`Unpin` stream
/// should wrap it with `Box::pin(stream)` before calling.
pub fn parse_sse_stream<S, B, E>(
    byte_stream: S,
) -> impl Stream<Item = Result<AnthropicEvent, ProviderError>>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    struct SseState<S> {
        inner: S,
        buf: Vec<u8>,
        done: bool,
    }

    futures_util::stream::unfold(
        SseState {
            inner: byte_stream,
            buf: Vec::new(),
            done: false,
        },
        |mut state| async move {
            loop {
                if state.done {
                    return None;
                }

                // Try to parse a complete event from the buffer.
                if let Some(drain_end) = find_sse_boundary(&state.buf) {
                    let event_block = state.buf[..drain_end].to_vec();
                    state.buf.drain(..drain_end);

                    match parse_event_block(&event_block) {
                        Ok(Some(event)) => return Some((Ok(event), state)),
                        Ok(None) => continue, // ping or comment — keep reading
                        Err(e) => {
                            state.done = true;
                            return Some((Err(e), state));
                        }
                    }
                }

                // Guard against a single event that exceeds 16 KiB.
                if state.buf.len() > 16 * 1024 {
                    state.done = true;
                    return Some((
                        Err(ProviderError::Transport(
                            "SSE parse error: buffer exceeded 16 KiB without event boundary"
                                .into(),
                        )),
                        state,
                    ));
                }

                // Pull more bytes from the underlying stream.
                match state.inner.next().await {
                    Some(Ok(chunk)) => {
                        state.buf.extend_from_slice(chunk.as_ref());
                    }
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

/// Return the byte offset just past the first `\n\n` or `\r\n\r\n` boundary
/// in `buf`, or `None` if no complete boundary is present yet.
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

/// Parse a single SSE event block (bytes up to and including the `\n\n`
/// boundary) into a typed [`AnthropicEvent`]. Returns `Ok(None)` for `ping`
/// events and for blocks with no `data:` line (e.g. comment-only blocks).
fn parse_event_block(block: &[u8]) -> Result<Option<AnthropicEvent>, ProviderError> {
    let text = std::str::from_utf8(block)
        .map_err(|e| ProviderError::Transport(format!("SSE parse error: invalid UTF-8: {e}")))?;

    let mut data_json: Option<&str> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data: ") {
            data_json = Some(rest);
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_json = Some(rest);
        }
    }

    let json_str = match data_json {
        Some(s) => s,
        None => return Ok(None),
    };

    let value: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| ProviderError::Transport(format!("SSE parse error: {e}")))?;

    let event_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ProviderError::Transport("SSE parse error: missing 'type' field".into()))?;

    let event = match event_type {
        "ping" => return Ok(None),

        "message_start" => {
            let usage = value
                .get("message")
                .and_then(|m| m.get("usage"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            AnthropicEvent::MessageStart { usage }
        }

        "content_block_start" => {
            let index = value
                .get("index")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| {
                    ProviderError::Transport(
                        "SSE parse error: content_block_start missing index".into(),
                    )
                })? as u32;
            let cb = value.get("content_block").ok_or_else(|| {
                ProviderError::Transport(
                    "SSE parse error: content_block_start missing content_block".into(),
                )
            })?;
            let kind = parse_content_block_kind(cb)?;
            AnthropicEvent::ContentBlockStart {
                index,
                content_block: kind,
            }
        }

        "content_block_delta" => {
            let index = value
                .get("index")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| {
                    ProviderError::Transport(
                        "SSE parse error: content_block_delta missing index".into(),
                    )
                })? as u32;
            let delta = value.get("delta").ok_or_else(|| {
                ProviderError::Transport(
                    "SSE parse error: content_block_delta missing delta".into(),
                )
            })?;
            let delta_kind = parse_delta_kind(delta)?;
            AnthropicEvent::ContentBlockDelta {
                index,
                delta: delta_kind,
            }
        }

        "content_block_stop" => {
            let index = value
                .get("index")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| {
                    ProviderError::Transport(
                        "SSE parse error: content_block_stop missing index".into(),
                    )
                })? as u32;
            AnthropicEvent::ContentBlockStop { index }
        }

        "message_delta" => {
            let stop_reason = value
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            let usage = value.get("usage").cloned();
            AnthropicEvent::MessageDelta { stop_reason, usage }
        }

        "message_stop" => AnthropicEvent::MessageStop,

        unknown => {
            return Err(ProviderError::Transport(format!(
                "SSE parse error: unknown event type: {unknown}"
            )))
        }
    };

    Ok(Some(event))
}

fn parse_content_block_kind(cb: &serde_json::Value) -> Result<ContentBlockKind, ProviderError> {
    match cb.get("type").and_then(|v| v.as_str()) {
        Some("text") => Ok(ContentBlockKind::Text),
        Some("tool_use") => {
            let id = cb
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ProviderError::Transport("SSE parse error: tool_use block missing id".into())
                })?
                .to_string();
            let name = cb
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ProviderError::Transport(
                        "SSE parse error: tool_use block missing name".into(),
                    )
                })?
                .to_string();
            Ok(ContentBlockKind::ToolUse { id, name })
        }
        Some("thinking") => Ok(ContentBlockKind::Thinking),
        Some("redacted_thinking") => {
            // The encrypted payload is delivered whole on the start event;
            // capture it now since no deltas will follow.
            let data = cb
                .get("data")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ProviderError::Transport(
                        "SSE parse error: redacted_thinking block missing data".into(),
                    )
                })?
                .to_string();
            Ok(ContentBlockKind::RedactedThinking { data })
        }
        Some(t) => Err(ProviderError::Transport(format!(
            "SSE parse error: unknown content_block type: {t}"
        ))),
        None => Err(ProviderError::Transport(
            "SSE parse error: content_block missing type field".into(),
        )),
    }
}

fn parse_delta_kind(delta: &serde_json::Value) -> Result<DeltaKind, ProviderError> {
    match delta.get("type").and_then(|v| v.as_str()) {
        Some("text_delta") => {
            let text = delta
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(DeltaKind::TextDelta { text })
        }
        Some("input_json_delta") => {
            let partial_json = delta
                .get("partial_json")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(DeltaKind::InputJsonDelta { partial_json })
        }
        Some("thinking_delta") => {
            // Reasoning text chunk. Anthropic chunks these at multi-character
            // boundaries (5–100+ chars per chunk on typical reasoning prompts),
            // never character-by-character. Empty string is acceptable.
            let text = delta
                .get("thinking")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(DeltaKind::ThinkingDelta { text })
        }
        Some("signature_delta") => {
            // Cryptographic attestation that closes a thinking block. The
            // signature has no UI surface but the consumer loop captures it
            // onto the in-flight thinking block so the assistant turn echoed
            // on the next iteration carries it verbatim. Empty signatures
            // are still valid (the field defaults to "" rather than absent
            // on some providers' wire shapes).
            let signature = delta
                .get("signature")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(DeltaKind::SignatureDelta { signature })
        }
        Some(t) => Err(ProviderError::Transport(format!(
            "SSE parse error: unknown delta type: {t}"
        ))),
        None => Err(ProviderError::Transport(
            "SSE parse error: delta missing type field".into(),
        )),
    }
}
