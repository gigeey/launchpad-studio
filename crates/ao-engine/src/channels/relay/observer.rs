//! Shared `EventBus` observer step for a synchronous chat channel's
//! outbound relay: buffer each thread's most recent `TextComplete` since its
//! run started, and on `RunEnded` resolve that thread's reply target through
//! a [`CorrelationMap`] and hand the buffered text to a channel-supplied
//! [`RelaySink`]. [`handle_relay_event`] is what both
//! [`crate::telegram::outbound::handle_event`] and
//! [`crate::channels::discord::outbound::handle_event`] delegate to today;
//! each channel still hand-rolls its own outer `run_outbound_observer`
//! subscribe loop (`tokio::select!` over shutdown vs. the next event) around
//! that call, since Telegram's also drives per-channel-only behavior
//! (the typing heartbeat) this shared step doesn't model.
//!
//! Deliberately excludes anything a channel treats as its own: the actual
//! network send — token resolution, chunking, wire-format conversion, the
//! request itself, and any per-chunk fallback — lives behind
//! [`RelaySink::relay`]. Per-channel-only behavior like Telegram's typing
//! heartbeat (driven off `RunStarted`, which this step doesn't even look at)
//! isn't part of this shared skeleton at all — a channel that needs it keeps
//! driving its own event handling for that outside this call.
//!
//! [`recover_lagged_replies`] is the other half both channels' outer loops
//! share: what to do when the `EventBus` broadcast receiver itself reports
//! `RecvError::Lagged` instead of handing back an event. A dropped
//! `TextComplete`/`RunEnded` pair used to mean the assistant's reply simply
//! never reached the user, indistinguishable from being ignored — this
//! recovers it from the thread's persisted transcript instead of the
//! (now-incomplete) live stream.
//!
//! [`handle_relay_event`]'s `RunEnded` arm covers the sibling case: a run
//! that ends live, on the happy path, but without ever producing a
//! substantive reply. [`terminal_failure_notice`] turns the bare
//! [`RunEndReason`] tag into a short, honest, sanitized notice for every
//! reason but `Completed` — a crash, timeout, or cancellation used to leave
//! the channel just as silent as a reply the user never got, with no way to
//! tell the two apart.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{broadcast, watch};
use tracing::warn;

use ao_persistence::PersistenceLayer;
use ao_protocol::event::{AgentEvent, AgentEventPayload, RunEndReason};
use ao_protocol::thread::ThreadScope;
use ao_protocol::transcript::TranscriptRole;

use crate::event_bus::EventBus;

use super::correlation_map::CorrelationMap;
use super::lease_gate::LeaseGate;

/// A channel's outbound send behavior, plugged into [`run_relay_observer`].
/// Everything channel-specific — resolving a bot token, chunking to the
/// channel's own limit, wire-format conversion, the actual request, and any
/// per-chunk fallback — lives behind one implementation of this trait.
#[async_trait]
pub(crate) trait RelaySink<V: Send + Sync>: Send + Sync {
    /// Relays `text` (already known non-empty) to `origin` on behalf of
    /// `agent_id`. Must never panic or propagate a failure out of this call
    /// — a failed relay must never crash the turn, the thread, or the
    /// process; implementations log and swallow their own failures, exactly
    /// as `telegram::outbound::relay_reply` and
    /// `channels::discord::outbound::relay_reply` already do.
    async fn relay(&self, agent_id: &str, origin: &V, text: &str);
}

/// Runs until `shutdown_rx` fires. One subscription for the whole process —
/// every agent's events flow through it; only threads `in_flight` is
/// currently tracking ever trigger a relay.
pub(crate) async fn run_relay_observer<V>(
    persistence: Arc<PersistenceLayer>,
    lease_gate: Arc<LeaseGate>,
    in_flight: Arc<CorrelationMap<V>>,
    sink: Arc<dyn RelaySink<V>>,
    event_bus: Arc<EventBus>,
    mut shutdown_rx: watch::Receiver<()>,
) where
    V: Clone + Send + Sync + 'static,
{
    let mut events = event_bus.subscribe();
    // thread_id -> latest TextComplete text seen since that thread's run started.
    let mut pending_text: HashMap<String, String> = HashMap::new();
    // thread_id -> text of the last reply actually relayed for that thread —
    // see `recover_lagged_replies` for why this is kept alongside `pending_text`.
    let mut last_relayed: HashMap<String, String> = HashMap::new();

    loop {
        let event = tokio::select! {
            _ = shutdown_rx.changed() => return,
            event = events.recv() => event,
        };

        let event = match event {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                recover_lagged_replies(
                    lease_gate.as_ref(),
                    persistence.as_ref(),
                    in_flight.as_ref(),
                    sink.as_ref(),
                    &mut last_relayed,
                    skipped,
                )
                .await;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };

        handle_relay_event(lease_gate.as_ref(), in_flight.as_ref(), sink.as_ref(), event, &mut pending_text, &mut last_relayed)
            .await;
    }
}

