//! Outbound half of the Discord bridge — the Discord analogue of
//! [`crate::telegram::outbound`], mirroring its Telegram-observer design
//! wholesale: a single shared `EventBus::subscribe()` observer relays a
//! bridge thread's finished reply back to the channel it arrived on
//! (reply-to-origin per turn), not a second gateway connection and not a
//! new event mechanism. Discord is a synchronous chat channel exactly like
//! Telegram — unlike email's send-tool model — so the same shape applies
//! directly; the only channel-specific differences are the REST call shape
//! (`POST /channels/{channel_id}/messages` with a bot token, not
//! `sendMessage`), the lower message-length limit (2000-char hard cap,
//! chunked at a 1900-char threshold instead of Telegram's 4096), and
//! `allowed_mentions`, which Discord requires on every send to keep an
//! echoed `@everyone`/`@here`/role mention in agent-authored text from
//! actually paging the server.
//!
//! [`AgentEvent`] carries `agent_id`/`thread_id` but never the Discord
//! channel a reply should go to — that only exists on
//! [`super::InFlightChannels`], recorded by the inbound gateway loop right
//! before it submits a message onto a bridge thread. This observer reads
//! (without clearing) that mapping every time a thread's run ends, for the
//! same reason Telegram's does: an async `Delegate` call spawned mid-turn
//! ends the *triggering* run immediately (a "delegated in background"
//! hand-off), and the delegate's real answer later re-enters the same
//! bridge thread as a second, independent run — both must find the mapping
//! still in place to relay. The mapping is only ever cleared by explicit
//! invalidation ([`DiscordTransport::invalidate_thread`]) when the binding
//! itself ends.
//!
//! Unlike Telegram (one bot token per agent), a Discord bot token is scoped
//! per *binding* — an agent can run more than one Discord bot — so
//! [`ChannelOrigin`] also carries the `binding_id` the inbound message
//! arrived on, letting this observer resolve the right token back out of
//! [`DiscordTransport`]'s secret store.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};

use ao_persistence::PersistenceLayer;
use ao_protocol::event::AgentEvent;

use crate::channels::relay::chunker::chunk_text;
use crate::channels::relay::lease_gate::LeaseGate;
use crate::channels::relay::observer::{handle_relay_event, recover_lagged_replies, RelaySink};
use crate::event_bus::EventBus;

use super::outbound_seam::{DiscordSendSeam, ReqwestSendSeam};
use super::{ChannelOrigin, DiscordTransport};

/// Discord's hard cap on a message's `content` field. Chunking targets
/// [`DISCORD_CHUNK_THRESHOLD_CHARS`], comfortably under this — the `const _`
/// assertion below makes that relationship a compile-time-checked fact
/// rather than a comment that could silently drift.
const DISCORD_HARD_MAX_MESSAGE_CHARS: usize = 2000;

/// Chunking threshold: a reply longer than this is split on newline
/// boundaries into multiple sequential messages, mirroring
/// [`crate::telegram::outbound::chunk_for_telegram`]'s shape at a lower
/// limit (Discord's cap is roughly half Telegram's 4096).
const DISCORD_CHUNK_THRESHOLD_CHARS: usize = 1900;

const _: () = assert!(DISCORD_CHUNK_THRESHOLD_CHARS < DISCORD_HARD_MAX_MESSAGE_CHARS);

