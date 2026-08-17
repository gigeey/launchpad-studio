//! Telegram's implementation of the channel-agnostic [`ChannelTransport`]
//! trait (see [`crate::channels`]).
//!
//! [`TelegramTransport`] owns everything Telegram-specific: the Bot API
//! client, the token store, and the `thread_id -> chat_id` outbound-relay
//! mapping ([`InFlightChats`], consumed by
//! [`super::outbound::run_outbound_observer`]). The supervisor
//! ([`super::bridge::ChannelBridge`]) only ever calls this type through the
//! trait — it never sees a bot token.
//!
//! This file also carries the inbound long-poll loop itself
//! ([`run_bot_poll_loop`]) and the `/start <code>` pairing flow
//! ([`try_link_chat`], [`parse_start_pairing_code`]), ported from the
//! previous `TelegramBridge` with the same behavior: reject-all chat
//! allow-listing until a chat pairs, case-sensitive/unexpired pairing codes,
//! and a fresh profile re-read every poll iteration so config changes take
//! effect without a supervisor restart. Pairing works the same way inside a
//! group/supergroup — Telegram sends `/start@<bot_username> <code>` there
//! instead of the bare `/start <code>` a private chat sends — and links the
//! group's own `chat_id`, not the pairing sender's.
//!
//! Inbound delivery is keyed on `chat_id`: each distinct
//! Telegram chat gets its own lazily-minted bridge thread via
//! [`resolve_telegram_conversation_thread`], the Telegram analogue of
//! [`crate::channels::discord::runner::resolve_discord_conversation_thread`].
//! A private chat's `chat_id` **is** the other user's id, so this alone
//! keeps two strangers DMing the same bot from ever sharing an agent
//! context. Telegram forum-topic sub-threading (`message_thread_id`) is a
//! distinct, deferred concern (a topic is a shared room, not a new sender)
//! and is not modeled here.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use ao_engine_tools_provider_config::{TelegramTokenStore, TelegramTokenStoreError};
use ao_persistence::PersistenceLayer;
use ao_protocol::agent::{AgentProfile, ChannelBinding, ChannelKind};
use ao_protocol::channel_connection_state::ChannelConnectionState;
use ao_protocol::channel_cursor::ChannelCursor;
use ao_protocol::conversation_registry::ConversationKey;
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::thread::{ChannelBridgeOrigin, Thread};

use crate::channels::relay::conversation_gc;
use crate::channels::relay::lease_gate::LeaseGate;
use crate::channels::{submit_inbound_message, ChannelRunContext, ChannelTransport};
use crate::event_bus::EventBus;
use crate::telegram::client::{
    TelegramBotInfo, TelegramChatType, TelegramClient, TelegramMessage, TelegramMessageEntity,
    TelegramMessageEntityType, TelegramUpdate,
};
use crate::telegram::outbound;
use crate::telegram::outbound::InFlightChats;

/// Telegram-side long-poll wait passed as `getUpdates`' `timeout` param.
const LONG_POLL_TIMEOUT_SECS: u32 = 30;

/// Fixed pause after a failed `getUpdates` call before retrying. Keeps one
/// unhealthy bot (revoked token, network blip) from hammering Telegram or
/// spinning the task hot.
const ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// Telegram's [`ChannelTransport`] implementation, and the shared home for
/// the Bot API client, the token store, and the outbound-relay mapping that
/// [`super::outbound`] reads.
pub struct TelegramTransport {
    client: Arc<TelegramClient>,
    /// Opened lazily, the first time a fingerprint or spawn call actually
    /// needs a token — see [`Self::token_store`]. An install with no
    /// Telegram agents configured never touches the OS keychain.
    token_store: OnceLock<TelegramTokenStore>,
    /// `thread_id -> chat_id` for turns this transport just dispatched onto
    /// a bridge thread. Written here (inbound side) at delivery time, read
    /// and cleared by the outbound observer at `RunEnded` — see
    /// [`super::outbound`] for why the mapping can't be reconstructed from
    /// the event stream alone.
    in_flight: Arc<InFlightChats>,
}

impl TelegramTransport {
    pub fn new(client: Arc<TelegramClient>) -> Self {
        Self { client, token_store: OnceLock::new(), in_flight: Arc::new(InFlightChats::new()) }
    }

    /// Returns the lazily-opened token store, opening it on first use.
    pub(super) fn token_store(&self) -> Result<&TelegramTokenStore, TelegramTokenStoreError> {
        if let Some(store) = self.token_store.get() {
            return Ok(store);
        }
        let store = TelegramTokenStore::open()?;
        // Both `fingerprint` and the outbound observer call this, so a
        // genuine race on first use is possible. `OnceLock::set` resolves it
        // safely: at most one caller's `store` wins, everyone reads it back
        // via `get()` below, and a losing `set` (this one, or a concurrent
        // one) is just a discarded `TelegramTokenStore` value, not an error.
        let _ = self.token_store.set(store);
        Ok(self
            .token_store
            .get()
            .expect("token store was just initialized above"))
    }

    /// Accessors for [`super::outbound`], which lives in a sibling module
    /// and needs read access to the client and in-flight map without the
    /// struct's other fields being made crate-visible.
    pub(super) fn client(&self) -> &Arc<TelegramClient> {
        &self.client
    }

    pub(super) fn in_flight(&self) -> &InFlightChats {
        &self.in_flight
    }

    /// Ends this transport's outbound-relay binding for `thread_id`
    /// outright: any chat mapping [`super::outbound`]'s observer would
    /// otherwise relay a later completion to is dropped. Called whenever a
    /// binding is torn down — [`super::bridge::ChannelBridge::reconcile`]
    /// calls this for every inbound task it stops, and the token-delete HTTP
    /// handler calls it directly for immediate effect instead of waiting out
    /// the reconcile interval.
    pub fn invalidate_thread(&self, thread_id: &str) {
        self.in_flight.remove(thread_id);
    }

    /// Ends this transport's outbound-relay binding for `thread_id` only if
    /// it currently points at `chat_id`. Used by the chat-unlink HTTP
    /// handler: several chats can share one dedicated bridge thread
    /// (multi-user pairing), so unlinking one must not discard an in-flight
    /// reply actually destined for a different, still-linked chat.
    pub fn invalidate_thread_for_chat(&self, thread_id: &str, chat_id: i64) {
        self.in_flight.remove_if_matches(thread_id, chat_id);
    }

    /// Resolves the bot token for `agent_id`, logging and returning `None`
    /// on any store failure or absence rather than propagating an error —
    /// callers ([`Self::fingerprint`], [`Self::spawn`]) treat "no token" as
    /// "not runnable yet", not a hard failure.
    fn resolve_token(&self, agent_id: &str) -> Option<String> {
        match self.token_store() {
            Ok(store) => match store.get(agent_id) {
                Ok(token) => token,
                Err(e) => {
                    warn!(agent_id = %agent_id, "TelegramTransport: failed to read token: {e}");
                    None
                }
            },
            Err(e) => {
                warn!(agent_id = %agent_id, "TelegramTransport: failed to open token store: {e}");
                None
            }
        }
    }
}

#[async_trait]
impl ChannelTransport for TelegramTransport {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Telegram
    }

    fn fingerprint(&self, agent: &AgentProfile, binding: &ChannelBinding) -> Option<String> {
        let token = self.resolve_token(&agent.id)?;
        // Folds the resolved secret and the binding's own config together —
        // a rotated token *or* a config change (e.g. thread_mode) restarts
        // the poll task. `kind_config`'s `Debug` output is adequate here:
        // it's only ever compared to its own prior value, never parsed.
        Some(format!("{token}|{:?}", binding.kind_config))
    }

    fn spawn(&self, ctx: ChannelRunContext, cancel: CancellationToken) -> JoinHandle<()> {
        let client = Arc::clone(&self.client);
        let in_flight = Arc::clone(&self.in_flight);
        let token = self.resolve_token(&ctx.agent_id);

        tokio::spawn(async move {
            let Some(token) = token else {
                warn!(
                    agent_id = %ctx.agent_id,
                    "TelegramTransport: token unavailable at spawn time, not starting poll task"
                );
                return;
            };
            run_bot_poll_loop(ctx, token, client, in_flight, cancel).await;
        })
    }

    fn invalidate_thread(&self, thread_id: &str) {
        self.in_flight.remove(thread_id);
    }

    fn spawn_outbound_observer(
        self: Arc<Self>,
        persistence: Arc<PersistenceLayer>,
        lease_gate: Arc<LeaseGate>,
        event_bus: Arc<EventBus>,
        shutdown_rx: watch::Receiver<()>,
    ) -> Option<JoinHandle<()>> {
        Some(tokio::spawn(async move {
            outbound::run_outbound_observer(self, persistence, lease_gate, event_bus, shutdown_rx).await;
        }))
    }
}

