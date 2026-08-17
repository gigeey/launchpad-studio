//! [`MessageNormalizer`] implementation for the OpenAI Chat Completions wire format.
//!
//! `OpenAINormalizer` converts between canonical [`Message`] values and the OpenAI
//! Chat Completions API JSON wire shapes. Both `to_provider` and `from_provider`
//! are fully implemented here.
//!
//! ## Arguments as strings
//!
//! OpenAI's tool_call `function.arguments` field is a JSON-encoded **string**,
//! not a nested object. `to_provider` stringifies the input value via
//! `serde_json::to_string`; `from_provider` parses it back with
//! `serde_json::from_str`. This is the most common first-time OpenAI integration
//! pitfall — the normalizer handles it transparently so callers never see raw
//! string arguments.
//!
//! ## Mixed User content
//!
//! When a `Message::User` contains both `ContentBlock::ToolResult` blocks and
//! `ContentBlock::Text` blocks, they are split into multiple OpenAI messages:
//! any `tool`-role messages are emitted **first** in the order their `ToolResult`
//! blocks appear, then a single `user`-role message for the concatenated text.
//! This ordering is required by OpenAI's API: tool result messages must appear
//! before the next user text in the array, as they correspond to the prior
//! assistant turn's `tool_calls`.
//!
//! ## is_error encoding
//!
//! OpenAI tool messages have no native error flag. `to_provider` encodes
//! `is_error: true` by prefixing the content string with `"Error: "`.
//! `from_provider` does not attempt to strip this prefix — the encoded string is
//! preserved as-is in the decoded content, with `is_error` always set to `false`.
//! This is a known lossy round-trip for the `is_error` field; the content itself
//! is preserved.

use ao_engine_tools_runner::message::{ContentBlock, Message, MessageNormalizer, NormalizerError};
use serde_json::{json, Value};

/// Render a media content block as a short textual placeholder.
///
/// OpenAI's `tool`-role messages carry a single string body and have no
/// channel for binary media, so a tool that returns an image or document
/// cannot be delivered as-is. Rather than drop the block silently (leaving the
/// model unaware the tool produced anything) we substitute a one-line
/// description of what was returned. The base64 payload itself is never
/// inlined.
fn media_placeholder(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Image { media_type, .. } => Some(format!("[image omitted: {media_type}]")),
        ContentBlock::Document {
            media_type, title, ..
        } => {
            let label = title.as_deref().unwrap_or("untitled");
            Some(format!("[document omitted: {media_type}, {label}]"))
        }
        _ => None,
    }
}

