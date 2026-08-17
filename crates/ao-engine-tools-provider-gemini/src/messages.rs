//! [`MessageNormalizer`] implementation for the Gemini wire format.
//!
//! `GeminiMessageNormalizer` converts between canonical [`Message`] values and
//! Gemini's `contents[].parts[]` JSON shape. This module is the primary home
//! for bidirectional translation; `ordering.rs` handles the positional
//! re-pairing of parallel tool call responses.
//!
//! ## Encoding decisions
//!
//! - `Message::User` → `{ role: "user", parts: [...] }`
//! - `Message::Assistant` → `{ role: "model", parts: [...] }`
//! - `ContentBlock::Text` ↔ `parts[].text`
//! - `ContentBlock::ToolUse` → `parts[].functionCall { name, args }` — id stripped on egress
//!   (Gemini's request shape has no id field on functionCall)
//! - `Message::ToolResult` → user-role message with `parts[].functionResponse { name, response }`
//!   Consecutive `ToolResult` messages are collapsed into a single user-role message.
//!   `functionResponse` parts are emitted in original parts-array order (by `part_index`
//!   parsed from the synthesised `tool_use_id`); missing results get an empty-but-valid
//!   `functionResponse` shape.
//! - `Message::System` returns `Err(NormalizerError::Unrepresentable)` — request.rs
//!   extracts system prompts before calling the normalizer.
//!
//! ## Media blocks
//!
//! `ContentBlock::Image` and `ContentBlock::Document` returned by a tool ride
//! as `inlineData` parts (`{ "mimeType": ..., "data": <base64> }`) in the
//! enclosing user-role message, sitting alongside the `functionResponse` parts
//! they accompany. On the inbound path, an `inlineData` part decodes back to an
//! image block (or a document block for `application/pdf`). Audio and other
//! part kinds remain unsupported and return
//! `Err(NormalizerError::Unrepresentable)`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ao_engine_tools_runner::message::{ContentBlock, Message, MessageNormalizer, NormalizerError};
use serde_json::{json, Value};

use crate::ordering::{parse_tool_use_id, ToolCallOrderTracker};

/// Normalizer that converts between canonical [`Message`]s and the Gemini
/// `contents[].parts[]` wire format.
#[derive(Debug)]
pub struct GeminiMessageNormalizer {
    pub(crate) tracker: Arc<Mutex<ToolCallOrderTracker>>,
}

impl GeminiMessageNormalizer {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            tracker: Arc::new(Mutex::new(ToolCallOrderTracker::new())),
        }
    }

    pub fn with_tracker(tracker: Arc<Mutex<ToolCallOrderTracker>>) -> Self {
        Self { tracker }
    }
}