/// Long-poll loop for a single bot binding. Runs until `cancel` fires.
/// Re-reads the agent's profile every iteration (mirroring the
/// queue-manager pump's re-read-before-dispatch pattern) so a mid-flight
/// `allowed_senders` change takes effect on the very next poll without
/// needing a supervisor restart.
///
/// `offset` is Telegram's own dedup cursor (see [`TelegramClient::get_updates`]):
/// restored from [`ao_persistence::channel_cursor_store::ChannelCursorStore`]
/// on entry so a backend restart resumes polling from where the previous
/// process left off, instead of Telegram re-serving every update since the
/// beginning. Persisted once per update (in the `for update in &updates`
/// loop below), immediately after that update is fully handled — so the
/// worst-case re-delivery window on a crash is exactly the single update
/// that was being processed at the time, never more, and never a silent
/// loss of one that was already answered.
async fn run_bot_poll_loop(
    ctx: ChannelRunContext,
    token: String,
    client: Arc<TelegramClient>,
    in_flight: Arc<InFlightChats>,
    cancel: CancellationToken,
) {
    let mut offset: Option<i64> = match ctx.persistence.channel_cursors.get(&ctx.agent_id, &ctx.binding_id).await {
        Ok(Some(ChannelCursor::Telegram { offset })) => offset,
        Ok(Some(other)) => {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, ?other, "TelegramTransport: persisted cursor is not a Telegram cursor, starting fresh");
            None
        }
        Ok(None) => None,
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "TelegramTransport: failed to load persisted cursor, starting fresh: {e}");
            None
        }
    };

    // Resolved once for this poll task's lifetime — a token's bot identity
    // never changes — the Telegram analogue of a Discord gateway connection
    // learning its own user id from `READY`. `handle_update` needs this to
    // decide whether a group/supergroup message actually addresses the bot
    // (see `group_addressing`); a private chat never consults it. A failure
    // here fails closed for groups only: private-chat delivery below is
    // entirely unaffected, and group messages are simply skipped until the
    // next process restart retries this call.
    let bot_identity = match client.get_me(&token).await {
        Ok(info) => Some(info),
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, "TelegramTransport: failed to resolve bot identity via getMe, group/supergroup messages will be skipped until restart: {e}");
            None
        }
    };

    loop {
        let mut profile = match ctx.persistence.agents.get(&ctx.agent_id).await {
            Ok(Some(profile)) => profile,
            Ok(None) => {
                debug!(agent_id = %ctx.agent_id, "TelegramTransport: agent no longer exists, stopping poll task");
                return;
            }
            Err(e) => {
                warn!(agent_id = %ctx.agent_id, "TelegramTransport: failed to re-read agent profile: {e}");
                ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Reconnecting);
                if wait_or_cancelled(&cancel, ERROR_BACKOFF).await {
                    return;
                }
                continue;
            }
        };
        let Some(binding_index) = profile.channels.iter().position(|b| b.binding_id == ctx.binding_id) else {
            debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "TelegramTransport: binding removed, stopping poll task");
            return;
        };
        if !profile.channels[binding_index].enabled {
            debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "TelegramTransport: binding disabled, stopping poll task");
            return;
        }
        // No `bridge_thread_id` readiness check here anymore: Telegram mints
        // a fresh per-conversation thread on demand for every distinct
        // `chat_id` it sees (see `resolve_telegram_conversation_thread`)
        // instead of routing every conversation through one
        // eagerly-provisioned thread, so this binding has nothing to wait on
        // before it can start polling. A binding provisioned before this
        // change may still carry a legacy `bridge_thread_id` — its thread
        // stays viewable, but no new inbound message is ever routed there
        // again (migration leaves it as-is, never reassigned).

        let updates = tokio::select! {
            _ = cancel.cancelled() => return,
            result = client.get_updates(&token, offset, LONG_POLL_TIMEOUT_SECS) => result,
        };

        let updates = match updates {
            Ok(updates) => {
                ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Connected);
                updates
            }
            Err(e) => {
                warn!(agent_id = %ctx.agent_id, "Telegram getUpdates failed: {e}");
                ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Reconnecting);
                if wait_or_cancelled(&cancel, ERROR_BACKOFF).await {
                    return;
                }
                continue;
            }
        };

        for update in &updates {
            handle_update(
                update,
                &ctx,
                &mut profile,
                binding_index,
                &client,
                &token,
                &in_flight,
                bot_identity.as_ref(),
            )
            .await;

            // Advance and persist the offset once this update has been
            // fully handled (delivered, rejected, or consumed as a pairing
            // command) regardless of which path `handle_update` took — see
            // this function's doc comment for the re-delivery window this
            // bounds.
            offset = Some(update.update_id + 1);
            if let Err(e) = ctx
                .persistence
                .channel_cursors
                .set(&ctx.agent_id, &ctx.binding_id, &ChannelCursor::Telegram { offset })
                .await
            {
                warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "TelegramTransport: failed to persist cursor: {e}");
            }
        }
    }
}

/// Handles a single inbound Telegram update: pairing-code consumption,
/// allow-list gating, and delivery via [`submit_inbound_message`]. Extracted
/// from [`run_bot_poll_loop`]'s per-update loop so every exit path — no
/// text, a consumed pairing command, an unauthorized chat, or a successful
/// delivery — returns to one call site, which then always advances and
/// persists the offset exactly once per update no matter which path was
/// taken here.
#[allow(clippy::too_many_arguments)]
async fn handle_update(
    update: &TelegramUpdate,
    ctx: &ChannelRunContext,
    profile: &mut AgentProfile,
    binding_index: usize,
    client: &TelegramClient,
    token: &str,
    in_flight: &InFlightChats,
    bot: Option<&TelegramBotInfo>,
) {
    let Some(message) = &update.message else {
        return;
    };
    let Some(text) = message.text.as_deref() else {
        return;
    };
    let chat_id = message.chat.id;

    let bot_username = bot.map(|info| info.username.as_str());
    if let Some(code) = parse_start_pairing_code(text, bot_username) {
        let linked = match try_link_chat(
            &ctx.persistence,
            profile,
            &ctx.binding_id,
            chat_id,
            code,
            Utc::now().timestamp(),
        )
        .await
        {
            Ok(linked) => linked,
            Err(e) => {
                warn!(agent_id = %ctx.agent_id, "TelegramTransport: failed to persist pairing link: {e}");
                false
            }
        };
        // `chat_id` above is `message.chat.id` — the inbound message's own
        // chat, which for a group/supergroup is that group's (negative)
        // id, not the sender's. Linking therefore always authorizes the
        // chat the `/start` command was actually sent in.
        let reply = if linked {
            match message.chat.chat_type {
                TelegramChatType::Group | TelegramChatType::Supergroup => GROUP_PAIRING_SUCCESS_REPLY,
                _ => PAIRING_SUCCESS_REPLY,
            }
        } else {
            PAIRING_FAILURE_REPLY
        };
        if let Err(e) = client.send_message(token, chat_id, reply, None).await {
            warn!(agent_id = %ctx.agent_id, chat_id, "TelegramTransport: failed to send pairing reply: {e}");
        }
        return;
    }

    let inline_allowed_senders = profile.channels[binding_index].allowed_senders.clone();
    let allowed_senders = match ctx
        .persistence
        .linked_senders
        .get_or_backfill(&ctx.agent_id, &ctx.binding_id, &inline_allowed_senders)
        .await
    {
        Ok(senders) => senders,
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "TelegramTransport: failed to read linked senders, dropping message: {e}");
            return;
        }
    };
    let allowed_chat_ids: Vec<i64> = allowed_senders.iter().filter_map(|s| s.parse().ok()).collect();
    if !is_chat_allowed(&allowed_chat_ids, chat_id) {
        debug!(
            agent_id = %ctx.agent_id,
            chat_id,
            "TelegramTransport: dropping message from unlinked chat"
        );
        return;
    }

    // Mirrors Telegram's own privacy-mode semantics: a private chat treats
    // every message as directed at the bot (unchanged from before this
    // check existed), but a group/supergroup only proceeds when the message
    // actually addresses the bot — see `group_addressing`'s doc for the
    // four ways that can happen. Checked before the thread resolve below so
    // an unaddressed group message never mints or touches a per-conversation
    // bridge thread it will never use.
    let dispatch_text = match message.chat.chat_type {
        TelegramChatType::Private => text.to_string(),
        TelegramChatType::Group | TelegramChatType::Supergroup => {
            let Some(bot) = bot else {
                debug!(
                    agent_id = %ctx.agent_id,
                    chat_id,
                    "TelegramTransport: bot identity unresolved, skipping group/supergroup message"
                );
                return;
            };
            let Some(clean_text) = group_addressing(
                text,
                &message.entities,
                message.reply_to_message.as_deref(),
                bot.id,
                &bot.username,
            ) else {
                debug!(
                    agent_id = %ctx.agent_id,
                    chat_id,
                    "TelegramTransport: group/supergroup message does not address the bot, skipping"
                );
                return;
            };
            clean_text
        }
        TelegramChatType::Channel | TelegramChatType::Other => {
            debug!(
                agent_id = %ctx.agent_id,
                chat_id,
                chat_type = ?message.chat.chat_type,
                "TelegramTransport: unsupported chat type, skipping"
            );
            return;
        }
    };

    // Resolved only now, after the pairing-command and allow-list gates
    // above have already passed — mirrors
    // `discord::runner::resolve_discord_conversation_thread`'s placement, so
    // a dropped or unauthorized update never mints a per-conversation thread
    // it will never use.
    let Some(thread_id) = resolve_telegram_conversation_thread(ctx, chat_id, Utc::now()).await else {
        warn!(agent_id = %ctx.agent_id, chat_id, "TelegramTransport: failed to resolve a per-conversation bridge thread, dropping message");
        return;
    };

    in_flight.record(&thread_id, chat_id);
    let conversation_id = chat_id.to_string();
    let sender_id = message
        .from
        .as_ref()
        .map(|user| user.id.to_string())
        .unwrap_or_else(|| conversation_id.clone());
    let sender_display_name = message.from.as_ref().and_then(|user| user.username.as_deref());
    // Telegram's `text` field is already plain UTF-8 — formatting arrives
    // out-of-band as `entities`, never inline markup — so `dispatch_text`
    // (the original text, or a group message with its addressing `@mention`
    // stripped) needs no further transport-specific cleaning before also
    // being used as the auto-title candidate.
    if let Err(e) = submit_inbound_message(
        ctx,
        profile,
        &thread_id,
        ChannelKind::Telegram,
        &conversation_id,
        &sender_id,
        sender_display_name,
        &dispatch_text,
        Some(dispatch_text.clone()),
    )
    .await
    {
        warn!(agent_id = %ctx.agent_id, "TelegramTransport: failed to deliver inbound message: {e}");
    }
}

