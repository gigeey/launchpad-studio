//! Cold/warm engagement state machine deciding whether the bot should
//! respond to a given inbound Discord message, so a binding stops replying
//! to every single message in a shared channel it was only ever mentioned
//! in once.
//!
//! Each Discord conversation — identified by its channel id, and note a
//! thread has its own channel id distinct from its parent — is either COLD
//! (respond only when explicitly @-mentioned) or WARM (respond to every
//! authorized message). [`is_bot_mentioned`](super::security::is_bot_mentioned)
//! already excludes `@everyone`/`@here` from counting as a mention, so
//! nothing here needs to special-case those broadcasts: a `bot_mentioned:
//! false` input is exactly what a broadcast produces.
//!
//! Four ways into WARM, only one of which ever decays:
//!   - a DM is always warm and never decays (there's no "other people's
//!     conversation" to leak into — it's just the bot and one user).
//!   - a thread the bot itself created (`owner_id` == the bot's own user
//!     id) is always warm and never decays, on the same reasoning.
//!   - a mention *in a thread* warms that thread, following whichever
//!     [`ThreadFollowMode`] the binding is configured with:
//!     [`ThreadFollowMode::OneShot`] answers the mentioned message and
//!     immediately returns to mention-only (the thread never actually
//!     enters `warm`); [`ThreadFollowMode::StickyDecay`] (the default)
//!     stays warm for a while, decaying per the rules below;
//!     [`ThreadFollowMode::Always`] stays warm forever once mentioned,
//!     never decaying.
//!   - a mention in a normal (non-thread) guild channel gets a response to
//!     that one message but deliberately does **not** warm the channel,
//!     regardless of `thread_follow` (which only ever governs threads).
//!     Warming a busy shared channel from a single mention is the exact
//!     failure mode this module exists to prevent, so main channels stay
//!     permanently mention-only.
//!
//! A [`ThreadFollowMode::StickyDecay`] thread decays back to COLD on
//! whichever comes first:
//!   - `idle_timeout` since the thread's *last mention* (not its last
//!     message) — a busy thread that keeps getting unmentioned traffic the
//!     bot dutifully answers should still go quiet if nobody has actually
//!     addressed it in a while, or
//!   - `message_budget` consecutive unmentioned messages answered since the
//!     last mention — a safety valve independent of wall-clock time, so a
//!     rapid-fire unmentioned conversation can't keep a thread warm forever
//!     just because the messages arrive faster than `idle_timeout` ticks.
//!
//! Neither knob applies to `OneShot` (never enters `warm` at all) or
//! `Always` (never leaves it once entered).
//!
//! [`EngagementTracker::decide`] takes the current instant as an explicit
//! `now` field on [`EngagementInput`] rather than ever calling
//! [`chrono::Utc::now`] itself, so decay is provable in tests by
//! constructing timestamps directly instead of sleeping.
//!
//! # Why in-memory only
//!
//! Warm state here is process memory, not a persisted store, and that's a
//! deliberate choice for this version rather than an oversight: the
//! existing `ChannelCursor::Discord` persisted cursor is keyed by
//! `(agent_id, binding_id)`, not per conversation, so persisting per-thread
//! warmth would mean standing up new storage infrastructure, which is out
//! of scope here. The consequence of *not* persisting is a process restart
//! reverts every conversation to COLD — the bot goes quiet in threads it
//! was previously warm in until someone re-mentions it once. That fails
//! toward silence, never toward spam, which is the direction this failure
//! mode should fail in: a bot that occasionally needs a nudge after a
//! restart is a minor annoyance, a bot that resumes replying to every
//! message in a shared channel it has no business being warm in is the bug
//! this whole module exists to prevent.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

use ao_protocol::agent::ThreadFollowMode;

/// Whether [`EngagementTracker::decide`] says the bot should respond to the
/// message described by the [`EngagementInput`] it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngagementDecision {
    Respond,
    Ignore,
}

