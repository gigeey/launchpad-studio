//! The single, pure inbound trigger decision for the Slack channel — the
//! "one legible place" obligation. Every parsed
//! [`super::protocol::SocketModeEvent`] runs through [`classify`] exactly
//! once, on its way from the socket to dispatch, and comes back either
//! [`FilterDecision::Accept`]ed (with the [`Trigger`] that admitted it) or
//! [`FilterDecision::Drop`]ped (with the [`DropReason`] that rejected it).
//!
//! Two rules live here, and they live here *together* on purpose:
//!
//! 1. **The bot-echo guard.** Drop anything a bot authored — an
//!    explicit `bot_id`, the `bot_message` subtype, or a `user` equal to our
//!    own captured `bot_user_id`. This is not defensive polish; it is
//!    load-bearing given one Slack app *per agent*. Two Launchpad
//!    agents sharing a channel each see the other's posts as ordinary inbound
//!    traffic. Without this guard the moment either one is triggerable, they
//!    answer each other forever. See [`is_bot_echo`].
//!
//! 2. **The trigger filter.** Admit only the three shapes Slack v1
//!    responds to — a direct `app_mention`, a DM (`message.im`), and a reply
//!    in a thread whose root the agent already participates in — and drop
//!    everything else: top-level channel chatter, joins, edits, reactions,
//!    non-participating threads. This scope cut is deliberate and recorded;
//!    Discord's engagement/backfill machinery is *not* ported.
//!
//! **The single-place obligation.** Keeping both rules here, rather
//! than inlined into [`super`]'s future runner or scattered across it, is
//! what lets a later engagement layer be an *insertion* into this module —
//! a new [`Trigger`] variant and a new admitting branch — rather than an
//! excavation of the runner. If a shortcut would make this filter harder to
//! extend, the shortcut is not worth taking.
//!
//! Everything here is pure: no socket, no clock, no registry I/O. The one
//! piece of outside knowledge a decision needs — *does this agent already
//! participate in this thread?* — is injected as a closure over the
//! conversation registry, so the trigger *logic* stays in this module while
//! the registry *data* stays in the runner.

use super::protocol::{SlackEvent, SlackMessageEvent, SocketModeEvent};

/// Why an inbound event was admitted for dispatch. Distinct variants (rather
/// than a bare `true`) so the runner can log *why* it dispatched, and so the
/// engagement layer anticipated here can add its own reason as a pure
/// insertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A direct `@mention` of the agent (an `app_mention` event).
    Mention,
    /// A message in a DM with the agent (`message.im`).
    DirectMessage,
    /// A reply in a thread whose root the agent already participates in.
    ThreadReply,
}

/// Why an inbound event was dropped before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The echo guard fired: `bot_id` present, `subtype ==
    /// "bot_message"`, or `user == bot_user_id`. See [`is_bot_echo`].
    BotEcho,
    /// A well-formed, human-authored event that simply is not one of the
    /// three triggers — channel chatter with no mention, a reply in a thread
    /// the agent is not in, a `channel_join` / `message_changed` subtype, a
    /// `reaction_added`, and so on.
    NotATrigger,
    /// A non-`events_api` envelope this filter never dispatches on: `hello`,
    /// `disconnect`, `slash_commands`, `interactive`, or an unknown envelope
    /// type. Slash-command and interactive payloads are ack-then-drop and the
    /// ack itself is the runner's job; here they are simply never a turn to
    /// hand to the agent.
    NotDispatchable,
}

/// The filter's verdict for one parsed inbound envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterDecision {
    /// Dispatch this event to the agent, admitted by the given [`Trigger`].
    Accept(Trigger),
    /// Do not dispatch; the [`DropReason`] records why.
    Drop(DropReason),
}

impl FilterDecision {
    /// Whether this verdict admits the event for dispatch.
    pub fn is_accept(&self) -> bool {
        matches!(self, Self::Accept(_))
    }