/// Resolves the conversation→thread registry row for a Telegram
/// `chat_id`, lazily minting a fresh Launchpad bridge thread on first
/// contact — the Telegram analogue of
/// [`crate::channels::discord::runner::resolve_discord_conversation_thread`],
/// keyed on Telegram's own `chat_id` rather than Discord's `channel_id`. A
/// private chat's `chat_id` **is** the other user's id, so two strangers
/// DMing the same bot are already two distinct keys — no composite key or
/// `message_thread_id` (forum-topic) component is needed for the v1 security
/// boundary; forum-topics remain deferred.
///
/// Runs the gc-and-release pass for this binding first, so an
/// idle-evicted conversation's `LeaseGate` state clears before this update's
/// own resolve/mint runs — if this update's own conversation was the one
/// just evicted, it simply re-mints a fresh thread below, exactly as a
/// returning sender safely would. Returns `None` (logged) only on a
/// persistence failure — an update this poll task cannot resolve a thread
/// for is dropped rather than mis-routed.
///
/// A freshly minted thread is left untitled (`title: None`): the caller's
/// own `submit_inbound_message` call already derives `auto_title` from this
/// same update's text generically, so no special-cased "is this the first
/// message" titling logic is needed here.
async fn resolve_telegram_conversation_thread(
    ctx: &ChannelRunContext,
    chat_id: i64,
    now: DateTime<Utc>,
) -> Option<String> {
    if let Err(e) = conversation_gc::run_gc_and_release_leases(
        &ctx.persistence.conversation_registry,
        &ctx.lease_gate,
        &ctx.agent_id,
        &ctx.binding_id,
        now,
    )
    .await
    {
        warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "TelegramTransport: conversation registry gc failed: {e}");
    }

    let key = ConversationKey::new(chat_id.to_string());
    let mut minted_thread: Option<Thread> = None;
    let mint = || {
        let mut thread = ctx.persistence.threads.build_fresh_thread(&ctx.agent_id, None);
        thread.channel_origin =
            Some(ChannelBridgeOrigin { kind: ChannelKind::Telegram, binding_id: ctx.binding_id.clone() });
        let id = thread.id.clone();
        minted_thread = Some(thread);
        id
    };

    let row = match ctx
        .persistence
        .conversation_registry
        .get_or_create(&ctx.agent_id, &ctx.binding_id, key, now, mint)
        .await
    {
        Ok(row) => row,
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, "TelegramTransport: failed to read the conversation registry: {e}");
            return None;
        }
    };

    if let Some(thread) = minted_thread {
        if let Err(e) = ctx.persistence.threads.create(thread.clone()).await {
            warn!(agent_id = %ctx.agent_id, "TelegramTransport: failed to create a per-conversation bridge thread: {e}");
            return None;
        }
        ctx.event_bus
            .emit(
                &format!("thread:{}", thread.id),
                &ctx.agent_id,
                Some(thread.id.clone()),
                AgentEventPayload::ThreadCreated { thread },
            )
            .await;
    }

    // Registered on every resolve, not just on first mint: `LeaseGate` is
    // process-local, so a conversation created by an earlier holder (or an
    // earlier run of this same process) is otherwise unknown to this
    // process's gate until it sees the conversation again — mirrors
    // `discord::runner::resolve_discord_conversation_thread`'s same
    // reasoning. This is also the only place Telegram's `LeaseGate` state is
    // ever marked active now — `ChannelBridge::reconcile` no longer marks a
    // single Telegram placeholder thread (see its own doc).
    ctx.lease_gate.mark_active(&ctx.binding_id, &row.thread_id);
    Some(row.thread_id)
}

/// Sleeps for `dur` unless `cancel` fires first. Returns `true` if cancelled.
async fn wait_or_cancelled(cancel: &CancellationToken, dur: Duration) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => true,
        _ = tokio::time::sleep(dur) => false,
    }
}

/// Whether a message from `chat_id` should be delivered. Reject-all: an
/// empty allow-list means no chat has linked yet, so *nothing* is delivered
/// — a bot is publicly messageable by anyone who finds its `@username`, and
/// the only way into the allow-list is a successful [`try_link_chat`] via
/// `/start <code>`. Once at least one chat is linked, only listed chats are
/// served.
fn is_chat_allowed(allowed_chat_ids: &[i64], chat_id: i64) -> bool {
    allowed_chat_ids.contains(&chat_id)
}

/// Whether a group/supergroup message addresses the bot at all, and — when
/// it does — the text to dispatch to the agent. Four ways a message counts
/// as addressed, matching Telegram's own privacy-mode semantics for a group
/// bot that must not answer every message in a shared chat:
///   - a `mention` entity whose text (sliced via [`utf16_slice`]) is
///     `@<bot_username>`, case-insensitively — the matched span is stripped
///     from the returned text so the agent sees the caller's actual request
///     rather than its own name;
///   - a `text_mention` entity whose embedded user id is the bot's own
///     numeric id;
///   - `reply_to_message` is present and its `from.id` is the bot's own —
///     a reply to one of the bot's own prior messages;
///   - a `bot_command` entity explicitly suffixed to this bot, e.g.
///     `/summarize@<bot_username>`. A bare `/cmd` with no `@suffix` is
///     Telegram's fan-out form (delivered to every bot in the chat when
///     privacy mode is off) and is deliberately NOT treated as addressed
///     here — answering it would make every bot in the chat respond to a
///     command aimed at only one of them.
///
/// Returns `None` when none of the above hold, in which case the caller
/// must skip the message entirely rather than dispatch it.
fn group_addressing(
    text: &str,
    entities: &[TelegramMessageEntity],
    reply_to_message: Option<&TelegramMessage>,
    bot_id: i64,
    bot_username: &str,
) -> Option<String> {
    let mut addressed =
        reply_to_message.and_then(|m| m.from.as_ref()).is_some_and(|from| from.id == bot_id);
    let mut mention_range: Option<(usize, usize)> = None;

    for entity in entities {
        match entity.entity_type {
            TelegramMessageEntityType::Mention => {
                let Some(slice) = utf16_slice(text, entity.offset, entity.length) else {
                    continue;
                };
                if slice.trim_start_matches('@').eq_ignore_ascii_case(bot_username) {
                    addressed = true;
                    mention_range = utf16_byte_range(text, entity.offset, entity.length);
                }
            }
            TelegramMessageEntityType::TextMention => {
                if entity.user.as_ref().is_some_and(|u| u.id == bot_id) {
                    addressed = true;
                }
            }
            TelegramMessageEntityType::BotCommand => {
                let Some(slice) = utf16_slice(text, entity.offset, entity.length) else {
                    continue;
                };
                if let Some((_, suffix)) = slice.split_once('@') {
                    if suffix.eq_ignore_ascii_case(bot_username) {
                        addressed = true;
                    }
                }
            }
            _ => {}
        }
    }

    if !addressed {
        return None;
    }

    Some(match mention_range {
        Some((start, end)) => {
            let mut cleaned = String::with_capacity(text.len());
            cleaned.push_str(&text[..start]);
            cleaned.push_str(&text[end..]);
            cleaned.trim().to_string()
        }
        None => text.to_string(),
    })
}

/// Extracts the UTF-16 code-unit range `[offset, offset+length)` of `text`
/// as a `&str`. Telegram measures entity `offset`/`length` in UTF-16 code
/// units, not UTF-8 bytes or Rust `char`s (see [`TelegramMessageEntity`]'s
/// doc), so indexing `text` directly at those numbers can panic or split a
/// multi-byte character in half. Returns `None` — rather than panicking —
/// when the range is negative, overflows, or doesn't land on a char
/// boundary (e.g. it splits a surrogate pair Telegram counts as two units),
/// since a malformed entity should be ignored, not crash the poll loop.
fn utf16_slice(text: &str, offset: i64, length: i64) -> Option<&str> {
    let (start, end) = utf16_byte_range(text, offset, length)?;
    text.get(start..end)
}

/// The byte-offset counterpart of [`utf16_slice`], shared with the mention
/// stripping in [`group_addressing`] (which needs the byte range to splice
/// `text`, not just the slice itself). Walks `text`'s chars once, tracking
/// each one's UTF-16 width via [`char::len_utf16`], and records the byte
/// offset at the moment the running UTF-16 count matches `offset` and
/// `offset + length` respectively.
fn utf16_byte_range(text: &str, offset: i64, length: i64) -> Option<(usize, usize)> {
    if offset < 0 || length < 0 {
        return None;
    }
    let start_unit = offset as usize;
    let end_unit = start_unit.checked_add(length as usize)?;

    let mut units = 0usize;
    let mut start_byte = None;
    let mut end_byte = None;
    for (byte_idx, ch) in text.char_indices() {
        if units == start_unit {
            start_byte = Some(byte_idx);
        }
        if units == end_unit {
            end_byte = Some(byte_idx);
        }
        units += ch.len_utf16();
    }
    if start_byte.is_none() && units == start_unit {
        start_byte = Some(text.len());
    }
    if end_byte.is_none() && units == end_unit {
        end_byte = Some(text.len());
    }

    Some((start_byte?, end_byte?))
}

/// Reply sent to a private chat that just linked successfully via
/// `/start <code>`.
const PAIRING_SUCCESS_REPLY: &str = "You're linked. I'll respond to messages in this chat from now on.";

/// Reply sent to a group/supergroup that just linked successfully via
/// `/start <code>` (typically `/start@<bot_username> <code>` — see
/// [`parse_start_pairing_code`]). Unlike a private chat, a linked group only
/// responds when explicitly addressed (see [`group_addressing`]), so this
/// tells the user to @-mention the bot rather than implying every message
/// in the group will now get a reply.
const GROUP_PAIRING_SUCCESS_REPLY: &str =
    "This group is now linked. @-mention me (or reply to one of my messages) to talk to me.";

