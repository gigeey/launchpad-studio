use ao_protocol::event::AgentEventPayload;
use serde_json::Value;

/// Extract text from JSON output using the collectText pattern.
/// Tries in order: result (string), content[].text, message.content[].text
pub fn collect_text(value: &Value) -> Option<String> {
    // Try "result" field first (string)
    if let Some(result) = value.get("result").and_then(|v| v.as_str()) {
        return Some(result.to_string());
    }

    // Try "content" array -> text entries
    if let Some(texts) = extract_content_texts(value.get("content")) {
        return Some(texts);
    }

    // Try "message.content" array
    if let Some(message) = value.get("message") {
        if let Some(texts) = extract_content_texts(message.get("content")) {
            return Some(texts);
        }
    }

    None
}

/// Extract concatenated text from a JSON content array.
/// Filters for items with `"type": "text"` and joins their `"text"` fields.
pub fn extract_content_texts(content: Option<&Value>) -> Option<String> {
    let arr = content?.as_array()?;
    let texts: Vec<&str> = arr
        .iter()
        .filter_map(|item| {
            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                item.get("text").and_then(|t| t.as_str())
            } else {
                None
            }
        })
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n\n"))
    }
}

/// Extract session ID from a JSON value using the given field names.
/// Returns the first matching field value found.
pub fn extract_session_id_from_value(value: &Value, session_id_fields: &[String]) -> Option<String> {
    // Default to "session_id" if no fields configured
    let default_fields = vec!["session_id".to_string()];
    let fields = if session_id_fields.is_empty() {
        &default_fields
    } else {
        session_id_fields
    };
    for field in fields {
        if let Some(id) = value.get(field).and_then(|v| v.as_str()) {
            return Some(id.to_string());
        }
    }
    None
}

/// Extract usage information from a JSON value.
///
/// Reads the canonical Anthropic field names (`input_tokens`, `output_tokens`,
/// `cache_read_input_tokens`, `cache_creation_input_tokens`) that the claude
/// binary emits in both `message_start.message.usage` and
/// `message_delta.usage`. The buggy historical names (`cache_read_tokens`,
/// `cache_creation_tokens`) are not produced by any real Anthropic surface, so
/// reading them silently yielded zero and made cache hits look like misses.
pub fn extract_usage(value: &Value) -> Option<AgentEventPayload> {
    let usage = value.get("usage")?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read_tokens = usage
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation_tokens = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // `total_tokens` mirrors the API path's accounting: every byte the model
    // saw on input, billed or otherwise, plus what it produced. Cache reads
    // count toward input (the model still processed those tokens, the provider
    // just charged a discount); cache_creation is part of `input_tokens` on
    // first-write turns and should NOT be double-counted here.
    let total_tokens = input_tokens + output_tokens + cache_read_tokens;
    Some(AgentEventPayload::Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        total_tokens,
    })
}