/// Flatten a tool result's content blocks into the single string body an
/// OpenAI `tool`-role message accepts. Text blocks pass through verbatim;
/// image/document blocks degrade to [`media_placeholder`] lines so the model
/// still learns the tool produced media even though this provider can't show
/// it. Non-content blocks (reasoning, nested tool calls) are skipped.
fn render_tool_result_body(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            ContentBlock::Image { .. } | ContentBlock::Document { .. } => media_placeholder(b),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalizer that converts between canonical [`Message`]s and the OpenAI
/// Chat Completions API wire format.
///
/// See [module-level documentation](self) for encoding decisions.
#[derive(Debug)]
pub struct OpenAINormalizer;

impl MessageNormalizer for OpenAINormalizer {
    /// Encode canonical messages into the OpenAI Chat Completions wire shape.
    ///
    /// Returns a `Value::Array` of OpenAI-format message objects. Returns
    /// `NormalizerError::Unrepresentable` if a `Message::System` is encountered
    /// (system prompts are rendered by `request.rs` as a leading message, not
    /// passed through this normalizer).
    ///
    /// Mixed `User` content (text + tool results) is split: tool-role messages
    /// first, then user-role message for concatenated text. See module docs for
    /// the rationale.
    fn to_provider(&self, messages: &[Message]) -> Result<Value, NormalizerError> {
        let mut wire: Vec<Value> = Vec::with_capacity(messages.len());

        for msg in messages {
            match msg {
                Message::System { .. } => {
                    return Err(NormalizerError::Unrepresentable(
                        "Message::System must be extracted to the request system message \
                         before calling to_provider"
                            .into(),
                    ));
                }

                Message::User { content } => {
                    // Collect tool-result and text blocks separately; tool messages
                    // are emitted before the user text message (OpenAI requirement).
                    let mut tool_msgs: Vec<Value> = Vec::new();
                    let mut text_parts: Vec<String> = Vec::new();

                    for block in content {
                        match block {
                            ContentBlock::Text { text } => {
                                text_parts.push(text.clone());
                            }
                            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                                let encoded = if *is_error {
                                    format!("Error: {content}")
                                } else {
                                    content.clone()
                                };
                                tool_msgs.push(json!({
                                    "role": "tool",
                                    "tool_call_id": tool_use_id,
                                    "content": encoded,
                                }));
                            }
                            ContentBlock::ToolUse { .. } => {
                                return Err(NormalizerError::Unrepresentable(
                                    "ContentBlock::ToolUse cannot appear inside a User message"
                                        .into(),
                                ));
                            }
                            ContentBlock::Image { .. } | ContentBlock::Document { .. } => {
                                // Media in a user-role message arrives via
                                // cross-provider replay. OpenAI's tool channel
                                // can't carry it, so fold a textual placeholder
                                // into the user text rather than emit an
                                // invalid request.
                                if let Some(placeholder) = media_placeholder(block) {
                                    text_parts.push(placeholder);
                                }
                            }
                            ContentBlock::Thinking { .. }
                            | ContentBlock::RedactedThinking { .. } => {
                                // Reasoning blocks belong to the assistant; if
                                // one shows up in a user-role message it's a
                                // cross-provider replay artifact (Anthropic
                                // transcript fed to an OpenAI-mode agent).
                                // Drop silently rather than failing the whole
                                // request — OpenAI has no equivalent shape and
                                // there's nothing useful to encode.
                            }
                        }
                    }

                    wire.extend(tool_msgs);

                    if !text_parts.is_empty() {
                        let combined = text_parts.join("\n");
                        wire.push(json!({ "role": "user", "content": combined }));
                    }
                }

                Message::ToolResult { tool_use_id, content, is_error } => {
                    let text = render_tool_result_body(content);
                    let encoded = if *is_error { format!("Error: {text}") } else { text };
                    wire.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": encoded,
                    }));
                }

                Message::Assistant { content } => {
                    let mut text_parts: Vec<String> = Vec::new();
                    let mut tool_calls: Vec<Value> = Vec::new();

                    for block in content {
                        match block {
                            ContentBlock::Text { text } => {
                                text_parts.push(text.clone());
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                let arguments =
                                    serde_json::to_string(input).map_err(|e| {
                                        NormalizerError::Unrepresentable(format!(
                                            "could not serialize tool_call arguments: {e}"
                                        ))
                                    })?;
                                tool_calls.push(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": arguments,
                                    },
                                }));
                            }
                            ContentBlock::ToolResult { .. } => {
                                return Err(NormalizerError::Unrepresentable(
                                    "ContentBlock::ToolResult cannot appear inside an Assistant \
                                     message"
                                        .into(),
                                ));
                            }
                            ContentBlock::Image { .. } | ContentBlock::Document { .. } => {
                                // Media never originates from an assistant turn
                                // in our model — tools surface it via
                                // Message::ToolResult. If one appears here it's
                                // a replay artifact; drop it silently rather
                                // than fabricate an assistant-authored media
                                // part OpenAI has no slot for.
                            }
                            ContentBlock::Thinking { .. }
                            | ContentBlock::RedactedThinking { .. } => {
                                // OpenAI's Chat Completions wire format has no
                                // reasoning channel; drop the block silently so
                                // an Anthropic-recorded transcript can be
                                // replayed against an OpenAI-mode agent without
                                // an Unrepresentable error. The reasoning text
                                // is lost on cross-provider replay; that's the
                                // intentional tradeoff.
                            }
                        }
                    }

                    let text_value: Value = if text_parts.is_empty() {
                        Value::Null
                    } else {
                        Value::String(text_parts.join("\n"))
                    };

                    if tool_calls.is_empty() {
                        wire.push(json!({ "role": "assistant", "content": text_value }));
                    } else {
                        wire.push(json!({
                            "role": "assistant",
                            "content": text_value,
                            "tool_calls": tool_calls,
                        }));
                    }
                }
            }
        }

        Ok(Value::Array(wire))
    }

    /// Decode OpenAI Chat Completions wire-format message objects back into
    /// canonical messages.
    ///
    /// Expects a `Value::Array` of the same shape that `to_provider` produces.
    /// Used by integration tests to verify round-trip fidelity; production
    /// streaming bypasses this — `response.rs` translates SSE events directly.
    ///
    /// `tool_call.function.arguments` strings are parsed back from JSON-encoded
    /// strings to `serde_json::Value` objects — the inverse of the stringification
    /// applied in `to_provider`.
    ///
    /// `is_error` is always `false` on decoded `Message::ToolResult` values because
    /// OpenAI's wire format has no native error flag. The `"Error: "` prefix
    /// (if present) is preserved verbatim in the decoded content.
    fn from_provider(&self, value: Value) -> Result<Vec<Message>, NormalizerError> {
        let arr = value.as_array().ok_or_else(|| {
            NormalizerError::Shape(
                "expected a JSON array of OpenAI message objects".into(),
            )
        })?;

        let mut messages: Vec<Message> = Vec::with_capacity(arr.len());

        for obj in arr {
            let role = obj
                .get("role")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    NormalizerError::Shape("message object missing 'role' field".into())
                })?;

            match role {
                "user" => {
                    let text = obj
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    messages.push(Message::User {
                        content: vec![ContentBlock::Text { text }],
                    });
                }

                "tool" => {
                    let tool_call_id = obj
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            NormalizerError::Shape(
                                "tool message missing 'tool_call_id' field".into(),
                            )
                        })?
                        .to_string();
                    let text = obj
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    // is_error cannot be recovered from the wire format; always false.
                    messages.push(Message::ToolResult {
                        tool_use_id: tool_call_id,
                        content: vec![ContentBlock::Text { text }],
                        is_error: false,
                    });
                }

                "assistant" => {
                    let mut content_blocks: Vec<ContentBlock> = Vec::new();

                    // Text content (may be null for tool-only assistant turns).
                    if let Some(text) = obj.get("content").and_then(Value::as_str) {
                        if !text.is_empty() {
                            content_blocks.push(ContentBlock::Text { text: text.to_string() });
                        }
                    }

                    // Tool calls (each has id + function.{name, arguments}).
                    if let Some(tool_calls) = obj.get("tool_calls").and_then(Value::as_array) {
                        for tc in tool_calls {
                            let id = tc
                                .get("id")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    NormalizerError::Shape(
                                        "tool_call object missing 'id' field".into(),
                                    )
                                })?
                                .to_string();
                            let func = tc.get("function").ok_or_else(|| {
                                NormalizerError::Shape(
                                    "tool_call object missing 'function' field".into(),
                                )
                            })?;
                            let name = func
                                .get("name")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    NormalizerError::Shape(
                                        "tool_call function missing 'name' field".into(),
                                    )
                                })?
                                .to_string();
                            let arguments_str = func
                                .get("arguments")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    NormalizerError::Shape(
                                        "tool_call function missing 'arguments' field".into(),
                                    )
                                })?;
                            let input: Value = serde_json::from_str(arguments_str).map_err(|e| {
                                NormalizerError::Shape(format!(
                                    "failed to parse tool_call arguments as JSON: {e}"
                                ))
                            })?;
                            content_blocks.push(ContentBlock::ToolUse { id, name, input });
                        }
                    }

                    messages.push(Message::Assistant { content: content_blocks });
                }

                "system" => {
                    let text = obj
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    messages.push(Message::System { content: text });
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

#[cfg(test)]
mod tests {
    use super::*;
    use ao_engine_tools_runner::message::{ContentBlock, Message};
    use serde_json::json;

    fn norm() -> OpenAINormalizer {
        OpenAINormalizer
    }

    // -------------------------------------------------------------------------
    // Round-trip: 5-message transcript
    // -------------------------------------------------------------------------

    #[test]
    fn round_trip_five_message_transcript() {
        let msgs = vec![
            Message::User {
                content: vec![ContentBlock::Text { text: "What does foo do?".into() }],
            },
            Message::Assistant {
                content: vec![
                    ContentBlock::Text { text: "I'll read the file.".into() },
                    ContentBlock::ToolUse {
                        id: "call_01".into(),
                        name: "Read".into(),
                        input: json!({ "file_path": "src/foo.rs" }),
                    },
                ],
            },
            Message::ToolResult {
                tool_use_id: "call_01".into(),
                content: vec![ContentBlock::Text { text: "fn foo() {}".into() }],
                is_error: false,
            },
            Message::Assistant {
                content: vec![ContentBlock::Text { text: "foo is an empty function.".into() }],
            },
            Message::User {
                content: vec![ContentBlock::Text { text: "Thanks!".into() }],
            },
        ];

        let wire = norm().to_provider(&msgs).expect("to_provider should succeed");
        let decoded = norm().from_provider(wire).expect("from_provider should succeed");

        assert_eq!(decoded, msgs);
    }

    // -------------------------------------------------------------------------
    // Round-trip: is_error ToolResult — "Error: " prefix preserved on output
    // -------------------------------------------------------------------------

    #[test]
    fn round_trip_tool_result_is_error_prefix_preserved() {
        // Message::User containing a ContentBlock::ToolResult with is_error: true.
        let msgs = vec![Message::User {
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_err".into(),
                content: "command not found".into(),
                is_error: true,
            }],
        }];

        let wire = norm().to_provider(&msgs).expect("to_provider should succeed");
        let decoded = norm().from_provider(wire).expect("from_provider should succeed");

        // After round-trip the message is a ToolResult (OpenAI has no native error flag,
        // so is_error is lost — always false). The content string retains the "Error: " prefix.
        assert_eq!(decoded.len(), 1);
        match &decoded[0] {
            Message::ToolResult { tool_use_id, content, is_error } => {
                assert_eq!(tool_use_id, "call_err");
                assert!(!is_error, "is_error cannot be recovered from OpenAI wire format");
                match &content[0] {
                    ContentBlock::Text { text } => {
                        assert_eq!(text, "Error: command not found", "Error: prefix must be preserved");
                    }
                    other => panic!("expected Text block, got {other:?}"),
                }
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // Round-trip: parallel ToolUse blocks with distinct ids
    // -------------------------------------------------------------------------

    #[test]
    fn round_trip_parallel_tool_use_ids_preserved() {
        let msgs = vec![Message::Assistant {
            content: vec![
                ContentBlock::ToolUse {
                    id: "call_a1".into(),
                    name: "Read".into(),
                    input: json!({ "file_path": "src/foo.rs" }),
                },
                ContentBlock::ToolUse {
                    id: "call_a2".into(),
                    name: "Read".into(),
                    input: json!({ "file_path": "src/bar.rs" }),
                },
            ],
        }];

        let wire = norm().to_provider(&msgs).expect("to_provider should succeed");
        let decoded = norm().from_provider(wire).expect("from_provider should succeed");

        assert_eq!(decoded, msgs);

        // Extra: verify both ids are intact and ordered.
        match &decoded[0] {
            Message::Assistant { content } => {
                assert_eq!(content.len(), 2);
                match (&content[0], &content[1]) {
                    (
                        ContentBlock::ToolUse { id: id0, .. },
                        ContentBlock::ToolUse { id: id1, .. },
                    ) => {
                        assert_eq!(id0, "call_a1");
                        assert_eq!(id1, "call_a2");
                    }
                    other => panic!("expected two ToolUse blocks, got {other:?}"),
                }
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // System message rejection
    // -------------------------------------------------------------------------

    #[test]
    fn to_provider_rejects_system_message() {
        let msgs = vec![Message::System { content: "You are helpful.".into() }];
        let err = norm().to_provider(&msgs).expect_err("should fail for System message");
        assert!(
            matches!(err, NormalizerError::Unrepresentable(_)),
            "expected Unrepresentable, got {err:?}"
        );
    }

    // -------------------------------------------------------------------------
    // to_provider specific shape tests
    // -------------------------------------------------------------------------

    #[test]
    fn assistant_text_only_no_tool_calls_key() {
        let msgs = vec![Message::Assistant {
            content: vec![ContentBlock::Text { text: "hello".into() }],
        }];
        let wire = norm().to_provider(&msgs).unwrap();
        let obj = &wire[0];
        assert_eq!(obj["role"], "assistant");
        assert_eq!(obj["content"], "hello");
        assert!(obj.get("tool_calls").is_none(), "tool_calls must be absent for text-only turn");
    }

    #[test]
    fn assistant_tool_use_only_content_is_null() {
        let msgs = vec![Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "Bash".into(),
                input: json!({ "command": "ls" }),
            }],
        }];
        let wire = norm().to_provider(&msgs).unwrap();
        let obj = &wire[0];
        assert_eq!(obj["role"], "assistant");
        assert!(obj["content"].is_null(), "content must be null for tool-use-only assistant turn");
        assert_eq!(obj["tool_calls"][0]["id"], "c1");
    }

    #[test]
    fn assistant_text_and_tool_use_emits_both() {
        let msgs = vec![Message::Assistant {
            content: vec![
                ContentBlock::Text { text: "Using tool".into() },
                ContentBlock::ToolUse {
                    id: "c2".into(),
                    name: "Edit".into(),
                    input: json!({ "file": "x.rs" }),
                },
            ],
        }];
        let wire = norm().to_provider(&msgs).unwrap();
        let obj = &wire[0];
        assert_eq!(obj["content"], "Using tool");
        assert_eq!(obj["tool_calls"][0]["id"], "c2");
    }

    #[test]
    fn tool_call_arguments_serialized_as_string() {
        let input = json!({ "command": "echo hello", "flag": true });
        let msgs = vec![Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "c3".into(),
                name: "Bash".into(),
                input: input.clone(),
            }],
        }];
        let wire = norm().to_provider(&msgs).unwrap();
        let arguments = wire[0]["tool_calls"][0]["function"]["arguments"].as_str()
            .expect("arguments must be a JSON string, not an object");
        let parsed: serde_json::Value = serde_json::from_str(arguments)
            .expect("arguments string must be valid JSON");
        assert_eq!(parsed, input, "round-tripped arguments must equal original input");
    }

    #[test]
    fn mixed_user_content_tool_messages_emitted_before_text() {
        let msgs = vec![Message::User {
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "c1".into(),
                    content: "result1".into(),
                    is_error: false,
                },
                ContentBlock::Text { text: "thanks".into() },
            ],
        }];
        let wire = norm().to_provider(&msgs).unwrap();
        let arr = wire.as_array().unwrap();
        assert_eq!(arr.len(), 2, "should emit 2 OpenAI messages");
        assert_eq!(arr[0]["role"], "tool", "tool message must come first");
        assert_eq!(arr[1]["role"], "user", "user text must come second");
    }

    // -------------------------------------------------------------------------
    // Media downgrade: image/document tool results become text placeholders
    // -------------------------------------------------------------------------

    #[test]
    fn tool_result_image_downgrades_to_text_placeholder() {
        let msgs = vec![Message::ToolResult {
            tool_use_id: "call_img".into(),
            content: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "iVBORw0KGgo=".into(),
            }],
            is_error: false,
        }];
        let wire = norm().to_provider(&msgs).unwrap();
        let body = wire[0]["content"].as_str().expect("tool content is a string");
        assert_eq!(wire[0]["role"], "tool");
        assert!(body.contains("[image omitted: image/png]"), "got: {body}");
        // The base64 payload must never leak into the OpenAI request body.
        assert!(!body.contains("iVBORw0KGgo="), "base64 must not be inlined");
    }

    #[test]
    fn tool_result_document_downgrades_with_title_and_summary() {
        let msgs = vec![Message::ToolResult {
            tool_use_id: "call_pdf".into(),
            content: vec![
                ContentBlock::Text {
                    text: "PDF read: report.pdf".into(),
                },
                ContentBlock::Document {
                    media_type: "application/pdf".into(),
                    data: "JVBERi0=".into(),
                    title: Some("report.pdf".into()),
                },
            ],
            is_error: false,
        }];
        let wire = norm().to_provider(&msgs).unwrap();
        let body = wire[0]["content"].as_str().expect("tool content is a string");
        // The leading text summary survives, the document degrades to a label.
        assert!(body.contains("PDF read: report.pdf"), "got: {body}");
        assert!(
            body.contains("[document omitted: application/pdf, report.pdf]"),
            "got: {body}"
        );
        assert!(!body.contains("JVBERi0="), "base64 must not be inlined");
    }
}