/// What [`EngagementTracker::decide`] returned: the respond/ignore decision
/// itself, plus whether this call just crossed the conversation from cold
/// (or decayed) to warm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngagementOutcome {
    pub decision: EngagementDecision,
    /// `true` exactly when a fresh mention in a thread just warmed it and
    /// the thread was *not* already warm the moment before — never set for
    /// a DM or a bot-owned thread (both start, and stay, warm without ever
    /// "transitioning"), and never set for a non-thread channel (which never
    /// enters `warm` at all). This is the signal `super::runner` uses to
    /// fire a one-time history backfill instead of on every message.
    pub became_warm: bool,
}

/// The facts [`EngagementTracker::decide`] needs about one inbound message.
/// Callers (eventually `super::runner`) build this from a parsed
/// `MESSAGE_CREATE` plus the [`super::channel_meta::ChannelMeta`] lookup and
/// [`super::security::is_bot_mentioned`] call it already performs for other
/// reasons.
pub struct EngagementInput<'a> {
    /// The Discord channel id the message arrived on — a thread's own
    /// channel id, not its parent's, matching what
    /// [`crate::channels::submit_inbound_message`] receives as
    /// `conversation_id`.
    pub conversation_id: &'a str,
    /// `true` when the message's `guild_id` is `None`, i.e. a DM.
    pub is_dm: bool,
    pub is_thread: bool,
    /// `true` when this is a thread and its `owner_id` equals the bot's own
    /// user id — the bot created this thread itself.
    pub thread_owner_is_bot: bool,
    /// Whether the bot was explicitly @-mentioned in this message. Must
    /// already exclude `@everyone`/`@here` — see the module doc. Also
    /// already folds in a binding's `require_mention: false` escape hatch:
    /// when that's set, the caller passes `true` here unconditionally, so
    /// this module has no opinion of its own on why a message counts as
    /// mentioned, only on what to do once it does.
    pub bot_mentioned: bool,
    pub now: DateTime<Utc>,
}

/// How long a `StickyDecay` thread stays warm after its last mention with no
/// further mention, and how many consecutive unmentioned messages it
/// tolerates answering in that window before going quiet anyway — see the
/// module doc for why both exist independently. Both are overridable per
/// call via [`EngagementParams`], which `super::runner` builds from the
/// binding's persisted `ChannelKindConfig::Discord` fields
/// (`thread_idle_timeout_minutes`/`thread_message_budget`).
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_MESSAGE_BUDGET: u32 = 10;

/// Overridable knobs for [`EngagementTracker::decide`]'s thread behavior.
/// See [`DEFAULT_IDLE_TIMEOUT`] / [`DEFAULT_MESSAGE_BUDGET`] for the decay
/// defaults [`Default`] uses; `thread_follow` defaults to
/// [`ThreadFollowMode::StickyDecay`], the only mode either decay knob
/// actually affects.
pub struct EngagementParams {
    pub idle_timeout: Duration,
    pub message_budget: u32,
    pub thread_follow: ThreadFollowMode,
}

impl Default for EngagementParams {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            message_budget: DEFAULT_MESSAGE_BUDGET,
            thread_follow: ThreadFollowMode::default(),
        }
    }
}

/// A warmed thread's decay-relevant state: when it was last mentioned, and
/// how many consecutive unmentioned messages have been answered since then.
#[derive(Debug, Clone, Copy)]
struct WarmState {
    last_mentioned_at: DateTime<Utc>,
    unmentioned_streak: u32,
}

/// Per-conversation warm state, held in process memory only — see the
/// module doc's "Why in-memory only" section. One instance is shared across
/// every conversation a binding sees; [`Self::decide`] takes `&self` and
/// guards the map with a `Mutex`, the same interior-mutability shape
/// [`super::channel_meta::ChannelMetaCache`] and
/// [`super::InFlightChannels`] already use for their own per-conversation
/// state in this module.
pub struct EngagementTracker {
    warm: Mutex<HashMap<String, WarmState>>,
}

impl EngagementTracker {
    pub fn new() -> Self {
        Self { warm: Mutex::new(HashMap::new()) }
    }

