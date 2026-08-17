//! Process-local record of which bridge threads this process currently
//! holds a binding's single-writer lease for.
//!
//! `ChannelBridge`'s outbound relay observers (Telegram, Discord, Slack) are
//! each a single task shared across every binding of that channel kind for
//! the whole process — they are not started or stopped per binding, so they
//! keep running even for a binding whose lease this process has lost (or
//! never held). [`LeaseGate`] is the explicit check that stands in front of
//! every outbound send: [`crate::channels::relay::observer::handle_relay_event`]
//! and [`crate::channels::relay::observer::recover_lagged_replies`] both
//! consult it before relaying anything, so a non-holder can never emit an
//! outbound message on a holder's behalf — two processes racing (or
//! failing over) on the same data root must never both reply to the same
//! chat.
//!
//! Keyed by *binding*, not by thread id alone: Telegram still gives a
//! binding exactly one dedicated `bridge_thread_id` for its whole lifetime,
//! but Discord and Slack each mint a fresh per-conversation thread id for
//! every distinct conversation they see (Discord's `channel_id`; Slack's
//! `(team_id, channel_id, thread_ts)` — see
//! `crate::channels::discord::runner::resolve_discord_conversation_thread`
//! and `crate::channels::slack::runner::resolve_bridge_thread`) — a single
//! binding can have many active thread ids at once, all of which must clear
//! together the moment that binding's lease is lost. [`mark_active`] adds
//! one thread id to a binding's set; [`mark_inactive`] drops the whole set
//! at once; [`forget_thread`] drops a single thread id without disturbing
//! any siblings still registered under the same binding — the counterpart a
//! conversation registry GC pass calls when it evicts one idle conversation
//! rather than the whole binding losing its lease. [`is_active`] stays keyed on the thread id alone, unchanged from
//! callers' point of view: it answers "is this thread registered under any
//! binding this process currently holds," without the relay path ever
//! needing to resolve a thread back to its owning binding itself.
//!
//! `ChannelBridge::reconcile` is the only writer for Telegram: it marks a
//! binding's one thread active the moment it starts (or keeps running) that
//! binding's inbound task under a successfully claimed lease, and marks the
//! binding inactive the moment it stops for any reason — disabled,
//! reconfigured, lease lost, or process shutdown. Discord and Slack each
//! additionally mark every freshly resolved per-conversation thread active
//! from within their own inbound dispatch (see
//! `resolve_discord_conversation_thread` / `resolve_bridge_thread`), since
//! `reconcile` never sees a placeholder thread id for either of them today —
//! Discord no longer provisions one at all, and Slack's is a placeholder
//! never used for real message routing.
//!
//! [`mark_active`]: LeaseGate::mark_active
//! [`mark_inactive`]: LeaseGate::mark_inactive
//! [`forget_thread`]: LeaseGate::forget_thread
//! [`is_active`]: LeaseGate::is_active

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// A binding's set of currently-active thread ids, plus the reverse index
/// that makes [`LeaseGate::is_active`] an O(1) lookup instead of a scan over
/// every binding's set.
#[derive(Default)]
struct LeaseGateState {
    threads_by_binding: HashMap<String, HashSet<String>>,
    binding_by_thread: HashMap<String, String>,
}

#[derive(Default)]
pub(crate) struct LeaseGate {
    state: Mutex<LeaseGateState>,
}

