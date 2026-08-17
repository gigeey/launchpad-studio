//! Small, pure pieces of Gateway connection state that need dedicated logic
//! to get right: bounded message-id de-duplication (so a Gateway `RESUME`
//! replaying already-seen dispatches doesn't double-deliver a message) and
//! heartbeat-ack tracking (zombie-connection detection). Both are plain data
//! structures with no I/O, so [`super::runner`]'s connect/reconnect loop can
//! drive them directly and they're unit-testable without a mock seam at all.

use std::collections::{HashSet, VecDeque};

/// Bounded FIFO de-dup set for inbound Gateway dispatch ids.
///
/// A `RESUME` replays every dispatch since the last acknowledged sequence,
/// which can include `MESSAGE_CREATE` events this transport already
/// delivered before the disconnect. `MESSAGE_CREATE.id` is stable across a
/// replay (it's the message's own Discord snowflake, not a per-delivery
/// value), so recording ids already delivered is sufficient to drop the
/// replayed duplicates. Bounded so a long-lived connection's memory doesn't
/// grow without limit — the oldest id is evicted once the set is full,
/// which is safe because a `RESUME` only ever replays a short recent window,
/// never a connection's entire history.
pub struct SeenMessageIds {
    order: VecDeque<String>,
    set: HashSet<String>,
    capacity: usize,
}

impl SeenMessageIds {
    pub fn new(capacity: usize) -> Self {
        Self { order: VecDeque::new(), set: HashSet::new(), capacity: capacity.max(1) }
    }

    /// Records `id` and returns `true` if this is the first time it's been
    /// seen; returns `false` (state left untouched beyond the lookup) for a
    /// repeat.
    pub fn insert_is_new(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return false;
        }
        self.set.insert(id.to_string());
        self.order.push_back(id.to_string());
        if self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        true
    }

    /// Oldest-first snapshot of every id currently tracked, for persistence
    /// (`ao_protocol::channel_cursor::ChannelCursor::Discord::seen_message_ids`).
    /// Always has at most `capacity` entries, since [`Self::insert_is_new`]
    /// never lets the set grow past it — the persisted cursor inherits that
    /// same bound for free.
    pub fn snapshot(&self) -> Vec<String> {
        self.order.iter().cloned().collect()
    }

    /// Rebuilds a [`SeenMessageIds`] from a persisted, oldest-first snapshot
    /// (the shape [`Self::snapshot`] produces), replaying each id through
    /// [`Self::insert_is_new`] so a snapshot longer than `capacity` — e.g.
    /// restored against a smaller capacity than it was written with — still
    /// comes out correctly bounded and keeps the most recent ids rather than
    /// the oldest.
    pub fn from_snapshot(ids: &[String], capacity: usize) -> Self {
        let mut seen = Self::new(capacity);
        for id in ids {
            seen.insert_is_new(id);
        }
        seen
    }
}

/// Tracks whether the most recently sent heartbeat has been acknowledged,
/// to detect a zombie connection: per the Gateway spec, if a new heartbeat
/// comes due while the previous one is still unacknowledged, the connection
/// is dead and must be explicitly closed and reconnected rather than trusted
/// further.
#[derive(Default)]
pub struct HeartbeatTracker {
    awaiting_ack: bool,
}

