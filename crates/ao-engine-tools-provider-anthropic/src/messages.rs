//! [`MessageNormalizer`] implementation for the Anthropic wire format.
//!
//! `AnthropicNormalizer` converts between canonical [`Message`] values and the
//! Anthropic Messages API JSON wire shapes. Both `to_provider` and `from_provider`
//! are fully implemented here.
//!
//! ## Dual ToolResult shapes
//!
//! Two distinct encodings exist for tool results in the wire format:
//!
//! - `Message::ToolResult` (transcript-level) encodes as a `user`-role message
//!   with a single `tool_result` block whose inner `content` is a JSON **array**
//!   of content blocks.
//! - `ContentBlock::ToolResult` (inline-level) encodes as a `tool_result` block
//!   whose inner `content` is a JSON **string**.
//!
//! `from_provider` distinguishes the two by inspecting the type of the inner
//! `content` field: array → `Message::ToolResult`; string → `ContentBlock::ToolResult`
//! inside `Message::User`.
//!
//! ## Thinking blocks
//!
//! `ContentBlock::Thinking` round-trips as a `{ "type": "thinking", "thinking":
//! "<text>", "signature": "<sig>" }` block. Both `thinking` and `signature` are
//! optional on the wire to mirror the canonical type — the `display = "omitted"`
//! case sends only the signature, and any future `display` value that flips the
//! shape stays representable without a schema change. The signature must be
//! preserved verbatim across multi-turn replay; Anthropic rejects a follow-up
//! turn whose transcript echoes a `thinking` block without the original
//! signature when the prior turn also emitted `tool_use`.
//!
//! `ContentBlock::RedactedThinking` round-trips as a `{ "type":
//! "redacted_thinking", "data": "<blob>" }` block. The provider withholds the
//! plaintext reasoning and returns this opaque payload instead; it carries no
//! `thinking` or `signature` field. The same continuity rule applies — a
//! redacted block dropped from a tool-using transcript is rejected — so the
//! `data` blob is echoed back byte-for-byte.

use ao_engine_tools_runner::message::{ContentBlock, Message, MessageNormalizer, NormalizerError};
use serde_json::{json, Value};

/// Normalizer that converts between canonical [`Message`]s and the Anthropic
/// Messages API wire format.
pub struct AnthropicNormalizer;

