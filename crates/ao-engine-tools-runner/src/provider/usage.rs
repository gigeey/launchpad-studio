use serde::{Deserialize, Serialize};

/// Token-usage accounting for a single completion turn.
///
/// Providers that expose usage information emit this as a
/// [`CompletionEvent::Usage`] variant, always before the matching
/// [`CompletionEvent::TurnComplete`]. Providers that do not surface usage
/// data omit the event entirely — the query loop treats it as optional.
///
/// `cache_read` and `cache_creation` are `None` when the provider does
/// not break out prompt-cache activity separately. Provider-specific
/// extras (e.g. Anthropic 5-minute vs 1-hour cache split, Gemini
/// `thoughtTokenCount`) stay inside each provider crate and are NOT
/// promoted to this struct.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens charged as input (prompt) for this turn.
    pub input_tokens: u64,
    /// Tokens charged as output (completion) for this turn.
    pub output_tokens: u64,
    /// Tokens served from a prompt cache (not billed at full input rate),
    /// or `None` if the provider does not report this breakdown.
    pub cache_read: Option<u64>,
    /// Tokens written into a prompt cache this turn (may carry a write
    /// surcharge depending on the provider), or `None` if not reported.
    pub cache_creation: Option<u64>,
}
