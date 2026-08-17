use serde::{Deserialize, Serialize};

use crate::agent::ChannelKind;

/// Tracks where a queued message originated from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessageSource {
    User,
    Schedule {
        task_id: String,
        #[serde(default)]
        is_recurring: bool,
    },
    /// A triggered assignment run. Treated as autonomous — not interactive —
    /// so it is never gated by the interactive-serialization lease.
    Assignment {
        assignment_id: String,
        run_id: String,
        /// Lowercase trigger class: `"cron"` | `"webhook"` | `"manual"`.
        trigger_kind: String,
    },
    /// An inbound message relayed from a bound Telegram bot's dedicated
    /// bridge thread. Carries the originating chat so a later phase can
    /// resolve where to relay the agent's reply. Not matched by either
    /// `is_recurring_schedule` or `is_interactive_message`'s autonomous
    /// cases, so it is classified and serialized exactly like a normal
    /// user-typed turn.
    ///
    /// Retained for transcript back-compat alongside the channel-agnostic
    /// `Channel` variant below — existing persisted messages still carry
    /// this shape, so it is never removed or renamed. New channel kinds
    /// (and new Telegram messages, once the bridge is ported in a later
    /// phase) use `Channel` instead.
    Telegram { chat_id: i64 },
    /// An inbound message relayed from any bound channel binding
    /// (Telegram, email, ...). `binding_id` identifies which of the agent's
    /// `ChannelBinding`s to reply through; `conversation_id` and
    /// `sender_id` are channel-specific (e.g. a Telegram chat id and user
    /// id, or an email thread and From address), both carried as strings so
    /// this variant doesn't grow a new shape per channel kind.
    Channel {
        kind: ChannelKind,
        binding_id: String,
        conversation_id: String,
        sender_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_telegram_variant_still_deserializes() {
        let json = r#"{"type":"Telegram","chat_id":555}"#;
        let source: MessageSource = serde_json::from_str(json).expect("legacy shape must parse");
        assert!(matches!(source, MessageSource::Telegram { chat_id: 555 }));
    }

    #[test]
    fn channel_variant_round_trips() {
        let source = MessageSource::Channel {
            kind: ChannelKind::Telegram,
            binding_id: "telegram".to_string(),
            conversation_id: "555".to_string(),
            sender_id: "42".to_string(),
        };
        let json = serde_json::to_string(&source).expect("serialize");
        assert!(json.contains("\"type\":\"Channel\""));
        let decoded: MessageSource = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            decoded,
            MessageSource::Channel { kind: ChannelKind::Telegram, .. }
        ));
    }
}