    /// The admitting [`Trigger`] if this verdict accepted, else `None`.
    pub fn trigger(&self) -> Option<Trigger> {
        match self {
            Self::Accept(trigger) => Some(*trigger),
            Self::Drop(_) => None,
        }
    }
}

/// The bot-echo guard, isolated so it is trivially testable and so the
/// runner can reason about it independently of the trigger rules. Returns
/// `true` — meaning *drop this* — for anything a bot authored:
///
/// - an explicit `bot_id` (any bot, including another Launchpad agent's app),
/// - the `bot_message` subtype, or
/// - a `user` equal to our own captured `bot_user_id`.
///
/// The `bot_user_id` arm is skipped when `bot_user_id` is empty: an
/// unconfigured id must not accidentally match the empty `user` that
/// bot-authored messages already default to (those are caught by the first
/// two arms regardless).
pub fn is_bot_echo(message: &SlackMessageEvent, bot_user_id: &str) -> bool {
    message.bot_id.is_some()
        || message.subtype.as_deref() == Some("bot_message")
        || (!bot_user_id.is_empty() && message.user == bot_user_id)
}

/// The single trigger decision. Runs on every parsed inbound
/// envelope and returns whether — and, if so, why — it should be dispatched.
///
/// `bot_user_id` is this agent's own Slack user id, captured from `auth.test`
/// during setup; it feeds the third arm of the echo guard.
///
/// `participates_in_thread` answers "does this agent already participate in
/// the thread rooted at `(channel_id, thread_ts)`?" — a lookup the runner
/// backs with the conversation registry. It is only consulted for a
/// threaded, non-DM message, so passing a closure keeps the *trigger logic*
/// here while the *registry state* stays out of this pure module.
pub fn classify(
    event: &SocketModeEvent,
    bot_user_id: &str,
    participates_in_thread: impl Fn(&str, &str) -> bool,
) -> FilterDecision {
    // Only inbound Events API traffic is ever a candidate. `hello` /
    // `disconnect` are connection lifecycle; `slash_commands` / `interactive`
    // are ack-then-drop (the ack is the runner's job); `unknown` is a future
    // envelope type. None is a turn to dispatch.
    let SocketModeEvent::EventsApi { event: payload, .. } = event else {
        return FilterDecision::Drop(DropReason::NotDispatchable);
    };

    match &payload.event {
        // A direct @mention is the least ambiguous trigger. The echo guard
        // still runs first: an `app_mention` our own app somehow authored is
        // an echo, not an invitation.
        SlackEvent::AppMention(message) => {
            if is_bot_echo(message, bot_user_id) {
                return FilterDecision::Drop(DropReason::BotEcho);
            }
            FilterDecision::Accept(Trigger::Mention)
        }

        SlackEvent::Message(message) => {
            if is_bot_echo(message, bot_user_id) {
                return FilterDecision::Drop(DropReason::BotEcho);
            }

            // A plain human message carries no `subtype`. Anything with a
            // subtype that survived the echo guard is a system / edit / join
            // event (`channel_join`, `message_changed`, `message_deleted`,
            // `thread_broadcast`, …) — not a turn to dispatch. Dropping the
            // whole class here is what "and nothing else" means; a
            // later layer that wants a specific subtype (say `file_share`)
            // would admit it by name as an insertion.
            if message.subtype.is_some() {
                return FilterDecision::Drop(DropReason::NotATrigger);
            }

            // A DM is unconditionally ours — a DM channel is 1:1 with the
            // agent, so there is no participation to check and a thread inside
            // it is still a DM. Slack mints DM channel ids in the `D`
            // namespace (`C…` public, `G…` private, `D…` DM); the parsed event
            // carries no `channel_type`, so that id prefix is the signal, and
            // it is the stable one.
            if is_direct_message(&message.channel) {
                FilterDecision::Accept(Trigger::DirectMessage)
            } else if let Some(thread_root) = message.thread_ts.as_deref() {
                // A reply in a channel thread is ours only if the agent is
                // already in that thread — otherwise it is someone else's
                // conversation the agent happens to share a channel with.
                if participates_in_thread(&message.channel, thread_root) {
                    FilterDecision::Accept(Trigger::ThreadReply)
                } else {
                    FilterDecision::Drop(DropReason::NotATrigger)
                }
            } else {
                // Top-level channel message with no mention — ambient chatter.
                FilterDecision::Drop(DropReason::NotATrigger)
            }
        }

        // A recognized-but-unhandled inner event: `reaction_added`,
        // `channel_join` delivered as its own event type, etc.
        SlackEvent::Other => FilterDecision::Drop(DropReason::NotATrigger),
    }
}

