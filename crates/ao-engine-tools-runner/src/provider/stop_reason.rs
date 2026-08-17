use serde::{Deserialize, Serialize};

/// The reason a provider ended the current turn.
///
/// Carried inside [`super::CompletionEvent::TurnComplete`] so downstream
/// consumers receive the terminal signal and its cause in a single event.
/// The type is intentionally forward-compatible: the [`Other`] variant
/// preserves any string a provider returns that does not map to a named
/// variant, so adding new variants here is always a non-breaking change
/// on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum StopReason {
    /// The model produced a complete response and stopped on its own.
    /// This is the normal exit for a non-tool-use turn.
    Natural,
    /// The provider stopped the turn because the model reached the
    /// configured maximum output-token limit. The transcript up to the
    /// cutoff is still available in the assistant turn. A caller that
    /// tracks context size may want to compact before looping.
    MaxTokens,
    /// The provider declined to complete the turn due to a content or
    /// safety policy. Output produced before the refusal boundary is
    /// still present in the assistant turn.
    Refusal,
    /// The model emitted one or more tool-use blocks and is waiting for
    /// results before continuing. The query loop uses this as a signal
    /// to execute the requested tools and loop.
    ToolUse,
    /// A reason not covered by the named variants above. The string
    /// value is the raw stop reason the provider returned, preserved
    /// for logging and forward-compatibility.
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::StopReason;

    fn round_trip(reason: &StopReason) -> StopReason {
        let json = serde_json::to_string(reason).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn natural_round_trips() {
        assert_eq!(round_trip(&StopReason::Natural), StopReason::Natural);
    }

    #[test]
    fn max_tokens_round_trips() {
        assert_eq!(round_trip(&StopReason::MaxTokens), StopReason::MaxTokens);
    }

    #[test]
    fn refusal_round_trips() {
        assert_eq!(round_trip(&StopReason::Refusal), StopReason::Refusal);
    }

    #[test]
    fn tool_use_round_trips() {
        assert_eq!(round_trip(&StopReason::ToolUse), StopReason::ToolUse);
    }

    #[test]
    fn other_round_trips() {
        let r = StopReason::Other("safety".into());
        assert_eq!(round_trip(&r), r);
    }

    #[test]
    fn natural_serialises_as_tagged_object() {
        let s = serde_json::to_string(&StopReason::Natural).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "natural");
    }

    #[test]
    fn other_serialises_with_value_field() {
        let s = serde_json::to_string(&StopReason::Other("safety".into())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "other");
        assert_eq!(v["value"], "safety");
    }
}
