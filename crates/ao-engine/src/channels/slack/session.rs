//! Bounded, pure de-duplication of inbound Socket Mode events, keyed on each
//! event's own `event_id`.
//!
//! Slack redelivers: an envelope the connection doesn't acknowledge within a
//! few seconds is retried, and a `disconnect`/reconnect can replay a recent
//! window of events. Left unguarded, a redelivery would make the agent answer
//! an already-answered event a second time. `event_id` is stable across a
//! redelivery — it identifies the inner event, not the delivery attempt — so
//! recording the ids already dispatched and dropping repeats is sufficient.
//!
//! Two subtleties this type deliberately encodes:
//!
//! - **Key on `event_id`, never `envelope_id` or the message `ts`.**
//!   `envelope_id` is per-*delivery* (a redelivery carries a fresh one, so it
//!   would defeat dedup entirely) and is what the *acknowledgement* is keyed
//!   on instead; `ts` identifies a message but isn't present on every event
//!   shape. Only `event_id` is both per-event-stable and universally present.
//! - **Bounded, so a long-lived connection's memory can't grow without
//!   limit.** The oldest id is evicted once the set is full — safe because a
//!   redelivery only ever replays a short recent window, never the whole
//!   history of the connection.
//!
//! This is the in-memory *live* mirror of the durable
//! [`ao_protocol::channel_cursor::ChannelCursor::Slack::seen_event_ids`]
//! cursor — not a second, independent store. The runner loads the persisted
//! cursor into a [`SeenEventIds`] at connect via [`SeenEventIds::from_snapshot`]
//! and writes it back with [`SeenEventIds::snapshot`]; those two methods are
//! the only bridge between this live set and its persisted form. Unlike
//! Discord there is no resumable session to carry alongside it — Socket Mode
//! always opens a fresh connection — so this dedup set is the whole of the
//! cursor.

use std::collections::{HashSet, VecDeque};

/// Bounded FIFO de-dup set for inbound Socket Mode `event_id`s.
///
/// Insertion order is tracked in `order` so the oldest id can be evicted once
/// `capacity` is reached; `set` backs the O(1) membership test. The two are
/// kept in lockstep by every mutating method, so `set` always holds exactly
/// the ids currently in `order`.
pub struct SeenEventIds {
    order: VecDeque<String>,
    set: HashSet<String>,
    capacity: usize,
}

impl SeenEventIds {
    pub fn new(capacity: usize) -> Self {
        Self { order: VecDeque::new(), set: HashSet::new(), capacity: capacity.max(1) }
    }

    /// Records `event_id` and returns `true` if this is the first time it's
    /// been seen; returns `false` (state left untouched beyond the lookup) for
    /// a redelivery. Inserting past `capacity` evicts the oldest id rather than
    /// rejecting the newest.
    pub fn insert_is_new(&mut self, event_id: &str) -> bool {
        if self.set.contains(event_id) {
            return false;
        }
        self.set.insert(event_id.to_string());
        self.order.push_back(event_id.to_string());
        if self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        true
    }

    /// Pure membership test — `true` if `event_id` is currently tracked, with
    /// no mutation. (`insert_is_new` is the primitive callers use to dedup and
    /// record in one step; this is for the read-only check.)
    pub fn contains(&self, event_id: &str) -> bool {
        self.set.contains(event_id)
    }

    /// Oldest-first snapshot of every id currently tracked, for persistence
    /// into [`ao_protocol::channel_cursor::ChannelCursor::Slack::seen_event_ids`].
    /// Always has at most `capacity` entries, since [`Self::insert_is_new`]
    /// never lets the set grow past it — the persisted cursor inherits that
    /// same bound for free.
    pub fn snapshot(&self) -> Vec<String> {
        self.order.iter().cloned().collect()
    }

    /// Rebuilds a [`SeenEventIds`] from a persisted, oldest-first snapshot (the
    /// shape [`Self::snapshot`] produces), replaying each id through
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sighting_of_an_event_is_new() {
        let mut seen = SeenEventIds::new(8);
        assert!(seen.insert_is_new("Ev001"));
    }

    #[test]
    fn a_redelivered_event_is_seen_only_once() {
        let mut seen = SeenEventIds::new(8);
        assert!(seen.insert_is_new("Ev001"), "first sighting must be new");
        assert!(
            !seen.insert_is_new("Ev001"),
            "a redelivery of the same event_id must be recognized as a duplicate"
        );
    }

