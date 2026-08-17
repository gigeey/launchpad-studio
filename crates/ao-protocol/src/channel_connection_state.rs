//! Per-binding connection state surfaced by `GET /agents/{id}/channels`.
//!
//! Every channel transport already runs its own internal connect/backoff
//! state machine (Discord's Gateway handshake, Telegram's and Email's poll
//! backoff). [`ChannelConnectionState`] is the four-value wire projection of
//! whichever phase that machine is currently in, plus one value that comes
//! from the binding lease (`crate::channel_lease`) rather than from any
//! transport: a binding this process isn't currently allowed to run because
//! another process holds it. That's a deliberate, displayable state rather
//! than the silence a bare "not running here" would otherwise read as.

use serde::{Deserialize, Serialize};

/// Honest, four-value connection state for one channel binding, as seen by
/// the backend process answering the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelConnectionState {
    /// The transport is up with a live, healthy session — Discord: past
    /// `READY` and heartbeating; Telegram/Email: the most recent poll
    /// succeeded.
    Connected,
    /// The transport is running in this process but does not currently have
    /// a healthy session — Discord: between connect attempts or in its
    /// close-code backoff; Telegram/Email: in the post-error backoff pause
    /// before the next poll.
    Reconnecting,
    /// No task is running this binding in this process, and (as far as this
    /// process's own lease claim can tell) nothing else is running it
    /// either — disabled, unprovisioned, unconfigured, or simply never
    /// started.
    Disconnected,
    /// Another process currently holds this binding's single-writer lease
    /// (see `crate::channel_lease::ChannelLease`). Not an error: the binding
    /// is connected, just from a different backend process (e.g. another
    /// worktree's server pointed at the same data directory) than the one
    /// answering this request.
    NotHoldingLease,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_to_the_exact_kebab_case_wire_values() {
        assert_eq!(serde_json::to_string(&ChannelConnectionState::Connected).unwrap(), "\"connected\"");
        assert_eq!(serde_json::to_string(&ChannelConnectionState::Reconnecting).unwrap(), "\"reconnecting\"");
        assert_eq!(serde_json::to_string(&ChannelConnectionState::Disconnected).unwrap(), "\"disconnected\"");
        assert_eq!(
            serde_json::to_string(&ChannelConnectionState::NotHoldingLease).unwrap(),
            "\"not-holding-lease\""
        );
    }

    #[test]
    fn round_trips_through_json() {
        for state in [
            ChannelConnectionState::Connected,
            ChannelConnectionState::Reconnecting,
            ChannelConnectionState::Disconnected,
            ChannelConnectionState::NotHoldingLease,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: ChannelConnectionState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back);
        }
    }
}
