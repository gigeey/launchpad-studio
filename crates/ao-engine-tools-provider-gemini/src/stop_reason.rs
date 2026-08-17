//! Gemini `finishReason` → canonical [`StopReason`] mapping.
//!
//! If the turn contained any `functionCall` parts, the stop reason is
//! overridden to `ToolUse` regardless of what `finishReason` the API
//! returned — Gemini sometimes emits `STOP` on the same event that carries
//! function calls.

use ao_engine_tools_runner::provider::StopReason;

/// Map a Gemini `finishReason` string to the canonical [`StopReason`].
///
/// `has_function_call` must be `true` if any `functionCall` part was seen in
/// the current turn. When set, the return value is always `ToolUse` regardless
/// of the raw reason string.
///
/// | Gemini `finishReason`       | Canonical [`StopReason`]    |
/// |-----------------------------|-----------------------------|
/// | `"STOP"`                    | `Natural`                   |
/// | `"MAX_TOKENS"`              | `MaxTokens`                 |
/// | `"SAFETY"` / `"RECITATION"` / `"OTHER"` / `"BLOCKLIST"` / |
/// | `"PROHIBITED_CONTENT"` / `"SPII"` / `"MALFORMED_FUNCTION_CALL"` | `Other(raw)` |
/// | anything else               | `Other(raw)`                |
///
/// When `has_function_call` is `true`, all of the above are replaced by `ToolUse`.
pub fn map_finish_reason(reason: &str, has_function_call: bool) -> StopReason {
    if has_function_call {
        return StopReason::ToolUse;
    }
    match reason {
        "STOP" => StopReason::Natural,
        "MAX_TOKENS" => StopReason::MaxTokens,
        other => StopReason::Other(other.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_maps_to_natural() {
        assert_eq!(map_finish_reason("STOP", false), StopReason::Natural);
    }

    #[test]
    fn max_tokens_maps_to_max_tokens() {
        assert_eq!(map_finish_reason("MAX_TOKENS", false), StopReason::MaxTokens);
    }

    #[test]
    fn safety_maps_to_other() {
        assert_eq!(
            map_finish_reason("SAFETY", false),
            StopReason::Other("SAFETY".into())
        );
    }

    #[test]
    fn recitation_maps_to_other() {
        assert_eq!(
            map_finish_reason("RECITATION", false),
            StopReason::Other("RECITATION".into())
        );
    }

    #[test]
    fn other_reason_maps_to_other() {
        assert_eq!(
            map_finish_reason("OTHER", false),
            StopReason::Other("OTHER".into())
        );
    }

    #[test]
    fn blocklist_maps_to_other() {
        assert_eq!(
            map_finish_reason("BLOCKLIST", false),
            StopReason::Other("BLOCKLIST".into())
        );
    }

    #[test]
    fn prohibited_content_maps_to_other() {
        assert_eq!(
            map_finish_reason("PROHIBITED_CONTENT", false),
            StopReason::Other("PROHIBITED_CONTENT".into())
        );
    }

    #[test]
    fn spii_maps_to_other() {
        assert_eq!(
            map_finish_reason("SPII", false),
            StopReason::Other("SPII".into())
        );
    }

    #[test]
    fn malformed_function_call_maps_to_other() {
        assert_eq!(
            map_finish_reason("MALFORMED_FUNCTION_CALL", false),
            StopReason::Other("MALFORMED_FUNCTION_CALL".into())
        );
    }

    #[test]
    fn unknown_reason_maps_to_other() {
        assert_eq!(
            map_finish_reason("FUTURE_UNKNOWN_REASON", false),
            StopReason::Other("FUTURE_UNKNOWN_REASON".into())
        );
    }

    #[test]
    fn function_call_override_replaces_stop() {
        assert_eq!(map_finish_reason("STOP", true), StopReason::ToolUse);
    }

    #[test]
    fn function_call_override_replaces_max_tokens() {
        assert_eq!(map_finish_reason("MAX_TOKENS", true), StopReason::ToolUse);
    }

    #[test]
    fn function_call_override_replaces_safety() {
        assert_eq!(map_finish_reason("SAFETY", true), StopReason::ToolUse);
    }
}