/// Reply sent when `/start <code>` doesn't resolve to a live pairing code —
/// missing, expired, or mismatched. Deliberately generic about which of the
/// three it was, so a guesser can't use the response to narrow down a code.
const PAIRING_FAILURE_REPLY: &str =
    "That code isn't valid. Generate a new pairing code from this agent's Telegram settings and send /start <code> again.";

/// Parses `text` as a `/start <code>` pairing attempt: exactly two
/// whitespace-separated tokens, the first either literally `/start` or
/// `/start@<bot_username>`. Telegram clients append the bot's own username
/// to slash commands sent in a group or supergroup — `/start` typed there
/// arrives as `/start@mybot CODE`, not the bare `/start CODE` a private chat
/// or deep link sends — so the `@suffix` (matched case-insensitively against
/// `bot_username`) is stripped before the command is compared. `bot_username`
/// should be `None` only when the bot's own identity hasn't resolved yet
/// (mirrors `bot: Option<&TelegramBotInfo>` elsewhere in this file); a
/// suffix addressed to some other bot never matches, `bot_username` or not.
/// Anything else — a bare `/start`, extra arguments, or ordinary
/// conversation — isn't a pairing attempt and falls through to the normal
/// allow-list gate.
fn parse_start_pairing_code<'a>(text: &'a str, bot_username: Option<&str>) -> Option<&'a str> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?;
    let code = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let is_start_command = match command.split_once('@') {
        Some((base, suffix)) => {
            base == "/start" && bot_username.is_some_and(|username| suffix.eq_ignore_ascii_case(username))
        }
        None => command == "/start",
    };
    if !is_start_command {
        return None;
    }
    Some(code)
}