/// Whether a channel id names a DM (`message.im`). Slack's direct-message
/// channels live in the `D` id namespace; see [`classify`]'s DM branch for
/// why the id prefix, not a `channel_type` field, is the signal we use.
fn is_direct_message(channel: &str) -> bool {
    channel.starts_with('D')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::slack::protocol::EventsApiPayload;

    /// A plain, human-authored message: no `bot_id`, no `subtype`.
    fn message(channel: &str, user: &str, thread_ts: Option<&str>) -> SlackMessageEvent {
        SlackMessageEvent {
            channel: channel.to_string(),
            user: user.to_string(),
            ts: "1701234567.000100".to_string(),
            thread_ts: thread_ts.map(|s| s.to_string()),
            bot_id: None,
            subtype: None,
            text: "hi".to_string(),
            team: "T1".to_string(),
        }
    }

    /// Wrap an inner [`SlackEvent`] in an `events_api` envelope.
    fn envelope(event: SlackEvent) -> SocketModeEvent {
        SocketModeEvent::EventsApi {
            envelope_id: "env-1".to_string(),
            event: EventsApiPayload { event_id: "Ev1".to_string(), event },
        }
    }

    /// A participation closure the message paths never reach, or that must
    /// deny — the fail-closed default for these tests.
    fn never_participating(_channel: &str, _thread_root: &str) -> bool {
        false
    }

    // --- Bot-echo guard ---

    #[test]
    fn a_message_with_a_bot_id_is_dropped_as_echo() {
        let mut msg = message("C123", "", None);
        msg.bot_id = Some("B999".to_string());
        let decision = classify(&envelope(SlackEvent::Message(msg)), "U0BOT", never_participating);
        assert_eq!(decision, FilterDecision::Drop(DropReason::BotEcho));
    }

    #[test]
    fn a_message_with_the_bot_message_subtype_is_dropped_as_echo() {
        let mut msg = message("C123", "", None);
        msg.subtype = Some("bot_message".to_string());
        let decision = classify(&envelope(SlackEvent::Message(msg)), "U0BOT", never_participating);
        assert_eq!(decision, FilterDecision::Drop(DropReason::BotEcho));
    }

    #[test]
    fn a_message_authored_by_our_own_bot_user_id_is_dropped_as_echo() {
        // Our own post, no bot_id/subtype, just our user id.
        let msg = message("C123", "U0BOT", None);
        let decision = classify(&envelope(SlackEvent::Message(msg)), "U0BOT", never_participating);
        assert_eq!(decision, FilterDecision::Drop(DropReason::BotEcho));
    }

    #[test]
    fn the_echo_guard_fires_before_the_trigger_rules_even_on_an_app_mention() {
        // An app_mention our own app somehow authored is still an echo.
        let mut msg = message("C123", "U0BOT", None);
        msg.bot_id = Some("B0SELF".to_string());
        let decision = classify(&envelope(SlackEvent::AppMention(msg)), "U0BOT", never_participating);
        assert_eq!(decision, FilterDecision::Drop(DropReason::BotEcho));
    }

    #[test]
    fn is_bot_echo_skips_the_user_arm_when_bot_user_id_is_empty() {
        // A misconfigured empty bot_user_id must not match a human message.
        let human = message("C123", "U456", None);
        assert!(!is_bot_echo(&human, ""));
    }

    // --- Trigger filter: accepts ---

    #[test]
    fn an_app_mention_is_accepted() {
        let msg = message("C123", "U456", None);
        let decision = classify(&envelope(SlackEvent::AppMention(msg)), "U0BOT", never_participating);
        assert_eq!(decision, FilterDecision::Accept(Trigger::Mention));
    }

    #[test]
    fn a_dm_message_is_accepted() {
        // DM channel id in the `D` namespace; participation is not consulted.
        let msg = message("D555", "U456", None);
        let decision = classify(&envelope(SlackEvent::Message(msg)), "U0BOT", |_, _| {
            panic!("participation must not be consulted for a DM");
        });
        assert_eq!(decision, FilterDecision::Accept(Trigger::DirectMessage));
    }

    #[test]
    fn a_reply_in_a_participating_thread_is_accepted() {
        let msg = message("C123", "U456", Some("1701234500.000000"));
        let decision = classify(&envelope(SlackEvent::Message(msg)), "U0BOT", |channel, thread_root| {
            channel == "C123" && thread_root == "1701234500.000000"
        });
        assert_eq!(decision, FilterDecision::Accept(Trigger::ThreadReply));
    }

    // --- Trigger filter: drops ---

    #[test]
    fn a_top_level_channel_message_with_no_mention_is_dropped() {
        let msg = message("C123", "U456", None);
        let decision = classify(&envelope(SlackEvent::Message(msg)), "U0BOT", never_participating);
        assert_eq!(decision, FilterDecision::Drop(DropReason::NotATrigger));
    }

    #[test]
    fn a_reply_in_a_thread_the_agent_is_not_in_is_dropped() {
        let msg = message("C123", "U456", Some("1701234500.000000"));
        let decision = classify(&envelope(SlackEvent::Message(msg)), "U0BOT", never_participating);
        assert_eq!(decision, FilterDecision::Drop(DropReason::NotATrigger));
    }

    #[test]
    fn a_subtype_bearing_message_such_as_an_edit_or_join_is_dropped() {
        // Even in a thread the agent participates in, an edit is not a turn.
        let mut msg = message("C123", "U456", Some("1701234500.000000"));
        msg.subtype = Some("message_changed".to_string());
        let decision = classify(&envelope(SlackEvent::Message(msg)), "U0BOT", |_, _| true);
        assert_eq!(decision, FilterDecision::Drop(DropReason::NotATrigger));
    }

    #[test]
    fn a_recognized_but_unhandled_inner_event_is_dropped() {
        let decision = classify(&envelope(SlackEvent::Other), "U0BOT", never_participating);
        assert_eq!(decision, FilterDecision::Drop(DropReason::NotATrigger));
    }

    // --- Non-events_api envelopes: not dispatchable ---

    #[test]
    fn slash_command_and_interactive_envelopes_are_classified_as_not_dispatchable() {
        for env in [
            SocketModeEvent::SlashCommands { envelope_id: "sc-1".to_string() },
            SocketModeEvent::Interactive { envelope_id: "ia-1".to_string() },
        ] {
            let decision = classify(&env, "U0BOT", never_participating);
            assert_eq!(decision, FilterDecision::Drop(DropReason::NotDispatchable));
        }
    }

    #[test]
    fn lifecycle_envelopes_are_classified_as_not_dispatchable() {
        for env in [
            SocketModeEvent::Hello,
            SocketModeEvent::Disconnect {
                reason: crate::channels::slack::protocol::DisconnectReason::RefreshRequested,
            },
            SocketModeEvent::Unknown,
        ] {
            let decision = classify(&env, "U0BOT", never_participating);
            assert_eq!(decision, FilterDecision::Drop(DropReason::NotDispatchable));
        }
    }

    // --- FilterDecision helpers ---

    #[test]
    fn filter_decision_exposes_accept_state_and_trigger() {
        let accept = FilterDecision::Accept(Trigger::Mention);
        assert!(accept.is_accept());
        assert_eq!(accept.trigger(), Some(Trigger::Mention));

        let drop = FilterDecision::Drop(DropReason::BotEcho);
        assert!(!drop.is_accept());
        assert_eq!(drop.trigger(), None);
    }
}