impl LeaseGate {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Registers `thread_id` as one this process currently holds
    /// `binding_id`'s single-writer lease for. A binding can have many
    /// thread ids registered at once (Slack's per-conversation threads);
    /// Telegram and Discord each register exactly one, mirroring their
    /// single `bridge_thread_id`.
    ///
    /// If `thread_id` was already registered under a *different* binding —
    /// which should never happen in practice, since a thread id belongs to
    /// exactly one binding for its lifetime — it's moved rather than left
    /// double-registered, keeping the reverse index consistent.
    pub(crate) fn mark_active(&self, binding_id: &str, thread_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prior_binding) = state.binding_by_thread.get(thread_id).cloned() {
            if prior_binding != binding_id {
                if let Some(prior_set) = state.threads_by_binding.get_mut(prior_binding.as_str()) {
                    prior_set.remove(thread_id);
                }
            }
        }
        state.threads_by_binding.entry(binding_id.to_string()).or_default().insert(thread_id.to_string());
        state.binding_by_thread.insert(thread_id.to_string(), binding_id.to_string());
    }

    /// Clears every thread id registered under `binding_id` — this process
    /// no longer holds (or never held) the binding's lease, so its own
    /// outbound observers must never relay for any of it any longer. A
    /// binding with nothing registered is a harmless no-op.
    pub(crate) fn mark_inactive(&self, binding_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(thread_ids) = state.threads_by_binding.remove(binding_id) {
            for thread_id in thread_ids {
                state.binding_by_thread.remove(&thread_id);
            }
        }
    }

    /// Whether this process currently holds the lease for `thread_id` — the
    /// gate every outbound relay call site checks first. `true` iff
    /// `thread_id` is registered under some currently-active binding.
    pub(crate) fn is_active(&self, thread_id: &str) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).binding_by_thread.contains_key(thread_id)
    }

    /// Drops `thread_id` alone, leaving the rest of its binding's registered
    /// threads untouched — the per-thread counterpart to [`mark_inactive`],
    /// which clears a whole binding at once. This is what a conversation
    /// registry GC pass calls for each row it evicts (idle timeout or
    /// over-cap LRU): the conversation is gone from durable storage, so this
    /// process must stop believing it holds the lease for that one thread,
    /// without disturbing any of the binding's other live conversations. A
    /// `thread_id` that isn't registered anywhere is a harmless no-op.
    ///
    /// [`mark_inactive`]: LeaseGate::mark_inactive
    pub(crate) fn forget_thread(&self, thread_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(binding_id) = state.binding_by_thread.remove(thread_id) {
            if let Some(thread_ids) = state.threads_by_binding.get_mut(&binding_id) {
                thread_ids.remove(thread_id);
                if thread_ids.is_empty() {
                    state.threads_by_binding.remove(&binding_id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_never_marked_active_is_inactive() {
        let gate = LeaseGate::new();
        assert!(!gate.is_active("thread-1"));
    }

    #[test]
    fn marking_active_makes_a_thread_pass_the_gate() {
        let gate = LeaseGate::new();
        gate.mark_active("binding-1", "thread-1");
        assert!(gate.is_active("thread-1"));
    }

    #[test]
    fn marking_inactive_closes_the_gate_again() {
        let gate = LeaseGate::new();
        gate.mark_active("binding-1", "thread-1");
        gate.mark_inactive("binding-1");
        assert!(!gate.is_active("thread-1"));
    }

    #[test]
    fn marking_inactive_a_binding_never_marked_active_is_a_harmless_no_op() {
        let gate = LeaseGate::new();
        gate.mark_inactive("never-active-binding");
        assert!(!gate.is_active("never-active"));
    }

    #[test]
    fn threads_are_gated_independently() {
        let gate = LeaseGate::new();
        gate.mark_active("binding-1", "thread-1");
        assert!(gate.is_active("thread-1"));
        assert!(!gate.is_active("thread-2"));
    }

    /// The core Slack scenario: one binding can register many thread ids —
    /// its per-conversation threads — and every one of them passes the gate
    /// while the binding holds its lease.
    #[test]
    fn many_thread_ids_can_be_registered_under_one_binding() {
        let gate = LeaseGate::new();
        gate.mark_active("slack-binding", "conv-thread-1");
        gate.mark_active("slack-binding", "conv-thread-2");
        gate.mark_active("slack-binding", "conv-thread-3");

        assert!(gate.is_active("conv-thread-1"));
        assert!(gate.is_active("conv-thread-2"));
        assert!(gate.is_active("conv-thread-3"));
    }

    /// [`LeaseGate::mark_inactive`] must clear an entire binding's thread set
    /// in one call — the lease-loss cleanup a busy Slack workspace with many
    /// live conversations under one binding depends on.
    #[test]
    fn mark_inactive_clears_every_thread_registered_under_that_binding() {
        let gate = LeaseGate::new();
        gate.mark_active("slack-binding", "conv-thread-1");
        gate.mark_active("slack-binding", "conv-thread-2");
        gate.mark_active("slack-binding", "conv-thread-3");

        gate.mark_inactive("slack-binding");

        assert!(!gate.is_active("conv-thread-1"));
        assert!(!gate.is_active("conv-thread-2"));
        assert!(!gate.is_active("conv-thread-3"));
    }

    /// Two bindings' thread sets are independent: clearing one must never
    /// disturb the other's, even when both are live at once (two bindings
    /// held by this process concurrently).
    #[test]
    fn clearing_one_binding_leaves_a_different_bindings_threads_active() {
        let gate = LeaseGate::new();
        gate.mark_active("binding-a", "thread-a-1");
        gate.mark_active("binding-a", "thread-a-2");
        gate.mark_active("binding-b", "thread-b-1");

        gate.mark_inactive("binding-a");

        assert!(!gate.is_active("thread-a-1"));
        assert!(!gate.is_active("thread-a-2"));
        assert!(gate.is_active("thread-b-1"), "clearing binding-a must not affect binding-b's threads");
    }

    /// Re-marking the same binding active after a clear (a lease reclaimed
    /// after being lost) must work exactly like a fresh registration.
    #[test]
    fn a_binding_can_be_marked_active_again_after_being_cleared() {
        let gate = LeaseGate::new();
        gate.mark_active("binding-1", "thread-1");
        gate.mark_inactive("binding-1");
        assert!(!gate.is_active("thread-1"));

        gate.mark_active("binding-1", "thread-1");
        assert!(gate.is_active("thread-1"));
    }

    /// [`LeaseGate::forget_thread`] must drop only the named thread — its
    /// siblings under the same binding stay active. This is the isolation
    /// guarantee the registry GC pass depends on: evicting one idle
    /// conversation must never take down every other live conversation
    /// sharing that binding (e.g. Slack's many-threads-per-binding case).
    #[test]
    fn forget_thread_removes_only_the_named_thread() {
        let gate = LeaseGate::new();
        gate.mark_active("binding-1", "thread-1");
        gate.mark_active("binding-1", "thread-2");

        gate.forget_thread("thread-1");

        assert!(!gate.is_active("thread-1"));
        assert!(gate.is_active("thread-2"), "forgetting one thread must not affect a sibling under the same binding");
    }

    #[test]
    fn forget_thread_on_an_unknown_thread_is_a_harmless_no_op() {
        let gate = LeaseGate::new();
        gate.mark_active("binding-1", "thread-1");

        gate.forget_thread("never-registered");

        assert!(gate.is_active("thread-1"), "forgetting an untracked thread must not disturb anything else");
    }

    /// Forgetting a binding's last remaining thread must not leave a
    /// dangling empty entry behind in `threads_by_binding` — re-marking that
    /// binding active later should behave exactly like a fresh binding, not
    /// one that already has stale bookkeeping.
    #[test]
    fn forget_thread_removes_the_binding_entry_once_its_last_thread_is_gone() {
        let gate = LeaseGate::new();
        gate.mark_active("binding-1", "thread-1");

        gate.forget_thread("thread-1");

        let state = gate.state.lock().unwrap();
        assert!(
            !state.threads_by_binding.contains_key("binding-1"),
            "a binding whose last thread was forgotten must leave no dangling entry"
        );
        assert!(!state.binding_by_thread.contains_key("thread-1"));
    }
}