    /// Decides whether to respond to `input`, performing whatever COLD/WARM
    /// transition that decision implies — see the module doc for the full
    /// rule set. Pure aside from the tracker's own interior state: never
    /// reads the clock itself, only `input.now`.
    pub fn decide(&self, input: &EngagementInput, params: &EngagementParams) -> EngagementOutcome {
        // (a) DMs are always warm and never decay — no shared-channel
        // failure mode to guard against here.
        if input.is_dm {
            return EngagementOutcome { decision: EngagementDecision::Respond, became_warm: false };
        }
        // (b) A thread the bot created itself is always warm and never
        // decays, for the same reason.
        if input.is_thread && input.thread_owner_is_bot {
            return EngagementOutcome { decision: EngagementDecision::Respond, became_warm: false };
        }
        // (d) A non-thread guild channel is permanently mention-only: a
        // mention gets a response to that one message, but the channel is
        // never entered into `warm` at all, so nothing here can ever warm
        // it.
        if !input.is_thread {
            let decision = if input.bot_mentioned { EngagementDecision::Respond } else { EngagementDecision::Ignore };
            return EngagementOutcome { decision, became_warm: false };
        }

        // (c) A thread that isn't bot-owned: mention-warmed, following
        // whichever `ThreadFollowMode` the binding is configured with.
        match params.thread_follow {
            ThreadFollowMode::OneShot => self.decide_one_shot(input),
            ThreadFollowMode::Always => self.decide_always(input),
            ThreadFollowMode::StickyDecay => self.decide_sticky_decay(input, params),
        }
    }

    /// `ThreadFollowMode::OneShot`: a mention gets exactly one response and
    /// the thread never actually enters `warm` — the very next unmentioned
    /// message finds it cold again. Still reports `became_warm: true` on a
    /// mention so `super::runner` fires its one-time backfill for that
    /// response, even though no state persists afterward.
    fn decide_one_shot(&self, input: &EngagementInput) -> EngagementOutcome {
        // Defends against a stale entry surviving a config change away from
        // `StickyDecay`/`Always` — OneShot must never read as warm.
        self.warm.lock().unwrap_or_else(|e| e.into_inner()).remove(input.conversation_id);
        let decision = if input.bot_mentioned { EngagementDecision::Respond } else { EngagementDecision::Ignore };
        EngagementOutcome { decision, became_warm: input.bot_mentioned }
    }

    /// `ThreadFollowMode::Always`: a mention warms the thread exactly like
    /// `StickyDecay`, but once warm it never decays — `idle_timeout` and
    /// `message_budget` are never consulted.
    fn decide_always(&self, input: &EngagementInput) -> EngagementOutcome {
        let mut warm = self.warm.lock().unwrap_or_else(|e| e.into_inner());
        let was_warm = warm.contains_key(input.conversation_id);

        if input.bot_mentioned {
            warm.insert(
                input.conversation_id.to_string(),
                WarmState { last_mentioned_at: input.now, unmentioned_streak: 0 },
            );
            return EngagementOutcome { decision: EngagementDecision::Respond, became_warm: !was_warm };
        }

        if !was_warm {
            return EngagementOutcome { decision: EngagementDecision::Ignore, became_warm: false };
        }
        EngagementOutcome { decision: EngagementDecision::Respond, became_warm: false }
    }

    /// `ThreadFollowMode::StickyDecay` (the default): a mention warms the
    /// thread and resets the message budget; unmentioned messages keep
    /// landing while warm until `idle_timeout` or `message_budget` decays it
    /// back to cold — see the module doc.
    fn decide_sticky_decay(&self, input: &EngagementInput, params: &EngagementParams) -> EngagementOutcome {
        let mut warm = self.warm.lock().unwrap_or_else(|e| e.into_inner());
        let existing = warm.get(input.conversation_id).copied();
        let currently_warm = existing.is_some_and(|w| !has_gone_idle(input.now, w.last_mentioned_at, params.idle_timeout));

        if input.bot_mentioned {
            // A mention (re-)warms the thread and resets the message
            // budget, whether it was cold, decayed, or already warm. Only
            // counts as a COLD->WARM transition when it wasn't already warm
            // the moment before — re-mentioning an already-warm thread just
            // extends its warmth, it doesn't re-trigger a transition.
            warm.insert(
                input.conversation_id.to_string(),
                WarmState { last_mentioned_at: input.now, unmentioned_streak: 0 },
            );
            return EngagementOutcome { decision: EngagementDecision::Respond, became_warm: !currently_warm };
        }

        if !currently_warm {
            // Cold, or warm but past its idle timeout: drop any stale entry
            // so the map never grows for conversations that aren't
            // actually warm, and ignore.
            warm.remove(input.conversation_id);
            return EngagementOutcome { decision: EngagementDecision::Ignore, became_warm: false };
        }

        let mut state = existing.expect("currently_warm is only true when an entry exists");
        state.unmentioned_streak += 1;
        if state.unmentioned_streak >= params.message_budget {
            // Budget exhausted: this message still lands while warm and
            // gets a response, but the thread decays as of right now — the
            // next unmentioned message finds it cold.
            warm.remove(input.conversation_id);
        } else {
            warm.insert(input.conversation_id.to_string(), state);
        }
        EngagementOutcome { decision: EngagementDecision::Respond, became_warm: false }
    }
}