    #[test]
    fn distinct_event_ids_are_each_new() {
        let mut seen = SeenEventIds::new(8);
        assert!(seen.insert_is_new("Ev001"));
        assert!(seen.insert_is_new("Ev002"));
        assert!(seen.insert_is_new("Ev003"));
    }

    #[test]
    fn contains_is_a_pure_lookup_tracking_insert_is_new() {
        let mut seen = SeenEventIds::new(8);
        assert!(!seen.contains("Ev001"), "nothing recorded yet");
        seen.insert_is_new("Ev001");
        assert!(seen.contains("Ev001"), "an inserted id must be found");
        assert!(!seen.contains("Ev002"), "an id never inserted must be absent");
        // A pure lookup must not itself record the id.
        assert!(!seen.contains("Ev002"));
        assert!(seen.insert_is_new("Ev002"), "contains() must not have recorded Ev002");
    }

    #[test]
    fn capacity_eviction_lets_the_oldest_event_be_reused() {
        let mut seen = SeenEventIds::new(2);
        assert!(seen.insert_is_new("a"));
        assert!(seen.insert_is_new("b"));
        assert!(
            seen.insert_is_new("c"),
            "inserting past capacity must evict the oldest, not reject the newest"
        );
        assert!(!seen.contains("a"), "the oldest id must have been evicted to make room for c");
        assert!(seen.contains("b"));
        assert!(seen.contains("c"));
        // "a" was evicted; an event legitimately reusing that long-since-
        // processed slot no longer dedups against it — expected, since the
        // set is bounded, not a full history.
        assert!(seen.insert_is_new("a"), "an evicted id must be treated as new again");
    }

    #[test]
    fn snapshot_is_empty_for_a_fresh_set() {
        let seen = SeenEventIds::new(8);
        assert!(seen.snapshot().is_empty());
    }

    #[test]
    fn snapshot_reflects_insertions_oldest_first() {
        let mut seen = SeenEventIds::new(8);
        seen.insert_is_new("a");
        seen.insert_is_new("b");
        seen.insert_is_new("c");
        assert_eq!(seen.snapshot(), vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn from_snapshot_restores_dedup_behavior_for_every_id() {
        let mut original = SeenEventIds::new(8);
        original.insert_is_new("Ev001");
        original.insert_is_new("Ev002");
        let snapshot = original.snapshot();

        // Simulates a reconnect after a restart: a fresh set rebuilt purely
        // from the persisted cursor, with no connection to `original`.
        let mut restored = SeenEventIds::from_snapshot(&snapshot, 8);

        assert!(
            !restored.insert_is_new("Ev001"),
            "an id present in the restored cursor must be recognized as already-seen"
        );
        assert!(!restored.insert_is_new("Ev002"), "same for the second persisted id");
        assert!(restored.insert_is_new("Ev003"), "an id never seen before must still be new");
    }

    #[test]
    fn from_snapshot_never_exceeds_capacity_even_given_an_oversized_snapshot() {
        let oversized: Vec<String> = (0..10).map(|i| format!("Ev{i:03}")).collect();
        let restored = SeenEventIds::from_snapshot(&oversized, 4);
        assert_eq!(
            restored.snapshot().len(),
            4,
            "restoring must respect the given capacity, not the snapshot's length"
        );
    }

    #[test]
    fn from_snapshot_with_an_oversized_snapshot_keeps_the_most_recent_ids() {
        let oversized: Vec<String> = (0..10).map(|i| format!("Ev{i:03}")).collect();
        let restored = SeenEventIds::from_snapshot(&oversized, 4);
        // The four most recent ids survive the capacity-bounded restore...
        for recent in ["Ev006", "Ev007", "Ev008", "Ev009"] {
            assert!(restored.contains(recent), "{recent} should have survived the restore");
        }
        // ...and the older ones were evicted during replay.
        assert!(!restored.contains("Ev000"), "an evicted-during-restore id must be gone");
        assert!(!restored.contains("Ev005"), "the last evicted id must be gone");
    }

    #[test]
    fn snapshot_and_restore_round_trip_is_bounded_by_capacity() {
        let mut seen = SeenEventIds::new(3);
        for i in 0..100 {
            seen.insert_is_new(&format!("Ev{i:03}"));
        }
        let snapshot = seen.snapshot();
        assert!(
            snapshot.len() <= 3,
            "a snapshot of a capacity-3 set must never carry more than 3 ids, however many were inserted"
        );

        let restored = SeenEventIds::from_snapshot(&snapshot, 3);
        assert_eq!(
            restored.snapshot(),
            snapshot,
            "restoring a within-capacity snapshot must reproduce it exactly"
        );
    }
}
