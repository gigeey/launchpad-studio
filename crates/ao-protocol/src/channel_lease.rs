//! Durable single-writer lease on one channel binding's `(agent_id,
//! binding_id)` key.
//!
//! Before this type existed, nothing stopped two backend processes pointed
//! at the same data dir from both starting the same channel binding: both
//! would hold the bot connection and both would answer, since
//! `ChannelBridge::reconcile` only ever read agent profiles and started
//! local tasks with no notion of "does anyone else already own this."
//! [`ChannelLease`] is the persisted claim `reconcile` checks (and renews)
//! before running a binding, so only one process drives it at a time.
//!
//! The TTL is what keeps a hard crash from wedging the binding forever: a
//! lease survives only as long as its holder keeps heartbeating it, so once
//! `expires_at` passes, any process (including a fresh restart of the same
//! one) may claim it again.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One channel binding's single-writer claim, persisted by
/// `ao_persistence::channel_lease_store::ChannelLeaseStore` and keyed
/// externally by `(agent_id, binding_id)`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelLease {
    /// Opaque identity of the process holding the lease. Deliberately not a
    /// PID — PIDs recycle across restarts and mean nothing across
    /// machines — just a random id one `ChannelBridge` generates once at
    /// construction and reuses for every claim/heartbeat it makes for its
    /// own lifetime.
    pub owner_id: String,
    pub claimed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl ChannelLease {
    /// Whether this lease's TTL has elapsed as of `now`, making it claimable
    /// by anyone regardless of `owner_id`.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_round_trips_through_json() {
        let now = Utc::now();
        let lease = ChannelLease {
            owner_id: "owner-a".to_string(),
            claimed_at: now,
            expires_at: now + chrono::Duration::seconds(15),
        };
        let json = serde_json::to_string(&lease).unwrap();
        let back: ChannelLease = serde_json::from_str(&json).unwrap();
        assert_eq!(lease, back);
    }

    #[test]
    fn is_expired_true_once_now_reaches_expires_at() {
        let now = Utc::now();
        let lease = ChannelLease {
            owner_id: "owner-a".to_string(),
            claimed_at: now,
            expires_at: now + chrono::Duration::seconds(10),
        };
        assert!(!lease.is_expired(now));
        assert!(!lease.is_expired(now + chrono::Duration::seconds(9)));
        assert!(lease.is_expired(now + chrono::Duration::seconds(10)));
        assert!(lease.is_expired(now + chrono::Duration::seconds(11)));
    }
}