impl Default for EngagementTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `idle_timeout` has fully elapsed between `last_mentioned_at` and
/// `now`. A `idle_timeout` too large for [`chrono::Duration`] to represent
/// (which `std::time::Duration` can express but `chrono`'s signed range
/// cannot) is treated as "never times out" rather than panicking — an
/// oversized config value should fail toward staying warm forever, not
/// toward crashing the gateway loop.
fn has_gone_idle(now: DateTime<Utc>, last_mentioned_at: DateTime<Utc>, idle_timeout: Duration) -> bool {
    match chrono::Duration::from_std(idle_timeout) {
        Ok(timeout) => now.signed_duration_since(last_mentioned_at) >= timeout,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_time() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().expect("valid fixed timestamp")
    }

    fn input<'a>(
        conversation_id: &'a str,
        is_dm: bool,
        is_thread: bool,
        thread_owner_is_bot: bool,
        bot_mentioned: bool,
        now: DateTime<Utc>,
    ) -> EngagementInput<'a> {
        EngagementInput { conversation_id, is_dm, is_thread, thread_owner_is_bot, bot_mentioned, now }
    }

    #[test]
    fn dm_always_responds_even_without_a_mention() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let outcome = tracker.decide(&input("dm-1", true, false, false, false, base_time()), &params);
        assert_eq!(outcome.decision, EngagementDecision::Respond);
        assert!(!outcome.became_warm, "a DM never 'transitions' — it's always warm");
    }

    #[test]
    fn dm_keeps_responding_across_many_messages_with_no_mention() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let mut now = base_time();
        for _ in 0..(DEFAULT_MESSAGE_BUDGET * 3) {
            let outcome = tracker.decide(&input("dm-1", true, false, false, false, now), &params);
            assert_eq!(outcome.decision, EngagementDecision::Respond);
            now += chrono::Duration::hours(1);
        }
    }

    #[test]
    fn bot_owned_thread_always_responds_without_a_mention() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let outcome = tracker.decide(&input("thread-1", false, true, true, false, base_time()), &params);
        assert_eq!(outcome.decision, EngagementDecision::Respond);
        assert!(!outcome.became_warm, "a bot-owned thread never 'transitions' — it's always warm");
    }

    #[test]
    fn bot_owned_thread_never_decays() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let mut now = base_time();
        for _ in 0..(DEFAULT_MESSAGE_BUDGET * 3) {
            let outcome = tracker.decide(&input("thread-1", false, true, true, false, now), &params);
            assert_eq!(outcome.decision, EngagementDecision::Respond);
            now += params.idle_timeout * 2;
        }
    }

    #[test]
    fn mention_in_a_thread_warms_it_and_then_unmentioned_messages_respond() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let now = base_time();

        let mentioned = tracker.decide(&input("thread-1", false, true, false, true, now), &params);
        assert_eq!(mentioned.decision, EngagementDecision::Respond);
        assert!(mentioned.became_warm, "a mention in a cold thread must report the COLD->WARM transition");

        let followup = tracker.decide(
            &input("thread-1", false, true, false, false, now + chrono::Duration::seconds(1)),
            &params,
        );
        assert_eq!(followup.decision, EngagementDecision::Respond);
        assert!(!followup.became_warm, "an unmentioned message on an already-warm thread is not a transition");
    }

    #[test]
    fn re_mentioning_an_already_warm_thread_does_not_report_a_new_transition() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let now = base_time();

        let first = tracker.decide(&input("thread-1", false, true, false, true, now), &params);
        assert!(first.became_warm, "the first mention does warm the thread");

        let second = tracker.decide(
            &input("thread-1", false, true, false, true, now + chrono::Duration::seconds(1)),
            &params,
        );
        assert_eq!(second.decision, EngagementDecision::Respond);
        assert!(!second.became_warm, "re-mentioning an already-warm thread must not report another transition");
    }

    #[test]
    fn mention_in_a_non_thread_channel_responds_but_does_not_warm_it() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let now = base_time();

        let mentioned = tracker.decide(&input("channel-1", false, false, false, true, now), &params);
        assert_eq!(mentioned.decision, EngagementDecision::Respond);
        assert!(!mentioned.became_warm, "a non-thread channel never warms, so it never transitions either");

        let followup = tracker.decide(
            &input("channel-1", false, false, false, false, now + chrono::Duration::seconds(1)),
            &params,
        );
        assert_eq!(followup.decision, EngagementDecision::Ignore, "a main channel must stay mention-only after a mention");
    }

    #[test]
    fn cold_thread_without_a_mention_is_ignored() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let outcome = tracker.decide(&input("thread-1", false, true, false, false, base_time()), &params);
        assert_eq!(outcome.decision, EngagementDecision::Ignore);
        assert!(!outcome.became_warm);
    }

    #[test]
    fn warm_thread_decays_after_idle_timeout_and_then_ignores_unmentioned_messages() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let now = base_time();

        let mentioned = tracker.decide(&input("thread-1", false, true, false, true, now), &params);
        assert_eq!(mentioned.decision, EngagementDecision::Respond);

        let after_timeout = now + params.idle_timeout;
        let outcome = tracker.decide(&input("thread-1", false, true, false, false, after_timeout), &params);
        assert_eq!(outcome.decision, EngagementDecision::Ignore, "idle_timeout elapsing since the last mention must decay the thread");
    }

    #[test]
    fn warm_thread_stays_warm_just_under_idle_timeout() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let now = base_time();

        tracker.decide(&input("thread-1", false, true, false, true, now), &params);

        let just_before_timeout = now + params.idle_timeout - chrono::Duration::seconds(1);
        let outcome = tracker.decide(&input("thread-1", false, true, false, false, just_before_timeout), &params);
        assert_eq!(outcome.decision, EngagementDecision::Respond);
    }

    #[test]
    fn warm_thread_decays_after_message_budget_unmentioned_messages() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams { idle_timeout: DEFAULT_IDLE_TIMEOUT, message_budget: 3, thread_follow: ThreadFollowMode::StickyDecay };
        let now = base_time();

        tracker.decide(&input("thread-1", false, true, false, true, now), &params);

        // Three consecutive unmentioned messages still land while warm.
        for i in 1..=3 {
            let t = now + chrono::Duration::seconds(i);
            let outcome = tracker.decide(&input("thread-1", false, true, false, false, t), &params);
            assert_eq!(outcome.decision, EngagementDecision::Respond, "message {i} of the budget must still be answered while warm");
        }

        // The budget is now exhausted; the next unmentioned message finds
        // the thread cold.
        let after_budget = now + chrono::Duration::seconds(4);
        let outcome = tracker.decide(&input("thread-1", false, true, false, false, after_budget), &params);
        assert_eq!(outcome.decision, EngagementDecision::Ignore, "exceeding message_budget must decay the thread");
    }

    #[test]
    fn a_mention_resets_the_message_budget_streak() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams { idle_timeout: DEFAULT_IDLE_TIMEOUT, message_budget: 2, thread_follow: ThreadFollowMode::StickyDecay };
        let now = base_time();

        tracker.decide(&input("thread-1", false, true, false, true, now), &params);
        tracker.decide(&input("thread-1", false, true, false, false, now + chrono::Duration::seconds(1)), &params);
        // A fresh mention here should reset the streak rather than letting
        // the next unmentioned message ride out a budget that was already
        // at 1/2.
        tracker.decide(&input("thread-1", false, true, false, true, now + chrono::Duration::seconds(2)), &params);
        let outcome = tracker.decide(
            &input("thread-1", false, true, false, false, now + chrono::Duration::seconds(3)),
            &params,
        );
        assert_eq!(outcome.decision, EngagementDecision::Respond, "a mention must reset the unmentioned-message streak");
    }

    #[test]
    fn a_new_mention_rewarms_a_decayed_thread() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let now = base_time();

        tracker.decide(&input("thread-1", false, true, false, true, now), &params);
        let after_timeout = now + params.idle_timeout;
        let decayed = tracker.decide(&input("thread-1", false, true, false, false, after_timeout), &params);
        assert_eq!(decayed.decision, EngagementDecision::Ignore);

        let remention = tracker.decide(&input("thread-1", false, true, false, true, after_timeout), &params);
        assert_eq!(remention.decision, EngagementDecision::Respond);
        assert!(remention.became_warm, "re-mentioning a decayed thread must report a fresh COLD->WARM transition");

        let followup = tracker.decide(
            &input("thread-1", false, true, false, false, after_timeout + chrono::Duration::seconds(1)),
            &params,
        );
        assert_eq!(followup.decision, EngagementDecision::Respond, "re-mentioning a decayed thread must warm it again");
    }

    #[test]
    fn two_conversations_track_independently() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let now = base_time();

        tracker.decide(&input("thread-a", false, true, false, true, now), &params);

        // thread-b was never mentioned, so it must stay cold regardless of
        // thread-a's warmth.
        let decision_b = tracker.decide(&input("thread-b", false, true, false, false, now), &params);
        assert_eq!(decision_b.decision, EngagementDecision::Ignore, "warming thread-a must not leak into thread-b");

        // thread-a must still be warm afterward.
        let decision_a = tracker.decide(
            &input("thread-a", false, true, false, false, now + chrono::Duration::seconds(1)),
            &params,
        );
        assert_eq!(decision_a.decision, EngagementDecision::Respond, "checking thread-b must not have disturbed thread-a's state");
    }

    #[test]
    fn at_everyone_does_not_warm_anything() {
        // `is_bot_mentioned` already excludes @everyone/@here (see
        // `security.rs`), so from this module's perspective an @everyone
        // message is simply `bot_mentioned: false` — this proves it behaves
        // exactly like any other unmentioned message and never warms a
        // thread or gets a response in a main channel.
        let tracker = EngagementTracker::new();
        let params = EngagementParams::default();
        let now = base_time();

        let thread_decision = tracker.decide(&input("thread-1", false, true, false, false, now), &params);
        assert_eq!(thread_decision.decision, EngagementDecision::Ignore);

        let channel_decision = tracker.decide(&input("channel-1", false, false, false, false, now), &params);
        assert_eq!(channel_decision.decision, EngagementDecision::Ignore);
    }

    // --- ThreadFollowMode::OneShot ---

    fn one_shot_params() -> EngagementParams {
        EngagementParams { thread_follow: ThreadFollowMode::OneShot, ..EngagementParams::default() }
    }

    #[test]
    fn one_shot_thread_mention_responds_and_reports_a_transition() {
        let tracker = EngagementTracker::new();
        let params = one_shot_params();
        let outcome = tracker.decide(&input("thread-1", false, true, false, true, base_time()), &params);
        assert_eq!(outcome.decision, EngagementDecision::Respond);
        assert!(outcome.became_warm, "a one-shot mention still reports a transition so the runner fires backfill");
    }

    #[test]
    fn one_shot_thread_without_a_mention_is_ignored() {
        let tracker = EngagementTracker::new();
        let params = one_shot_params();
        let outcome = tracker.decide(&input("thread-1", false, true, false, false, base_time()), &params);
        assert_eq!(outcome.decision, EngagementDecision::Ignore);
        assert!(!outcome.became_warm);
    }

    #[test]
    fn one_shot_thread_never_stays_warm_after_the_mentioned_message() {
        let tracker = EngagementTracker::new();
        let params = one_shot_params();
        let now = base_time();

        let mentioned = tracker.decide(&input("thread-1", false, true, false, true, now), &params);
        assert_eq!(mentioned.decision, EngagementDecision::Respond);

        let followup = tracker.decide(
            &input("thread-1", false, true, false, false, now + chrono::Duration::seconds(1)),
            &params,
        );
        assert_eq!(followup.decision, EngagementDecision::Ignore, "one-shot must return to cold immediately, unlike sticky-decay");
    }

    #[test]
    fn one_shot_re_mentioning_responds_again_and_reports_another_transition() {
        // Unlike sticky-decay/always, a one-shot thread never actually
        // "stays" warm, so every fresh mention is its own transition —
        // there's no persisted warmth for a re-mention to merely extend.
        let tracker = EngagementTracker::new();
        let params = one_shot_params();
        let now = base_time();

        tracker.decide(&input("thread-1", false, true, false, true, now), &params);
        let second = tracker.decide(
            &input("thread-1", false, true, false, true, now + chrono::Duration::seconds(1)),
            &params,
        );
        assert_eq!(second.decision, EngagementDecision::Respond);
        assert!(second.became_warm, "each one-shot mention is its own transition");
    }

    // --- ThreadFollowMode::Always ---

    fn always_params() -> EngagementParams {
        EngagementParams { thread_follow: ThreadFollowMode::Always, ..EngagementParams::default() }
    }

    #[test]
    fn always_cold_thread_without_a_mention_is_ignored() {
        let tracker = EngagementTracker::new();
        let params = always_params();
        let outcome = tracker.decide(&input("thread-1", false, true, false, false, base_time()), &params);
        assert_eq!(outcome.decision, EngagementDecision::Ignore);
        assert!(!outcome.became_warm);
    }

    #[test]
    fn always_thread_mention_warms_it_and_reports_the_transition() {
        let tracker = EngagementTracker::new();
        let params = always_params();
        let now = base_time();

        let mentioned = tracker.decide(&input("thread-1", false, true, false, true, now), &params);
        assert_eq!(mentioned.decision, EngagementDecision::Respond);
        assert!(mentioned.became_warm);

        let followup = tracker.decide(
            &input("thread-1", false, true, false, false, now + chrono::Duration::seconds(1)),
            &params,
        );
        assert_eq!(followup.decision, EngagementDecision::Respond);
        assert!(!followup.became_warm, "an unmentioned message on an already-warm thread is not a fresh transition");
    }

    #[test]
    fn always_thread_never_decays_after_a_huge_idle_gap() {
        let tracker = EngagementTracker::new();
        let params = always_params();
        let now = base_time();

        tracker.decide(&input("thread-1", false, true, false, true, now), &params);

        // Far beyond DEFAULT_IDLE_TIMEOUT, which `Always` must ignore
        // entirely.
        let much_later = now + params.idle_timeout * 100;
        let outcome = tracker.decide(&input("thread-1", false, true, false, false, much_later), &params);
        assert_eq!(outcome.decision, EngagementDecision::Respond, "Always must never decay on idle time");
    }

    #[test]
    fn always_thread_never_decays_after_exceeding_the_message_budget() {
        let tracker = EngagementTracker::new();
        let params = EngagementParams { message_budget: 2, ..always_params() };
        let now = base_time();

        tracker.decide(&input("thread-1", false, true, false, true, now), &params);

        // Far more unmentioned messages than `message_budget`, which
        // `Always` must ignore entirely.
        for i in 1..=10 {
            let outcome = tracker.decide(
                &input("thread-1", false, true, false, false, now + chrono::Duration::seconds(i)),
                &params,
            );
            assert_eq!(outcome.decision, EngagementDecision::Respond, "message {i}: Always must never decay on message budget");
        }
    }

    #[test]
    fn always_re_mentioning_an_already_warm_thread_does_not_report_a_new_transition() {
        let tracker = EngagementTracker::new();
        let params = always_params();
        let now = base_time();

        let first = tracker.decide(&input("thread-1", false, true, false, true, now), &params);
        assert!(first.became_warm);

        let second = tracker.decide(
            &input("thread-1", false, true, false, true, now + chrono::Duration::seconds(1)),
            &params,
        );
        assert!(!second.became_warm, "re-mentioning an already-warm Always thread must not report another transition");
    }
}
