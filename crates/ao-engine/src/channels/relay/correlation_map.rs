//! Generic in-flight correlation map: `thread_id -> reply-target` for a
//! synchronous chat channel's outbound relay.
//!
//! Telegram (`chat_id: i64`, see
//! [`crate::telegram::outbound::InFlightChats`]) and Discord (`ChannelOrigin`,
//! see [`crate::channels::discord::InFlightChannels`]) each keep their own
//! copy of this exact shape today — identical locking, insertion, and
//! lookup semantics, differing only in the value type a thread is
//! correlated to. [`CorrelationMap<V>`] is that shared shape, generic over
//! `V`.
//!
//! # Why `peek`, not `take`
//!
//! An `AgentEvent` carries `agent_id`/`thread_id` but never a channel's
//! reply target — that only exists at inbound dispatch time, when it's
//! [`Self::record`]ed here. An observer resolves it again at `RunEnded`. A
//! consuming read (`take`) looks tempting there — the observer isn't going
//! to see another `RunEnded` for this thread... except it can: an async
//! `Delegate` call spawned mid-turn ends the *triggering* run immediately
//! (a "delegated in background" hand-off), and the delegate's real answer
//! later re-enters the same bridge thread as a second, independent run.
//! Both runs' `RunEnded` need to resolve the *same* mapping to relay.
//! Consuming it on the first read would leave the second completion with
//! nothing to relay to — the delegate's real answer would simply be
//! dropped. [`Self::peek`] exists for exactly that: a repeatable,
//! non-consuming read, and it's what every relay call site uses.
//!
//! No consuming read is offered here, deliberately: the mapping is cleared
//! only by explicit invalidation ([`Self::remove`] /
//! [`Self::remove_if_matches`]), when the binding itself ends.

use std::collections::HashMap;
use std::sync::Mutex;

/// `thread_id -> V` for turns a transport just dispatched onto a bridge
/// thread. See the module doc for why reads here never consume.
pub(crate) struct CorrelationMap<V> {
    by_thread: Mutex<HashMap<String, V>>,
}

impl<V> Default for CorrelationMap<V> {
    fn default() -> Self {
        Self { by_thread: Mutex::new(HashMap::new()) }
    }
}

impl<V> CorrelationMap<V> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Called by a channel's inbound loop right before it submits a message
    /// onto `thread_id`. Overwrites any prior mapping for the same thread.
    pub(crate) fn record(&self, thread_id: &str, value: V) {
        self.by_thread.lock().unwrap_or_else(|e| e.into_inner()).insert(thread_id.to_string(), value);
    }

    /// Unconditionally drops the mapping for `thread_id`, discarding
    /// whatever value it held. Called when a binding ends outright —
    /// disabled, token rotated away, or deleted — so a later run on this
    /// thread (e.g. a stray delegate completion) has nothing left to relay
    /// to.
    pub(crate) fn remove(&self, thread_id: &str) {
        self.by_thread.lock().unwrap_or_else(|e| e.into_inner()).remove(thread_id);
    }

}

impl<V: Clone> CorrelationMap<V> {
    /// Reads the value mapped to `thread_id` without removing it — see the
    /// module doc for why this, not [`Self::take`], is the read every relay
    /// call site uses.
    pub(crate) fn peek(&self, thread_id: &str) -> Option<V> {
        self.by_thread.lock().unwrap_or_else(|e| e.into_inner()).get(thread_id).cloned()
    }

    /// Every currently-recorded `(thread_id, value)` pair. Unlike every
    /// other reader here, broadcast-lag recovery
    /// (`crate::channels::relay::observer::recover_lagged_replies`) doesn't
    /// know which thread(s) a lag actually affected — a `Lagged` error
    /// carries only a skip count, no thread context — so it has to check
    /// every thread this process is still tracking a reply target for.
    pub(crate) fn snapshot(&self) -> Vec<(String, V)> {
        self.by_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

impl<V: PartialEq> CorrelationMap<V> {
    /// Drops the mapping for `thread_id` only if it currently equals
    /// `value`. Used when unlinking one specific reply target: several
    /// origins can share one dedicated bridge thread (Telegram's
    /// multi-chat pairing), so unlinking one must not discard an in-flight
    /// reply actually destined for a different, still-linked origin.
    pub(crate) fn remove_if_matches(&self, thread_id: &str, value: &V) {
        let mut map = self.by_thread.lock().unwrap_or_else(|e| e.into_inner());
        if map.get(thread_id) == Some(value) {
            map.remove(thread_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peek_does_not_consume_the_mapping() {
        let map: CorrelationMap<i64> = CorrelationMap::new();
        map.record("thread-1", 555);
        assert_eq!(map.peek("thread-1"), Some(555));
        assert_eq!(
            map.peek("thread-1"),
            Some(555),
            "peek must be repeatable — it never consumes the mapping"
        );
    }

    #[test]
    fn peek_returns_none_for_an_unrecorded_thread() {
        let map: CorrelationMap<i64> = CorrelationMap::new();
        assert_eq!(map.peek("never-recorded"), None);
    }

    #[test]
    fn remove_clears_the_mapping() {
        let map: CorrelationMap<i64> = CorrelationMap::new();
        map.record("thread-1", 555);
        map.remove("thread-1");
        assert_eq!(map.peek("thread-1"), None);
    }

    #[test]
    fn remove_is_a_harmless_no_op_for_an_unrecorded_thread() {
        let map: CorrelationMap<i64> = CorrelationMap::new();
        map.remove("never-recorded");
        assert_eq!(map.peek("never-recorded"), None);
    }

    #[test]
    fn remove_if_matches_clears_only_on_an_exact_match() {
        let map: CorrelationMap<i64> = CorrelationMap::new();
        map.record("thread-1", 555);

        // A different value than what's recorded: the mapping survives —
        // the multi-origin-per-thread case, e.g. unlinking one chat must not
        // discard an in-flight reply actually destined for a different,
        // still-linked chat.
        map.remove_if_matches("thread-1", &111);
        assert_eq!(
            map.peek("thread-1"),
            Some(555),
            "a mismatched value must not clear a different value's mapping"
        );

        map.remove_if_matches("thread-1", &555);
        assert_eq!(map.peek("thread-1"), None, "an exact match must clear the mapping");
    }

    #[test]
    fn remove_if_matches_is_a_harmless_no_op_for_an_unrecorded_thread() {
        let map: CorrelationMap<i64> = CorrelationMap::new();
        map.remove_if_matches("never-recorded", &555);
        assert_eq!(map.peek("never-recorded"), None);
    }

    #[test]
    fn recording_again_overwrites_the_prior_mapping_for_the_same_thread() {
        let map: CorrelationMap<i64> = CorrelationMap::new();
        map.record("thread-1", 111);
        map.record("thread-1", 222);
        assert_eq!(map.peek("thread-1"), Some(222));
    }

    #[test]
    fn snapshot_returns_every_recorded_pair() {
        let map: CorrelationMap<i64> = CorrelationMap::new();
        map.record("thread-1", 111);
        map.record("thread-2", 222);
        let mut snap = map.snapshot();
        snap.sort();
        assert_eq!(snap, vec![("thread-1".to_string(), 111), ("thread-2".to_string(), 222)]);
    }

    #[test]
    fn snapshot_is_empty_for_a_fresh_map() {
        let map: CorrelationMap<i64> = CorrelationMap::new();
        assert!(map.snapshot().is_empty());
    }
}
