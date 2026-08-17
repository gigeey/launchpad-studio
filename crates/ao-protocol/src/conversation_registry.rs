//! Channel-agnostic conversation → thread registry row.
//!
//! Mirrors [`crate::slack_conversation_registry::SlackConversationRow`], but
//! generalized for the three channels (Discord, Telegram, Email) that share
//! one registry rather than each growing their own Slack-shaped clone.
//! Slack's own registry is intentionally left untouched — migrating it onto
//! this generic shape is a documented fast-follow, not part of this phase.
//!
//! Unlike Slack's registry (workspace-scoped, keyed on
//! `(team_id, channel_id, thread_ts)`), the generic registry is sharded by
//! `(agent_id, binding_id)` and the per-channel key is opaque to this type:
//! each channel composes its own [`ConversationKey`] from whichever fields
//! already separate one sender/room from another (Discord `channel_id`,
//! Telegram `chat_id`, Email `sender + normalized subject`). This type only
//! carries the value half of that mapping. `agent_id` is part of the
//! sharding key, not just this row's payload — `binding_id` alone repeats
//! across agents (e.g. every Telegram binding is `"telegram"`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Opaque per-channel conversation key, scoped externally to an
/// `(agent_id, binding_id)` pair by
/// `ao_persistence::conversation_registry_store::ConversationRegistryStore`.
/// Each channel is responsible for composing a key that already separates
/// distinct senders/rooms — this type does not
/// interpret or validate its contents.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ConversationKey(pub String);

impl ConversationKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ConversationKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One row of the generic conversation→thread registry: which agent, and
/// which Launchpad bridge thread, an `(agent_id, binding_id, ConversationKey)`
/// conversation is bound to.
///
/// Persisted by
/// `ao_persistence::conversation_registry_store::ConversationRegistryStore`,
/// which owns the `(agent_id, binding_id, ConversationKey)` key externally —
/// this type only carries the value half, mirroring
/// [`crate::slack_conversation_registry::SlackConversationRow`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationRow {
    /// The agent this conversation is routed to.
    pub agent_id: String,
    /// The Launchpad bridge thread this conversation's messages flow
    /// through, minted once on first inbound (see the store's
    /// `get_or_create`) and stable for the conversation's lifetime.
    pub thread_id: String,
    pub created_at: DateTime<Utc>,
    /// Updated on every inbound message for this conversation. The GC policy
    /// evicts by this field, oldest first, not by `created_at` — an old but
    /// still-active conversation must outlive a newer one that already went
    /// quiet.
    pub last_seen_at: DateTime<Utc>,
}