/// Processes one event from the shared bus: buffers `TextComplete` text per
/// thread, and on `RunEnded` looks up that thread's reply target and relays
/// the buffered text. Split out from [`run_relay_observer`]'s loop so tests
/// can drive it directly with synthetic events instead of racing a spawned
/// task against a real broadcast channel.
pub(crate) async fn handle_relay_event<V>(
    lease_gate: &LeaseGate,
    in_flight: &CorrelationMap<V>,
    sink: &(dyn RelaySink<V> + Send + Sync),
    event: AgentEvent,
    pending_text: &mut HashMap<String, String>,
    last_relayed: &mut HashMap<String, String>,
) where
    V: Clone + Send + Sync,
{
    let Some(thread_id) = event.thread_id else {
        return;
    };

    match event.payload {
        AgentEventPayload::TextComplete { text } => {
            pending_text.insert(thread_id, text);
        }
        AgentEventPayload::RunStarted => {
            // A bridge thread is reused across every run on its binding for
            // as long as that binding lives, so `last_relayed` would
            // otherwise carry a much earlier run's entry forever — nothing
            // else ever clears it. Without this, the very first successful
            // relay on a thread would permanently suppress every later
            // run's failure notice below, not just the lag-recovery race
            // this map exists for. A new run starting is the right boundary
            // to draw the line: only an entry made *during this run*
            // (i.e., by `recover_lagged_replies`, since a normal relay on
            // this same run would populate `pending_text` instead and take
            // the branch above) should count against this run's own
            // `RunEnded`.
            last_relayed.remove(&thread_id);
        }
        AgentEventPayload::RunEnded { reason } => {
            // Always drop any buffered text for this thread on completion,
            // whether or not it turns out to be a channel-triggered run —
            // otherwise a thread `in_flight` never recorded (e.g. a bridge
            // thread used directly from the UI) would leak an entry
            // forever. This unconditional drop is the "error-path
            // invalidation": it happens on the no-mapping / empty-text path
            // exactly as it does on the happy path below.
            let text = pending_text.remove(&thread_id);
            // `peek`, not a consuming read — see `CorrelationMap`'s module
            // doc for why: an async `Delegate` call spawned from this turn
            // ends the run immediately with a hand-off reply, and the
            // delegate's real answer later fires a *second* `RunEnded` on
            // this same thread once it completes. Consuming the mapping
            // here would leave that second completion with nothing to
            // relay to.
            let Some(origin) = in_flight.peek(&thread_id) else {
                return;
            };
            // This process's outbound observer runs process-wide, not
            // per-binding, so it keeps receiving every agent's events even
            // for a binding whose single-writer lease it has lost (or never
            // held) — see `crate::telegram::bridge::ChannelBridge::reconcile`.
            // Without this check a standby process could relay a duplicate
            // reply into the same chat the real lease holder just answered.
            if !lease_gate.is_active(&thread_id) {
                warn!(
                    thread_id = %thread_id,
                    "channel outbound relay: dropped a relay for a binding this process does not hold the lease for"
                );
                return;
            }
            let text = text.filter(|t| !t.trim().is_empty());

            if let Some(text) = text {
                // A substantive reply was buffered — relay it regardless of
                // `reason`. This also covers a post-reply teardown failure
                // (TextComplete already fired before the terminal error):
                // the reply already answers the user, so a failure notice
                // on top of it would only contradict what they just
                // received.
                sink.relay(&event.agent_id, &origin, &text).await;
                // Recorded so a later broadcast lag on this thread can tell
                // "this reply already went out" apart from "this is new" —
                // see `recover_lagged_replies`.
                last_relayed.insert(thread_id, text);
            } else if last_relayed.contains_key(&thread_id) {
                // This run's own reply already went out — not through this
                // event (its `TextComplete` never made it into
                // `pending_text`), but through a `recover_lagged_replies`
                // pass earlier in this same run (the broadcast lag that lost
                // `TextComplete` typically loses the surrounding events
                // too). Sending a failure notice on top of a reply the user
                // already received would flatly contradict it.
            } else if let Some(notice) = terminal_failure_notice(reason) {
                // Nothing was ever relayed for this run and it didn't end
                // cleanly — an honest, sanitized notice beats leaving the
                // conversation looking silently ignored (indistinguishable
                // from being ignored otherwise).
                sink.relay(&event.agent_id, &origin, notice).await;
                last_relayed.insert(thread_id, notice.to_string());
            }
        }
        _ => {}
    }
}

/// Plain, non-technical text relayed in place of a reply when a run ends
/// with nothing else to show for it — a `RunEnded` whose `reason` isn't
/// [`RunEndReason::Completed`], and for which no `TextComplete` was ever
/// buffered. Returns `None` for `Completed` (no buffered text there is just
/// an ordinary no-op, not a failure worth reporting).
///
/// Deliberately never includes the underlying Rust error, a backtrace, a
/// provider response body, or any file path — none of those are safe to
/// drop into a chat channel (they can carry secrets or internal detail the
/// user has no use for). [`AgentEventPayload::RunEnded`] doesn't even carry
/// an error message alongside `reason`, so there is nothing raw to
/// sanitize in the first place; these are fixed, hand-written strings.
fn terminal_failure_notice(reason: RunEndReason) -> Option<&'static str> {
    match reason {
        RunEndReason::Completed => None,
        RunEndReason::Cancelled => Some(RUN_CANCELLED_NOTICE),
        RunEndReason::TimedOut | RunEndReason::NoOutputTimeout => Some(RUN_TIMED_OUT_NOTICE),
        RunEndReason::TurnLimitReached => Some(RUN_TURN_LIMIT_NOTICE),
        RunEndReason::Error | RunEndReason::Signal => Some(RUN_FAILED_NOTICE),
    }
}

/// Relayed when a run ends in [`RunEndReason::Error`] or
/// [`RunEndReason::Signal`] before producing any reply. `pub(crate)` so each
/// channel's own integration tests can assert against it directly rather
/// than duplicating the literal.
pub(crate) const RUN_FAILED_NOTICE: &str = "The agent hit an error and couldn't finish this reply. Please try again.";

/// Relayed when a run ends in [`RunEndReason::TimedOut`] or
/// [`RunEndReason::NoOutputTimeout`] before producing any reply.
pub(crate) const RUN_TIMED_OUT_NOTICE: &str =
    "The agent took too long to respond, so this run was stopped before finishing a reply. Please try again.";

