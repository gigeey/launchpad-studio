//! In-memory registry of per-binding [`ChannelConnectionState`].
//!
//! One instance lives for the whole process, owned by
//! [`crate::telegram::ChannelBridge`] and handed to every
//! [`super::ChannelRunContext`] so a transport's inbound loop can report its
//! own connect/backoff transitions without needing a copy of its own.
//! `ChannelBridge::reconcile` is the other writer: it owns the
//! lease-derived `NotHoldingLease` value and the cleanup that removes an
//! entry once a binding stops running here for any other reason (falling
//! back to the `Disconnected` default on the next read).
//!
//! Deliberately just a `Mutex<HashMap<..>>` behind a small interface rather
//! than exposing the map directly — every write is a single key, so there's
//! no reason for a caller to need the lock held across more than one
//! operation.

use std::collections::HashMap;
use std::sync::Mutex;

use ao_protocol::channel_connection_state::ChannelConnectionState;

/// Process-wide `(agent_id, binding_id) -> ChannelConnectionState`, reported
/// by whichever transport (or the supervisor itself, for the lease-derived
/// state) most recently observed a transition for that binding.
#[derive(Default)]
pub struct ConnectionStateRegistry {
    states: Mutex<HashMap<(String, String), ChannelConnectionState>>,
}

impl ConnectionStateRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the current state for `(agent_id, binding_id)`, overwriting
    /// whatever was there before.
    pub fn set(&self, agent_id: &str, binding_id: &str, state: ChannelConnectionState) {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((agent_id.to_string(), binding_id.to_string()), state);
    }

    /// Drops any reported state for `(agent_id, binding_id)` — the next
    /// [`Self::get`] falls back to [`ChannelConnectionState::Disconnected`].
    /// Called whenever the supervisor stops a binding's task for a reason
    /// other than losing the lease to another owner (disabled, removed,
    /// reconfigured) — a stopped task's last-reported state must not linger
    /// and read as still live.
    pub fn remove(&self, agent_id: &str, binding_id: &str) {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(agent_id.to_string(), binding_id.to_string()));
    }

    /// Reads the currently reported state, defaulting to
    /// [`ChannelConnectionState::Disconnected`] when nothing has ever been
    /// reported for this binding in this process.
    pub fn get(&self, agent_id: &str, binding_id: &str) -> ChannelConnectionState {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(agent_id.to_string(), binding_id.to_string()))
            .copied()
            .unwrap_or(ChannelConnectionState::Disconnected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_defaults_to_disconnected_when_nothing_reported() {
        let registry = ConnectionStateRegistry::new();
        assert_eq!(registry.get("agent-a", "binding-a"), ChannelConnectionState::Disconnected);
    }

    #[test]
    fn set_then_get_round_trips() {
        let registry = ConnectionStateRegistry::new();
        registry.set("agent-a", "binding-a", ChannelConnectionState::Connected);
        assert_eq!(registry.get("agent-a", "binding-a"), ChannelConnectionState::Connected);

        registry.set("agent-a", "binding-a", ChannelConnectionState::Reconnecting);
        assert_eq!(registry.get("agent-a", "binding-a"), ChannelConnectionState::Reconnecting);
    }

    #[test]
    fn remove_falls_back_to_disconnected() {
        let registry = ConnectionStateRegistry::new();
        registry.set("agent-a", "binding-a", ChannelConnectionState::Connected);
        registry.remove("agent-a", "binding-a");
        assert_eq!(registry.get("agent-a", "binding-a"), ChannelConnectionState::Disconnected);
    }

    #[test]
    fn different_bindings_are_isolated() {
        let registry = ConnectionStateRegistry::new();
        registry.set("agent-a", "telegram", ChannelConnectionState::Connected);
        registry.set("agent-a", "discord", ChannelConnectionState::NotHoldingLease);
        assert_eq!(registry.get("agent-a", "telegram"), ChannelConnectionState::Connected);
        assert_eq!(registry.get("agent-a", "discord"), ChannelConnectionState::NotHoldingLease);
    }
}