/// Runs until `shutdown_rx` fires. One subscription for the whole process —
/// every agent's events flow through it; only threads [`super::InFlightChannels`]
/// is currently tracking ever trigger a relay. Mirrors
/// [`crate::telegram::outbound::run_outbound_observer`].
pub(crate) async fn run_outbound_observer(
    transport: Arc<DiscordTransport>,
    persistence: Arc<PersistenceLayer>,
    lease_gate: Arc<LeaseGate>,
    event_bus: Arc<EventBus>,
    mut shutdown_rx: watch::Receiver<()>,
) {
    let mut events = event_bus.subscribe();
    let seam = ReqwestSendSeam::new(transport.http.clone());
    // thread_id -> latest TextComplete text seen since that thread's run started.
    let mut pending_text: HashMap<String, String> = HashMap::new();
    // thread_id -> text of the last reply actually relayed for that thread —
    // see `recover_lagged_replies` for why this is kept alongside `pending_text`.
    let mut last_relayed: HashMap<String, String> = HashMap::new();

    info!("DiscordBridge outbound observer starting");

    loop {
        let event = tokio::select! {
            _ = shutdown_rx.changed() => {
                info!("DiscordBridge outbound observer shutting down");
                return;
            }
            event = events.recv() => event,
        };

        let event = match event {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                let sink = DiscordRelaySink { transport: &transport, seam: &seam };
                recover_lagged_replies(
                    lease_gate.as_ref(),
                    persistence.as_ref(),
                    transport.in_flight.correlation_map(),
                    &sink,
                    &mut last_relayed,
                    skipped,
                )
                .await;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };

        handle_event(&transport, &lease_gate, &seam, event, &mut pending_text, &mut last_relayed).await;
    }
}

/// Processes one event from the shared bus: buffers `TextComplete` text per
/// thread, and on `RunEnded` looks up that thread's reply target and relays
/// the buffered text. Split out from [`run_outbound_observer`]'s loop so
/// tests can drive it directly with synthetic events instead of racing a
/// spawned task against the real broadcast channel — mirrors
/// [`crate::telegram::outbound::handle_event`]. Discord has no per-channel
/// side effects beyond the relay itself (no typing heartbeat, no delegate
/// logging), so this delegates the whole TextComplete-buffering /
/// RunEnded-resolve-and-relay sequence to the shared
/// [`handle_relay_event`].
async fn handle_event(
    transport: &DiscordTransport,
    lease_gate: &LeaseGate,
    seam: &dyn DiscordSendSeam,
    event: AgentEvent,
    pending_text: &mut HashMap<String, String>,
    last_relayed: &mut HashMap<String, String>,
) {
    let sink = DiscordRelaySink { transport, seam };
    handle_relay_event(lease_gate, transport.in_flight.correlation_map(), &sink, event, pending_text, last_relayed)
        .await;
}

/// Bundles the two halves a Discord relay send needs — the transport (for
/// token resolution) and the send seam (for the actual REST call) — behind
/// one [`RelaySink`] the shared observer can drive, since
/// [`RelaySink::relay`] only takes `&self`.
struct DiscordRelaySink<'a> {
    transport: &'a DiscordTransport,
    seam: &'a dyn DiscordSendSeam,
}

#[async_trait]
impl RelaySink<ChannelOrigin> for DiscordRelaySink<'_> {
    async fn relay(&self, agent_id: &str, origin: &ChannelOrigin, text: &str) {
        relay_reply(self.transport, self.seam, agent_id, origin, text).await;
    }
}

/// Looks up the origin binding's bot token and sends `text` to its channel,
/// chunked to Discord's message-length limit, with `allowed_mentions` set on
/// every chunk so agent-authored `@everyone`/`@here`/role mentions in the
/// reply can never actually page the channel. Chunks are sent sequentially,
/// in order, over the `seam`; if one fails the rest are dropped rather than
/// sent out of order. Any failure (missing/invalid token, network error,
/// non-2xx) is logged as a warning and swallowed — a failed relay must never
/// crash the turn, the thread, or the process. The token itself is never
/// logged, only the outcome.
async fn relay_reply(
    transport: &DiscordTransport,
    seam: &dyn DiscordSendSeam,
    agent_id: &str,
    origin: &ChannelOrigin,
    text: &str,
) {
    let Some(token) = transport.resolve_token(agent_id, &origin.binding_id) else {
        warn!(
            agent_id = %agent_id,
            binding_id = %origin.binding_id,
            "DiscordBridge: no bot token on file, cannot relay reply"
        );
        return;
    };

    for chunk in chunk_for_discord(text) {
        // `text` as a whole already passed the non-empty guard in
        // `handle_event`, but a chunk boundary can still land entirely on
        // whitespace (e.g. a long run of trailing blank lines split onto
        // its own chunk) — Discord rejects an effectively-empty `content`,
        // which would otherwise abort every chunk after it. Skip silently:
        // dropping pure whitespace from the middle of a reply loses nothing
        // meaningful.
        if chunk.trim().is_empty() {
            continue;
        }
        let payload = build_message_payload(chunk);
        if let Err(e) = seam.send(&token, &origin.channel_id, &payload).await {
            warn!(
                agent_id = %agent_id,
                channel_id = %origin.channel_id,
                "DiscordBridge: failed to relay reply chunk to Discord: {e}"
            );
            return;
        }
    }
}

