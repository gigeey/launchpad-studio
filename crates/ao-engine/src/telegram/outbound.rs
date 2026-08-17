//! Outbound half of the Telegram bridge.
//!
//! Complements [`super::bridge`]'s inbound long-poll loop with the reverse
//! path: when an agent finishes a turn that was triggered by an inbound
//! Telegram message, relay its final reply back to the originating chat via
//! the Bot API's `sendMessage`. Implemented as a single shared
//! `EventBus::subscribe()` observer (not a second poll loop and not a new
//! event mechanism) — the same terminal contract the SSE route already
//! relies on: the last `TextComplete` seen since a thread's run started,
//! flushed at `RunEnded`.
//!
//! `AgentEvent` carries `agent_id`/`thread_id` but never the Telegram
//! `chat_id` a reply should go to — that only exists on the `QueuedMessage`
//! at dispatch time. [`InFlightChats`] bridges the gap: the inbound side
//! records `thread_id -> chat_id` right before submitting a message, and
//! this observer reads (without clearing) the mapping every time that
//! thread's run ends. The mapping deliberately outlives a single run: an
//! async `Delegate` call spawned mid-turn ends the *triggering* run
//! immediately (a "delegated in background" hand-off), and the delegate's
//! real answer later re-enters the same bridge thread as a second,
//! independent run (see `crate::delegate_completion`) — both must find the
//! mapping still in place to relay. The mapping is only ever cleared by
//! explicit invalidation ([`TelegramTransport::invalidate_thread`] /
//! [`TelegramTransport::invalidate_thread_for_chat`]) when the binding
//! itself ends: disabled, token rotated away or deleted, or a chat unlinked.
//!
//! This observer also owns the outbound *typing indicator*: on `RunStarted`
//! for a thread `InFlightChats` recognizes, it spawns a background task that
//! pings `sendChatAction(chat_id, "typing")` every few seconds until
//! `RunEnded` cancels it, keeping Telegram's native indicator alive for the
//! whole turn instead of just its final reply.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use ao_persistence::PersistenceLayer;
use ao_protocol::event::{AgentEvent, AgentEventPayload};

use crate::channels::relay::chunker::chunk_text;
use crate::channels::relay::correlation_map::CorrelationMap;
use crate::channels::relay::lease_gate::LeaseGate;
use crate::channels::relay::observer::{handle_relay_event, recover_lagged_replies, RelaySink};
use crate::event_bus::EventBus;

use super::client::TelegramClient;
use super::html::markdown_to_telegram_html;
use super::transport::TelegramTransport;

/// Telegram's hard `sendMessage` cap. Approximated in `char`s rather than
/// UTF-16 code units (the unit Telegram actually counts) — adequate for the
/// plain-text replies this MVP relays; exact multilingual counting is a
/// follow-up if it ever matters.
const TELEGRAM_MAX_MESSAGE_CHARS: usize = 4096;

/// How often the typing heartbeat re-pings `sendChatAction` while a turn is
/// running. Telegram's own "typing…" indicator auto-expires ~5s after the
/// last ping, so this must stay comfortably under that or the indicator
/// visibly drops before the real reply lands.
const TYPING_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(4);

/// Tracks which chat a bridge thread's in-flight (or just-finished, until
/// explicitly invalidated) turn should reply to. Keyed by thread id alone:
/// thread ids are globally unique UUIDs (one dedicated bridge thread per
/// agent), so no agent id is needed to disambiguate.
/// A single thread can have several chats linked to it (multi-user pairing);
/// this map only ever holds the most recently recorded one — a known,
/// pre-existing single-slot limitation, not something the invalidation
/// methods below attempt to fix.
///
/// A thin `chat_id`-specialized wrapper over the shared
/// [`CorrelationMap`], re-exposing each operation at this type's
/// original by-value `chat_id: i64` signature (the shared map takes
/// `remove_if_matches`'s value by reference, generic over non-`Copy`
/// value types too).
pub(super) struct InFlightChats(CorrelationMap<i64>);

impl InFlightChats {
    pub(super) fn new() -> Self {
        Self(CorrelationMap::new())
    }