impl MessageNormalizer for AnthropicNormalizer {
    /// Encode canonical messages into the Anthropic wire shape.
    ///
    /// Returns a `Value::Array` of Anthropic-format message objects. Filters out
    /// `Message::System` entries (Anthropic carries system at the request top level,
    /// not inside the messages array); returns `NormalizerError::Unrepresentable` if
    /// a `System` message appears in a non-leading position.
    fn to_provider(&self, messages: &[Message]) -> Result<Value, NormalizerError> {
        let mut wire = Vec::with_capacity(messages.len());
        for msg in messages {
            match msg {
                Message::System { .. } => {
                    return Err(NormalizerError::Unrepresentable(
                        "Message::System must be extracted to the request top-level \
                         system field before calling to_provider"
                            .into(),
                    ));
                }
                Message::User { content } => {
                    let blocks = map_blocks(content)?;
                    wire.push(json!({ "role": "user", "content": blocks }));
                }
                Message::Assistant { content } => {
                    let blocks = map_blocks(content)?;
                    wire.push(json!({ "role": "assistant", "content": blocks }));
                }
                Message::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let inner = map_blocks(content)?;
                    wire.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": inner,
                            "is_error": is_error
                        }]
                    }));
                }
            }
        }
        Ok(Value::Array(wire))
    }

    /// Decode Anthropic wire-format message objects back into canonical messages.
    ///
    /// Expects a `Value::Array` of the same shape that `to_provider` produces.
    /// Used by integration tests to verify round-trip fidelity; production
    /// streaming bypasses this — `response.rs` translates SSE events directly.
    ///
    /// ## ToolResult disambiguation
    ///
    /// A `user`-role message with exactly one `tool_result` block whose inner
    /// `content` is a JSON array is decoded as `Message::ToolResult`. All other
    /// `user`-role messages (including those whose `tool_result` blocks carry a
    /// string `content`) are decoded as `Message::User`.
    fn from_provider(&self, value: Value) -> Result<Vec<Message>, NormalizerError> {
        let arr = value.as_array().ok_or_else(|| {
            NormalizerError::Shape(
                "expected a JSON array of Anthropic message objects".into(),
            )
        })?;

        let mut messages = Vec::with_capacity(arr.len());
        for obj in arr {
            let role = obj
                .get("role")
                .and_then(Value::as_str)
                .ok_or_else(|| NormalizerError::Shape("message object missing 'role' field".into()))?;

            match role {
                "user" => {
                    let content_arr = obj
                        .get("content")
                        .and_then(Value::as_array)
                        .ok_or_else(|| {
                            NormalizerError::Shape(
                                "user message missing 'content' array".into(),
                            )
                        })?;

                    // Detect Message::ToolResult encoding:
                    // exactly one block, type == "tool_result", inner content is an array.
                    if let Some(decoded) = try_decode_tool_result_message(content_arr)? {
                        messages.push(decoded);
                    } else {
                        let blocks = parse_content_blocks(content_arr)?;
                        messages.push(Message::User { content: blocks });
                    }
                }
                "assistant" => {
                    let content_arr = obj
                        .get("content")
                        .and_then(Value::as_array)
                        .ok_or_else(|| {
                            NormalizerError::Shape(
                                "assistant message missing 'content' array".into(),
                            )
                        })?;
                    let blocks = parse_content_blocks(content_arr)?;
                    messages.push(Message::Assistant { content: blocks });
                }
                other => {
                    return Err(NormalizerError::Shape(format!(
                        "unknown message role: {other}"
                    )));
                }
            }
        }

        Ok(messages)
    }
}

/// Returns `Some(Message::ToolResult)` when `content_arr` is a single-item
/// array containing a `tool_result` block whose inner `content` is an array
/// (the `Message::ToolResult` encoding). Returns `None` otherwise so the
/// caller falls back to decoding as `Message::User`.
fn try_decode_tool_result_message(
    content_arr: &[Value],
) -> Result<Option<Message>, NormalizerError> {
    if content_arr.len() != 1 {
        return Ok(None);
    }
    let block = &content_arr[0];
    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
        return Ok(None);
    }
    let inner_content = match block.get("content") {
        Some(v) if v.is_array() => v.as_array().unwrap(),
        _ => return Ok(None),
    };

    let tool_use_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            NormalizerError::Shape("tool_result block missing 'tool_use_id'".into())
        })?
        .to_string();
    let is_error = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let content = parse_content_blocks(inner_content)?;

    Ok(Some(Message::ToolResult {
        tool_use_id,
        content,
        is_error,
    }))
}

fn parse_content_blocks(blocks: &[Value]) -> Result<Vec<ContentBlock>, NormalizerError> {
    blocks.iter().map(parse_content_block).collect()
}

