//! Extracts canonical [`Usage`] structs from OpenAI usage JSON blocks.
//!
//! OpenAI emits usage in the final SSE chunk when
//! `stream_options.include_usage: true` is set. The block shape includes
//! `prompt_tokens`, `completion_tokens`, and an optional
//! `prompt_tokens_details` object that carries `cached_tokens` for prompt
//! cache hits.
//!
//! OpenAI does not currently report cache-creation tokens; `cache_creation`
//! is hardcoded to `None` and will require a one-line update if OpenAI ships
//! this field in the future.

use ao_engine_tools_runner::provider::Usage;

/// Extract a canonical [`Usage`] from an OpenAI usage JSON block.
///
/// Returns `None` when the value lacks any recognisable usage fields (e.g. a
/// `null` or empty object).
///
/// OpenAI usage block shape:
/// ```json
/// {
///   "prompt_tokens": 100,
///   "completion_tokens": 50,
///   "total_tokens": 150,
///   "prompt_tokens_details": { "cached_tokens": 20 }
/// }
/// ```
///
/// Mapping:
/// - `input_tokens` ← `prompt_tokens`
/// - `output_tokens` ← `completion_tokens`
/// - `cache_read` ← `prompt_tokens_details.cached_tokens` (or `None`)
/// - `cache_creation` ← always `None` (OpenAI does not expose this yet)
pub fn extract_usage(value: &serde_json::Value) -> Option<Usage> {
    let prompt_tokens = value.get("prompt_tokens").and_then(|v| v.as_u64());
    let completion_tokens = value.get("completion_tokens").and_then(|v| v.as_u64());

    if prompt_tokens.is_none() && completion_tokens.is_none() {
        return None;
    }

    let cached_tokens = value
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64());

    Some(Usage {
        input_tokens: prompt_tokens.unwrap_or(0),
        output_tokens: completion_tokens.unwrap_or(0),
        cache_read: cached_tokens,
        // OpenAI does not currently report cache creation tokens.
        cache_creation: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_fields_present_extracts_correctly() {
        let v = json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "total_tokens": 150,
            "prompt_tokens_details": {
                "cached_tokens": 20,
                "audio_tokens": 0,
                "reasoning_tokens": 0
            },
            "completion_tokens_details": {
                "reasoning_tokens": 0,
                "audio_tokens": 0
            }
        });
        let u = extract_usage(&v).expect("should extract");
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.cache_read, Some(20));
        assert_eq!(u.cache_creation, None);
    }

    #[test]
    fn missing_prompt_tokens_details_produces_none_cache_read() {
        let v = json!({
            "prompt_tokens": 10,
            "completion_tokens": 5
        });
        let u = extract_usage(&v).expect("should extract");
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 5);
        assert_eq!(u.cache_read, None);
        assert_eq!(u.cache_creation, None);
    }

    #[test]
    fn missing_usage_block_returns_none() {
        assert!(extract_usage(&serde_json::Value::Null).is_none());
        assert!(extract_usage(&json!({})).is_none());
    }
}