    /// Called by the inbound poll loop right before it submits a message
    /// onto `thread_id`.
    pub(super) fn record(&self, thread_id: &str, chat_id: i64) {
        self.0.record(thread_id, chat_id);
    }

    /// Unconditionally drops the mapping for `thread_id`. Called when a
    /// binding ends outright — disabled, token rotated away, or deleted —
    /// so a later run on this thread (e.g. a stray delegate completion) has
    /// nothing left to relay to.
    pub(super) fn remove(&self, thread_id: &str) {
        self.0.remove(thread_id);
    }

    /// Drops the mapping for `thread_id` only if it currently points at
    /// `chat_id`. Used when unlinking one specific chat: with several chats
    /// sharing one dedicated thread, an in-flight reply actually destined
    /// for a different, still-linked chat must survive.
    pub(super) fn remove_if_matches(&self, thread_id: &str, chat_id: i64) {
        self.0.remove_if_matches(thread_id, &chat_id);
    }

    /// Reads the chat mapped to `thread_id` without removing it. Used both
    /// at `RunStarted` (to decide whether a turn needs a typing heartbeat)
    /// and at `RunEnded` (to resolve the relay target) — the mapping is
    /// never consumed by reading it; only [`Self::remove`] and
    /// [`Self::remove_if_matches`] clear it.
    pub(super) fn peek(&self, thread_id: &str) -> Option<i64> {
        self.0.peek(thread_id)
    }

    /// Exposes the underlying shared map for the relay observer's own
    /// [`handle_relay_event`] call, which operates generically over
    /// [`CorrelationMap`] rather than this type's by-value wrapper methods.
    fn correlation_map(&self) -> &CorrelationMap<i64> {
        &self.0
    }
}

/// Runs until `shutdown_rx` fires. One subscription for the whole process —
/// every agent's events flow through it; only threads [`InFlightChats`] is
/// currently tracking ever trigger a relay.
pub(super) async fn run_outbound_observer(
    transport: Arc<TelegramTransport>,
    persistence: Arc<PersistenceLayer>,
    lease_gate: Arc<LeaseGate>,
    event_bus: Arc<EventBus>,
    mut shutdown_rx: watch::Receiver<()>,
) {
    let mut events = event_bus.subscribe();
    // thread_id -> latest TextComplete text seen since that thread's run started.
    let mut pending_text: HashMap<String, String> = HashMap::new();
    // thread_id -> text of the last reply actually relayed for that thread —
    // see `recover_lagged_replies` for why this is kept alongside `pending_text`.
    let mut last_relayed: HashMap<String, String> = HashMap::new();
    // thread_id -> cancel signal for that thread's in-flight typing heartbeat.
    let mut heartbeats: HashMap<String, CancellationToken> = HashMap::new();

    info!("TelegramBridge outbound observer starting");

    loop {
        let event = tokio::select! {
            _ = shutdown_rx.changed() => {
                info!("TelegramBridge outbound observer shutting down");
                for cancel in heartbeats.values() {
                    cancel.cancel();
                }
                return;
            }
            event = events.recv() => event,
        };

        let event = match event {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                recover_lagged_replies(
                    lease_gate.as_ref(),
                    persistence.as_ref(),
                    transport.in_flight().correlation_map(),
                    transport.as_ref(),
                    &mut last_relayed,
                    skipped,
                )
                .await;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };

        handle_event(&transport, &lease_gate, event, &mut pending_text, &mut last_relayed, &mut heartbeats).await;
    }
}