impl MessageNormalizer for GeminiMessageNormalizer {
    fn to_provider(&self, messages: &[Message]) -> Result<Value, NormalizerError> {
        let mut contents: Vec<Value> = Vec::new();
        let mut i = 0;

        while i < messages.len() {
            match &messages[i] {
                Message::System { .. } => {
                    return Err(NormalizerError::Unrepresentable(
                        "System messages must be extracted by request.rs before calling to_provider".into(),
                    ));
                }
                Message::User { content } => {
                    let parts = encode_user_content(content, &self.tracker)?;
                    contents.push(json!({ "role": "user", "parts": parts }));
                    i += 1;
                }
                Message::Assistant { content } => {
                    let parts = encode_assistant_content(content)?;
                    contents.push(json!({ "role": "model", "parts": parts }));
                    i += 1;
                }
                Message::ToolResult { .. } => {
                    // Collapse consecutive ToolResult messages into one user-role message.
                    let mut collected: Vec<(String, Vec<ContentBlock>, bool)> = Vec::new();
                    while i < messages.len() {
                        if let Message::ToolResult { tool_use_id, content, is_error } = &messages[i] {
                            collected.push((tool_use_id.clone(), content.clone(), *is_error));
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    let parts = build_tool_result_parts(&collected, &self.tracker);
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }

        Ok(Value::Array(contents))
    }

    fn from_provider(&self, value: Value) -> Result<Vec<Message>, NormalizerError> {
        let contents = value
            .as_array()
            .ok_or_else(|| NormalizerError::Shape("expected a contents array".into()))?;

        let mut messages = Vec::new();

        for item in contents {
            let role = item["role"]
                .as_str()
                .ok_or_else(|| NormalizerError::Shape("contents item missing role".into()))?;

            let parts = item["parts"]
                .as_array()
                .ok_or_else(|| NormalizerError::Shape("contents item missing parts array".into()))?;

            let blocks: Result<Vec<ContentBlock>, NormalizerError> = parts
                .iter()
                .enumerate()
                .map(|(idx, part)| parse_part(part, idx))
                .collect();

            match role {
                "user" => messages.push(Message::User { content: blocks? }),
                "model" => messages.push(Message::Assistant { content: blocks? }),
                other => {
                    return Err(NormalizerError::Shape(format!("unknown role: {other}")));
                }
            }
        }

        Ok(messages)
    }
}

/// Build the `parts[]` array for a user-role message that answers one or more
/// recorded `functionCall` parts.
///
/// Emits the `functionResponse` parts (see [`function_response_parts`] for the
/// ordering algorithm), then appends one `inlineData` part for every
/// image/document block any of the results carried. Gemini's `functionResponse`
/// shape has no slot for binary media, so tool-returned media rides alongside
/// the responses as sibling parts in the same user turn — the model associates
/// them positionally.
fn build_tool_result_parts(
    collected: &[(String, Vec<ContentBlock>, bool)],
    tracker: &Arc<Mutex<ToolCallOrderTracker>>,
) -> Vec<Value> {
    let mut parts = function_response_parts(collected, tracker);
    for (_, content, _) in collected {
        parts.extend(media_inline_parts(content));
    }
    parts
}

/// Translate the image/document blocks of a content array into Gemini
/// `inlineData` parts. Text and other block kinds are skipped — they're
/// handled elsewhere (text rides in the `functionResponse` output). The
/// base64 payload is passed through verbatim under `mimeType`/`data`.
fn media_inline_parts(content: &[ContentBlock]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Image { media_type, data } => Some(json!({
                "inlineData": { "mimeType": media_type, "data": data }
            })),
            ContentBlock::Document {
                media_type, data, ..
            } => Some(json!({
                "inlineData": { "mimeType": media_type, "data": data }
            })),
            _ => None,
        })
        .collect()
}

/// Build only the `functionResponse` parts for a run of collapsed tool results.
///
/// Algorithm:
/// 1. Parse each `tool_use_id` → `(turn_index, part_index)`.
/// 2. Consult the tracker for all recorded calls in that turn.
/// 3. Emit `functionResponse` parts in `part_index` order, using the tracker's
///    recorded function name.
/// 4. If a recorded call has no matching `ToolResult` in `collected`, emit an
///    empty-but-valid `functionResponse` at that position (defensive).
/// 5. If the tracker has no record for a given `part_index`, fall back to using
///    `tool_use_id` as the name (graceful degradation for tests without a
///    populated tracker).
fn function_response_parts(
    collected: &[(String, Vec<ContentBlock>, bool)],
    tracker: &Arc<Mutex<ToolCallOrderTracker>>,
) -> Vec<Value> {
    // Build a map from part_index → response_text for the results we received.
    let mut result_map: HashMap<usize, String> = HashMap::new();
    let mut first_turn_index: Option<usize> = None;

    for (tool_use_id, content, _) in collected {
        if let Some((turn_idx, part_idx)) = parse_tool_use_id(tool_use_id) {
            if first_turn_index.is_none() {
                first_turn_index = Some(turn_idx);
            }
            result_map.insert(part_idx, extract_text(content));
        }
    }

    // If we have a turn_index, try to use tracker-driven ordering.
    if let Some(turn_idx) = first_turn_index {
        let recorded = {
            let guard = tracker.lock().expect("tracker poisoned");
            guard
                .parts_for_turn(turn_idx)
                .into_iter()
                .map(|(p, n)| (p, n.to_owned()))
                .collect::<Vec<_>>()
        };

        if !recorded.is_empty() {
            // Emit in recorded parts-array order; fill missing positions with
            // empty-but-valid functionResponse shapes.
            return recorded
                .into_iter()
                .map(|(part_idx, recorded_name)| {
                    let response_text = result_map
                        .get(&part_idx)
                        .cloned()
                        .unwrap_or_default();
                    json!({
                        "functionResponse": {
                            "name": recorded_name,
                            "response": { "output": response_text }
                        }
                    })
                })
                .collect();
        }

        // Tracker has no record for this turn — sort results by part_index
        // and use tool_use_id as fallback name.
        if !result_map.is_empty() {
            let mut sorted: Vec<(usize, Value)> = collected
                .iter()
                .filter_map(|(id, content, _)| {
                    parse_tool_use_id(id).map(|(_, part_idx)| {
                        (
                            part_idx,
                            json!({
                                "functionResponse": {
                                    "name": id,
                                    "response": { "output": extract_text(content) }
                                }
                            }),
                        )
                    })
                })
                .collect();
            sorted.sort_by_key(|(idx, _)| *idx);
            return sorted.into_iter().map(|(_, v)| v).collect();
        }
    }

    // Fallback: emit in input order with tool_use_id as name (legacy path for
    // ids that don't match the synthesised format, e.g. non-Gemini origins).
    collected
        .iter()
        .map(|(id, content, _)| {
            json!({
                "functionResponse": {
                    "name": id,
                    "response": { "output": extract_text(content) }
                }
            })
        })
        .collect()
}

fn encode_user_content(
    content: &[ContentBlock],
    tracker: &Arc<Mutex<ToolCallOrderTracker>>,
) -> Result<Vec<Value>, NormalizerError> {
    let mut parts = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text } => {
                parts.push(json!({ "text": text }));
            }
            ContentBlock::ToolResult { tool_use_id, content, .. } => {
                // Look up the original function name from the tracker when available.
                let name = if let Some((turn_idx, part_idx)) = parse_tool_use_id(tool_use_id) {
                    let guard = tracker.lock().expect("tracker poisoned");
                    guard
                        .lookup_name(turn_idx, part_idx)
                        .unwrap_or(tool_use_id.as_str())
                        .to_owned()
                } else {
                    tool_use_id.clone()
                };
                parts.push(json!({
                    "functionResponse": {
                        "name": name,
                        "response": { "output": content }
                    }
                }));
            }
            ContentBlock::ToolUse { .. } => {
                return Err(NormalizerError::Unrepresentable(
                    "ToolUse blocks cannot appear in user-role content".into(),
                ));
            }
            ContentBlock::Image { media_type, data } => {
                parts.push(json!({
                    "inlineData": { "mimeType": media_type, "data": data }
                }));
            }
            ContentBlock::Document { media_type, data, .. } => {
                parts.push(json!({
                    "inlineData": { "mimeType": media_type, "data": data }
                }));
            }
            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {
                // Reasoning blocks belong to the model role; if one shows up
                // in a user-role message it's a cross-provider replay
                // artifact. Gemini has no equivalent shape, so drop silently.
            }
        }
    }
    Ok(parts)
}

