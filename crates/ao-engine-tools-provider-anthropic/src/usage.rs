//! Extracts canonical [`Usage`] structs from Anthropic usage JSON blocks.
//!
//! Anthropic emits usage in two places: `message_start` (carries `input_tokens`
//! and optional cache fields) and `message_delta` (carries the final
//! `output_tokens`). Both shapes are handled by [`extract_usage`]; the caller
//! decides which emission to treat as the partial vs. final reading.

use ao_engine_tools_runner::provider::Usage;

/// Extract a canonical [`Usage`] from an Anthropic usage JSON block.
///
/// Returns `None` when the value lacks any recognisable usage fields (e.g. a
/// `null` or empty object), so callers can safely skip the emission.
///
/// `message_start` shape: `input_tokens`, `cache_creation_input_tokens`,
/// `cache_read_input_tokens` — `output_tokens` is absent or zero.
/// `message_delta` shape: `output_tokens` — input/cache fields may repeat.
pub fn extract_usage(value: &serde_json::Value) -> Option<Usage> {
    let input_tokens = value.get("input_tokens").and_then(|v| v.as_u64());
    let output_tokens = value.get("output_tokens").and_then(|v| v.as_u64());
    let cache_read = value
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64());
    let cache_creation = value
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64());

    if input_tokens.is_none() && output_tokens.is_none() {
        return None;
    }

    Some(Usage {
        input_tokens: input_tokens.unwrap_or(0),
        output_tokens: output_tokens.unwrap_or(0),
        cache_read,
        cache_creation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_start_extracts_input_and_cache_fields() {
        let v = json!({
            "input_tokens": 100,
            "cache_creation_input_tokens": 20,
            "cache_read_input_tokens": 5
        });
        let u = extract_usage(&v).expect("should extract");
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.cache_creation, Some(20));
        assert_eq!(u.cache_read, Some(5));
    }

    #[test]
    fn message_delta_extracts_output_tokens() {
        let v = json!({ "output_tokens": 42 });
        let u = extract_usage(&v).expect("should extract");
        assert_eq!(u.output_tokens, 42);
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.cache_read, None);
        assert_eq!(u.cache_creation, None);
    }

    #[test]
    fn empty_object_returns_none() {
        let v = json!({});
        assert!(extract_usage(&v).is_none());
    }

    #[test]
    fn cache_fields_absent_produce_none() {
        let v = json!({ "input_tokens": 10 });
        let u = extract_usage(&v).expect("should extract");
        assert_eq!(u.cache_read, None);
        assert_eq!(u.cache_creation, None);
    }
}
