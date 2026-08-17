//! Durable per-binding sender allow-list — the server-authoritative home for
//! a channel binding's linked senders, replacing the clobber-prone inline
//! `ChannelBinding::allowed_senders` field as the thing enforcement actually
//! reads.
//!
//! Before this type existed, a channel binding's allow-list lived only on
//! `ChannelBinding` inside the whole `AgentProfile` document, so two writers
//! that each round-trip the whole document — an out-of-band pairing flow
//! appending a sender, and a general profile save persisting whatever the
//! client last fetched — could each stomp the other's change. Keeping the
//! allow-list in its own small per-`(agent_id, binding_id)` file (see
//! `ao_persistence::linked_sender_store::LinkedSenderStore`) means a profile
//! save can never touch it at all.

use serde::{Deserialize, Serialize};

/// One channel binding's durable sender allow-list, keyed externally by
/// `(agent_id, binding_id)` — see `LinkedSenderStore`. An empty list is the
/// default and, for every enforcement point that reads it, means "reject
/// everyone" (fail-closed), not "allow everyone."
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkedSenderList {
    #[serde(default)]
    pub senders: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let list = LinkedSenderList { senders: vec!["555".to_string(), "666".to_string()] };
        let json = serde_json::to_string(&list).unwrap();
        let back: LinkedSenderList = serde_json::from_str(&json).unwrap();
        assert_eq!(list, back);
    }

    #[test]
    fn default_is_an_empty_sender_list() {
        assert_eq!(LinkedSenderList::default(), LinkedSenderList { senders: vec![] });
    }
}
