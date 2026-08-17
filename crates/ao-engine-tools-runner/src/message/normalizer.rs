//! [`MessageNormalizer`] trait and [`NormalizerError`] for converting between
//! canonical [`Message`]s and provider wire formats.
//!
//! # Error taxonomy
//!
//! [`NormalizerError`] is distinct from `ProviderError`: `ProviderError` covers
//! the streaming and transport surface (network failures, cancellation, stream
//! exhaustion), while `NormalizerError` covers the conversion and
//! representability surface (a canonical message that has no valid encoding in a
//! given provider's wire format, or a provider response that does not match the
//! expected shape).
//!
//! # Trait contract
//!
//! [`MessageNormalizer`] is `Send + Sync` so the runner can hold a reference
//! through an `Arc<dyn ProviderClient>` without additional synchronisation.
//! Implementations must be stateless with respect to the conversion — no
//! borrowed state, no mutable interior state on the conversion path. Caching of
//! intermediate representations is a provider-internal concern and must not
//! affect the trait surface.
//!
//! [`MessageNormalizer::to_provider`] returns a single [`serde_json::Value`]
//! rather than `Vec<Value>` so providers like Gemini — whose request body wraps
//! messages inside a top-level `contents`/`systemInstruction` split — can return
//! whatever shape their request builder consumes without forcing the trait to
//! know about provider-specific envelope shapes.

use serde_json::Value;
use thiserror::Error;

use crate::message::Message;

/// Conversion error for the normalizer surface.
///
/// Distinct from `ProviderError` — see module-level documentation for the
/// rationale behind keeping the two error types separate.
#[derive(Debug, Error, PartialEq)]
pub enum NormalizerError {
    /// A canonical message (or one of its content blocks) has no valid
    /// representation in the target provider's wire format.
    #[error("could not represent canonical message in provider format: {0}")]
    Unrepresentable(String),

    /// A value returned by the provider did not match the expected shape and
    /// could not be deserialised into canonical messages.
    #[error("provider value did not match expected shape: {0}")]
    Shape(String),
}

/// Converts between canonical [`Message`]s and a provider's wire format.
///
/// Every concrete provider client implements this trait so that the runner
/// can hand canonical messages to the request builder and receive canonical
/// messages back from the response parser without knowing about provider-specific
/// JSON shapes.
///
/// # `Send + Sync` requirement
///
/// Implementations must be `Send + Sync`. The runner holds provider clients
/// through `Arc<dyn ProviderClient>`, and `message_normalizer` returns a
/// `&dyn MessageNormalizer` borrowed from the client. The `Send + Sync` bound
/// ensures that reference is safe to move across task boundaries.
///
/// # Stateless contract
///
/// Implementations must be stateless with respect to the conversion: the same
/// `messages` slice always produces the same `Value` and vice-versa. Caching
/// stays inside the provider crate; the trait surface does not expose it.
pub trait MessageNormalizer: Send + Sync {
    /// Encode canonical messages into the provider's wire shape.
    ///
    /// Returns a single `Value` so providers with non-array envelopes
    /// (e.g. a top-level `{"contents": [...], "systemInstruction": {...}}`
    /// wrapper) can return whatever shape their request builder consumes.
    fn to_provider(&self, messages: &[Message]) -> Result<Value, NormalizerError>;

    /// Decode a provider response value back into canonical messages.
    ///
    /// Used by integration tests to verify round-trip fidelity and by the
    /// runner to absorb assistant turns that arrive as provider-format JSON.
    fn from_provider(&self, value: Value) -> Result<Vec<Message>, NormalizerError>;
}

// =============================================================================
// Mock normalizer — for tests and the `mock` feature.
// =============================================================================

/// Identity-style normalizer for the scripted mock provider.
///
/// Serialises canonical messages with [`serde_json::to_value`] and
/// deserialises them back with [`serde_json::from_value`]. The mock does not
/// inspect message content (it replays scripted events, not request bodies),
/// but this implementation still exercises the `Serialize`/`Deserialize`
/// derives on the canonical types so test coverage stays honest.
#[cfg(any(test, feature = "mock"))]
#[derive(Debug, Default)]
pub struct MockNormalizer;

#[cfg(any(test, feature = "mock"))]
impl MessageNormalizer for MockNormalizer {
    fn to_provider(&self, messages: &[Message]) -> Result<Value, NormalizerError> {
        serde_json::to_value(messages)
            .map_err(|e| NormalizerError::Unrepresentable(e.to_string()))
    }

    fn from_provider(&self, value: Value) -> Result<Vec<Message>, NormalizerError> {
        serde_json::from_value(value).map_err(|e| NormalizerError::Shape(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::message::{ContentBlock, Message};

    fn five_message_fixture() -> Vec<Message> {
        vec![
            Message::System {
                content: "You are a helpful assistant.".into(),
            },
            Message::User {
                content: vec![ContentBlock::Text {
                    text: "What is 2+2?".into(),
                }],
            },
            Message::Assistant {
                content: vec![
                    ContentBlock::Text {
                        text: "Let me calculate that.".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "call_001".into(),
                        name: "calculator".into(),
                        input: json!({"operation": "add", "a": 2, "b": 2}),
                    },
                ],
            },
            Message::ToolResult {
                tool_use_id: "call_001".into(),
                content: vec![ContentBlock::Text {
                    text: "4".into(),
                }],
                is_error: false,
            },
            Message::User {
                content: vec![
                    ContentBlock::Text {
                        text: "Thanks!".into(),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_001".into(),
                        content: "4".into(),
                        is_error: false,
                    },
                ],
            },
        ]
    }

    #[test]
    fn mock_normalizer_round_trips_five_message_fixture() {
        let normalizer = MockNormalizer;
        let messages = five_message_fixture();

        let wire = normalizer
            .to_provider(&messages)
            .expect("to_provider should succeed");
        let recovered = normalizer
            .from_provider(wire)
            .expect("from_provider should succeed");

        assert_eq!(messages, recovered);
    }

    #[test]
    fn mock_normalizer_to_provider_is_json_array() {
        let normalizer = MockNormalizer;
        let messages = five_message_fixture();
        let wire = normalizer.to_provider(&messages).unwrap();
        assert!(wire.is_array(), "expected a JSON array from to_provider");
        assert_eq!(wire.as_array().unwrap().len(), messages.len());
    }

    #[test]
    fn mock_normalizer_from_provider_rejects_bad_shape() {
        let normalizer = MockNormalizer;
        let bad = json!({"not": "an array of messages"});
        let err = normalizer.from_provider(bad).unwrap_err();
        match err {
            NormalizerError::Shape(_) => {}
            other => panic!("expected Shape error, got: {other:?}"),
        }
    }

    #[test]
    fn assert_mock_normalizer_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MockNormalizer>();
    }

    #[test]
    fn assert_dyn_message_normalizer_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn MessageNormalizer>();
    }
}
