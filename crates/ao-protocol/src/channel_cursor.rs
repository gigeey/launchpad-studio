//! Durable per-binding dedup cursor — the piece of state that lets a channel
//! transport resume "where it left off" after a backend restart instead of
//! re-delivering every message the previous process hadn't yet acknowledged.
//!
//! Before this type existed, each transport's cursor was a plain local
//! variable (Telegram's `offset` in `run_bot_poll_loop`, Discord's
//! `SeenMessageIds` + gateway session in `run_discord_gateway_loop`) that
//! reset to empty on every process restart. Telegram's `getUpdates` then
//! re-served every update since the beginning, and a Discord `RESUME` could
//! replay dispatches already delivered — in both cases the agent would
//! answer an already-answered message a second time.
//!
//! Kept kind-specific and explicit (one variant per [`crate::agent::ChannelKind`]
//! that needs one) rather than an untyped blob, so a given channel's cursor
//! shape is checked at compile time by both the writer (the transport) and
//! the reader (`ao_persistence::channel_cursor_store::ChannelCursorStore`).
//! Email is deliberately absent: it's backed by the server-side IMAP `\Seen`
//! flag and already survives restarts for free.

use serde::{Deserialize, Serialize};

/// One channel binding's durable dedup cursor, keyed externally by
/// `(agent_id, binding_id)` — see `ChannelCursorStore`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum ChannelCursor {
    /// Telegram's `getUpdates` offset: the smallest `update_id` Telegram is
    /// still allowed to re-serve. A single ever-increasing scalar, so it's
    /// naturally bounded — nothing to prune.
    Telegram {
        #[serde(default)]
        offset: Option<i64>,
    },
    /// Discord's Gateway dedup state. `seen_message_ids` mirrors
    /// `ao_engine`'s `SeenMessageIds` bounded FIFO set (oldest-first order,
    /// capped well below any realistic `RESUME` replay window — see
    /// `SeenMessageIds::snapshot`/`from_snapshot`, which are the only
    /// producers/consumers of this field). `session_id`/`seq` are the
    /// gateway session identifiers `RESUME` would need; restoring them alone
    /// (without the ephemeral `resume_gateway_url`, deliberately not
    /// persisted) doesn't let a restart itself `RESUME` — it still
    /// `IDENTIFY`s fresh — but keeps the cursor forward-compatible and the
    /// dedup set is what actually protects against re-processing a message
    /// already answered before the restart.
    Discord {
        #[serde(default)]
        seen_message_ids: Vec<String>,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        seq: Option<u64>,
    },
    /// Slack Socket Mode's dedup state: the `event_id`s of envelopes already
    /// dispatched, so a redelivery (Slack retries anything not ack'd within
    /// 3s, and a `disconnect`/reconnect can also redeliver) doesn't answer
    /// the same event twice after a restart. Unlike Discord there is no
    /// resumable session to persist alongside it — Socket Mode always opens
    /// a fresh `apps.connections.open` connection.
    Slack {
        #[serde(default)]
        seen_event_ids: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_cursor_round_trips_through_json() {
        let cursor = ChannelCursor::Telegram { offset: Some(42) };
        let json = serde_json::to_string(&cursor).unwrap();
        let back: ChannelCursor = serde_json::from_str(&json).unwrap();
        assert_eq!(cursor, back);
    }

    #[test]
    fn discord_cursor_round_trips_through_json() {
        let cursor = ChannelCursor::Discord {
            seen_message_ids: vec!["1".to_string(), "2".to_string()],
            session_id: Some("sess-abc".to_string()),
            seq: Some(7),
        };
        let json = serde_json::to_string(&cursor).unwrap();
        let back: ChannelCursor = serde_json::from_str(&json).unwrap();
        assert_eq!(cursor, back);
    }

    #[test]
    fn telegram_and_discord_cursors_are_distinguishable_on_the_wire() {
        let telegram = ChannelCursor::Telegram { offset: None };
        let discord = ChannelCursor::Discord { seen_message_ids: vec![], session_id: None, seq: None };
        assert_ne!(serde_json::to_string(&telegram).unwrap(), serde_json::to_string(&discord).unwrap());
    }

    #[test]
    fn slack_cursor_round_trips_through_json() {
        let cursor = ChannelCursor::Slack {
            seen_event_ids: vec!["Ev0MYUXG2M".to_string(), "Ev0MYUXG2N".to_string()],
        };
        let json = serde_json::to_string(&cursor).unwrap();
        let back: ChannelCursor = serde_json::from_str(&json).unwrap();
        assert_eq!(cursor, back);
    }
}