/// Splits `text` into chunks no longer than [`DISCORD_CHUNK_THRESHOLD_CHARS`].
/// Prefers to break at the last newline within a chunk so a reply doesn't
/// get cut mid-sentence; falls back to a hard character cut when a single
/// line exceeds the threshold on its own. Delegates to the shared
/// [`crate::channels::relay::chunker::chunk_text`], parameterized at
/// Discord's own threshold.
fn chunk_for_discord(text: &str) -> Vec<&str> {
    chunk_text(text, DISCORD_CHUNK_THRESHOLD_CHARS)
}

/// `allowed_mentions` sent on every outbound message. Discord's
/// `allowed_mentions` contract is an allow-list, not a deny-list: only
/// mention types named in `parse` are actually pinged, everything else is
/// rendered inert. Naming only `"users"` here means a genuine `@user`
/// mention in the reply still pings that user, while `@everyone`/`@here`
/// and role mentions are always denied by omission — this must be attached
/// to every send regardless of the reply's content, since an LLM-authored
/// `@everyone` in agent text must never be able to page the server.
fn allowed_mentions_payload() -> serde_json::Value {
    serde_json::json!({ "parse": ["users"] })
}

/// Builds one outbound message body: `content` plus the mandatory
/// `allowed_mentions` guard.
fn build_message_payload(content: &str) -> serde_json::Value {
    serde_json::json!({
        "content": content,
        "allowed_mentions": allowed_mentions_payload(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use chrono::Utc;
    use uuid::Uuid;

    use ao_protocol::event::AgentEventPayload;

    use ao_engine_tools_provider_config::{ChannelSecretStore, DISCORD_TOKEN_SECRET_ROLE};
    use ao_persistence::paths::DataRoot;
    use ao_protocol::data_root::DATA_DIR_ENV_VAR;
    use ao_protocol::event::RunEndReason;
    use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

    use crate::channels::relay::observer::recover_lagged_replies;

    use super::super::outbound_seam::SendSeamError;

    struct EnvGuard {
        entries: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn set(pairs: &[(&'static str, &str)]) -> Self {
            let entries = pairs
                .iter()
                .map(|(k, v)| {
                    let prior = std::env::var(k).ok();
                    std::env::set_var(k, v);
                    (*k, prior)
                })
                .collect();
            Self { entries }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, prior) in &self.entries {
                match prior {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    // `LAUNCHPAD_STUDIO_DATA_DIR` is mutated by tests across this crate
    // (`lib.rs`, `agent_runner`, `telegram`, `plugin_paths`), so this must
    // serialize through the one crate-wide env lock rather than a mutex
    // local to this file — two uncoordinated mutexes over the same
    // process-wide var give no mutual exclusion against each other and let
    // tests on either side observe a sibling's temp root mid-run.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        crate::plugin_paths::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Fake [`DiscordSendSeam`] that records every call instead of hitting
    /// the network, so chunking/order/`allowed_mentions` can be asserted
    /// directly on the captured payloads.
    #[derive(Default)]
    struct RecordingSeam {
        calls: StdMutex<Vec<(String, String, serde_json::Value)>>,
    }

    impl RecordingSeam {
        fn calls(&self) -> Vec<(String, String, serde_json::Value)> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait]
    impl DiscordSendSeam for RecordingSeam {
        async fn send(&self, token: &str, channel_id: &str, body: &serde_json::Value) -> Result<(), SendSeamError> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((token.to_string(), channel_id.to_string(), body.clone()));
            Ok(())
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

    fn set_token(agent_id: &str, binding_id: &str, token: &str) {
        ChannelSecretStore::open()
            .expect("secret store opens")
            .set(agent_id, binding_id, DISCORD_TOKEN_SECRET_ROLE, token)
            .expect("token stored");
    }

    // --- chunk_for_discord ---------------------------------------------------

    #[test]
    fn chunk_for_discord_returns_one_chunk_when_under_the_threshold() {
        let chunks = chunk_for_discord("hello from the agent");
        assert_eq!(chunks, vec!["hello from the agent"]);
    }

    #[test]
    fn chunk_for_discord_splits_long_text_at_newline_boundaries_under_the_threshold() {
        let line = "x".repeat(100);
        let text = std::iter::repeat(line.as_str()).take(30).collect::<Vec<_>>().join("\n");
        assert!(text.chars().count() > DISCORD_HARD_MAX_MESSAGE_CHARS);

        let chunks = chunk_for_discord(&text);
        assert!(chunks.len() > 1, "expected more than one chunk");
        for chunk in &chunks {
            assert!(chunk.chars().count() <= DISCORD_CHUNK_THRESHOLD_CHARS);
        }
        assert_eq!(chunks.concat(), text, "chunking must preserve order and content");
    }

    #[test]
    fn chunk_for_discord_hard_cuts_a_single_line_that_exceeds_the_threshold() {
        let text = "y".repeat(DISCORD_CHUNK_THRESHOLD_CHARS * 2 + 10);
        let chunks = chunk_for_discord(&text);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= DISCORD_CHUNK_THRESHOLD_CHARS);
        }
        assert_eq!(chunks.concat(), text);
    }

    /// A run of trailing blank lines just past the threshold can land
    /// entirely on its own chunk once the preceding content is split off —
    /// this fixture exists so `relay_reply`'s whitespace-only-chunk skip
    /// (below) has a real case to prove itself against.
    #[test]
    fn chunk_for_discord_can_produce_a_whitespace_only_trailing_chunk() {
        let text = format!("{}{}", "a".repeat(1899), "\n".repeat(50));
        let chunks = chunk_for_discord(&text);
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks[1].trim().is_empty(),
            "the trailing chunk must be whitespace-only for this fixture to be meaningful"
        );
        assert_eq!(chunks.concat(), text);
    }

    // --- allowed_mentions ------------------------------------------------------

    #[test]
    fn allowed_mentions_payload_always_denies_everyone_here_and_roles() {
        let payload = allowed_mentions_payload();
        assert_eq!(payload, serde_json::json!({ "parse": ["users"] }));
        let parse = payload["parse"].as_array().expect("parse is an array");
        assert!(!parse.iter().any(|v| v == "everyone"), "everyone must never be in parse");
        assert!(!parse.iter().any(|v| v == "here"), "here must never be in parse");
        assert!(!parse.iter().any(|v| v == "roles"), "roles must never be in parse");
    }

    #[test]
    fn build_message_payload_always_carries_the_allowed_mentions_guard() {
        let payload = build_message_payload("hello");
        assert_eq!(
            payload,
            serde_json::json!({ "content": "hello", "allowed_mentions": { "parse": ["users"] } })
        );
    }

    // --- handle_event / relay_reply --------------------------------------------

    #[tokio::test]
    async fn a_short_reply_sends_as_a_single_message_to_the_recorded_channel() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);
        set_token("agent-x", "binding-1", "tok-1");

        let transport = DiscordTransport::new();

        let lease_gate = LeaseGate::new();
        transport.in_flight.record("bridge-thread-1", "channel-9".to_string(), "binding-1".to_string(), false);
        lease_gate.mark_active("test-binding", "bridge-thread-1");

        let seam = RecordingSeam::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event(
                "agent-x",
                "bridge-thread-1",
                AgentEventPayload::TextComplete { text: "short reply".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event("agent-x", "bridge-thread-1", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        let calls = seam.calls();
        assert_eq!(calls.len(), 1, "a reply under the threshold must send as one message");
        assert_eq!(calls[0].0, "tok-1");
        assert_eq!(calls[0].1, "channel-9");
        assert_eq!(calls[0].2["content"], "short reply");
        assert_eq!(calls[0].2["allowed_mentions"], serde_json::json!({ "parse": ["users"] }));
    }

    #[tokio::test]
    async fn a_long_reply_sends_as_multiple_sequential_messages_in_order() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);
        set_token("agent-y", "binding-1", "tok-2");

        let transport = DiscordTransport::new();

        let lease_gate = LeaseGate::new();
        transport.in_flight.record("bridge-thread-2", "channel-1".to_string(), "binding-1".to_string(), false);
        lease_gate.mark_active("test-binding", "bridge-thread-2");

        let line = "z".repeat(100);
        let long_text = std::iter::repeat(line.as_str()).take(30).collect::<Vec<_>>().join("\n");
        let expected_chunks: Vec<String> = chunk_for_discord(&long_text).into_iter().map(str::to_string).collect();
        assert!(expected_chunks.len() > 1, "test fixture must actually exercise multi-chunk chunking");

        let seam = RecordingSeam::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event("agent-y", "bridge-thread-2", AgentEventPayload::TextComplete { text: long_text.clone() }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event("agent-y", "bridge-thread-2", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        let calls = seam.calls();
        assert_eq!(calls.len(), expected_chunks.len(), "one send per chunk");
        for (call, expected_chunk) in calls.iter().zip(expected_chunks.iter()) {
            assert_eq!(call.0, "tok-2");
            assert_eq!(call.1, "channel-1");
            assert_eq!(call.2["content"].as_str().unwrap(), expected_chunk.as_str());
        }
        assert_eq!(
            calls.iter().map(|c| c.2["content"].as_str().unwrap()).collect::<String>(),
            long_text,
            "chunks sent in order must reassemble the original reply"
        );
    }

    /// A reply that as a whole is non-empty (so `handle_event`'s guard lets
    /// it through) can still chunk into a trailing whitespace-only piece —
    /// see `chunk_for_discord_can_produce_a_whitespace_only_trailing_chunk`.
    /// `relay_reply` must skip that chunk rather than sending an
    /// effectively-empty message (which Discord would reject, aborting
    /// every chunk after it under the abort-on-failure contract).
    #[tokio::test]
    async fn a_trailing_whitespace_only_chunk_is_skipped_without_erroring() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);
        set_token("agent-blank", "binding-1", "tok-blank");

        let transport = DiscordTransport::new();

        let lease_gate = LeaseGate::new();
        transport.in_flight.record("bridge-thread-blank", "channel-5".to_string(), "binding-1".to_string(), false);
        lease_gate.mark_active("test-binding", "bridge-thread-blank");

        let text = format!("{}{}", "a".repeat(1899), "\n".repeat(50));
        assert_eq!(chunk_for_discord(&text).len(), 2, "test fixture must actually produce two chunks");

        let seam = RecordingSeam::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event("agent-blank", "bridge-thread-blank", AgentEventPayload::TextComplete { text: text.clone() }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event(
                "agent-blank",
                "bridge-thread-blank",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        let calls = seam.calls();
        assert_eq!(calls.len(), 1, "the whitespace-only trailing chunk must be skipped, not sent");
        assert_eq!(calls[0].2["content"].as_str().unwrap(), format!("{}\n", "a".repeat(1899)));
    }

    #[tokio::test]
    async fn empty_or_whitespace_only_reply_sends_nothing() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);
        // Deliberately no token stored: the empty-text guard must short
        // circuit before token resolution (and thus the send) ever runs.

        let transport = DiscordTransport::new();

        let lease_gate = LeaseGate::new();
        transport.in_flight.record("bridge-thread-3", "channel-2".to_string(), "binding-1".to_string(), false);
        lease_gate.mark_active("test-binding", "bridge-thread-3");

        let seam = RecordingSeam::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        // No TextComplete buffered at all.
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event("agent-z", "bridge-thread-3", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        // Whitespace-only TextComplete.
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event("agent-z", "bridge-thread-3", AgentEventPayload::TextComplete { text: "   ".to_string() }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event("agent-z", "bridge-thread-3", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert!(seam.calls().is_empty(), "no meaningful text must never reach the Discord REST send");
    }

    /// Regression for the async-delegate relay case, mirroring
    /// [`crate::telegram::outbound::tests::relay_delivers_both_the_hand_off_and_the_later_delegate_completion_on_the_same_thread`]:
    /// the triggering turn ends immediately with a hand-off reply, then the
    /// delegate's real answer later fires a second, independent `RunEnded`
    /// on the same thread. Both must relay, which requires `peek` (not a
    /// consuming read) to resolve on both lookups.
    #[tokio::test]
    async fn peek_not_take_lets_a_second_run_ended_on_the_same_thread_still_resolve_and_relay() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);
        set_token("agent-w", "binding-1", "tok-delegate");

        let transport = DiscordTransport::new();

        let lease_gate = LeaseGate::new();
        transport.in_flight.record("bridge-thread-4", "channel-3".to_string(), "binding-1".to_string(), false);
        lease_gate.mark_active("test-binding", "bridge-thread-4");

        let seam = RecordingSeam::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        // The triggering turn: spawns an async Delegate and ends immediately
        // with a hand-off acknowledgement.
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event(
                "agent-w",
                "bridge-thread-4",
                AgentEventPayload::TextComplete { text: "Delegated in background.".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event("agent-w", "bridge-thread-4", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert_eq!(
            transport.in_flight.peek("bridge-thread-4").map(|o| o.channel_id),
            Some("channel-3".to_string()),
            "the hand-off relay must not consume the mapping — the delegate's completion hasn't happened yet"
        );

        // The delegate's completion re-enters the same thread as a second,
        // independent run — no `record` call happens for it.
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event(
                "agent-w",
                "bridge-thread-4",
                AgentEventPayload::TextComplete { text: "here is the delegate's real answer".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event("agent-w", "bridge-thread-4", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        let calls = seam.calls();
        assert_eq!(calls.len(), 2, "both the hand-off and the delegate completion must relay");
        assert_eq!(calls[0].2["content"], "Delegated in background.");
        assert_eq!(calls[1].2["content"], "here is the delegate's real answer");
        assert_eq!(
            transport.in_flight.peek("bridge-thread-4").map(|o| o.channel_id),
            Some("channel-3".to_string()),
            "peek must be repeatable — two lookups (and two relays) both resolved the mapping"
        );
    }

    #[tokio::test]
    async fn a_thread_the_bridge_never_delivered_a_discord_message_to_relays_nothing() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = DiscordTransport::new();

        let lease_gate = LeaseGate::new();
        let seam = RecordingSeam::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event(
                "agent-x",
                "main-thread",
                AgentEventPayload::TextComplete { text: "typed directly in the UI".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event("agent-x", "main-thread", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert!(seam.calls().is_empty());
    }

    #[tokio::test]
    async fn missing_token_relays_nothing_and_does_not_panic() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);
        // No token stored for this (agent, binding) pair.

        let transport = DiscordTransport::new();

        let lease_gate = LeaseGate::new();
        transport.in_flight.record("bridge-thread-5", "channel-4".to_string(), "binding-1".to_string(), false);
        lease_gate.mark_active("test-binding", "bridge-thread-5");

        let seam = RecordingSeam::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event(
                "agent-no-token",
                "bridge-thread-5",
                AgentEventPayload::TextComplete { text: "hello".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event(
                "agent-no-token",
                "bridge-thread-5",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert!(seam.calls().is_empty());
    }

    // --- RunEnded terminal-failure notices, through the real send path -----

    /// (a) A run that ends in `Error` with no `TextComplete` ever buffered
    /// must relay a sanitized failure notice through Discord's real send
    /// seam — the same terminal-reason handling proven generically in
    /// `crate::channels::relay::observer::tests`, exercised here end-to-end
    /// so it's also verified through token resolution and the REST payload
    /// shape (including the mandatory `allowed_mentions` guard).
    #[tokio::test]
    async fn run_ended_error_with_no_reply_relays_a_failure_notice_through_discord() {
        use crate::channels::relay::observer::RUN_FAILED_NOTICE;

        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);
        set_token("agent-error", "binding-1", "tok-error");

        let transport = DiscordTransport::new();

        let lease_gate = LeaseGate::new();
        transport.in_flight.record("bridge-thread-error", "channel-error".to_string(), "binding-1".to_string(), false);
        lease_gate.mark_active("test-binding", "bridge-thread-error");

        let seam = RecordingSeam::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event(
                "agent-error",
                "bridge-thread-error",
                AgentEventPayload::RunEnded { reason: RunEndReason::Error },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        let calls = seam.calls();
        assert_eq!(calls.len(), 1, "an errored run with no reply must still relay exactly one notice");
        assert_eq!(calls[0].0, "tok-error");
        assert_eq!(calls[0].1, "channel-error");
        assert_eq!(calls[0].2["content"], RUN_FAILED_NOTICE);
        assert_eq!(calls[0].2["allowed_mentions"], serde_json::json!({ "parse": ["users"] }));
    }

    /// (b) Regression guard: a normal `Completed` run with no reply text
    /// still sends nothing at all through Discord — the new failure-notice
    /// branch must not fire for a clean completion.
    #[tokio::test]
    async fn run_ended_completed_with_no_reply_still_sends_nothing_through_discord() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);
        set_token("agent-clean", "binding-1", "tok-clean");

        let transport = DiscordTransport::new();

        let lease_gate = LeaseGate::new();
        transport.in_flight.record("bridge-thread-clean", "channel-clean".to_string(), "binding-1".to_string(), false);
        lease_gate.mark_active("test-binding", "bridge-thread-clean");

        let seam = RecordingSeam::default();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            &seam,
            make_event(
                "agent-clean",
                "bridge-thread-clean",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
        )
        .await;

        assert!(
            seam.calls().is_empty(),
            "a clean completion with nothing buffered must never send a failure notice"
        );
    }

    // --- lag recovery, through the real Discord send path -----------------

    /// Integration proof that `recover_lagged_replies` (shared with
    /// Telegram, unit-tested generically in
    /// `crate::channels::relay::observer::tests`) also works end-to-end
    /// through Discord's own token resolution and REST payload shape: a
    /// reply recovered from the persisted transcript must reach
    /// `DiscordSendSeam::send` exactly as `relay_reply` would have sent it
    /// live.
    #[tokio::test]
    async fn lag_recovery_relays_a_missed_reply_through_the_real_discord_send_path() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);
        set_token("agent-lag-discord", "binding-1", "tok-lag");

        let persistence = PersistenceLayer::init_with_root(DataRoot::new(tmp.path()))
            .await
            .expect("persistence layer inits");
        let thread = persistence
            .threads
            .ensure_default_thread("agent-lag-discord")
            .await
            .expect("thread created");
        persistence
            .transcripts
            .append(
                "agent-lag-discord",
                &TranscriptEntry {
                    ts: Utc::now(),
                    role: TranscriptRole::Agent { agent: "assistant".to_string() },
                    content: "reply recovered from the transcript".to_string(),
                    event_type: "response".to_string(),
                    metadata: None,
                    hidden_from_user: false,
                },
            )
            .await
            .expect("transcript written");

        let transport = DiscordTransport::new();

        let lease_gate = LeaseGate::new();
        transport.in_flight.record(&thread.id, "channel-lag".to_string(), "binding-1".to_string(), false);
        lease_gate.mark_active("test-binding", &thread.id);

        let seam = RecordingSeam::default();
        let sink = DiscordRelaySink { transport: &transport, seam: &seam };
        let mut last_relayed = HashMap::new();

        recover_lagged_replies(&lease_gate, &persistence, transport.in_flight.correlation_map(), &sink, &mut last_relayed, 5).await;

        let calls = seam.calls();
        assert_eq!(calls.len(), 1, "the recovered reply must reach the Discord send seam exactly once");
        assert_eq!(calls[0].0, "tok-lag");
        assert_eq!(calls[0].1, "channel-lag");
        assert_eq!(calls[0].2["content"], "reply recovered from the transcript");
        assert_eq!(calls[0].2["allowed_mentions"], serde_json::json!({ "parse": ["users"] }));
    }
}
