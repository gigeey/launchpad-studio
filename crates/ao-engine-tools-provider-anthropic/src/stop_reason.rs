//! Maps Anthropic `stop_reason` strings to canonical [`StopReason`] values.
//!
//! The mapping table is fixed by the Anthropic API contract; any string not
//! covered by a named variant is forwarded verbatim inside `StopReason::Other`
//! for forward-compatibility and logging.

use ao_engine_tools_runner::provider::StopReason;

/// Map an Anthropic `stop_reason` string to the canonical [`StopReason`].
///
/// The version pin keeps this mapping stable — any value not in the table
/// below is preserved as `StopReason::Other(raw)` so new Anthropic stop
/// reasons don't silently disappear before the table is updated.
pub fn map_stop_reason(raw: &str) -> StopReason {
    match raw {
        "end_turn" => StopReason::Natural,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::Natural,
        "tool_use" => StopReason::ToolUse,
        "refusal" => StopReason::Refusal,
        "pause_turn" => StopReason::Other("pause_turn".into()),
        other => StopReason::Other(other.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_turn_maps_to_natural() {
        assert_eq!(map_stop_reason("end_turn"), StopReason::Natural);
    }

    #[test]
    fn max_tokens_maps_to_max_tokens() {
        assert_eq!(map_stop_reason("max_tokens"), StopReason::MaxTokens);
    }

    #[test]
    fn stop_sequence_maps_to_natural() {
        assert_eq!(map_stop_reason("stop_sequence"), StopReason::Natural);
    }

    #[test]
    fn tool_use_maps_to_tool_use() {
        assert_eq!(map_stop_reason("tool_use"), StopReason::ToolUse);
    }

    #[test]
    fn refusal_maps_to_refusal() {
        assert_eq!(map_stop_reason("refusal"), StopReason::Refusal);
    }

    #[test]
    fn pause_turn_maps_to_other_pause_turn() {
        assert_eq!(
            map_stop_reason("pause_turn"),
            StopReason::Other("pause_turn".into())
        );
    }

    #[test]
    fn unknown_reason_maps_to_other_with_raw_string() {
        assert_eq!(
            map_stop_reason("some_future_reason"),
            StopReason::Other("some_future_reason".into())
        );
    }
}
