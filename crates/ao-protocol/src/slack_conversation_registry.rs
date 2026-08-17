//! Conversation → thread registry row.
//!
//! The conversation→thread mapping is locked at 1:1 so thread-keyed
//! correlation in the outbound relay is correct by construction — one
//! Launchpad bridge thread per Slack conversation, never a many-to-one
//! collapse. The key is then widened one more step: the registry is
//! keyed workspace-wide as `(team_id, channel_id, thread_ts)` rather than
//! per-binding, and the value names which agent owns that conversation. That
//! is deliberate — it is the same lookup a future "two agents in one Slack
//! channel" world needs for routing a reply to the right agent, so building
//! it workspace-scoped now means multi-agent routing is a data question
//! (two rows, two different `agent_id`s) on day one, not a rebuild later.
//!
//! `thread_ts` is `None` for a DM: a DM's conversation key is
//! the channel id alone (one persistent thread per DM), while a channel
//! `@mention` or a reply inside one keys on `channel_id` *and* the Slack
//! thread's root `ts`.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One row of the conversation→thread registry: which agent, and which
/// Launchpad bridge thread, a `(team_id, channel_id, thread_ts)` Slack
/// conversation is bound to.
///
/// Persisted by
/// `ao_persistence::slack_conversation_registry_store::SlackConversationRegistryStore`,
/// which owns the `(team_id, channel_id, thread_ts)` key externally — this
/// type only carries the value half so a second agent bound to a different
/// key in the same channel is just a second row, never a shape change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlackConversationRow {
    /// The agent this conversation is routed to. The reason the key alone
    /// (which doesn't mention an agent) isn't enough: two agents can each
    /// own a distinct conversation in the same Slack channel, and a reply
    /// must land on the one that started it.
    pub agent_id: String,
    /// The Launchpad bridge thread this conversation's messages flow
    /// through, provisioned once on first inbound (see the store's
    /// `get_or_create`) and stable for the conversation's lifetime.
    pub thread_id: String,
    pub created_at: DateTime<Utc>,
    /// Updated on every inbound message for this conversation. The GC policy
    /// evicts by this field, oldest first, not by `created_at` — an old but
    /// still-active conversation must outlive a newer one that already went
    /// quiet.
    pub last_seen_at: DateTime<Utc>,
}