/// Processes one event from the shared bus: buffers `TextComplete` text per
/// thread, and on `RunEnded` looks up that thread's reply target and relays
/// the buffered text. Split out from [`run_outbound_observer`]'s loop so
/// tests can drive it directly with synthetic events instead of racing a
/// spawned task against the real broadcast channel.
async fn handle_event(
    transport: &TelegramTransport,
    lease_gate: &LeaseGate,
    event: AgentEvent,
    pending_text: &mut HashMap<String, String>,
    last_relayed: &mut HashMap<String, String>,
    heartbeats: &mut HashMap<String, CancellationToken>,
) {
    let Some(thread_id) = event.thread_id.clone() else {
        return;
    };

    // Telegram-only side effects (typing heartbeat lifecycle, delegate
    // logging) inspect the event by reference here — `event` itself is
    // still handed to `handle_relay_event` below intact, which owns the
    // shared TextComplete-buffering / RunEnded-resolve-and-relay logic
    // (see `crate::channels::relay::observer`).
    match &event.payload {
        AgentEventPayload::RunStarted => {
            start_typing_heartbeat(transport, &event.agent_id, &thread_id, heartbeats);
        }
        AgentEventPayload::RunEnded { .. } => {
            // Always stop a heartbeat for this thread on completion, whether
            // or not it turns out to be a Telegram-triggered run — mirrors
            // the shared observer's unconditional `pending_text` cleanup,
            // and matters for the same reason: a thread that never matches
            // `in_flight` must not leak state.
            if let Some(cancel) = heartbeats.remove(&thread_id) {
                cancel.cancel();
                info!(thread_id = %thread_id, "telegram heartbeat: cancelled on run end");
            }
        }
        AgentEventPayload::DelegateStarted { delegate_name, delegation_id, .. } => {
            info!(
                thread_id = %thread_id,
                delegate_name = %delegate_name,
                delegation_id = %delegation_id,
                "telegram outbound: delegate started"
            );
        }
        AgentEventPayload::DelegateComplete { delegate_name, delegation_id, status, .. } => {
            info!(
                thread_id = %thread_id,
                delegate_name = %delegate_name,
                delegation_id = %delegation_id,
                status = %status,
                "telegram outbound: delegate complete"
            );
        }
        _ => {}
    }

    handle_relay_event(lease_gate, transport.in_flight().correlation_map(), transport, event, pending_text, last_relayed)
        .await;
}

#[async_trait]
impl RelaySink<i64> for TelegramTransport {
    async fn relay(&self, agent_id: &str, origin: &i64, text: &str) {
        relay_reply(self, agent_id, *origin, text).await;
    }
}

/// Starts a typing-heartbeat task for `thread_id` if it's actually
/// Telegram-correlated — i.e. [`InFlightChats`] already holds a chat_id for
/// it, recorded by the inbound side before the message was ever submitted —
/// and a bot token is on file. Reads the mapping with the non-consuming
/// `peek`, same as `RunEnded`'s relay does.
fn start_typing_heartbeat(
    transport: &TelegramTransport,
    agent_id: &str,
    thread_id: &str,
    heartbeats: &mut HashMap<String, CancellationToken>,
) {
    let maybe_chat_id = transport.in_flight().peek(thread_id);
    info!(thread_id = %thread_id, chat_id = ?maybe_chat_id, "telegram heartbeat: run started, chat_id lookup");
    let Some(chat_id) = maybe_chat_id else {
        return;
    };

    let token = match transport.token_store() {
        Ok(store) => match store.get(agent_id) {
            Ok(Some(token)) => token,
            Ok(None) => {
                warn!(agent_id = %agent_id, "TelegramBridge: no bot token on file, cannot start typing heartbeat");
                return;
            }
            Err(e) => {
                warn!(agent_id = %agent_id, "TelegramBridge: failed to read bot token: {e}");
                return;
            }
        },
        Err(e) => {
            warn!(agent_id = %agent_id, "TelegramBridge: failed to open token store: {e}");
            return;
        }
    };

    let cancel = CancellationToken::new();
    let client = Arc::clone(transport.client());
    let agent_id = agent_id.to_string();
    tokio::spawn(run_typing_heartbeat(client, token, chat_id, agent_id, cancel.clone()));
    heartbeats.insert(thread_id.to_string(), cancel);
}