impl HeartbeatTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Call when a heartbeat has just been sent. Returns `true` if this send
    /// proves the *previous* heartbeat was never acknowledged — the caller
    /// must treat the connection as a zombie: close it explicitly, then
    /// reconnect (never just drop it, which would risk a duplicate delivery
    /// on the next connection racing an in-flight read on this one).
    pub fn on_heartbeat_sent(&mut self) -> bool {
        let zombie = self.awaiting_ack;
        self.awaiting_ack = true;
        zombie
    }

    /// Call on `op11 HeartbeatAck`.
    pub fn on_ack(&mut self) {
        self.awaiting_ack = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- SeenMessageIds ---

    #[test]
    fn first_sighting_of_an_id_is_new() {
        let mut seen = SeenMessageIds::new(8);
        assert!(seen.insert_is_new("msg-1"));
    }

    #[test]
    fn a_replayed_id_is_seen_only_once() {
        let mut seen = SeenMessageIds::new(8);
        assert!(seen.insert_is_new("msg-1"), "first sighting must be new");
        assert!(!seen.insert_is_new("msg-1"), "a RESUME replay of the same id must be recognized as a duplicate");
    }

    #[test]
    fn distinct_ids_are_each_new() {
        let mut seen = SeenMessageIds::new(8);
        assert!(seen.insert_is_new("msg-1"));
        assert!(seen.insert_is_new("msg-2"));
    }

    #[test]
    fn capacity_eviction_lets_the_oldest_id_be_reused_by_a_future_message() {
        let mut seen = SeenMessageIds::new(2);
        assert!(seen.insert_is_new("a"));
        assert!(seen.insert_is_new("b"));
        assert!(seen.insert_is_new("c"), "inserting past capacity must evict the oldest, not reject the newest");
        // "a" was evicted to make room for "c"; a message legitimately
        // reusing that (long-since-processed) slot no longer dedups against
        // it — expected, since the seen-set is bounded, not a full history.
        assert!(seen.insert_is_new("a"), "an evicted id must be treated as new again");
    }

    // --- SeenMessageIds snapshot / restore (durable cursor round-trip) ---

    #[test]
    fn snapshot_is_empty_for_a_fresh_set() {
        let seen = SeenMessageIds::new(8);
        assert!(seen.snapshot().is_empty());
    }

    #[test]
    fn snapshot_reflects_insertions_oldest_first() {
        let mut seen = SeenMessageIds::new(8);
        seen.insert_is_new("a");
        seen.insert_is_new("b");
        seen.insert_is_new("c");
        assert_eq!(seen.snapshot(), vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn from_snapshot_restores_dedup_behavior_for_every_id() {
        let mut original = SeenMessageIds::new(8);
        original.insert_is_new("msg-1");
        original.insert_is_new("msg-2");
        let snapshot = original.snapshot();

        // Simulates a restart: a fresh `SeenMessageIds` rebuilt purely from
        // the persisted snapshot, with no connection to `original`.
        let mut restored = SeenMessageIds::from_snapshot(&snapshot, 8);

        assert!(!restored.insert_is_new("msg-1"), "an id present in the snapshot must be recognized as already-seen after a simulated restart");
        assert!(!restored.insert_is_new("msg-2"), "same for the second persisted id");
        assert!(restored.insert_is_new("msg-3"), "an id never seen before must still be treated as new");
    }

    #[test]
    fn from_snapshot_never_exceeds_capacity_even_given_an_oversized_snapshot() {
        let oversized: Vec<String> = (0..10).map(|i| format!("id-{i}")).collect();
        let restored = SeenMessageIds::from_snapshot(&oversized, 4);
        assert_eq!(restored.snapshot().len(), 4, "restoring must respect the given capacity, not the snapshot's length");
    }

    #[test]
    fn from_snapshot_with_an_oversized_snapshot_keeps_the_most_recent_ids() {
        let oversized: Vec<String> = (0..10).map(|i| format!("id-{i}")).collect();
        let mut restored = SeenMessageIds::from_snapshot(&oversized, 4);
        // Check the survivors first: `insert_is_new` on an id already in the
        // set is a pure lookup (no mutation), but on a genuinely new id like
        // "id-0" below it inserts and evicts the oldest survivor — so that
        // check must run last, or it would evict "id-6" out from under the
        // very assertion meant to prove it survived.
        for recent in ["id-6", "id-7", "id-8", "id-9"] {
            assert!(!restored.insert_is_new(recent), "{recent} should have survived the capacity-bounded restore");
        }
        // The oldest ids (id-0..id-5) must have been evicted during replay,
        // so they're treated as new again.
        assert!(restored.insert_is_new("id-0"), "an evicted-during-restore id must be treated as new");
    }

    #[test]
    fn snapshot_and_restore_round_trip_is_bounded_by_capacity() {
        let mut seen = SeenMessageIds::new(3);
        for i in 0..100 {
            seen.insert_is_new(&format!("msg-{i}"));
        }
        let snapshot = seen.snapshot();
        assert!(snapshot.len() <= 3, "a snapshot of a capacity-3 set must never carry more than 3 ids, however many were ever inserted");

        let restored = SeenMessageIds::from_snapshot(&snapshot, 3);
        assert_eq!(restored.snapshot(), snapshot, "restoring a within-capacity snapshot must reproduce it exactly");
    }

    // --- HeartbeatTracker (zombie detection) ---

    #[test]
    fn first_heartbeat_never_reports_a_zombie() {
        let mut tracker = HeartbeatTracker::new();
        assert!(!tracker.on_heartbeat_sent(), "nothing was outstanding before the first beat");
    }

    #[test]
    fn a_second_heartbeat_with_no_intervening_ack_is_a_zombie() {
        let mut tracker = HeartbeatTracker::new();
        assert!(!tracker.on_heartbeat_sent());
        assert!(tracker.on_heartbeat_sent(), "the prior heartbeat was never acked before this one went out");
    }

    #[test]
    fn an_ack_between_heartbeats_keeps_the_connection_healthy() {
        let mut tracker = HeartbeatTracker::new();
        assert!(!tracker.on_heartbeat_sent());
        tracker.on_ack();
        assert!(!tracker.on_heartbeat_sent(), "the prior heartbeat was acked, so this is not a zombie");
    }

    #[test]
    fn zombie_state_persists_until_the_next_send_after_recovery() {
        let mut tracker = HeartbeatTracker::new();
        tracker.on_heartbeat_sent(); // beat 1: not a zombie
        assert!(tracker.on_heartbeat_sent(), "beat 2: zombie (beat 1 never acked)");
        // A late ack finally arrives (e.g. right before the reconnect logic
        // gets to act on it) — the tracker must recover cleanly.
        tracker.on_ack();
        assert!(!tracker.on_heartbeat_sent(), "beat 3: healthy again after the late ack");
    }
}
