//! Runs [`ConversationRegistryStore::gc`] for a binding and releases every
//! evicted row's in-memory lease state in one call.
//!
//! [`ConversationRegistryStore::gc`] (persistence layer) and
//! [`LeaseGate::forget_thread`] (this process's in-memory lease state) are
//! the two halves of the conversation lifecycle: the registry
//! owns *when* a conversation is stale enough to evict, `LeaseGate` only
//! tracks *whether this process currently holds the lease* for a thread it's
//! already been told about. Neither side should know how to drive the
//! other — [`run_gc_and_release_leases`] is the seam that composes them, so
//! each per-channel inbound dispatch (Discord/Telegram/Email; Slack keeps
//! its own separately-sharded registry and gc pass) has one call to make
//! instead of threading `gc`'s evicted rows back out to the caller itself.

use chrono::{DateTime, Utc};

use ao_persistence::conversation_registry_store::ConversationRegistryStore;
use ao_protocol::error::AoError;

use super::lease_gate::LeaseGate;

/// Runs a standalone GC pass for `(agent_id, binding_id)` against `registry`,
/// then calls [`LeaseGate::forget_thread`] for every evicted row's
/// `thread_id` so this process stops believing it holds the lease for a
/// conversation that's no longer in durable storage. A returning sender
/// simply re-mints a fresh thread on its next inbound message — see the
/// module doc.
pub(crate) async fn run_gc_and_release_leases(
    registry: &ConversationRegistryStore,
    lease_gate: &LeaseGate,
    agent_id: &str,
    binding_id: &str,
    now: DateTime<Utc>,
) -> Result<(), AoError> {
    let evicted = registry.gc(agent_id, binding_id, now).await?;
    for row in evicted {
        lease_gate.forget_thread(&row.thread_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use ao_protocol::conversation_registry::{ConversationKey, ConversationRow};
    use chrono::Duration;
    use tempfile::tempdir;

    fn row(agent_id: &str, thread_id: &str, at: DateTime<Utc>) -> ConversationRow {
        ConversationRow { agent_id: agent_id.to_string(), thread_id: thread_id.to_string(), created_at: at, last_seen_at: at }
    }

    /// The wiring this module exists for: a conversation the registry idle-
    /// evicts must also stop passing `LeaseGate::is_active` — proving the
    /// two independently-testable halves (`ConversationRegistryStore::gc`,
    /// `LeaseGate::forget_thread`) actually compose end to end.
    #[tokio::test]
    async fn gc_eviction_releases_the_evicted_threads_lease() {
        use ao_persistence::conversation_registry_store::IDLE_EVICT_AFTER_DAYS;

        let tmp = tempdir().unwrap();
        let registry = ConversationRegistryStore::new(ao_persistence::paths::DataRoot::new(tmp.path()));
        let lease_gate = LeaseGate::new();
        let now = Utc::now();

        let stale_at = now - Duration::days(IDLE_EVICT_AFTER_DAYS + 1);
        registry
            .upsert("agent-a", "binding-1", ConversationKey::new("stale-conv"), row("agent-a", "thread-stale", stale_at), stale_at)
            .await
            .unwrap();
        lease_gate.mark_active("binding-1", "thread-stale");
        lease_gate.mark_active("binding-1", "thread-fresh");
        assert!(lease_gate.is_active("thread-stale"));

        run_gc_and_release_leases(&registry, &lease_gate, "agent-a", "binding-1", now).await.unwrap();

        assert!(!lease_gate.is_active("thread-stale"), "an idle-evicted conversation's thread must no longer be active");
        assert!(lease_gate.is_active("thread-fresh"), "a sibling thread under the same binding must be untouched");
    }

    #[tokio::test]
    async fn gc_with_nothing_to_evict_leaves_leases_untouched() {
        let tmp = tempdir().unwrap();
        let registry = ConversationRegistryStore::new(ao_persistence::paths::DataRoot::new(tmp.path()));
        let lease_gate = LeaseGate::new();
        let now = Utc::now();

        registry
            .upsert("agent-a", "binding-1", ConversationKey::new("fresh-conv"), row("agent-a", "thread-fresh", now), now)
            .await
            .unwrap();
        lease_gate.mark_active("binding-1", "thread-fresh");

        run_gc_and_release_leases(&registry, &lease_gate, "agent-a", "binding-1", now).await.unwrap();

        assert!(lease_gate.is_active("thread-fresh"));
    }
}