fn encode_assistant_content(content: &[ContentBlock]) -> Result<Vec<Value>, NormalizerError> {
    let mut parts = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text } => {
                parts.push(json!({ "text": text }));
            }
            ContentBlock::ToolUse { name, input, .. } => {
                // id is stripped on egress — Gemini's functionCall has no id field.
                parts.push(json!({
                    "functionCall": {
                        "name": name,
                        "args": input
                    }
                }));
            }
            ContentBlock::ToolResult { .. } => {
                return Err(NormalizerError::Unrepresentable(
                    "ToolResult blocks cannot appear in model-role content".into(),
                ));
            }
            ContentBlock::Image { .. } | ContentBlock::Document { .. } => {
                // Media never originates from a model turn in our design —
                // tools surface it through Message::ToolResult. A media block
                // here is a replay artifact; drop it silently rather than
                // fabricate a model-authored inlineData part.
            }
            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {
                // Gemini's request shape has no reasoning channel. Drop
                // silently so an Anthropic-recorded transcript can replay
                // against a Gemini-mode agent without erroring; the reasoning
                // text is lost on cross-provider replay by design.
            }
        }
    }
    Ok(parts)
}

fn extract_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Translate a single Gemini `parts[i]` into a canonical [`ContentBlock`].
///
/// - `{ "text": "..." }` → [`ContentBlock::Text`]
/// - `{ "functionCall": { "name": ..., "args": ... } }` → [`ContentBlock::ToolUse`]
///   with a stub id (`gemini-call-stub-{index}`); the streaming translator uses
///   the real `gemini-call-{turn_index}-{part_index}` format via the tracker.
/// - `{ "inlineData": { "mimeType": ..., "data": ... } }` → [`ContentBlock::Document`]
///   for `application/pdf`, otherwise [`ContentBlock::Image`]. The base64 `data`
///   is carried through verbatim.
/// - Any other part type → `Err(NormalizerError::Unrepresentable)`
pub fn parse_part(part: &Value, index: usize) -> Result<ContentBlock, NormalizerError> {
    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
        return Ok(ContentBlock::Text {
            text: text.to_owned(),
        });
    }

    if let Some(fc) = part.get("functionCall") {
        let name = fc["name"]
            .as_str()
            .ok_or_else(|| NormalizerError::Shape("functionCall missing name".into()))?;
        let args = fc.get("args").cloned().unwrap_or(Value::Object(Default::default()));
        let id = format!("gemini-call-stub-{index}");
        return Ok(ContentBlock::ToolUse {
            id,
            name: name.to_owned(),
            input: args,
        });
    }

    if let Some(inline) = part.get("inlineData") {
        let media_type = inline["mimeType"]
            .as_str()
            .ok_or_else(|| NormalizerError::Shape("inlineData missing mimeType".into()))?
            .to_owned();
        let data = inline["data"]
            .as_str()
            .ok_or_else(|| NormalizerError::Shape("inlineData missing data".into()))?
            .to_owned();
        // PDFs map to a document block; everything else (image/*) maps to an
        // image block. The decode side has no title channel, so documents come
        // back untitled.
        if media_type == "application/pdf" {
            return Ok(ContentBlock::Document {
                media_type,
                data,
                title: None,
            });
        }
        return Ok(ContentBlock::Image { media_type, data });
    }

    Err(NormalizerError::Unrepresentable(
        "unsupported part type".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn norm() -> GeminiMessageNormalizer {
        GeminiMessageNormalizer::new()
    }

    fn norm_with_tracker(tracker: Arc<Mutex<ToolCallOrderTracker>>) -> GeminiMessageNormalizer {
        GeminiMessageNormalizer::with_tracker(tracker)
    }

    // --- outbound: User message with text ---

    #[test]
    fn to_provider_user_text_round_trips() {
        let messages = vec![Message::User {
            content: vec![ContentBlock::Text { text: "hello".into() }],
        }];
        let value = norm().to_provider(&messages).expect("to_provider failed");

        let contents = value.as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "hello");

        // inbound round-trip
        let back = norm().from_provider(value).expect("from_provider failed");
        assert_eq!(back, messages);
    }

    // --- outbound: Assistant message with text + functionCall ---

    #[test]
    fn to_provider_assistant_text_and_tool_use() {
        let messages = vec![Message::Assistant {
            content: vec![
                ContentBlock::Text { text: "I will read the file.".into() },
                ContentBlock::ToolUse {
                    id: "gemini-call-0-1".into(),
                    name: "Read".into(),
                    input: json!({ "file_path": "/tmp/test.txt" }),
                },
            ],
        }];
        let value = norm().to_provider(&messages).expect("to_provider failed");

        let contents = value.as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "model");

        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);

        // text part
        assert_eq!(parts[0]["text"], "I will read the file.");

        // functionCall part — id must be stripped
        let fc = &parts[1]["functionCall"];
        assert_eq!(fc["name"], "Read");
        assert_eq!(fc["args"]["file_path"], "/tmp/test.txt");
        assert!(fc.get("id").is_none(), "id must not appear in functionCall");
    }

    #[test]
    fn from_provider_assistant_turn_synthesises_stub_id() {
        let raw = json!([{
            "role": "model",
            "parts": [
                { "text": "Let me look that up." },
                { "functionCall": { "name": "Read", "args": { "file_path": "/etc/hosts" } } }
            ]
        }]);
        let messages = norm().from_provider(raw).expect("from_provider failed");

        assert_eq!(messages.len(), 1);
        if let Message::Assistant { content } = &messages[0] {
            assert_eq!(content.len(), 2);
            assert!(matches!(&content[0], ContentBlock::Text { text } if text == "Let me look that up."));
            if let ContentBlock::ToolUse { id, name, input } = &content[1] {
                // stub id format used in from_provider path (streaming path uses real format)
                assert_eq!(id, "gemini-call-stub-1");
                assert_eq!(name, "Read");
                assert_eq!(input["file_path"], "/etc/hosts");
            } else {
                panic!("expected ToolUse block");
            }
        } else {
            panic!("expected Assistant message");
        }
    }

    // --- outbound: ToolResult → functionResponse (fallback: no tracker record) ---

    #[test]
    fn to_provider_tool_result_emits_function_response() {
        let messages = vec![Message::ToolResult {
            tool_use_id: "gemini-call-0-0".into(),
            content: vec![ContentBlock::Text { text: "file contents here".into() }],
            is_error: false,
        }];
        let value = norm().to_provider(&messages).expect("to_provider failed");

        let contents = value.as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");

        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);

        let fr = &parts[0]["functionResponse"];
        // No tracker record → falls back to tool_use_id as name
        assert_eq!(fr["name"], "gemini-call-0-0");
        assert_eq!(fr["response"]["output"], "file contents here");
    }

    #[test]
    fn to_provider_consecutive_tool_results_collapse_into_one_user_turn() {
        let messages = vec![
            Message::ToolResult {
                tool_use_id: "gemini-call-0-0".into(),
                content: vec![ContentBlock::Text { text: "result 0".into() }],
                is_error: false,
            },
            Message::ToolResult {
                tool_use_id: "gemini-call-0-1".into(),
                content: vec![ContentBlock::Text { text: "result 1".into() }],
                is_error: false,
            },
        ];
        let value = norm().to_provider(&messages).expect("to_provider failed");

        let contents = value.as_array().unwrap();
        assert_eq!(contents.len(), 1, "two ToolResults must collapse into one user-role message");
        assert_eq!(contents[0]["role"], "user");

        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["functionResponse"]["name"], "gemini-call-0-0");
        assert_eq!(parts[1]["functionResponse"]["name"], "gemini-call-0-1");
    }

    // --- system message rejected ---

    #[test]
    fn to_provider_rejects_system_message() {
        let messages = vec![Message::System { content: "You are helpful.".into() }];
        let result = norm().to_provider(&messages);
        assert!(result.is_err(), "System messages must return an error");
    }

    // --- parse_part helper ---

    #[test]
    fn parse_part_text() {
        let part = json!({ "text": "hello world" });
        let block = parse_part(&part, 0).expect("parse_part failed");
        assert_eq!(block, ContentBlock::Text { text: "hello world".into() });
    }

    #[test]
    fn parse_part_function_call_synthesises_stub_id() {
        let part = json!({ "functionCall": { "name": "Bash", "args": { "command": "ls" } } });
        let block = parse_part(&part, 2).expect("parse_part failed");
        if let ContentBlock::ToolUse { id, name, input } = block {
            assert_eq!(id, "gemini-call-stub-2");
            assert_eq!(name, "Bash");
            assert_eq!(input["command"], "ls");
        } else {
            panic!("expected ToolUse");
        }
    }

    #[test]
    fn parse_part_unsupported_returns_error() {
        // `executableCode` is a Gemini part kind we don't model; it must still
        // surface as an Unrepresentable error.
        let part = json!({ "executableCode": { "language": "PYTHON", "code": "print(1)" } });
        let result = parse_part(&part, 0);
        assert!(result.is_err(), "unsupported part types must return an error");
    }

    #[test]
    fn parse_part_inline_image_maps_to_image_block() {
        let part = json!({ "inlineData": { "mimeType": "image/png", "data": "QUJDRA==" } });
        let block = parse_part(&part, 0).expect("inline image should parse");
        assert_eq!(
            block,
            ContentBlock::Image {
                media_type: "image/png".into(),
                data: "QUJDRA==".into(),
            }
        );
    }

    #[test]
    fn parse_part_inline_pdf_maps_to_document_block() {
        let part = json!({ "inlineData": { "mimeType": "application/pdf", "data": "JVBER" } });
        let block = parse_part(&part, 0).expect("inline pdf should parse");
        assert_eq!(
            block,
            ContentBlock::Document {
                media_type: "application/pdf".into(),
                data: "JVBER".into(),
                title: None,
            }
        );
    }

    #[test]
    fn to_provider_tool_result_image_appends_inline_data_part() {
        // An image-only tool result still emits a functionResponse (with empty
        // output text) plus a sibling inlineData part carrying the image.
        let messages = vec![Message::ToolResult {
            tool_use_id: "gemini-call-0-0".into(),
            content: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "iVBORw0KGgo=".into(),
            }],
            is_error: false,
        }];
        let value = norm().to_provider(&messages).expect("to_provider failed");
        let contents = value.as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");

        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2, "functionResponse + inlineData");
        assert_eq!(parts[0]["functionResponse"]["name"], "gemini-call-0-0");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn to_provider_tool_result_pdf_emits_summary_and_inline_data() {
        // A PDF read carries a text summary plus a Document block. The summary
        // lands in the functionResponse output; the document becomes inlineData.
        let messages = vec![Message::ToolResult {
            tool_use_id: "gemini-call-0-0".into(),
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
        let value = norm().to_provider(&messages).expect("to_provider failed");
        let parts = value[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0]["functionResponse"]["response"]["output"],
            "PDF read: report.pdf"
        );
        assert_eq!(parts[1]["inlineData"]["mimeType"], "application/pdf");
        assert_eq!(parts[1]["inlineData"]["data"], "JVBERi0=");
    }

    // --- (c) denormalizer emits in parts_index order even when ToolResult blocks arrive in reverse ---

    #[test]
    fn to_provider_tool_results_reordered_by_part_index() {
        let tracker = Arc::new(Mutex::new(ToolCallOrderTracker::new()));
        {
            let mut t = tracker.lock().unwrap();
            t.record(0, 0, "Read");
            t.record(0, 1, "Bash");
        }

        // ToolResult blocks arrive in REVERSE order (executor completed 1 before 0)
        let messages = vec![
            Message::ToolResult {
                tool_use_id: "gemini-call-0-1".into(),  // Bash result (arrived first)
                content: vec![ContentBlock::Text { text: "bash output".into() }],
                is_error: false,
            },
            Message::ToolResult {
                tool_use_id: "gemini-call-0-0".into(),  // Read result (arrived second)
                content: vec![ContentBlock::Text { text: "read output".into() }],
                is_error: false,
            },
        ];

        let norm = norm_with_tracker(Arc::clone(&tracker));
        let value = norm.to_provider(&messages).expect("to_provider failed");

        let contents = value.as_array().unwrap();
        assert_eq!(contents.len(), 1);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);

        // parts[0] must be Read (part_index=0), parts[1] must be Bash (part_index=1)
        assert_eq!(parts[0]["functionResponse"]["name"], "Read");
        assert_eq!(parts[0]["functionResponse"]["response"]["output"], "read output");
        assert_eq!(parts[1]["functionResponse"]["name"], "Bash");
        assert_eq!(parts[1]["functionResponse"]["response"]["output"], "bash output");
    }

    // --- (d) defensive empty-response insertion for missing results ---

    #[test]
    fn to_provider_missing_tool_result_emits_empty_response() {
        let tracker = Arc::new(Mutex::new(ToolCallOrderTracker::new()));
        {
            let mut t = tracker.lock().unwrap();
            t.record(0, 0, "Read");
            t.record(0, 1, "Bash");
        }

        // Only provide a result for "gemini-call-0-1" (Bash); Read result is missing.
        let messages = vec![Message::ToolResult {
            tool_use_id: "gemini-call-0-1".into(),
            content: vec![ContentBlock::Text { text: "bash output".into() }],
            is_error: false,
        }];

        let norm = norm_with_tracker(Arc::clone(&tracker));
        let value = norm.to_provider(&messages).expect("to_provider failed");

        let contents = value.as_array().unwrap();
        assert_eq!(contents.len(), 1);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2, "must emit 2 parts even though only 1 result arrived");

        // parts[0] = empty Read response (defensive insertion)
        assert_eq!(parts[0]["functionResponse"]["name"], "Read");
        assert_eq!(parts[0]["functionResponse"]["response"]["output"], "");

        // parts[1] = real Bash response
        assert_eq!(parts[1]["functionResponse"]["name"], "Bash");
        assert_eq!(parts[1]["functionResponse"]["response"]["output"], "bash output");
    }
}
