//! Maps OpenAI `finish_reason` strings to canonical [`StopReason`] values.
//!
//! The mapping table is fixed by the OpenAI Chat Completions API contract; any
//! string not covered by a named variant is forwarded verbatim inside
//! `StopReason::Other` for forward-compatibility and logging.

use ao_engine_tools_runner::provider::StopReason;

/// Map an OpenAI `finish_reason` string to the canonical [`StopReason`].
///
/// Mapping table:
/// | OpenAI `finish_reason` | Canonical [`StopReason`]                  |
/// |------------------------|-------------------------------------------|
/// | `"stop"`               | `Natural`                                 |
/// | `"length"`             | `MaxTokens`                               |
/// | `"tool_calls"`         | `ToolUse`                                 |
/// | `"content_filter"`     | `Refusal`                                 |
/// | `"function_call"`      | `ToolUse` (+ `tracing::warn!`)           |
/// | anything else          | `Other(raw_string)`                       |
///
/// Unknown reasons produce `StopReason::Other(raw)` so new OpenAI finish
/// reasons don't silently disappear before the table is updated.
pub fn map_finish_reason(raw: &str) -> StopReason {
    match raw {
        "stop" => StopReason::Natural,
        "length" => StopReason::MaxTokens,
        "tool_calls" => StopReason::ToolUse,
        "content_filter" => StopReason::Refusal,
        "function_call" => {
            // Legacy OpenAI function-calling finish_reason — callers should upgrade
            // to the tool_calls API and modern function-calling models.
            tracing::warn!("OpenAI legacy function_call finish_reason; upgrade to tool_calls");
            StopReason::ToolUse
        }
        other => StopReason::Other(other.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_maps_to_natural() {
        assert_eq!(map_finish_reason("stop"), StopReason::Natural);
    }

    #[test]
    fn length_maps_to_max_tokens() {
        assert_eq!(map_finish_reason("length"), StopReason::MaxTokens);
    }

    #[test]
    fn tool_calls_maps_to_tool_use() {
        assert_eq!(map_finish_reason("tool_calls"), StopReason::ToolUse);
    }

    #[test]
    fn content_filter_maps_to_refusal() {
        assert_eq!(map_finish_reason("content_filter"), StopReason::Refusal);
    }

    #[test]
    fn function_call_legacy_maps_to_tool_use() {
        assert_eq!(map_finish_reason("function_call"), StopReason::ToolUse);
    }

    #[test]
    fn unknown_reason_maps_to_other_with_raw_string() {
        assert_eq!(
            map_finish_reason("some_future_reason"),
            StopReason::Other("some_future_reason".into())
        );
    }
}