/// Resolves a `/start <code>` attempt against `binding_id`'s pending pairing
/// code at `now_unix`. On a match (present, unexpired, exact case-sensitive
/// match against [`ao_protocol::agent::PairingCode::code`]): links `chat_id`
/// into [`ao_persistence::linked_sender_store::LinkedSenderStore`] (deduped)
/// and clears the code in memory. Returns whether linking happened so the
/// caller can pick the confirmation or failure reply; a non-match leaves
/// `profile` and the stored profile untouched.
///
/// Deliberately never calls `persistence.agents.update` — that whole-document
/// write is exactly the clobber `LinkedSenderStore` exists to close (see
/// `ChannelBinding::allowed_senders`'s doc), since this pairing flow runs
/// out-of-band from any `PUT /agents/{id}` a client might be issuing at the
/// same moment. Clearing `pending_pairing_code` in memory only guards a
/// repeat `/start <code>` within this same long-poll batch; the code's own
/// `expires_at_unix` (`PairingCode::generate`, 10 minutes) is what actually
/// bounds reuse across a restart or the next poll iteration's fresh profile
/// fetch.
async fn try_link_chat(
    persistence: &Arc<ao_persistence::PersistenceLayer>,
    profile: &mut AgentProfile,
    binding_id: &str,
    chat_id: i64,
    code: &str,
    now_unix: i64,
) -> Result<bool, AoError> {
    let Some(binding) = profile.channels.iter_mut().find(|b| b.binding_id == binding_id) else {
        return Ok(false);
    };

    let matches = binding
        .pending_pairing_code
        .as_ref()
        .is_some_and(|pending| !pending.is_expired(now_unix) && pending.code == code);
    if !matches {
        return Ok(false);
    }

    binding.pending_pairing_code = None;

    persistence
        .linked_senders
        .add_sender(&profile.id, binding_id, &chat_id.to_string())
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use async_trait::async_trait;

    use ao_persistence::PersistenceLayer;
    use ao_protocol::agent::{
        AgentRunnerMode, ChannelKindConfig, CliProviderConfig, InputMode, OutputFormat, PairingCode,
        ProviderConfig, TelegramThreadMode,
    };
    use ao_protocol::data_root::DATA_DIR_ENV_VAR;
    use ao_protocol::error::AoError;
    use ao_protocol::event::RunEndReason;

    use crate::agent_runner::{AgentRunRequest, AgentRunner, RunComplete, RunnerDispatcher};
    use crate::channels::connection_state::ConnectionStateRegistry;
    use crate::channels::relay::lease_gate::LeaseGate;
    use crate::instance_registry::InstanceRegistry;
    use crate::queue_manager::QueueManagerRegistry;
    use crate::telegram::client::{
        TelegramBotInfo, TelegramChat, TelegramChatType, TelegramMessage, TelegramMessageEntity,
        TelegramMessageEntityType, TelegramUser,
    };
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

    async fn make_persistence() -> (Arc<PersistenceLayer>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
        let layer = PersistenceLayer::init_with_root(data_root)
            .await
            .expect("init persistence");
        (Arc::new(layer), tmp)
    }

    fn make_agent(id: &str, telegram_binding: Option<ChannelBinding>) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: String::new(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30_000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 2,
            timeout_seconds: 60,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: telegram_binding.into_iter().collect(),
            max_turns: None,
        }
    }

    fn enabled_telegram_binding(bridge_thread_id: Option<&str>) -> ChannelBinding {
        ChannelBinding {
            binding_id: "telegram".to_string(),
            kind: ChannelKind::Telegram,
            enabled: true,
            bridge_thread_id: bridge_thread_id.map(|s| s.to_string()),
            allowed_senders: vec![],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Telegram {
                bot_username: Some("@test_bot".to_string()),
                thread_mode: TelegramThreadMode::Dedicated,
            },
        }
    }

    fn pairing_code(code: &str, expires_at_unix: i64) -> PairingCode {
        PairingCode {
            code: code.to_string(),
            expires_at_unix,
        }
    }

    // --- Pure-function tests: allow-list filtering and pairing parsing ---

    #[test]
    fn is_chat_allowed_rejects_everything_when_allowlist_empty() {
        assert!(!is_chat_allowed(&[], 555));
        assert!(!is_chat_allowed(&[], -1));
    }

    #[test]
    fn is_chat_allowed_permits_only_listed_chats() {
        let allowed = [111, 222];
        assert!(is_chat_allowed(&allowed, 111));
        assert!(!is_chat_allowed(&allowed, 333));
    }

    #[test]
    fn parse_start_pairing_code_extracts_code_from_exact_start_command() {
        assert_eq!(parse_start_pairing_code("/start ABC123", None), Some("ABC123"));
        assert_eq!(
            parse_start_pairing_code("  /start ABC123  ", None),
            Some("ABC123"),
            "surrounding whitespace should be trimmed"
        );
    }

    #[test]
    fn parse_start_pairing_code_rejects_non_matching_shapes() {
        assert_eq!(parse_start_pairing_code("/start", None), None, "bare /start has no code");
        assert_eq!(
            parse_start_pairing_code("/start ABC123 extra", None),
            None,
            "extra arguments aren't a pairing attempt"
        );
        assert_eq!(parse_start_pairing_code("hello there", None), None);
        assert_eq!(
            parse_start_pairing_code("/started ABC123", None),
            None,
            "must match /start exactly, not just a prefix"
        );
    }

    #[test]
    fn parse_start_pairing_code_strips_a_matching_bot_suffix_case_insensitively() {
        assert_eq!(
            parse_start_pairing_code("/start@MyBot ABC123", Some("mybot")),
            Some("ABC123"),
            "a group's /start@<bot_username> must match case-insensitively against bot_username"
        );
        assert_eq!(
            parse_start_pairing_code("/start@mybot ABC123", Some("mybot")),
            Some("ABC123")
        );
    }

    #[test]
    fn parse_start_pairing_code_ignores_a_suffix_addressed_to_a_different_bot() {
        assert_eq!(
            parse_start_pairing_code("/start@othername ABC123", Some("mybot")),
            None,
            "a command suffixed to a different bot must not be treated as our /start"
        );
    }

    #[test]
    fn parse_start_pairing_code_ignores_a_suffixed_command_when_bot_identity_is_unresolved() {
        assert_eq!(
            parse_start_pairing_code("/start@mybot ABC123", None),
            None,
            "with no resolved bot_username there's nothing to match the suffix against, so it must fail closed"
        );
    }

    // --- Pairing (`/start <code>`) tests ---

    #[tokio::test]
    async fn try_link_chat_links_in_the_store_and_never_writes_the_whole_profile_document() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(Some("thread-f"));
        binding.pending_pairing_code = Some(pairing_code("ABC123", 1_700_000_600));
        let mut agent = make_agent("agent-f", Some(binding));
        persistence.agents.create(&agent).await.unwrap();

        let linked = try_link_chat(&persistence, &mut agent, "telegram", 555, "ABC123", 1_700_000_000)
            .await
            .unwrap();

        assert!(linked);
        let binding = agent.telegram_binding().unwrap();
        assert!(binding.pending_pairing_code.is_none(), "in-memory code must be cleared for this batch");

        let linked_senders = persistence.linked_senders.get("agent-f", "telegram").await.unwrap();
        assert_eq!(
            linked_senders.unwrap().senders,
            vec!["555".to_string()],
            "a successful link must land in LinkedSenderStore"
        );

        let stored = persistence.agents.get("agent-f").await.unwrap().unwrap();
        let stored_binding = stored.telegram_binding().expect("telegram binding persisted");
        assert!(
            stored_binding.allowed_senders.is_empty(),
            "the deprecated inline field must stay untouched"
        );
        assert!(
            stored_binding.pending_pairing_code.is_some(),
            "try_link_chat must never round-trip the whole profile document — the persisted \
             pairing code is left as-is; its own expiry, not a persisted clear, bounds reuse \
             across a fresh profile fetch"
        );
    }

    #[tokio::test]
    async fn try_link_chat_dedupes_a_chat_already_linked_in_the_store() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(Some("thread-g"));
        binding.pending_pairing_code = Some(pairing_code("ABC123", 1_700_000_600));
        let mut agent = make_agent("agent-g", Some(binding));
        persistence.agents.create(&agent).await.unwrap();
        persistence.linked_senders.add_sender("agent-g", "telegram", "555").await.unwrap();

        let linked = try_link_chat(&persistence, &mut agent, "telegram", 555, "ABC123", 1_700_000_000)
            .await
            .unwrap();

        assert!(linked);
        let senders = persistence.linked_senders.get("agent-g", "telegram").await.unwrap().unwrap().senders;
        assert_eq!(senders, vec!["555".to_string()], "must not duplicate");
    }

    #[tokio::test]
    async fn try_link_chat_rejects_expired_code_without_mutating() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(Some("thread-h"));
        binding.pending_pairing_code = Some(pairing_code("ABC123", 1_700_000_000));
        let mut agent = make_agent("agent-h", Some(binding));
        persistence.agents.create(&agent).await.unwrap();

        // now_unix == expires_at_unix: expired at the exact expiry instant.
        let linked = try_link_chat(&persistence, &mut agent, "telegram", 555, "ABC123", 1_700_000_000)
            .await
            .unwrap();

        assert!(!linked);
        let binding = agent.telegram_binding().unwrap();
        assert!(binding.allowed_senders.is_empty());
        assert!(
            binding.pending_pairing_code.is_some(),
            "an expired attempt must not consume the code"
        );
        assert_eq!(
            persistence.linked_senders.get("agent-h", "telegram").await.unwrap(),
            None,
            "a rejected pairing attempt must not write anything to the store"
        );
    }

    #[tokio::test]
    async fn try_link_chat_rejects_mismatched_code_case_sensitively() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(Some("thread-i"));
        binding.pending_pairing_code = Some(pairing_code("ABC123", 1_700_000_600));
        let mut agent = make_agent("agent-i", Some(binding));
        persistence.agents.create(&agent).await.unwrap();

        let wrong_code = try_link_chat(&persistence, &mut agent, "telegram", 555, "XYZ999", 1_700_000_000)
            .await
            .unwrap();
        assert!(!wrong_code);

        let wrong_case = try_link_chat(&persistence, &mut agent, "telegram", 555, "abc123", 1_700_000_000)
            .await
            .unwrap();
        assert!(!wrong_case, "code matching must be case-sensitive");

        let binding = agent.telegram_binding().unwrap();
        assert!(binding.allowed_senders.is_empty());
        assert!(binding.pending_pairing_code.is_some());
        assert_eq!(persistence.linked_senders.get("agent-i", "telegram").await.unwrap(), None);
    }

    #[tokio::test]
    async fn try_link_chat_rejects_when_no_pairing_is_pending() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap())]);

        let binding = enabled_telegram_binding(Some("thread-j"));
        let mut agent = make_agent("agent-j", Some(binding));
        persistence.agents.create(&agent).await.unwrap();

        let linked = try_link_chat(&persistence, &mut agent, "telegram", 555, "ABC123", 1_700_000_000)
            .await
            .unwrap();

        assert!(!linked);
        assert!(agent.telegram_binding().unwrap().allowed_senders.is_empty());
        assert_eq!(persistence.linked_senders.get("agent-j", "telegram").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_chat_linked_via_pairing_then_passes_the_allowlist_gate() {
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(Some("thread-k"));
        binding.pending_pairing_code = Some(pairing_code("ABC123", 1_700_000_600));
        let mut agent = make_agent("agent-k", Some(binding));
        persistence.agents.create(&agent).await.unwrap();

        let allowed_before = persistence
            .linked_senders
            .get("agent-k", "telegram")
            .await
            .unwrap()
            .map(|l| l.senders)
            .unwrap_or_default();
        let allowed_before_ids: Vec<i64> = allowed_before.iter().filter_map(|s| s.parse().ok()).collect();
        assert!(
            !is_chat_allowed(&allowed_before_ids, 555),
            "unlinked chat must be rejected before pairing"
        );

        try_link_chat(&persistence, &mut agent, "telegram", 555, "ABC123", 1_700_000_000)
            .await
            .unwrap();

        let allowed_after = persistence.linked_senders.get("agent-k", "telegram").await.unwrap().unwrap().senders;
        let allowed_after_ids: Vec<i64> = allowed_after.iter().filter_map(|s| s.parse().ok()).collect();
        assert!(
            is_chat_allowed(&allowed_after_ids, 555),
            "a normal message from the now-linked chat must pass"
        );
    }

    #[tokio::test]
    async fn a_sender_linked_via_pairing_survives_a_concurrent_update_agent_style_profile_clobber() {
        // Regression for the clobber this store exists to close: the
        // pairing writer and a `PUT /agents/{id}` handler used to both
        // round-trip the whole profile document through the same inline
        // `allowed_senders` field, so whichever wrote last won. Now that
        // the pairing writer only ever touches LinkedSenderStore, a stale
        // whole-document write from elsewhere can no longer erase it.
        let _lock = lock_env();
        let (persistence, tmp) = make_persistence().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(Some("thread-l"));
        binding.pending_pairing_code = Some(pairing_code("ABC123", 1_700_000_600));
        let mut agent = make_agent("agent-l", Some(binding));
        persistence.agents.create(&agent).await.unwrap();

        try_link_chat(&persistence, &mut agent, "telegram", 555, "ABC123", 1_700_000_000)
            .await
            .unwrap();

        // Simulate a concurrent `PUT /agents/{id}` landing with a stale,
        // empty `allowed_senders` on its copy of the Telegram binding — the
        // exact shape of the bug this store closes.
        let mut stale_profile = persistence.agents.get("agent-l").await.unwrap().unwrap();
        let stale_binding = stale_profile.telegram_binding_mut().expect("telegram binding present");
        assert!(
            stale_binding.allowed_senders.is_empty(),
            "the inline field was never populated by the pairing writer"
        );
        persistence.agents.update(&stale_profile).await.unwrap();

        let senders = persistence.linked_senders.get("agent-l", "telegram").await.unwrap().unwrap().senders;
        assert_eq!(
            senders,
            vec!["555".to_string()],
            "the linked sender must survive a whole-document profile save that never touches the store"
        );
        let allowed_ids: Vec<i64> = senders.iter().filter_map(|s| s.parse().ok()).collect();
        assert!(is_chat_allowed(&allowed_ids, 555), "the linked chat must still be enforced as allowed");
    }

    // --- Auto-title on first inbound message ---

    /// A stub [`AgentRunner`] that completes immediately without spawning a
    /// real process, mirroring `slack::runner`'s test double of the same
    /// name — just enough for `submit_inbound_message`'s real
    /// `QueueManagerRegistry` path to run inside a unit test.
    struct StubRunner;

    #[async_trait]
    impl AgentRunner for StubRunner {
        fn mode(&self) -> AgentRunnerMode {
            AgentRunnerMode::Cli
        }

        async fn run(&self, req: AgentRunRequest) -> Result<RunComplete, AoError> {
            let run_id = req.pre_registered_run_id.unwrap_or_else(|| "test-run".to_string());
            let rc = RunComplete {
                run_id,
                output_text: "ok".to_string(),
                workflow_followups: vec![],
                end_reason: RunEndReason::Completed,
            };
            let _ = req.run_complete_tx.send(rc.clone()).await;
            Ok(rc)
        }
    }

    /// Shared plumbing every `handle_update` resolve test below needs — a
    /// real `QueueManagerRegistry` (so `submit_inbound_message` can run) plus
    /// empty connection-state/lease-gate registries. Mirrors
    /// `discord::runner::tests::TestHarness`.
    struct TestHarness {
        persistence: Arc<PersistenceLayer>,
        _tmp: tempfile::TempDir,
        event_bus: Arc<EventBus>,
        queue_registry: Arc<QueueManagerRegistry>,
        lease_gate: Arc<LeaseGate>,
    }

    impl TestHarness {
        fn ctx(&self, agent_id: &str) -> ChannelRunContext {
            ChannelRunContext {
                agent_id: agent_id.to_string(),
                binding_id: "telegram".to_string(),
                persistence: Arc::clone(&self.persistence),
                queue_registry: Arc::clone(&self.queue_registry),
                connection_state: Arc::new(ConnectionStateRegistry::new()),
                lease_gate: Arc::clone(&self.lease_gate),
                event_bus: Arc::clone(&self.event_bus),
            }
        }
    }

    async fn make_test_harness() -> TestHarness {
        let (persistence, tmp) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let instance_registry = Arc::new(InstanceRegistry::new());
        let dispatcher =
            Arc::new(RunnerDispatcher::with_runners(Arc::new(StubRunner), Arc::new(StubRunner)));
        let queue_registry = Arc::new(QueueManagerRegistry::new(
            dispatcher,
            instance_registry,
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
        ));
        TestHarness { persistence, _tmp: tmp, event_bus, queue_registry, lease_gate: Arc::new(LeaseGate::new()) }
    }

    fn make_update(update_id: i64, message_id: i64, chat_id: i64, sender_id: i64, text: &str) -> TelegramUpdate {
        TelegramUpdate {
            update_id,
            message: Some(TelegramMessage {
                message_id,
                chat: TelegramChat { id: chat_id, chat_type: TelegramChatType::Private },
                text: Some(text.to_string()),
                from: Some(TelegramUser { id: sender_id, username: Some("alice".to_string()) }),
                entities: vec![],
                reply_to_message: None,
            }),
        }
    }

    /// SHARE-WITHIN, combined with the pre-existing
    /// auto-title guarantee: repeated inbound updates on the *same* `chat_id`
    /// resolve to the *same* per-conversation thread, and only the first
    /// update's text ever seeds `auto_title`.
    #[tokio::test]
    async fn handle_update_sets_auto_title_on_a_fresh_thread_and_a_later_message_does_not_overwrite_it() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, harness._tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(None);
        binding.allowed_senders = vec!["555".to_string()];
        let mut agent = make_agent("agent-title", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-title");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();

        let first_update = make_update(1, 100, 555, 555, "please help with the deploy");
        // `handle_update` awaits `submit_inbound_message` directly (nothing
        // is spawned onto a background task here), so the auto_title write
        // is guaranteed to be visible the moment this call returns — no
        // polling needed, unlike the Slack socket-loop equivalent of this
        // test.
        handle_update(&first_update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, None).await;

        let row = ctx
            .persistence
            .conversation_registry
            .get("agent-title", "telegram", &ConversationKey::new("555"))
            .await
            .expect("read registry")
            .expect("row exists after the first inbound update");
        let thread_id = row.thread_id.clone();

        let after_first = ctx.persistence.threads.get(&thread_id).await.unwrap().expect("thread exists");
        assert_eq!(after_first.auto_title.as_deref(), Some("please help with the deploy"));
        assert!(after_first.title.is_none(), "a fresh Telegram bridge thread must stay renamable (title unset)");

        let second_update = make_update(2, 101, 555, 555, "totally unrelated follow-up text");
        handle_update(&second_update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, None).await;

        let row_again = ctx
            .persistence
            .conversation_registry
            .get("agent-title", "telegram", &ConversationKey::new("555"))
            .await
            .expect("read registry")
            .expect("row still exists");
        assert_eq!(row_again.thread_id, thread_id, "the same chat_id must reuse the same per-conversation thread");

        let after_second = ctx.persistence.threads.get(&thread_id).await.unwrap().expect("thread exists");
        assert_eq!(
            after_second.auto_title.as_deref(),
            Some("please help with the deploy"),
            "a later message must never overwrite the auto_title set from the first one"
        );
    }

    /// ISOLATE-ACROSS: two distinct
    /// `chat_id`s mint two distinct threads, and the second conversation's
    /// transcript never carries the first's content — the actual security
    /// guarantee this whole phase exists to prove. A private chat's `chat_id`
    /// **is** the sender's own id, so this is exactly the "two strangers DM
    /// the same bot" scenario.
    #[tokio::test]
    async fn isolate_across_different_chat_ids_mint_distinct_threads_with_no_shared_context() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, harness._tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(None);
        binding.allowed_senders = vec!["111111".to_string(), "222222".to_string()];
        let mut agent = make_agent("agent-isolate", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-isolate");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();

        let joans_secret = "joans-secret-token-12345";
        let joan = make_update(1, 100, 111111, 111111, joans_secret);
        handle_update(&joan, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, None).await;

        let mathew = make_update(2, 101, 222222, 222222, "hey, what's up?");
        handle_update(&mathew, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, None).await;

        let joan_row = ctx
            .persistence
            .conversation_registry
            .get("agent-isolate", "telegram", &ConversationKey::new("111111"))
            .await
            .expect("read registry")
            .expect("joan's row exists");
        let mathew_row = ctx
            .persistence
            .conversation_registry
            .get("agent-isolate", "telegram", &ConversationKey::new("222222"))
            .await
            .expect("read registry")
            .expect("mathew's row exists");

        assert_ne!(joan_row.thread_id, mathew_row.thread_id, "different chat_ids must mint distinct threads");

        let mathews_thread =
            ctx.persistence.threads.get(&mathew_row.thread_id).await.expect("read thread").expect("thread exists");
        let mathews_transcript = ctx
            .persistence
            .transcripts
            .read_all_at(&std::path::PathBuf::from(&mathews_thread.transcript_path))
            .await
            .expect("read mathew's transcript");
        assert!(
            !mathews_transcript.iter().any(|entry| entry.content.contains(joans_secret)),
            "mathew's thread must never carry joan's message content"
        );
    }

    /// Resolving a conversation must register its thread with the process's
    /// `LeaseGate` from Telegram's own inbound dispatch, not
    /// from `ChannelBridge::reconcile`'s placeholder — mirrors the equivalent
    /// Discord guarantee for `resolve_discord_conversation_thread`.
    #[tokio::test]
    async fn resolving_a_conversation_marks_its_thread_active_in_the_lease_gate() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, harness._tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(None);
        binding.allowed_senders = vec!["777777".to_string()];
        let mut agent = make_agent("agent-lease", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-lease");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();

        let update = make_update(1, 100, 777777, 777777, "hello");
        handle_update(&update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, None).await;

        let row = ctx
            .persistence
            .conversation_registry
            .get("agent-lease", "telegram", &ConversationKey::new("777777"))
            .await
            .expect("read registry")
            .expect("row exists");

        assert!(ctx.lease_gate.is_active(&row.thread_id), "resolving a conversation must mark its thread active");
    }

    /// REACHABILITY — the bug this whole phase exists to fix, driven through
    /// the actual live inbound entry point (`handle_update`), not just the
    /// persistence-layer store directly. `binding_id` is the constant
    /// `"telegram"` for every agent's Telegram binding, and a private chat's
    /// `chat_id` **is** the human's own user id — identical no matter which
    /// agent's bot they're DMing. Before the fix, `conversation_registry`
    /// sharded only by `binding_id`, so agent B's inbound update here would
    /// have read and silently rewritten agent A's already-registered row:
    /// no thread ever minted for B, no `ThreadCreated` ever emitted for B,
    /// and every later message from this `chat_id` would keep routing into
    /// A's thread forever.
    ///
    /// Asserts, from a chat_id already registered to agent A: agent B's
    /// inbound message (1) persists its own row, distinct from A's, and (2)
    /// emits a `ThreadCreated` event scoped to agent B specifically.
    #[tokio::test]
    async fn a_second_agent_inbound_on_a_chat_id_already_owned_by_a_first_agent_mints_its_own_thread_and_event() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, harness._tmp.path().to_str().unwrap())]);

        let shared_chat_id: i64 = 918273645;

        let mut binding_a = enabled_telegram_binding(None);
        binding_a.allowed_senders = vec![shared_chat_id.to_string()];
        let mut agent_a = make_agent("agent-collide-a", Some(binding_a));
        harness.persistence.agents.create(&agent_a).await.unwrap();

        let mut binding_b = enabled_telegram_binding(None);
        binding_b.allowed_senders = vec![shared_chat_id.to_string()];
        let mut agent_b = make_agent("agent-collide-b", Some(binding_b));
        harness.persistence.agents.create(&agent_b).await.unwrap();

        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();

        // Subscribed before either `handle_update` call so both agents'
        // `ThreadCreated` events land in this receiver's buffer.
        let mut events = harness.event_bus.subscribe();

        let ctx_a = harness.ctx("agent-collide-a");
        let update_a = make_update(1, 100, shared_chat_id, shared_chat_id, "hello, this is a stranger to agent B");
        handle_update(&update_a, &ctx_a, &mut agent_a, 0, &client, "fake-token", &in_flight, None).await;

        let ctx_b = harness.ctx("agent-collide-b");
        let update_b =
            make_update(2, 101, shared_chat_id, shared_chat_id, "hello, this is the same human, a different agent");
        handle_update(&update_b, &ctx_b, &mut agent_b, 0, &client, "fake-token", &in_flight, None).await;

        let row_a = ctx_a
            .persistence
            .conversation_registry
            .get("agent-collide-a", "telegram", &ConversationKey::new(shared_chat_id.to_string()))
            .await
            .expect("read registry")
            .expect("agent A's row must exist");
        let row_b = ctx_b
            .persistence
            .conversation_registry
            .get("agent-collide-b", "telegram", &ConversationKey::new(shared_chat_id.to_string()))
            .await
            .expect("read registry")
            .expect(
                "agent B's row must exist — the live inbound path must mint a thread scoped to B \
                 rather than silently reusing or clobbering A's row",
            );

        assert_ne!(
            row_a.thread_id, row_b.thread_id,
            "two different agents sharing a chat_id must never collide onto one thread"
        );
        assert_eq!(row_a.agent_id, "agent-collide-a");
        assert_eq!(row_b.agent_id, "agent-collide-b");

        let mut found_b_thread_created = false;
        while let Ok(event) = events.try_recv() {
            if event.agent_id == "agent-collide-b" {
                if let AgentEventPayload::ThreadCreated { thread } = &event.payload {
                    assert_eq!(
                        thread.id, row_b.thread_id,
                        "the emitted ThreadCreated must be for the thread actually persisted for agent B"
                    );
                    found_b_thread_created = true;
                }
            }
        }
        assert!(
            found_b_thread_created,
            "a ThreadCreated event scoped to agent B must be emitted on B's first inbound message for a \
             chat_id already registered to agent A"
        );
    }

    // --- UTF-16 entity slicing (`utf16_slice`) ---

    #[test]
    fn utf16_slice_slices_correctly_when_a_multibyte_char_precedes_the_target_range() {
        // "🎉" sits outside the Basic Multilingual Plane, so it counts as 2
        // UTF-16 code units even though it's a single `char` (and 4 UTF-8
        // bytes) — the mention right after it therefore starts at UTF-16
        // offset 2, not byte offset 2. Byte-indexing at offset 2 would land
        // mid-emoji.
        let text = "🎉@mybot hello";
        assert_eq!(utf16_slice(text, 2, 6), Some("@mybot"));
    }

    #[test]
    fn utf16_slice_returns_none_for_an_out_of_range_entity() {
        assert!(utf16_slice("short", 100, 5).is_none());
    }

    #[test]
    fn utf16_slice_returns_none_for_a_negative_offset_or_length() {
        assert!(utf16_slice("hello", -1, 3).is_none());
        assert!(utf16_slice("hello", 0, -1).is_none());
    }

    // --- Group addressing decision + text cleanup (`group_addressing`) ---

    fn mention_entity(offset: i64, length: i64) -> TelegramMessageEntity {
        TelegramMessageEntity { entity_type: TelegramMessageEntityType::Mention, offset, length, user: None }
    }

    fn text_mention_entity(offset: i64, length: i64, user_id: i64) -> TelegramMessageEntity {
        TelegramMessageEntity {
            entity_type: TelegramMessageEntityType::TextMention,
            offset,
            length,
            user: Some(TelegramUser { id: user_id, username: None }),
        }
    }

    fn bot_command_entity(offset: i64, length: i64) -> TelegramMessageEntity {
        TelegramMessageEntity { entity_type: TelegramMessageEntityType::BotCommand, offset, length, user: None }
    }

    fn message_from(user_id: i64) -> TelegramMessage {
        TelegramMessage {
            message_id: 1,
            chat: TelegramChat { id: 1, chat_type: TelegramChatType::Group },
            text: Some("a prior message".to_string()),
            from: Some(TelegramUser { id: user_id, username: None }),
            entities: vec![],
            reply_to_message: None,
        }
    }

    #[test]
    fn group_addressing_returns_none_without_a_mention_reply_or_command() {
        assert!(group_addressing("just chatting away", &[], None, 42, "mybot").is_none());
    }

    #[test]
    fn group_addressing_strips_a_matched_mention_case_insensitively() {
        let text = "@MyBot summarize this";
        let entities = vec![mention_entity(0, 6)]; // "@MyBot" is 6 UTF-16 units
        let result = group_addressing(text, &entities, None, 42, "mybot");
        assert_eq!(result.as_deref(), Some("summarize this"));
    }

    #[test]
    fn group_addressing_ignores_a_mention_of_a_different_username() {
        let text = "@someoneelse can you help?";
        let entities = vec![mention_entity(0, 12)];
        assert!(group_addressing(text, &entities, None, 42, "mybot").is_none());
    }

    #[test]
    fn group_addressing_treats_a_reply_to_the_bots_own_message_as_addressed_and_leaves_text_untouched() {
        let text = "thanks for the last answer";
        let reply = message_from(42);
        let result = group_addressing(text, &[], Some(&reply), 42, "mybot");
        assert_eq!(result.as_deref(), Some(text), "a reply-based trigger must not alter the dispatched text");
    }

    #[test]
    fn group_addressing_ignores_a_reply_to_someone_other_than_the_bot() {
        let text = "thanks!";
        let reply = message_from(999);
        assert!(group_addressing(text, &[], Some(&reply), 42, "mybot").is_none());
    }

    #[test]
    fn group_addressing_treats_a_text_mention_of_the_bot_id_as_addressed_and_leaves_text_untouched() {
        let text = "hey Assistant can you help?";
        let entities = vec![text_mention_entity(4, 9, 42)]; // "Assistant"
        let result = group_addressing(text, &entities, None, 42, "mybot");
        assert_eq!(result.as_deref(), Some(text), "a text_mention trigger must not alter the dispatched text");
    }

    #[test]
    fn group_addressing_ignores_a_text_mention_of_someone_else() {
        let text = "hey Alex can you help?";
        let entities = vec![text_mention_entity(4, 4, 222)]; // "Alex"
        assert!(group_addressing(text, &entities, None, 42, "mybot").is_none());
    }

    #[test]
    fn group_addressing_treats_a_bot_command_suffixed_to_this_bot_as_addressed() {
        let text = "/summarize@mybot please recap";
        let entities = vec![bot_command_entity(0, 16)]; // "/summarize@mybot"
        let result = group_addressing(text, &entities, None, 42, "mybot");
        assert_eq!(result.as_deref(), Some(text), "a bot_command trigger must not alter the dispatched text");
    }

    #[test]
    fn group_addressing_ignores_a_bare_bot_command_with_no_bot_suffix() {
        let text = "/start";
        let entities = vec![bot_command_entity(0, 6)];
        assert!(
            group_addressing(text, &entities, None, 42, "mybot").is_none(),
            "a bare /command with no @suffix is Telegram's fan-out form, not addressed to this bot specifically"
        );
    }

    #[test]
    fn group_addressing_ignores_a_bot_command_suffixed_to_a_different_bot() {
        let text = "/summarize@otherbot please";
        let entities = vec![bot_command_entity(0, 19)]; // "/summarize@otherbot"
        assert!(group_addressing(text, &entities, None, 42, "mybot").is_none());
    }

    #[test]
    fn group_addressing_slices_a_mention_correctly_past_a_multibyte_prefix() {
        // Same UTF-16-vs-byte-offset concern as the `utf16_slice` tests
        // above, exercised through the full addressing + cleanup path.
        let text = "🎉@mybot hello";
        let entities = vec![mention_entity(2, 6)]; // "@mybot", after the 2-unit emoji
        let result = group_addressing(text, &entities, None, 42, "mybot");
        assert_eq!(result.as_deref(), Some("🎉 hello"));
    }

    // --- Group addressing wired through `handle_update` ---

    fn test_bot_info() -> TelegramBotInfo {
        TelegramBotInfo { id: 42, is_bot: true, username: "mybot".to_string(), first_name: "My Bot".to_string() }
    }

    #[allow(clippy::too_many_arguments)]
    fn make_group_update(
        update_id: i64,
        message_id: i64,
        chat_id: i64,
        sender_id: i64,
        text: &str,
        chat_type: TelegramChatType,
        entities: Vec<TelegramMessageEntity>,
        reply_to_message: Option<Box<TelegramMessage>>,
    ) -> TelegramUpdate {
        TelegramUpdate {
            update_id,
            message: Some(TelegramMessage {
                message_id,
                chat: TelegramChat { id: chat_id, chat_type },
                text: Some(text.to_string()),
                from: Some(TelegramUser { id: sender_id, username: Some("alice".to_string()) }),
                entities,
                reply_to_message,
            }),
        }
    }

    #[tokio::test]
    async fn handle_update_skips_a_group_message_that_does_not_address_the_bot() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, harness._tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(None);
        binding.allowed_senders = vec!["-100999".to_string()];
        let mut agent = make_agent("agent-group-skip", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-group-skip");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();
        let bot = test_bot_info();

        let update = make_group_update(
            1,
            100,
            -100999,
            555,
            "just chatting, no mention here",
            TelegramChatType::Group,
            vec![],
            None,
        );
        handle_update(&update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, Some(&bot)).await;

        let row = ctx
            .persistence
            .conversation_registry
            .get("agent-group-skip", "telegram", &ConversationKey::new("-100999"))
            .await
            .expect("read registry");
        assert!(row.is_none(), "an unaddressed group message must never dispatch or mint a bridge thread");
    }

    #[tokio::test]
    async fn handle_update_dispatches_a_group_mention_with_the_mention_stripped() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, harness._tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(None);
        binding.allowed_senders = vec!["-100888".to_string()];
        let mut agent = make_agent("agent-group-mention", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-group-mention");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();
        let bot = test_bot_info();

        let text = "@mybot summarize this thread";
        let entities = vec![mention_entity(0, 6)];
        let update = make_group_update(1, 100, -100888, 555, text, TelegramChatType::Group, entities, None);
        handle_update(&update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, Some(&bot)).await;

        let row = ctx
            .persistence
            .conversation_registry
            .get("agent-group-mention", "telegram", &ConversationKey::new("-100888"))
            .await
            .expect("read registry")
            .expect("an addressed group message must mint a bridge thread");
        let thread = ctx.persistence.threads.get(&row.thread_id).await.unwrap().expect("thread exists");
        let transcript = ctx
            .persistence
            .transcripts
            .read_all_at(&std::path::PathBuf::from(&thread.transcript_path))
            .await
            .expect("read transcript");
        assert!(
            transcript.iter().any(|entry| entry.content.contains("summarize this thread")),
            "the dispatched text must reach the transcript"
        );
        assert!(
            !transcript.iter().any(|entry| entry.content.contains("@mybot")),
            "the addressing @mention must be stripped before dispatch"
        );
    }

    #[tokio::test]
    async fn handle_update_dispatches_a_group_reply_to_the_bot_unchanged() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, harness._tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(None);
        binding.allowed_senders = vec!["-100777".to_string()];
        let mut agent = make_agent("agent-group-reply", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-group-reply");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();
        let bot = test_bot_info();

        let reply_to = Box::new(TelegramMessage {
            message_id: 50,
            chat: TelegramChat { id: -100777, chat_type: TelegramChatType::Group },
            text: Some("here's my answer".to_string()),
            from: Some(TelegramUser { id: 42, username: Some("mybot".to_string()) }),
            entities: vec![],
            reply_to_message: None,
        });
        let text = "thanks, makes sense";
        let update =
            make_group_update(1, 100, -100777, 555, text, TelegramChatType::Group, vec![], Some(reply_to));
        handle_update(&update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, Some(&bot)).await;

        let row = ctx
            .persistence
            .conversation_registry
            .get("agent-group-reply", "telegram", &ConversationKey::new("-100777"))
            .await
            .expect("read registry")
            .expect("a reply to the bot must mint a bridge thread");
        let thread = ctx.persistence.threads.get(&row.thread_id).await.unwrap().expect("thread exists");
        let transcript = ctx
            .persistence
            .transcripts
            .read_all_at(&std::path::PathBuf::from(&thread.transcript_path))
            .await
            .expect("read transcript");
        assert!(transcript.iter().any(|entry| entry.content.contains(text)), "reply text must dispatch unchanged");
    }

    #[tokio::test]
    async fn handle_update_dispatches_a_group_text_mention_of_the_bot_id() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, harness._tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(None);
        binding.allowed_senders = vec!["-100666".to_string()];
        let mut agent = make_agent("agent-group-textmention", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-group-textmention");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();
        let bot = test_bot_info();

        let text = "hey Assistant can you help?";
        let entities = vec![text_mention_entity(4, 9, 42)]; // "Assistant"
        let update = make_group_update(1, 100, -100666, 555, text, TelegramChatType::Group, entities, None);
        handle_update(&update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, Some(&bot)).await;

        let row = ctx
            .persistence
            .conversation_registry
            .get("agent-group-textmention", "telegram", &ConversationKey::new("-100666"))
            .await
            .expect("read registry")
            .expect("a text_mention of the bot's id must mint a bridge thread");
        let thread = ctx.persistence.threads.get(&row.thread_id).await.unwrap().expect("thread exists");
        let transcript = ctx
            .persistence
            .transcripts
            .read_all_at(&std::path::PathBuf::from(&thread.transcript_path))
            .await
            .expect("read transcript");
        assert!(transcript.iter().any(|entry| entry.content.contains(text)), "text_mention text must dispatch unchanged");
    }

    #[tokio::test]
    async fn handle_update_leaves_a_private_chat_dispatch_unaffected_by_group_gating() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, harness._tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(None);
        binding.allowed_senders = vec!["555".to_string()];
        let mut agent = make_agent("agent-private-unaffected", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-private-unaffected");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();
        // A resolved bot identity is present, and the message carries no
        // mention/reply/command entity — if group-gating logic somehow ran
        // for a private chat, this message would be skipped. It must not
        // be: private chats always proceed, exactly as before this feature.
        let bot = test_bot_info();

        let text = "just a private message, no mention needed";
        let update = make_group_update(1, 100, 555, 555, text, TelegramChatType::Private, vec![], None);
        handle_update(&update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, Some(&bot)).await;

        let row = ctx
            .persistence
            .conversation_registry
            .get("agent-private-unaffected", "telegram", &ConversationKey::new("555"))
            .await
            .expect("read registry")
            .expect("a private message must always dispatch and mint a bridge thread");
        let thread = ctx.persistence.threads.get(&row.thread_id).await.unwrap().expect("thread exists");
        let transcript = ctx
            .persistence
            .transcripts
            .read_all_at(&std::path::PathBuf::from(&thread.transcript_path))
            .await
            .expect("read transcript");
        assert!(transcript.iter().any(|entry| entry.content.contains(text)), "private-chat text must dispatch unchanged");
    }

    #[tokio::test]
    async fn handle_update_skips_a_group_message_when_bot_identity_is_unresolved() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let _env = EnvGuard::set(&[(DATA_DIR_ENV_VAR, harness._tmp.path().to_str().unwrap())]);

        let mut binding = enabled_telegram_binding(None);
        binding.allowed_senders = vec!["-100555".to_string()];
        let mut agent = make_agent("agent-group-no-identity", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-group-no-identity");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();

        let text = "@mybot summarize this";
        let entities = vec![mention_entity(0, 6)];
        let update = make_group_update(1, 100, -100555, 555, text, TelegramChatType::Group, entities, None);
        // `bot: None` — mirrors a `getMe` failure at poll-task startup.
        handle_update(&update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, None).await;

        let row = ctx
            .persistence
            .conversation_registry
            .get("agent-group-no-identity", "telegram", &ConversationKey::new("-100555"))
            .await
            .expect("read registry");
        assert!(row.is_none(), "a group message must fail closed when the bot's own identity couldn't be resolved");
    }

    // --- Group pairing (`/start@<bot_username> <code>`) ---

    /// Mounts a `sendMessage` stub on `mock_server` for `token` and points
    /// `LAUNCHPAD_TELEGRAM_API_BASE_URL` at it, alongside the usual data-root
    /// override — every pairing attempt below sends a confirmation/failure
    /// reply, and without this the real `TelegramClient` would otherwise
    /// dial out to `api.telegram.org` with a fake token during a unit test.
    async fn group_pairing_env(
        tmp: &std::path::Path,
        token: &str,
    ) -> (wiremock::MockServer, EnvGuard) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/bot{token}/sendMessage")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "result": {}})))
            .mount(&mock_server)
            .await;
        let env = EnvGuard::set(&[
            (DATA_DIR_ENV_VAR, tmp.to_str().unwrap()),
            ("LAUNCHPAD_TELEGRAM_API_BASE_URL", &mock_server.uri()),
        ]);
        (mock_server, env)
    }

    /// (i) `/start@mybot CODE` sent in a supergroup, with a valid pending
    /// code, must authorize the *group's* `chat_id` — the inbound message's
    /// own chat, not the sender's private id.
    #[tokio::test]
    async fn handle_update_group_start_with_bot_suffix_and_valid_code_authorizes_the_group() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let (_mock_server, _env) = group_pairing_env(harness._tmp.path(), "fake-token").await;

        let mut binding = enabled_telegram_binding(None);
        binding.pending_pairing_code = Some(pairing_code("GRPCODE", Utc::now().timestamp() + 600));
        let mut agent = make_agent("agent-group-pair-ok", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-group-pair-ok");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();
        let bot = test_bot_info(); // username "mybot"

        let update = make_group_update(
            1,
            100,
            -100321,
            555,
            "/start@MyBot GRPCODE",
            TelegramChatType::Supergroup,
            vec![],
            None,
        );
        handle_update(&update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, Some(&bot)).await;

        let senders = ctx
            .persistence
            .linked_senders
            .get("agent-group-pair-ok", "telegram")
            .await
            .unwrap()
            .map(|l| l.senders)
            .unwrap_or_default();
        assert_eq!(
            senders,
            vec!["-100321".to_string()],
            "the group's own (negative) chat_id must be authorized, not the sender's"
        );
        assert!(agent.telegram_binding().unwrap().pending_pairing_code.is_none(), "the code must be consumed");
    }

    /// (ii) The bare `/start CODE` form (private chat / deep link) must
    /// still pair correctly through `handle_update`, unaffected by the new
    /// `@bot_username` handling.
    #[tokio::test]
    async fn handle_update_private_start_without_bot_suffix_still_pairs() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let (_mock_server, _env) = group_pairing_env(harness._tmp.path(), "fake-token").await;

        let mut binding = enabled_telegram_binding(None);
        binding.pending_pairing_code = Some(pairing_code("PRIVCODE", Utc::now().timestamp() + 600));
        let mut agent = make_agent("agent-private-pair-ok", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-private-pair-ok");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();

        let update = make_update(1, 100, 555, 555, "/start PRIVCODE");
        handle_update(&update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, None).await;

        let senders = ctx
            .persistence
            .linked_senders
            .get("agent-private-pair-ok", "telegram")
            .await
            .unwrap()
            .map(|l| l.senders)
            .unwrap_or_default();
        assert_eq!(senders, vec!["555".to_string()], "private /start CODE pairing must be unaffected");
    }

    /// (iii) A wrong code sent in a group — even with a correctly-suffixed
    /// command — must not authorize anything.
    #[tokio::test]
    async fn handle_update_group_start_with_wrong_code_does_not_authorize() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let (_mock_server, _env) = group_pairing_env(harness._tmp.path(), "fake-token").await;

        let mut binding = enabled_telegram_binding(None);
        binding.pending_pairing_code = Some(pairing_code("GRPCODE", Utc::now().timestamp() + 600));
        let mut agent = make_agent("agent-group-pair-wrong", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-group-pair-wrong");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();
        let bot = test_bot_info();

        let update = make_group_update(
            1,
            100,
            -100654,
            555,
            "/start@mybot WRONGCODE",
            TelegramChatType::Supergroup,
            vec![],
            None,
        );
        handle_update(&update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, Some(&bot)).await;

        let senders = ctx.persistence.linked_senders.get("agent-group-pair-wrong", "telegram").await.unwrap();
        assert!(senders.is_none(), "a wrong code must never authorize the group");
        assert!(
            agent.telegram_binding().unwrap().pending_pairing_code.is_some(),
            "a wrong attempt must not consume the still-pending correct code"
        );
    }

    /// (iv) `/start@othername CODE` — suffixed to a *different* bot's
    /// username in the same group — must not be treated as our `/start`
    /// command at all: the correct code must not be consumed, and the group
    /// must stay unauthorized. Guards against a naive parser that strips any
    /// `@suffix` rather than checking it against this bot's own username.
    #[tokio::test]
    async fn handle_update_group_start_suffixed_to_a_different_bot_is_not_our_command() {
        let _lock = lock_env();
        let harness = make_test_harness().await;
        let (_mock_server, _env) = group_pairing_env(harness._tmp.path(), "fake-token").await;

        let mut binding = enabled_telegram_binding(None);
        binding.pending_pairing_code = Some(pairing_code("GRPCODE", Utc::now().timestamp() + 600));
        let mut agent = make_agent("agent-group-pair-otherbot", Some(binding));
        harness.persistence.agents.create(&agent).await.unwrap();

        let ctx = harness.ctx("agent-group-pair-otherbot");
        let client = TelegramClient::new();
        let in_flight = InFlightChats::new();
        let bot = test_bot_info(); // username "mybot"

        // Same, otherwise-valid code — only the suffix differs.
        let update = make_group_update(
            1,
            100,
            -100987,
            555,
            "/start@othername GRPCODE",
            TelegramChatType::Supergroup,
            vec![],
            None,
        );
        handle_update(&update, &ctx, &mut agent, 0, &client, "fake-token", &in_flight, Some(&bot)).await;

        let senders = ctx.persistence.linked_senders.get("agent-group-pair-otherbot", "telegram").await.unwrap();
        assert!(senders.is_none(), "a command suffixed to a different bot must never authorize the group");
        assert!(
            agent.telegram_binding().unwrap().pending_pairing_code.is_some(),
            "the pending code must survive untouched, since this was never recognized as a pairing attempt"
        );
    }
}