/// Pings `sendChatAction(chat_id, "typing")` immediately, then every
/// [`TYPING_HEARTBEAT_INTERVAL`] until `cancel` fires (from `RunEnded` or
/// observer shutdown). A failed ping is logged and swallowed — it must never
/// affect the real reply relay, which is a separate call on a separate path.
async fn run_typing_heartbeat(
    client: Arc<TelegramClient>,
    token: String,
    chat_id: i64,
    agent_id: String,
    cancel: CancellationToken,
) {
    loop {
        match client.send_chat_action(&token, chat_id, "typing").await {
            Ok(()) => {
                info!(chat_id = %chat_id, "telegram typing action sent");
            }
            Err(e) => {
                warn!(
                    agent_id = %agent_id,
                    chat_id,
                    "TelegramBridge: failed to send typing heartbeat: {e}"
                );
                warn!(chat_id = %chat_id, error = %e, "telegram typing action FAILED");
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(TYPING_HEARTBEAT_INTERVAL) => {}
        }
    }
}

/// Looks up the agent's bot token and sends `text` to `chat_id`, chunked to
/// Telegram's message-length limit. Each chunk is converted from the agent's
/// CommonMark-ish markdown to Telegram's HTML subset and sent with
/// `parse_mode: "HTML"` so formatting actually renders on the phone instead
/// of showing up as literal `**`/`#`/`-` characters. If Telegram rejects the
/// HTML send (e.g. a converter bug produces unbalanced markup, surfaced as a
/// 400 "can't parse entities"), that one chunk is retried once as plain text
/// with no `parse_mode` — delivering the reply, even unformatted, always
/// beats dropping it. Any other failure (missing/invalid token, network
/// error) is logged as a warning and swallowed — a failed relay must never
/// crash the turn, the thread, or the process.
async fn relay_reply(transport: &TelegramTransport, agent_id: &str, chat_id: i64, text: &str) {
    let token = match transport.token_store() {
        Ok(store) => match store.get(agent_id) {
            Ok(Some(token)) => token,
            Ok(None) => {
                warn!(agent_id = %agent_id, "TelegramBridge: no bot token on file, cannot relay reply");
                return;
            }
            Err(e) => {
                warn!(agent_id = %agent_id, "TelegramBridge: failed to read bot token: {e}");
                return;
            }
        },
        Err(e) => {
            warn!(agent_id = %agent_id, "TelegramBridge: failed to open token store: {e}");
            return;
        }
    };

    for chunk in chunk_for_telegram(text) {
        let html = markdown_to_telegram_html(chunk);
        if let Err(e) = transport.client().send_message(&token, chat_id, &html, Some("HTML")).await {
            warn!(
                agent_id = %agent_id,
                chat_id,
                "TelegramBridge: HTML send failed, retrying this chunk as plain text: {e}"
            );
            if let Err(e) = transport.client().send_message(&token, chat_id, chunk, None).await {
                warn!(
                    agent_id = %agent_id,
                    chat_id,
                    "TelegramBridge: plain-text fallback also failed to relay reply to Telegram: {e}"
                );
                return;
            }
        }
    }
}

/// Splits `text` into chunks no longer than Telegram's `sendMessage` limit.
/// Prefers to break at the last newline within a chunk so a reply doesn't
/// get cut mid-sentence; falls back to a hard character cut when a single
/// line exceeds the limit on its own. Delegates to the shared
/// [`crate::channels::relay::chunker::chunk_text`], parameterized at
/// Telegram's own limit.
fn chunk_for_telegram(text: &str) -> Vec<&str> {
    chunk_text(text, TELEGRAM_MAX_MESSAGE_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Utc;
    use uuid::Uuid;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use ao_persistence::paths::DataRoot;
    use ao_protocol::data_root::DATA_DIR_ENV_VAR;
    use ao_protocol::event::RunEndReason;
    use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

    use crate::telegram::client::TelegramClient;

    // Shared across `telegram`'s submodules — see `super::super::test_env`
    // — since `client`, `bridge`, and `transport`'s tests mutate the same
    // process-wide env vars (data root, Telegram API base, file-fallback
    // flag) and would otherwise race this module's tests under parallel test
    // threads.
    use crate::telegram::test_env::lock as lock_env;

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

    /// Builds a `TelegramTransport` wired to a Telegram client pointed at
    /// the test's mock server. `LAUNCHPAD_TELEGRAM_API_BASE_URL` must
    /// already be set before calling this — `TelegramClient::new()` reads it
    /// synchronously at construction time, inside this function.
    fn make_transport() -> TelegramTransport {
        TelegramTransport::new(Arc::new(TelegramClient::new()))
    }

    fn make_event(
        agent_id: &str,
        thread_id: &str,
        payload: AgentEventPayload,
    ) -> AgentEvent {
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
    async fn relay_sends_last_text_complete_to_the_recorded_chat_on_run_ended() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = make_transport();

        let lease_gate = LeaseGate::new();

        Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/bottok-1/sendMessage"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "chat_id": 555,
                "text": "final reply text",
                "parse_mode": "HTML"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 1 }
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        transport
            .token_store()
            .expect("token store opens")
            .set("agent-x", "tok-1")
            .expect("token stored");
        transport.in_flight().record("bridge-thread-1", 555);
        lease_gate.mark_active("test-binding", "bridge-thread-1");

        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();
        // A turn typically emits several TextComplete events (interleaved
        // with tool calls) — only the last one before RunEnded should relay.
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-x",
                "bridge-thread-1",
                AgentEventPayload::TextComplete { text: "draft reply, superseded".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-x",
                "bridge-thread-1",
                AgentEventPayload::TextComplete { text: "final reply text".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-x",
                "bridge-thread-1",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        mock_server.verify().await;
        assert_eq!(
            transport.in_flight().peek("bridge-thread-1"),
            Some(555),
            "in-flight mapping must survive a relay — a later async-delegate \
             completion on the same thread needs it too"
        );
    }

    /// If Telegram rejects the HTML-formatted send (e.g. a converter bug
    /// produces markup Telegram's parser calls a 400 "can't parse entities"
    /// on), the relay must retry the same chunk once as plain text rather
    /// than dropping the reply — delivery matters more than formatting.
    #[tokio::test]
    async fn relay_falls_back_to_plain_text_when_the_html_send_is_rejected() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = make_transport();

        let lease_gate = LeaseGate::new();

        Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/bottok-fallback/sendMessage"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "chat_id": 333,
                "text": "<b>final</b> reply",
                "parse_mode": "HTML"
            })))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Bad Request: can't parse entities"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;
        Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/bottok-fallback/sendMessage"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "chat_id": 333,
                "text": "**final** reply"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 1 }
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        transport
            .token_store()
            .expect("token store opens")
            .set("agent-fallback", "tok-fallback")
            .expect("token stored");
        transport.in_flight().record("bridge-thread-fallback", 333);
        lease_gate.mark_active("test-binding", "bridge-thread-fallback");

        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-fallback",
                "bridge-thread-fallback",
                AgentEventPayload::TextComplete { text: "**final** reply".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-fallback",
                "bridge-thread-fallback",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        mock_server.verify().await;
    }

    /// Regression for the async-delegate relay bug: spawning a `Delegate`
    /// mid-turn ends the *triggering* run immediately with a hand-off reply
    /// ("delegated in background"), then the delegate's real answer later
    /// re-enters the same transport thread as a second, independent run (see
    /// `crate::delegate_completion::QueueDelegateCompletionSink`) — which
    /// only ever calls `submit_to_agent`, never `InFlightChats::record`
    /// again. Both RunEnded completions on this thread must relay to the
    /// same chat, and the mapping must still be there for the second one.
    #[tokio::test]
    async fn relay_delivers_both_the_hand_off_and_the_later_delegate_completion_on_the_same_thread() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = make_transport();

        let lease_gate = LeaseGate::new();

        Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/bottok-delegate/sendMessage"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "chat_id": 777,
                "text": "Delegated in background.",
                "parse_mode": "HTML"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 1 }
            })))
            .expect(1)
            .mount(&mock_server)
            .await;
        Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/bottok-delegate/sendMessage"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "chat_id": 777,
                "text": "here is the delegate's real answer",
                "parse_mode": "HTML"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 2 }
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        transport
            .token_store()
            .expect("token store opens")
            .set("agent-y", "tok-delegate")
            .expect("token stored");
        transport.in_flight().record("bridge-thread-delegate", 777);
        lease_gate.mark_active("test-binding", "bridge-thread-delegate");

        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();

        // The parent turn: it spawns an async Delegate and ends immediately
        // with a hand-off acknowledgement.
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-y",
                "bridge-thread-delegate",
                AgentEventPayload::TextComplete { text: "Delegated in background.".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-y",
                "bridge-thread-delegate",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        assert_eq!(
            transport.in_flight().peek("bridge-thread-delegate"),
            Some(777),
            "the hand-off relay must not consume the mapping — the delegate's \
             completion run hasn't happened yet"
        );

        // The delegate's completion re-enters the same transport thread as a
        // second, independent run — no `record` call happens for it.
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-y",
                "bridge-thread-delegate",
                AgentEventPayload::TextComplete {
                    text: "here is the delegate's real answer".to_string(),
                },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-y",
                "bridge-thread-delegate",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        mock_server.verify().await;
    }

    /// A `RunEnded` with no meaningful final text — no `TextComplete` was
    /// ever buffered for this thread, or it was empty/whitespace-only — must
    /// not send anything to Telegram, even though the mapping is present and
    /// would otherwise resolve to a real chat.
    #[tokio::test]
    async fn relay_skips_run_ended_with_no_meaningful_text_even_with_mapping_present() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = make_transport();

        let lease_gate = LeaseGate::new();
        // No `Mock` mounted: any sendMessage call would fail the test below.

        transport
            .token_store()
            .expect("token store opens")
            .set("agent-z", "tok-empty")
            .expect("token stored");
        transport.in_flight().record("bridge-thread-empty", 888);
        lease_gate.mark_active("test-binding", "bridge-thread-empty");

        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();

        // RunEnded with no TextComplete buffered at all.
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-z",
                "bridge-thread-empty",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        // RunEnded with a whitespace-only TextComplete.
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-z",
                "bridge-thread-empty",
                AgentEventPayload::TextComplete { text: "   ".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-z",
                "bridge-thread-empty",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        assert!(
            mock_server.received_requests().await.unwrap().is_empty(),
            "no meaningful text must never reach sendMessage"
        );
        assert_eq!(
            transport.in_flight().peek("bridge-thread-empty"),
            Some(888),
            "a no-op relay must not disturb the mapping either"
        );
    }

    /// Once a thread's mapping is invalidated (the disable/token-delete/
    /// chat-unlink path — see `TelegramTransport::invalidate_thread`), a
    /// subsequent `RunEnded` on that thread must not relay, even with a
    /// perfectly good buffered reply.
    #[tokio::test]
    async fn relay_skips_run_ended_after_the_mapping_was_invalidated() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = make_transport();

        let lease_gate = LeaseGate::new();
        // No `Mock` mounted: any sendMessage call would fail the test below.

        transport
            .token_store()
            .expect("token store opens")
            .set("agent-w", "tok-invalidated")
            .expect("token stored");
        transport.in_flight().record("bridge-thread-invalidated", 999);
        lease_gate.mark_active("test-binding", "bridge-thread-invalidated");

        // Simulates the disable / token-delete / chat-unlink invalidation
        // hook firing before the delegate's completion run ends.
        transport.in_flight().remove("bridge-thread-invalidated");

        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-w",
                "bridge-thread-invalidated",
                AgentEventPayload::TextComplete {
                    text: "a reply that must never be relayed".to_string(),
                },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-w",
                "bridge-thread-invalidated",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        assert!(
            mock_server.received_requests().await.unwrap().is_empty(),
            "an invalidated mapping must never relay"
        );
    }

    #[tokio::test]
    async fn relay_skips_threads_the_bridge_never_delivered_a_telegram_message_to() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = make_transport();

        let lease_gate = LeaseGate::new();
        // No `Mock` mounted at all: any request to the mock server fails
        // the test, proving a non-Telegram-triggered thread (no `in_flight`
        // entry) never calls out.

        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-x",
                "main-thread",
                AgentEventPayload::TextComplete { text: "typed directly in the UI".to_string() },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-x",
                "main-thread",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_started_spawns_heartbeat_only_for_telegram_correlated_threads() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = make_transport();

        let lease_gate = LeaseGate::new();

        transport
            .token_store()
            .expect("token store opens")
            .set("agent-x", "tok-2")
            .expect("token stored");
        transport.in_flight().record("bridge-thread-2", 555);
        lease_gate.mark_active("test-binding", "bridge-thread-2");

        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();

        // Telegram-correlated thread: a heartbeat is spawned and tracked.
        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "bridge-thread-2", AgentEventPayload::RunStarted),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        assert!(
            heartbeats.contains_key("bridge-thread-2"),
            "a thread InFlightChats recognizes must get a typing heartbeat"
        );
        assert_eq!(
            transport.in_flight().peek("bridge-thread-2"),
            Some(555),
            "RunStarted must not consume the mapping — RunEnded's relay still needs it"
        );

        // App-only thread (never recorded in InFlightChats): no heartbeat.
        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "main-thread", AgentEventPayload::RunStarted),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        assert!(
            !heartbeats.contains_key("main-thread"),
            "an app-typed turn must never get a typing heartbeat"
        );
    }

    #[tokio::test]
    async fn run_ended_cancels_and_removes_the_threads_heartbeat() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = make_transport();

        let lease_gate = LeaseGate::new();

        transport
            .token_store()
            .expect("token store opens")
            .set("agent-x", "tok-3")
            .expect("token stored");
        transport.in_flight().record("bridge-thread-3", 555);
        lease_gate.mark_active("test-binding", "bridge-thread-3");

        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "bridge-thread-3", AgentEventPayload::RunStarted),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        let cancel = heartbeats
            .get("bridge-thread-3")
            .cloned()
            .expect("heartbeat must be tracked after RunStarted");
        assert!(!cancel.is_cancelled());

        // No TextComplete was ever buffered for this thread, so RunEnded's
        // relay short-circuits before touching the (unmounted) sendMessage
        // endpoint — this test only cares about heartbeat lifecycle.
        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-x",
                "bridge-thread-3",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        assert!(cancel.is_cancelled(), "RunEnded must cancel the thread's heartbeat");
        assert!(
            !heartbeats.contains_key("bridge-thread-3"),
            "RunEnded must remove the thread's heartbeat entry"
        );
    }

    #[test]
    fn chunk_for_telegram_returns_one_chunk_when_under_limit() {
        let chunks = chunk_for_telegram("hello from the agent");
        assert_eq!(chunks, vec!["hello from the agent"]);
    }

    #[test]
    fn chunk_for_telegram_splits_long_text_at_newline_boundaries() {
        let line = "x".repeat(100);
        let text = std::iter::repeat(line.as_str())
            .take(50)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.chars().count() > TELEGRAM_MAX_MESSAGE_CHARS);

        let chunks = chunk_for_telegram(&text);
        assert!(chunks.len() > 1, "expected more than one chunk");
        for chunk in &chunks {
            assert!(chunk.chars().count() <= TELEGRAM_MAX_MESSAGE_CHARS);
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn chunk_for_telegram_hard_cuts_a_single_line_that_exceeds_the_limit() {
        let text = "y".repeat(TELEGRAM_MAX_MESSAGE_CHARS * 2 + 10);
        let chunks = chunk_for_telegram(&text);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= TELEGRAM_MAX_MESSAGE_CHARS);
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn in_flight_chats_peek_does_not_clear_the_entry() {
        let map = InFlightChats::new();
        map.record("thread-1", 555);
        assert_eq!(map.peek("thread-1"), Some(555));
        assert_eq!(
            map.peek("thread-1"),
            Some(555),
            "peek must be repeatable — it never consumes the mapping"
        );
    }

    #[test]
    fn in_flight_chats_peek_returns_none_for_unknown_thread() {
        let map = InFlightChats::new();
        assert_eq!(map.peek("never-recorded"), None);
    }

    #[test]
    fn in_flight_chats_remove_clears_the_entry() {
        let map = InFlightChats::new();
        map.record("thread-1", 555);
        map.remove("thread-1");
        assert_eq!(map.peek("thread-1"), None);
    }

    #[test]
    fn in_flight_chats_remove_is_a_harmless_no_op_for_unknown_thread() {
        let map = InFlightChats::new();
        map.remove("never-recorded");
        assert_eq!(map.peek("never-recorded"), None);
    }

    #[test]
    fn in_flight_chats_remove_if_matches_clears_only_on_an_exact_chat_match() {
        let map = InFlightChats::new();
        map.record("thread-1", 555);

        // A different chat_id than what's recorded: the mapping survives —
        // this is the multi-chat-per-thread case, e.g. unlinking chat 111
        // must not discard an in-flight reply actually destined for 555.
        map.remove_if_matches("thread-1", 111);
        assert_eq!(
            map.peek("thread-1"),
            Some(555),
            "a mismatched chat_id must not clear a different chat's mapping"
        );

        map.remove_if_matches("thread-1", 555);
        assert_eq!(
            map.peek("thread-1"),
            None,
            "an exact chat_id match must clear the mapping"
        );
    }

    // --- RunEnded terminal-failure notices, through the real send path -----

    /// (a) A run that ends in `Error` with no `TextComplete` ever buffered
    /// must relay a sanitized failure notice through Telegram's real
    /// `sendMessage` path — the same terminal-reason handling proven
    /// generically in `crate::channels::relay::observer::tests`, exercised
    /// here end-to-end so it's also verified through token resolution and
    /// the HTML send.
    #[tokio::test]
    async fn run_ended_error_with_no_reply_relays_a_failure_notice_through_telegram() {
        use crate::channels::relay::observer::RUN_FAILED_NOTICE;

        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = make_transport();

        let lease_gate = LeaseGate::new();

        Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/bottok-error/sendMessage"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "chat_id": 321,
                "text": RUN_FAILED_NOTICE,
                "parse_mode": "HTML"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 1 }
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        transport
            .token_store()
            .expect("token store opens")
            .set("agent-error", "tok-error")
            .expect("token stored");
        transport.in_flight().record("bridge-thread-error", 321);
        lease_gate.mark_active("test-binding", "bridge-thread-error");

        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-error",
                "bridge-thread-error",
                AgentEventPayload::RunEnded { reason: RunEndReason::Error },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        mock_server.verify().await;
    }

    /// (b) Regression guard: a normal `Completed` run with no reply text
    /// still sends nothing at all through Telegram — the new failure-notice
    /// branch must not fire for a clean completion.
    #[tokio::test]
    async fn run_ended_completed_with_no_reply_still_sends_nothing_through_telegram() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = make_transport();

        let lease_gate = LeaseGate::new();
        // No `Mock` mounted: any sendMessage call fails the test below.

        transport
            .token_store()
            .expect("token store opens")
            .set("agent-clean", "tok-clean")
            .expect("token stored");
        transport.in_flight().record("bridge-thread-clean", 654);
        lease_gate.mark_active("test-binding", "bridge-thread-clean");

        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            make_event(
                "agent-clean",
                "bridge-thread-clean",
                AgentEventPayload::RunEnded { reason: RunEndReason::Completed },
            ),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        assert!(
            mock_server.received_requests().await.unwrap().is_empty(),
            "a clean completion with nothing buffered must never send a failure notice"
        );
    }

    // --- lag recovery, through the real Telegram send path -----------------

    /// Integration proof that `recover_lagged_replies` (shared with
    /// Discord, unit-tested generically in
    /// `crate::channels::relay::observer::tests`) also works end-to-end
    /// through Telegram's own token resolution and `sendMessage` call: a
    /// reply recovered from the persisted transcript must reach the Bot API
    /// exactly as `relay_reply` would have sent it live.
    #[tokio::test]
    async fn lag_recovery_relays_a_missed_reply_through_the_real_telegram_send_path() {
        let _lock = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let mock_server = MockServer::start().await;
        let _env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
            ("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = make_transport();

        let lease_gate = LeaseGate::new();

        Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/bottok-lag/sendMessage"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "chat_id": 444,
                "text": "reply recovered from the transcript",
                "parse_mode": "HTML"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 1 }
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        transport
            .token_store()
            .expect("token store opens")
            .set("agent-lag-telegram", "tok-lag")
            .expect("token stored");

        let persistence = PersistenceLayer::init_with_root(DataRoot::new(tmp.path()))
            .await
            .expect("persistence layer inits");
        let thread = persistence
            .threads
            .ensure_default_thread("agent-lag-telegram")
            .await
            .expect("thread created");
        persistence
            .transcripts
            .append(
                "agent-lag-telegram",
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

        transport.in_flight().record(&thread.id, 444);

        lease_gate.mark_active("test-binding", &thread.id);
        let mut last_relayed = HashMap::new();

        recover_lagged_replies(&lease_gate, &persistence, transport.in_flight().correlation_map(), &transport, &mut last_relayed, 6)
            .await;

        mock_server.verify().await;
        assert_eq!(
            last_relayed.get(&thread.id),
            Some(&"reply recovered from the transcript".to_string())
        );
    }
}