fn parse_content_block(block: &Value) -> Result<ContentBlock, NormalizerError> {
    let typ = block
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| NormalizerError::Shape("content block missing 'type' field".into()))?;

    match typ {
        "text" => {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| NormalizerError::Shape("text block missing 'text' field".into()))?
                .to_string();
            Ok(ContentBlock::Text { text })
        }
        "tool_use" => {
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    NormalizerError::Shape("tool_use block missing 'id' field".into())
                })?
                .to_string();
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    NormalizerError::Shape("tool_use block missing 'name' field".into())
                })?
                .to_string();
            let input = block
                .get("input")
                .cloned()
                .ok_or_else(|| {
                    NormalizerError::Shape("tool_use block missing 'input' field".into())
                })?;
            Ok(ContentBlock::ToolUse { id, name, input })
        }
        "tool_result" => {
            // Inline ContentBlock::ToolResult shape — content is a string.
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    NormalizerError::Shape(
                        "tool_result block missing 'tool_use_id' field".into(),
                    )
                })?
                .to_string();
            let content = block
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    NormalizerError::Shape(
                        "inline tool_result block 'content' must be a string".into(),
                    )
                })?
                .to_string();
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            })
        }
        "thinking" => {
            // Reasoning block. Both fields are optional on the canonical
            // type, so the wire shape can omit either without erroring.
            // Empty strings collapse to `None` to keep the round-trip
            // canonical for the `display = "omitted"` case (signature-only).
            let text = block
                .get("thinking")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from);
            let signature = block
                .get("signature")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from);
            Ok(ContentBlock::Thinking { text, signature })
        }
        "redacted_thinking" => {
            // Opaque encrypted reasoning payload. Round-tripped verbatim;
            // the `data` field is the whole block.
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    NormalizerError::Shape(
                        "redacted_thinking block missing 'data' field".into(),
                    )
                })?
                .to_string();
            Ok(ContentBlock::RedactedThinking { data })
        }
        "image" => {
            let (media_type, data) = parse_base64_source(block, "image")?;
            Ok(ContentBlock::Image { media_type, data })
        }
        "document" => {
            let (media_type, data) = parse_base64_source(block, "document")?;
            let title = block
                .get("title")
                .and_then(Value::as_str)
                .map(String::from);
            Ok(ContentBlock::Document {
                media_type,
                data,
                title,
            })
        }
        other => Err(NormalizerError::Shape(format!(
            "unknown content block type: {other}"
        ))),
    }
}

fn map_blocks(blocks: &[ContentBlock]) -> Result<Vec<Value>, NormalizerError> {
    blocks.iter().map(map_block).collect()
}

fn map_block(block: &ContentBlock) -> Result<Value, NormalizerError> {
    match block {
        ContentBlock::Text { text } => Ok(json!({ "type": "text", "text": text })),
        ContentBlock::ToolUse { id, name, input } => Ok(json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input
        })),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => Ok(json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error
        })),
        ContentBlock::Thinking { text, signature } => {
            // Anthropic accepts a `thinking` block on the *replay* path —
            // i.e. we echo back what the model previously emitted so the
            // multi-turn continuity check passes. Always serialise both
            // fields when present; downstream replay rejects the turn if
            // the signature is missing on a tool-using transcript.
            let mut obj = serde_json::Map::new();
            obj.insert("type".into(), json!("thinking"));
            if let Some(t) = text {
                obj.insert("thinking".into(), json!(t));
            }
            if let Some(sig) = signature {
                obj.insert("signature".into(), json!(sig));
            }
            Ok(Value::Object(obj))
        }
        ContentBlock::RedactedThinking { data } => Ok(json!({
            "type": "redacted_thinking",
            "data": data,
        })),
        ContentBlock::Image { media_type, data } => Ok(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        })),
        ContentBlock::Document {
            media_type,
            data,
            title,
        } => {
            // The optional display title is only emitted when present so the
            // wire object stays minimal for callers that don't supply one.
            let mut obj = serde_json::Map::new();
            obj.insert("type".into(), json!("document"));
            obj.insert(
                "source".into(),
                json!({
                    "type": "base64",
                    "media_type": media_type,
                    "data": data,
                }),
            );
            if let Some(t) = title {
                obj.insert("title".into(), json!(t));
            }
            Ok(Value::Object(obj))
        }
    }
}

/// Extract the base64 `media_type`/`data` pair from a wire block's `source`
/// object. Shared by the `image` and `document` decode arms, which carry the
/// same `{ "type": "base64", "media_type": ..., "data": ... }` source shape.
fn parse_base64_source(
    block: &Value,
    block_kind: &str,
) -> Result<(String, String), NormalizerError> {
    let source = block.get("source").ok_or_else(|| {
        NormalizerError::Shape(format!("{block_kind} block missing 'source' field"))
    })?;
    let media_type = source
        .get("media_type")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            NormalizerError::Shape(format!("{block_kind} source missing 'media_type' field"))
        })?
        .to_string();
    let data = source
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            NormalizerError::Shape(format!("{block_kind} source missing 'data' field"))
        })?
        .to_string();
    Ok((media_type, data))
}