/// Relayed when a run ends in [`RunEndReason::TurnLimitReached`] before
/// producing any reply.
pub(crate) const RUN_TURN_LIMIT_NOTICE: &str =
    "The agent reached its configured turn limit before finishing a reply. Please try again.";

/// Relayed when a run ends in [`RunEndReason::Cancelled`] before producing
/// any reply.
pub(crate) const RUN_CANCELLED_NOTICE: &str = "This run was stopped before the agent could finish a reply.";

/// Text relayed in place of the real reply when this process's outbound
/// relay lagged badly enough on the shared event bus that
/// [`recover_lagged_replies`] couldn't resolve what the agent actually
/// said. Delivering an honest "something might be missing" notice always
/// beats leaving the user's message looking silently ignored.
const LAG_RECOVERY_FAILED_NOTICE: &str =
    "(A reply may not have fully reached you due to a brief internal hiccup — reply here if something seems missing.)";

/// How many of a thread's most recent transcript entries
/// [`recover_lagged_replies`] scans for the latest assistant reply. The
/// entry it actually wants is always the last `"response"` entry written
/// for the thread; this margin only exists to tolerate other entry kinds
/// (`tool_use`/`tool_result`/`thinking`, a delegate hand-off marker, ...)
/// interleaved after it.
const LAG_RECOVERY_TRANSCRIPT_SCAN: usize = 20;

/// Recovers from the outbound relay's broadcast receiver reporting
/// `Lagged(skipped)`. A lag on the shared [`EventBus`] doesn't say which
/// thread(s) the skipped events belonged to — `RecvError::Lagged` carries
/// only a skip count — so, instead of the old "warn and drop", every thread
/// this process is still tracking a reply target for
/// ([`CorrelationMap::snapshot`]) is checked against its own persisted
/// transcript (the same store [`crate::channels::submit_inbound_message`]
/// writes an agent's turns into) and relayed if it holds a reply that
/// hasn't gone out yet.
///
/// Best-effort and thread-isolated: a failure recovering one thread is
/// logged and never stops the others (see [`recover_thread`]), and this
/// function itself never panics or propagates an error — it runs straight
/// off a lagged broadcast receiver's own error arm, which must keep
/// looping regardless of what recovery finds.
pub(crate) async fn recover_lagged_replies<V>(
    lease_gate: &LeaseGate,
    persistence: &PersistenceLayer,
    in_flight: &CorrelationMap<V>,
    sink: &(dyn RelaySink<V> + Send + Sync),
    last_relayed: &mut HashMap<String, String>,
    skipped: u64,
) where
    V: Clone + Send + Sync,
{
    warn!(
        skipped,
        "channel outbound relay observer lagged on the event bus; recovering any missed replies from the transcript"
    );
    for (thread_id, origin) in in_flight.snapshot() {
        recover_thread(lease_gate, persistence, sink, last_relayed, &thread_id, &origin).await;
    }
}

