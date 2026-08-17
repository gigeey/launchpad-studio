//! Gemini `usageMetadata` → canonical [`Usage`] mapping.
//!
//! Gemini's terminal SSE event carries a top-level `usageMetadata` block.
//! `cachedContentTokenCount` maps to `cache_read`; `cache_creation` is not
//! reported by Gemini v1beta and remains `None`.

use ao_engine_tools_runner::provider::Usage;
use serde_json::Value;

/// Extract a canonical [`Usage`] from a Gemini `usageMetadata` JSON block.
///
/// Field mapping:
/// - `input_tokens` ← `promptTokenCount`
/// - `output_tokens` ← `candidatesTokenCount`
/// - `cache_read` ← `cachedContentTokenCount` (logged once via `tracing::debug!`)
/// - `cache_creation` ← always `None` (Gemini v1beta does not report this)
///
/// Missing fields default to `0` / `None`.
pub fn map_usage_metadata(metadata: &Value) -> Usage {
    let input_tokens = metadata
        .get("promptTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = metadata
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = metadata
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_u64());

    if let Some(cached) = cache_read {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| {
            tracing::debug!(cached_tokens = cached, "Gemini cachedContentTokenCount present; mapped to cache_read");
        });
    }

    Usage {
        input_tokens,
        output_tokens,
        cache_read,
        cache_creation: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_all_three_counts() {
        let metadata = json!({
            "promptTokenCount": 100,
            "candidatesTokenCount": 50,
            "totalTokenCount": 150,
        });
        let u = map_usage_metadata(&metadata);
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.cache_read, None);
        assert_eq!(u.cache_creation, None);
    }

    #[test]
    fn maps_cached_content_token_count_to_cache_read() {
        let metadata = json!({
            "promptTokenCount": 200,
            "candidatesTokenCount": 80,
            "totalTokenCount": 280,
            "cachedContentTokenCount": 120,
        });
        let u = map_usage_metadata(&metadata);
        assert_eq!(u.input_tokens, 200);
        assert_eq!(u.output_tokens, 80);
        assert_eq!(u.cache_read, Some(120));
        assert_eq!(u.cache_creation, None);
    }

    #[test]
    fn missing_counts_default_to_zero() {
        let metadata = json!({});
        let u = map_usage_metadata(&metadata);
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.cache_read, None);
        assert_eq!(u.cache_creation, None);
    }
}