/// One thread's half of [`recover_lagged_replies`]: resolves the thread's
/// owning agent and latest persisted assistant reply, relays it unless
/// `last_relayed` shows it already went out, and degrades to
/// [`LAG_RECOVERY_FAILED_NOTICE`] if the transcript itself can't be
/// resolved or read — never to silence. A thread with no persisted reply
/// yet (never started, or genuinely still in flight) is left alone:
/// nothing here proves a reply was actually missed, so there is nothing to
/// recover or warn about.
///
/// Gated by `lease_gate` exactly like the live path in
/// [`handle_relay_event`]: a lag says nothing about which process's lease
/// state changed, so a non-holder must never use it as an excuse to relay
/// either.
async fn recover_thread<V>(
    lease_gate: &LeaseGate,
    persistence: &PersistenceLayer,
    sink: &(dyn RelaySink<V> + Send + Sync),
    last_relayed: &mut HashMap<String, String>,
    thread_id: &str,
    origin: &V,
) where
    V: Send + Sync,
{
    if !lease_gate.is_active(thread_id) {
        return;
    }

    let thread = match persistence.threads.get(thread_id).await {
        Ok(Some(thread)) => thread,
        // Nothing on file for this thread — not a resolvable failure, and
        // with no known owning agent there is no token to relay a notice
        // through anyway.
        Ok(None) => return,
        Err(e) => {
            warn!(thread_id = %thread_id, "channel outbound relay: lag recovery could not resolve thread: {e}");
            return;
        }
    };
    let ThreadScope::AgentChat { agent_id } = &thread.scope else {
        return;
    };

    let path = PathBuf::from(&thread.transcript_path);
    match persistence.transcripts.read_tail_at(&path, LAG_RECOVERY_TRANSCRIPT_SCAN).await {
        Ok(tail) => {
            let Some(text) = tail.entries.into_iter().rev().find_map(|entry| {
                let is_reply = matches!(entry.role, TranscriptRole::Agent { .. })
                    && entry.event_type == "response"
                    && !entry.hidden_from_user
                    && !entry.content.trim().is_empty();
                is_reply.then_some(entry.content)
            }) else {
                return;
            };
            if last_relayed.get(thread_id).is_some_and(|last| last == &text) {
                return;
            }
            sink.relay(agent_id, origin, &text).await;
            last_relayed.insert(thread_id.to_string(), text);
        }
        Err(e) => {
            warn!(thread_id = %thread_id, "channel outbound relay: lag recovery failed to read the transcript: {e}");
            sink.relay(agent_id, origin, LAG_RECOVERY_FAILED_NOTICE).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex as StdMutex;

    use chrono::Utc;
    use uuid::Uuid;

    use ao_protocol::event::RunEndReason;

    /// Fake [`RelaySink`] that records every call instead of doing any real
    /// network I/O, so relay order/target/text can be asserted directly.
    #[derive(Default)]
    struct RecordingSink {
        calls: StdMutex<Vec<(String, i64, String)>>,
    }

    impl RecordingSink {
        fn calls(&self) -> Vec<(String, i64, String)> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait]
    impl RelaySink<i64> for RecordingSink {
        async fn relay(&self, agent_id: &str, origin: &i64, text: &str) {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).push((
                agent_id.to_string(),
                *origin,
                text.to_string(),
            ));
        }
    }

    fn make_event(agent_id: &str, thread_id: &str, payload: AgentEventPayload) -> AgentEvent {
        AgentEvent {
            event_id: Uuid::new_v4().to_string(),
            run_id: format!("run-{}", Uuid::new_v4()),
            seq: 0,
            ts: Utc::now(),
            agent_id: agent_id.to_string(),
            thread_id: Some(thread_id.to_string()),
            payload,
        }
    }

    #[tokio::test]
    async fn relays_the_last_text_complete_seen_before_run_ended() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        in_flight.record("bridge-thread-1", 555);
        lease_gate.mark_active("test-binding", "bridge-thread-1");
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        // A turn typically emits several TextComplete events (interleaved
        // with tool calls) — only the last one before RunEnded should relay.
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-x",
                "bridge-thread-1",
                AgentEventPayload::TextComplete { text: "draft, superseded".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-x",
                "bridge-thread-1",
                AgentEventPayload::TextComplete { text: "final reply text".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-x", "bridge-thread-1", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert_eq!(sink.calls(), vec![("agent-x".to_string(), 555, "final reply text".to_string())]);
        assert_eq!(
            in_flight.peek("bridge-thread-1"),
            Some(555),
            "a relay must not consume the mapping — a later async-delegate completion on the \
             same thread needs it too"
        );
    }

    /// Regression for the async-delegate relay case: a triggering turn ends
    /// immediately with a hand-off reply, then the delegate's real answer
    /// later fires a second, independent `RunEnded` on the same thread. Both
    /// must relay, which requires `peek` (not a consuming read) to resolve
    /// on both lookups.
    #[tokio::test]
    async fn peek_not_take_lets_a_second_run_ended_on_the_same_thread_still_resolve_and_relay() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        in_flight.record("bridge-thread-2", 777);
        lease_gate.mark_active("test-binding", "bridge-thread-2");
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-y",
                "bridge-thread-2",
                AgentEventPayload::TextComplete { text: "Delegated in background.".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-y", "bridge-thread-2", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert_eq!(
            in_flight.peek("bridge-thread-2"),
            Some(777),
            "the hand-off relay must not consume the mapping — the delegate's completion hasn't \
             happened yet"
        );

        // No `record` call happens for the delegate completion — it re-enters
        // the same thread as a second, independent run.
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-y",
                "bridge-thread-2",
                AgentEventPayload::TextComplete { text: "here is the delegate's real answer".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-y", "bridge-thread-2", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert_eq!(
            sink.calls(),
            vec![
                ("agent-y".to_string(), 777, "Delegated in background.".to_string()),
                ("agent-y".to_string(), 777, "here is the delegate's real answer".to_string()),
            ],
            "both the hand-off and the delegate completion must relay"
        );
    }

    /// A `RunEnded` with no meaningful final text — no `TextComplete` was
    /// ever buffered, or it was empty/whitespace-only — must not relay, even
    /// though the mapping is present and would otherwise resolve fine. This
    /// is the "empty text is a no-op" guard.
    #[tokio::test]
    async fn empty_or_whitespace_only_text_is_a_no_op() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        in_flight.record("bridge-thread-3", 888);
        lease_gate.mark_active("test-binding", "bridge-thread-3");
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        // No TextComplete buffered at all.
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-z", "bridge-thread-3", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        // Whitespace-only TextComplete.
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-z",
                "bridge-thread-3",
                AgentEventPayload::TextComplete { text: "   ".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-z", "bridge-thread-3", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert!(sink.calls().is_empty(), "no meaningful text must never relay");
        assert_eq!(
            in_flight.peek("bridge-thread-3"),
            Some(888),
            "a no-op relay must not disturb the mapping either"
        );
    }

    /// A thread `in_flight` was never told about (the bridge never delivered
    /// a channel message to it, e.g. the app-typed-directly-in-the-UI case)
    /// must never relay — and the "error path" (no mapping resolved) must
    /// still drop any buffered text for that thread rather than leaking it
    /// forever.
    #[tokio::test]
    async fn a_thread_with_no_recorded_mapping_relays_nothing_and_still_invalidates_its_buffer() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-x",
                "main-thread",
                AgentEventPayload::TextComplete { text: "typed directly in the UI".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        assert!(pending_text.contains_key("main-thread"));

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-x", "main-thread", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert!(sink.calls().is_empty());
        assert!(
            !pending_text.contains_key("main-thread"),
            "the buffered text must be invalidated on RunEnded even when no mapping resolved"
        );
    }

    /// Once a thread's mapping is explicitly invalidated (the
    /// disable/token-delete/unlink path), a subsequent `RunEnded` must not
    /// relay, even with a perfectly good buffered reply.
    #[tokio::test]
    async fn relay_skips_run_ended_after_the_mapping_was_invalidated() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        in_flight.record("bridge-thread-4", 999);
        lease_gate.mark_active("test-binding", "bridge-thread-4");
        in_flight.remove("bridge-thread-4");
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-w",
                "bridge-thread-4",
                AgentEventPayload::TextComplete { text: "must never be relayed".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-w", "bridge-thread-4", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert!(sink.calls().is_empty(), "an invalidated mapping must never relay");
    }

    // --- RunEnded terminal-failure notices ----------------------------------

    /// (a) A run that ends in `Error` with nothing ever buffered must relay
    /// a sanitized, honest notice instead of leaving the channel silent —
    /// the whole point of this behavior.
    #[tokio::test]
    async fn run_ended_error_with_no_buffered_text_relays_a_failure_notice() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        in_flight.record("bridge-thread-err", 111);
        lease_gate.mark_active("test-binding", "bridge-thread-err");
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-err", "bridge-thread-err", AgentEventPayload::RunEnded { reason: RunEndReason::Error }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert_eq!(sink.calls(), vec![("agent-err".to_string(), 111, RUN_FAILED_NOTICE.to_string())]);
        assert_eq!(last_relayed.get("bridge-thread-err"), Some(&RUN_FAILED_NOTICE.to_string()));
    }

    /// (b) Regression guard: `RunEnded { reason: Completed }` behaviour is
    /// unchanged by the new failure-notice branch — no text buffered still
    /// means a silent no-op, never a notice.
    #[tokio::test]
    async fn run_ended_completed_with_no_buffered_text_is_still_a_silent_no_op() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        in_flight.record("bridge-thread-completed-noop", 222);
        lease_gate.mark_active("test-binding", "bridge-thread-completed-noop");
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-ok",
                "bridge-thread-completed-noop",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert!(sink.calls().is_empty(), "a normal completion with nothing buffered must never send a notice");
    }

    /// A `RunEnded { reason: Error }` that follows a successfully buffered
    /// reply (the post-reply-teardown-failure case) must relay only the
    /// reply, never an additional notice on top of it — sending both would
    /// contradict the reply the user already received.
    #[tokio::test]
    async fn run_ended_error_after_a_buffered_reply_relays_only_the_reply_not_a_notice() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        in_flight.record("bridge-thread-teardown", 333);
        lease_gate.mark_active("test-binding", "bridge-thread-teardown");
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-teardown",
                "bridge-thread-teardown",
                AgentEventPayload::TextComplete { text: "the real reply landed fine".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-teardown",
                "bridge-thread-teardown",
                AgentEventPayload::RunEnded { reason: RunEndReason::Error },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert_eq!(
            sink.calls(),
            vec![("agent-teardown".to_string(), 333, "the real reply landed fine".to_string())],
            "a post-reply teardown failure must not also send a contradicting failure notice"
        );
    }

    /// `Cancelled` gets its own, differently worded notice from `Error`.
    #[tokio::test]
    async fn run_ended_cancelled_with_no_buffered_text_relays_a_stopped_notice() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        in_flight.record("bridge-thread-cancelled", 444);
        lease_gate.mark_active("test-binding", "bridge-thread-cancelled");
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-cancel",
                "bridge-thread-cancelled",
                AgentEventPayload::RunEnded { reason: RunEndReason::Cancelled },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert_eq!(sink.calls(), vec![("agent-cancel".to_string(), 444, RUN_CANCELLED_NOTICE.to_string())]);
        assert_ne!(RUN_CANCELLED_NOTICE, RUN_FAILED_NOTICE, "cancelled must read differently from a crash");
    }

    /// `TimedOut` and `NoOutputTimeout` share one notice, distinct from both
    /// `Error`/`Signal`'s and `Cancelled`'s.
    #[tokio::test]
    async fn run_ended_timeout_variants_with_no_buffered_text_relay_a_timeout_notice() {
        for reason in [RunEndReason::TimedOut, RunEndReason::NoOutputTimeout] {
            let in_flight: CorrelationMap<i64> = CorrelationMap::new();
            let lease_gate = LeaseGate::new();
            in_flight.record("bridge-thread-timeout", 555);
            lease_gate.mark_active("test-binding", "bridge-thread-timeout");
            let sink = RecordingSink::default();
            let mut pending_text = HashMap::new();
            let mut last_relayed = HashMap::new();

            handle_relay_event(
                &lease_gate,
                &in_flight,
                &sink,
                make_event("agent-timeout", "bridge-thread-timeout", AgentEventPayload::RunEnded { reason }),
                &mut pending_text,
                &mut last_relayed,
            )
            .await;

            assert_eq!(
                sink.calls(),
                vec![("agent-timeout".to_string(), 555, RUN_TIMED_OUT_NOTICE.to_string())],
                "{reason:?} must relay the shared timeout notice"
            );
        }
    }

    /// `Signal` (the process was killed/interrupted) shares `Error`'s
    /// notice — both are "the agent crashed", from the user's perspective.
    #[tokio::test]
    async fn run_ended_signal_with_no_buffered_text_relays_the_same_notice_as_error() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        in_flight.record("bridge-thread-signal", 666);
        lease_gate.mark_active("test-binding", "bridge-thread-signal");
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-signal", "bridge-thread-signal", AgentEventPayload::RunEnded { reason: RunEndReason::Signal }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert_eq!(sink.calls(), vec![("agent-signal".to_string(), 666, RUN_FAILED_NOTICE.to_string())]);
    }

    /// A thread `in_flight` never recorded a mapping for must not relay a
    /// failure notice either — with no resolvable channel origin there is
    /// nowhere to send it, exactly as the happy path already requires.
    #[tokio::test]
    async fn run_ended_error_with_no_recorded_mapping_relays_nothing() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-x", "main-thread", AgentEventPayload::RunEnded { reason: RunEndReason::Error }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert!(sink.calls().is_empty());
    }

    /// (c) None of the terminal-failure notices may carry anything that
    /// looks like a raw Rust error, a debug-formatted payload, or a
    /// filesystem path — this is a fixed sanity check on the literal
    /// strings themselves, since `RunEnded` carries no error detail for
    /// `terminal_failure_notice` to leak in the first place.
    #[test]
    fn terminal_failure_notices_contain_no_raw_error_or_debug_payload() {
        for notice in [RUN_FAILED_NOTICE, RUN_TIMED_OUT_NOTICE, RUN_CANCELLED_NOTICE] {
            for marker in ["Error(", "panicked", "src/", ".rs:", "backtrace", "{\"", "Err(", "0x"] {
                assert!(
                    !notice.contains(marker),
                    "notice {notice:?} must not contain the suspicious marker {marker:?}"
                );
            }
        }
    }

    // --- recover_lagged_replies / recover_thread ---------------------------

    async fn make_persistence() -> (tempfile::TempDir, PersistenceLayer) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
        let persistence = PersistenceLayer::init_with_root(data_root)
            .await
            .expect("persistence layer inits");
        (tmp, persistence)
    }

    /// Builds a transcript entry shaped like the one
    /// `agent_runner::timeline_adapter::queue_response_entry` writes for a
    /// turn's final assistant reply — the entry [`recover_thread`] looks for.
    fn agent_response_entry(text: &str) -> ao_protocol::transcript::TranscriptEntry {
        ao_protocol::transcript::TranscriptEntry {
            ts: Utc::now(),
            role: ao_protocol::transcript::TranscriptRole::Agent { agent: "assistant".to_string() },
            content: text.to_string(),
            event_type: "response".to_string(),
            metadata: None,
            hidden_from_user: false,
        }
    }

    /// (a) Lagged recovery reads the thread's persisted transcript and
    /// relays the assistant's latest reply — the one a dropped
    /// `TextComplete`/`RunEnded` pair would otherwise have silently lost.
    #[tokio::test]
    async fn lag_recovery_relays_the_latest_unrelayed_transcript_reply() {
        let (_tmp, persistence) = make_persistence().await;
        let thread = persistence.threads.ensure_default_thread("agent-lag").await.expect("thread created");
        persistence
            .transcripts
            .append("agent-lag", &agent_response_entry("the reply that got lost in the lag"))
            .await
            .expect("transcript written");

        let in_flight: CorrelationMap<i64> = CorrelationMap::new();

        let lease_gate = LeaseGate::new();
        in_flight.record(&thread.id, 4242);
        lease_gate.mark_active("test-binding", &thread.id);
        let sink = RecordingSink::default();
        let mut last_relayed = HashMap::new();

        recover_lagged_replies(&lease_gate, &persistence, &in_flight, &sink, &mut last_relayed, 3).await;

        assert_eq!(
            sink.calls(),
            vec![("agent-lag".to_string(), 4242, "the reply that got lost in the lag".to_string())]
        );
        assert_eq!(
            last_relayed.get(&thread.id),
            Some(&"the reply that got lost in the lag".to_string()),
            "a successful recovery must record what it relayed, so a later lag doesn't re-send it"
        );
    }

    /// (b) A reply already relayed before the lag (tracked via
    /// `last_relayed`, exactly as a normal `handle_relay_event` call would
    /// have recorded it) must never be sent a second time by recovery.
    #[tokio::test]
    async fn lag_recovery_does_not_double_send_a_reply_already_relayed() {
        let (_tmp, persistence) = make_persistence().await;
        let thread = persistence.threads.ensure_default_thread("agent-dup").await.expect("thread created");
        persistence
            .transcripts
            .append("agent-dup", &agent_response_entry("already delivered before the lag"))
            .await
            .expect("transcript written");

        let in_flight: CorrelationMap<i64> = CorrelationMap::new();

        let lease_gate = LeaseGate::new();
        in_flight.record(&thread.id, 111);
        lease_gate.mark_active("test-binding", &thread.id);
        let sink = RecordingSink::default();
        let mut last_relayed = HashMap::new();
        last_relayed.insert(thread.id.clone(), "already delivered before the lag".to_string());

        recover_lagged_replies(&lease_gate, &persistence, &in_flight, &sink, &mut last_relayed, 7).await;

        assert!(sink.calls().is_empty(), "a reply already relayed before the lag must never be sent twice");
    }

    /// (b), end-to-end: a reply that went out through the normal
    /// `handle_relay_event` path (not a pre-seeded `last_relayed` entry)
    /// must still be recognized by a subsequent lag recovery pass over the
    /// same, now-stale transcript content — proving the two code paths
    /// actually share one dedup ledger rather than each keeping its own.
    #[tokio::test]
    async fn lag_recovery_does_not_double_send_after_a_normal_relay_already_covered_it() {
        let (_tmp, persistence) = make_persistence().await;
        let thread = persistence.threads.ensure_default_thread("agent-e2e-dup").await.expect("thread created");
        persistence
            .transcripts
            .append("agent-e2e-dup", &agent_response_entry("delivered the normal way"))
            .await
            .expect("transcript written");

        let in_flight: CorrelationMap<i64> = CorrelationMap::new();

        let lease_gate = LeaseGate::new();
        in_flight.record(&thread.id, 321);
        lease_gate.mark_active("test-binding", &thread.id);
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        // The turn completes through the ordinary event-driven path first —
        // no lag involved yet.
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-e2e-dup",
                &thread.id,
                AgentEventPayload::TextComplete { text: "delivered the normal way".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-e2e-dup", &thread.id, AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        assert_eq!(sink.calls().len(), 1, "the normal path must have relayed exactly once");

        // A lag now hits — the transcript's latest reply is the very same
        // one already relayed above.
        recover_lagged_replies(&lease_gate, &persistence, &in_flight, &sink, &mut last_relayed, 4).await;

        assert_eq!(
            sink.calls().len(),
            1,
            "recovery must recognize a reply the normal path already relayed and not send it again"
        );
    }

    /// (c) When the transcript itself can't be read (corrupt/unparseable
    /// file), recovery must degrade to an honest user-facing notice rather
    /// than staying silent — and must never panic doing so.
    #[tokio::test]
    async fn lag_recovery_degrades_to_a_notice_when_the_transcript_cannot_be_read() {
        let (_tmp, persistence) = make_persistence().await;
        let thread = persistence.threads.ensure_default_thread("agent-broken").await.expect("thread created");
        let path = std::path::PathBuf::from(&thread.transcript_path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.expect("parent dir created");
        }
        tokio::fs::write(&path, b"not valid json\n").await.expect("corrupt transcript written");

        let in_flight: CorrelationMap<i64> = CorrelationMap::new();

        let lease_gate = LeaseGate::new();
        in_flight.record(&thread.id, 999);
        lease_gate.mark_active("test-binding", &thread.id);
        let sink = RecordingSink::default();
        let mut last_relayed = HashMap::new();

        recover_lagged_replies(&lease_gate, &persistence, &in_flight, &sink, &mut last_relayed, 2).await;

        assert_eq!(
            sink.calls(),
            vec![("agent-broken".to_string(), 999, LAG_RECOVERY_FAILED_NOTICE.to_string())],
            "an unreadable transcript must degrade to an honest notice, not silence"
        );
    }

    /// A thread that exists but has no persisted assistant reply yet (the
    /// turn is still running, or never produced one) is left alone: nothing
    /// proves a reply was actually missed, so recovery must not manufacture
    /// a notice for it.
    #[tokio::test]
    async fn lag_recovery_is_a_silent_no_op_when_nothing_has_been_persisted_yet() {
        let (_tmp, persistence) = make_persistence().await;
        let thread = persistence.threads.ensure_default_thread("agent-quiet").await.expect("thread created");

        let in_flight: CorrelationMap<i64> = CorrelationMap::new();

        let lease_gate = LeaseGate::new();
        in_flight.record(&thread.id, 1);
        lease_gate.mark_active("test-binding", &thread.id);
        let sink = RecordingSink::default();
        let mut last_relayed = HashMap::new();

        recover_lagged_replies(&lease_gate, &persistence, &in_flight, &sink, &mut last_relayed, 5).await;

        assert!(sink.calls().is_empty(), "nothing persisted means nothing was necessarily missed");
    }

    /// No threads currently in flight: recovery must be a trivial no-op,
    /// never panicking on an empty snapshot.
    #[tokio::test]
    async fn lag_recovery_is_a_no_op_when_no_threads_are_in_flight() {
        let (_tmp, persistence) = make_persistence().await;
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        let sink = RecordingSink::default();
        let mut last_relayed = HashMap::new();

        recover_lagged_replies(&lease_gate, &persistence, &in_flight, &sink, &mut last_relayed, 1).await;

        assert!(sink.calls().is_empty());
    }

    // --- lag-recovery / terminal-failure-notice interaction -----------------

    /// Regression for the double-notification race: if
    /// `recover_lagged_replies` already relayed this run's reply straight
    /// from the transcript (because the broadcast lag that dropped the live
    /// `TextComplete` typically drops the surrounding events too, so
    /// `pending_text` never gets filled for this run), the live `RunEnded`
    /// that follows must not also emit a terminal-failure notice on top of
    /// it — even though its own buffered text is empty and its `reason`
    /// isn't `Completed`.
    #[tokio::test]
    async fn run_ended_after_a_lag_recovery_relay_does_not_also_send_a_failure_notice() {
        let (_tmp, persistence) = make_persistence().await;
        let thread = persistence.threads.ensure_default_thread("agent-lag-race").await.expect("thread created");
        persistence
            .transcripts
            .append("agent-lag-race", &agent_response_entry("recovered from the transcript"))
            .await
            .expect("transcript written");

        let in_flight: CorrelationMap<i64> = CorrelationMap::new();

        let lease_gate = LeaseGate::new();
        in_flight.record(&thread.id, 777);
        lease_gate.mark_active("test-binding", &thread.id);
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        // The lag hits first: recovery relays the reply straight from the
        // transcript because the live `TextComplete` for this run never
        // arrived.
        recover_lagged_replies(&lease_gate, &persistence, &in_flight, &sink, &mut last_relayed, 9).await;
        assert_eq!(sink.calls().len(), 1, "the lag recovery pass must have relayed the reply");

        // The live `RunEnded` for that same run then arrives — its own
        // `pending_text` is empty (the `TextComplete` that would have
        // filled it was lost in the same lag) — and it ends in `Error`.
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-lag-race", &thread.id, AgentEventPayload::RunEnded { reason: RunEndReason::Error }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert_eq!(
            sink.calls().len(),
            1,
            "the reply already recovered from the transcript must not also get a contradicting failure notice"
        );
    }

    /// The flip side of the regression above: `last_relayed` must not
    /// permanently silence every failure notice on a long-lived bridge
    /// thread just because *some* earlier run once relayed successfully.
    /// `RunStarted` is what draws the per-run boundary — a later, unrelated
    /// run on the very same thread must still get its own failure notice.
    #[tokio::test]
    async fn a_later_run_on_the_same_thread_still_gets_its_own_failure_notice() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        let lease_gate = LeaseGate::new();
        in_flight.record("bridge-thread-later-failure", 321);
        lease_gate.mark_active("test-binding", "bridge-thread-later-failure");
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        // First run: succeeds normally.
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-a",
                "bridge-thread-later-failure",
                AgentEventPayload::TextComplete { text: "first reply".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-a",
                "bridge-thread-later-failure",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        assert_eq!(sink.calls().len(), 1);

        // A later, independent run on the very same thread starts...
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event("agent-a", "bridge-thread-later-failure", AgentEventPayload::RunStarted),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        // ...and ends in error, with nothing ever buffered.
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-a",
                "bridge-thread-later-failure",
                AgentEventPayload::RunEnded { reason: RunEndReason::Error },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert_eq!(
            sink.calls(),
            vec![
                ("agent-a".to_string(), 321, "first reply".to_string()),
                ("agent-a".to_string(), 321, RUN_FAILED_NOTICE.to_string()),
            ],
            "the earlier run's success must not suppress this later run's own failure notice"
        );
    }

    // --- lease gate ----------------------------------------------------------
    //
    // The negative case these prove: a process that does not hold a
    // binding's single-writer lease must never relay on its behalf, even
    // though `run_outbound_observer` itself runs process-wide and keeps
    // receiving every agent's events regardless of lease state — see
    // `crate::telegram::bridge::ChannelBridge::reconcile` and `LeaseGate`'s
    // module doc.

    /// The core negative case: a thread with a populated correlation entry
    /// AND a buffered, substantive reply — everything the happy path needs
    /// to relay — must still not relay if this process's `LeaseGate` was
    /// never told it holds that thread's binding.
    #[tokio::test]
    async fn a_non_holder_with_a_populated_correlation_entry_does_not_relay_a_reply() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        in_flight.record("bridge-thread-not-leased", 555);
        // Deliberately never marked active — this process does not hold
        // (or has lost) this binding's lease.
        let lease_gate = LeaseGate::new();
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-x",
                "bridge-thread-not-leased",
                AgentEventPayload::TextComplete { text: "a reply a non-holder must never send".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-x",
                "bridge-thread-not-leased",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert!(sink.calls().is_empty(), "a process without this binding's lease must never relay its reply");
    }

    /// Same negative case for the terminal-failure-notice branch: a
    /// non-holder must not relay a failure notice either, even with a
    /// resolvable correlation entry and a genuinely failed run.
    #[tokio::test]
    async fn a_non_holder_with_a_populated_correlation_entry_does_not_relay_a_failure_notice() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        in_flight.record("bridge-thread-not-leased-err", 111);
        let lease_gate = LeaseGate::new();
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-err",
                "bridge-thread-not-leased-err",
                AgentEventPayload::RunEnded { reason: RunEndReason::Error },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert!(sink.calls().is_empty(), "a process without this binding's lease must never relay a failure notice");
    }

    /// A binding this process used to hold, and lost, must stop relaying
    /// immediately — proven by marking a thread active, relaying once, then
    /// marking it inactive (mirroring `ChannelBridge::reconcile`'s lease-loss
    /// path) and confirming a second, otherwise-identical run no longer
    /// relays.
    #[tokio::test]
    async fn a_thread_stops_relaying_the_instant_its_lease_is_marked_inactive() {
        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        in_flight.record("bridge-thread-loses-lease", 222);
        let lease_gate = LeaseGate::new();
        lease_gate.mark_active("test-binding", "bridge-thread-loses-lease");
        let sink = RecordingSink::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-x",
                "bridge-thread-loses-lease",
                AgentEventPayload::TextComplete { text: "reply while still the holder".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-x",
                "bridge-thread-loses-lease",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        assert_eq!(sink.calls().len(), 1, "the relay while still the holder must have gone through");

        // The lease is lost — `reconcile` would mark this inactive right
        // alongside `invalidate_thread`.
        lease_gate.mark_inactive("test-binding");

        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-x",
                "bridge-thread-loses-lease",
                AgentEventPayload::TextComplete { text: "reply after losing the lease".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_relay_event(
            &lease_gate,
            &in_flight,
            &sink,
            make_event(
                "agent-x",
                "bridge-thread-loses-lease",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert_eq!(
            sink.calls().len(),
            1,
            "once the lease is marked inactive, this process must never relay for this thread again"
        );
    }

    /// The same negative case for lag recovery: a non-holder must not relay
    /// a reply straight from the transcript either, even with a resolvable
    /// correlation entry and a real persisted reply waiting to be "found".
    #[tokio::test]
    async fn a_non_holder_does_not_relay_during_lag_recovery_either() {
        let (_tmp, persistence) = make_persistence().await;
        let thread = persistence.threads.ensure_default_thread("agent-not-leased").await.expect("thread created");
        persistence
            .transcripts
            .append("agent-not-leased", &agent_response_entry("must never reach a non-holder's sink"))
            .await
            .expect("transcript written");

        let in_flight: CorrelationMap<i64> = CorrelationMap::new();
        in_flight.record(&thread.id, 999);
        // Deliberately never marked active.
        let lease_gate = LeaseGate::new();
        let sink = RecordingSink::default();
        let mut last_relayed = HashMap::new();

        recover_lagged_replies(&lease_gate, &persistence, &in_flight, &sink, &mut last_relayed, 3).await;

        assert!(sink.calls().is_empty(), "lag recovery must never relay on behalf of a binding this process doesn't hold the lease for");
    }

    // `run_relay_observer` itself (subscribe + `tokio::select!` shutdown
    // handling) is deliberately not exercised here against a real
    // `EventBus`: a receiver only sees events sent *after* its own
    // `subscribe()` call, and a spawned task's `subscribe()` racing a
    // test's `emit()` calls is exactly the flaky setup `handle_relay_event`
    // exists to let tests avoid — mirroring
    // `crate::telegram::outbound::handle_event` and
    // `crate::channels::discord::outbound::handle_event`'s own doc comments
    // on why they're split out from their observer loops the same way.
}
