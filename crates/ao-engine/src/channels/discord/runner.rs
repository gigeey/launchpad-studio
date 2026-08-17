//! The Discord Gateway v10 connect/heartbeat/reconnect state machine —
//! Discord's analogue of `crate::channels::email::run_email_poll_loop` /
//! `crate::telegram::transport::run_bot_poll_loop`, adapted to a long-lived
//! push connection instead of a poll cycle.
//!
//! [`run_discord_gateway_loop`] is the outer "never return except on
//! cancel" loop `DiscordTransport::spawn` hands to `tokio::spawn`: each
//! iteration opens a fresh [`TungsteniteGatewaySeam`], hands it to
//! [`run_connection`] to drive until that connection ends (a zombie
//! heartbeat, a read error, or a server-initiated close), then
//! unconditionally closes the socket before looping back to reconnect —
//! regardless of *why* the connection ended. Centralizing the close there
//! (rather than in each individual error path inside `run_connection`) is
//! what makes "never just drop the socket" a structural guarantee instead
//! of something every exit path has to remember on its own.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use ao_protocol::agent::{ChannelKind, ChannelKindConfig};
use ao_protocol::channel_connection_state::ChannelConnectionState;
use ao_protocol::channel_cursor::ChannelCursor;
use ao_protocol::conversation_registry::ConversationKey;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::thread::{ChannelBridgeOrigin, Thread};

use crate::channels::relay::conversation_gc;
use crate::channels::{submit_inbound_message, ChannelRunContext};

use super::backfill::{fetch_reply_chain_backfill, fetch_thread_backfill, format_backfill};
use super::channel_meta::{resolve_channel_meta, ChannelMetaCache};
use super::engagement::{EngagementDecision, EngagementInput, EngagementParams, EngagementTracker};
use super::gateway_seam::{GatewaySeam, GatewaySeamError, TungsteniteGatewaySeam};
use super::protocol::{self, DispatchKind, GatewayEvent, MessageCreateEvent};
use super::security;
use super::session::{HeartbeatTracker, SeenMessageIds};
use super::title::clean_discord_markup;
use super::InFlightChannels;

/// Fixed pause after a failed connect, a dead connection, or a fatal read
/// error before the outer loop tries again. Keeps one unhealthy binding
/// (revoked token, network blip, Discord outage) from hot-looping.
const ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// Bound on the RESUME-replay dedup set — comfortably larger than any
/// realistic replay window (a `RESUME` only ever replays a short recent
/// backlog, never a connection's full history), so eviction never drops an
/// id a real replay could still reference.
const SEEN_IDS_CAPACITY: usize = 4096;

const INVALID_SESSION_MIN_DELAY_MS: u64 = 1_000;
const INVALID_SESSION_MAX_DELAY_MS: u64 = 5_000;

/// Backoff used instead of [`ERROR_BACKOFF`] after a close Discord has
/// documented as fatal to the current config (bad token, disallowed/invalid
/// intents — see `protocol::NON_RESUMABLE_CLOSE_CODES`). The binding still
/// never gives up outright (mirroring email/telegram's "never return except
/// on cancel" contract — the fix might be a token rotation or an intent
/// approval landing in the Discord developer portal, which this task has no
/// way to observe directly), but retrying a config Discord has explicitly
/// rejected every [`ERROR_BACKOFF`] would hammer their API with an identical
/// doomed `IDENTIFY` far more aggressively than a transient-failure retry
/// warrants.
const FATAL_CLOSE_BACKOFF: Duration = Duration::from_secs(60);

/// Gateway session state that must survive across a reconnect within one
/// binding's lifetime — cleared or replaced as connections come and go,
/// never touched by the pure logic in `protocol`/`security`/`session`.
#[derive(Default)]
struct GatewayConnectionState {
    session_id: Option<String>,
    resume_gateway_url: Option<String>,
    last_seq: Option<u64>,
    own_user_id: Option<String>,
}

impl GatewayConnectionState {
    /// Whether enough state is on hand to attempt a `RESUME` instead of a
    /// fresh `IDENTIFY`. The single source of truth for that decision — both
    /// the outer loop's connect-URL choice and [`send_identify_or_resume`]'s
    /// payload choice go through this rather than each re-deriving their own
    /// (potentially drifting) subset of the same three fields.
    fn can_resume(&self) -> bool {
        self.session_id.is_some() && self.resume_gateway_url.is_some() && self.last_seq.is_some()
    }

    /// Drops everything a `RESUME` would need, forcing the next connection
    /// to `IDENTIFY` fresh. Called on a non-resumable close or invalid
    /// session.
    fn clear_session(&mut self) {
        self.session_id = None;
        self.resume_gateway_url = None;
        self.last_seq = None;
    }
}

/// What [`run_connection`] decided when it returned.
enum ConnectionOutcome {
    /// `cancel` fired — the outer loop must return without reconnecting.
    Cancelled,
    /// The connection ended but the binding is still live — reconnect after
    /// the standard backoff.
    Reconnect,
    /// The connection ended on a close Discord documents as fatal to the
    /// current config (see [`FATAL_CLOSE_BACKOFF`]) — still reconnect (never
    /// give up outright), but after the longer backoff instead of hammering
    /// Discord with the same doomed request every [`ERROR_BACKOFF`].
    ReconnectAfterFatalClose,
    /// The binding itself is gone (disabled, deleted, agent deleted) — the
    /// outer loop must return without reconnecting. Distinct from
    /// `Cancelled` only in *why* we're stopping, mirroring email/telegram's
    /// early returns for the same conditions.
    Stop,
}

/// The outer loop `DiscordTransport::spawn` runs. Never returns except on
/// `cancel` or the binding disappearing — see the module doc.
pub(super) async fn run_discord_gateway_loop(
    ctx: ChannelRunContext,
    token: String,
    http: reqwest::Client,
    in_flight: Arc<InFlightChannels>,
    channel_meta_cache: ChannelMetaCache,
    engagement: Arc<EngagementTracker>,
    cancel: CancellationToken,
) {
    let mut state = GatewayConnectionState::default();
    let mut seen_ids = SeenMessageIds::new(SEEN_IDS_CAPACITY);

    // Restore the durable cursor persisted by a previous process (see
    // `persist_cursor` below), so a backend restart doesn't let a replayed
    // `MESSAGE_CREATE` (or, in the future, any other re-delivery mechanism)
    // get processed a second time. `session_id`/`seq` are restored too, but
    // `resume_gateway_url` deliberately isn't persisted (it's short-lived
    // and tied to one specific connection) — so `state.can_resume()` is
    // false right after a restart regardless, and this process always
    // `IDENTIFY`s fresh rather than attempting a `RESUME` across a restart.
    // The persisted `seen_ids` is what actually protects against
    // re-processing here, not the session fields.
    match ctx.persistence.channel_cursors.get(&ctx.agent_id, &ctx.binding_id).await {
        Ok(Some(ChannelCursor::Discord { seen_message_ids, session_id, seq })) => {
            seen_ids = SeenMessageIds::from_snapshot(&seen_message_ids, SEEN_IDS_CAPACITY);
            state.session_id = session_id;
            state.last_seq = seq;
        }
        Ok(Some(other)) => {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, ?other, "DiscordTransport: persisted cursor is not a Discord cursor, starting fresh");
        }
        Ok(None) => {}
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "DiscordTransport: failed to load persisted cursor, starting fresh: {e}");
        }
    }

    loop {
        // Re-read the agent's profile every reconnect (mirroring
        // `run_email_poll_loop`/`run_bot_poll_loop`'s re-read-before-dispatch
        // pattern) so a mid-flight config change — disabled, removed,
        // allow-list edits — takes effect without a supervisor restart.
        let profile = match ctx.persistence.agents.get(&ctx.agent_id).await {
            Ok(Some(profile)) => profile,
            Ok(None) => {
                debug!(agent_id = %ctx.agent_id, "DiscordTransport: agent no longer exists, stopping gateway task");
                return;
            }
            Err(e) => {
                warn!(agent_id = %ctx.agent_id, "DiscordTransport: failed to re-read agent profile: {e}");
                if wait_or_cancelled(&cancel, ERROR_BACKOFF).await {
                    return;
                }
                continue;
            }
        };
        let Some(binding) = profile.channels.iter().find(|b| b.binding_id == ctx.binding_id) else {
            debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "DiscordTransport: binding removed, stopping gateway task");
            return;
        };
        if !binding.enabled {
            debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "DiscordTransport: binding disabled, stopping gateway task");
            return;
        }
        // No `bridge_thread_id` readiness check here anymore: Discord mints
        // a fresh per-conversation thread on demand for every distinct
        // `channel_id` it sees (see `resolve_discord_conversation_thread`)
        // instead of routing every conversation through one
        // eagerly-provisioned thread, so this binding has nothing to wait
        // on before it can start listening. A binding provisioned before
        // this change may still carry a legacy `bridge_thread_id` — its
        // thread stays viewable, but no new inbound message is ever routed
        // there again (migration leaves it as-is, never
        // reassigned).
        let ChannelKindConfig::Discord { allowed_users, allowed_roles, .. } = &binding.kind_config else {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "DiscordTransport: binding kind_config is not Discord, stopping gateway task");
            return;
        };
        let intents = protocol::compute_intents(security::needs_members_intent(allowed_users, allowed_roles));

        let connect_url = if state.can_resume() {
            state.resume_gateway_url.clone().expect("can_resume() implies Some")
        } else {
            protocol::DEFAULT_GATEWAY_URL.to_string()
        };

        let mut seam = TungsteniteGatewaySeam::new();
        let connected = tokio::select! {
            _ = cancel.cancelled() => return,
            result = seam.connect(&connect_url) => result,
        };
        if let Err(e) = connected {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "DiscordTransport: gateway connect failed: {e}");
            ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Reconnecting);
            if wait_or_cancelled(&cancel, ERROR_BACKOFF).await {
                return;
            }
            continue;
        }

        let outcome = run_connection(
            &mut seam,
            &mut state,
            &mut seen_ids,
            &token,
            intents,
            &ctx,
            &in_flight,
            &http,
            &channel_meta_cache,
            &engagement,
            &cancel,
        )
        .await;

        // Mandatory: never leave a socket un-closed before the next
        // connect — an un-closed-but-still-alive socket risks delivering
        // one more (duplicate) frame after we've already moved on.
        if let Err(e) = seam.close().await {
            debug!(agent_id = %ctx.agent_id, "DiscordTransport: error closing gateway socket (likely already gone): {e}");
        }

        let backoff = match outcome {
            ConnectionOutcome::Cancelled | ConnectionOutcome::Stop => return,
            ConnectionOutcome::Reconnect => ERROR_BACKOFF,
            ConnectionOutcome::ReconnectAfterFatalClose => FATAL_CLOSE_BACKOFF,
        };
        ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Reconnecting);

        if wait_or_cancelled(&cancel, backoff).await {
            return;
        }
    }
}

/// Drives one open connection: `HELLO` -> `IDENTIFY`/`RESUME`, heartbeats,
/// and dispatch handling, until the socket dies, the connection is judged a
/// zombie, or `cancel` fires. Never closes the socket itself — see the
/// module doc on why that's centralized in the outer loop.
#[allow(clippy::too_many_arguments)]
async fn run_connection(
    seam: &mut TungsteniteGatewaySeam,
    state: &mut GatewayConnectionState,
    seen_ids: &mut SeenMessageIds,
    token: &str,
    intents: u32,
    ctx: &ChannelRunContext,
    in_flight: &InFlightChannels,
    http: &reqwest::Client,
    channel_meta_cache: &ChannelMetaCache,
    engagement: &EngagementTracker,
    cancel: &CancellationToken,
) -> ConnectionOutcome {
    let mut heartbeats = HeartbeatTracker::new();

    loop {
        let event = tokio::select! {
            _ = cancel.cancelled() => return ConnectionOutcome::Cancelled,
            result = seam.next_event(state.last_seq) => result,
        };

        let event = match event {
            Ok(event) => event,
            Err(GatewaySeamError::ClosedByPeer { code }) => {
                warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, ?code, "DiscordTransport: gateway connection closed by peer");
                let is_fatal = match code {
                    Some(code) if !protocol::is_resumable_close_code(code) => {
                        state.clear_session();
                        true
                    }
                    _ => false,
                };
                return if is_fatal { ConnectionOutcome::ReconnectAfterFatalClose } else { ConnectionOutcome::Reconnect };
            }
            Err(e) => {
                warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "DiscordTransport: gateway read failed: {e}");
                return ConnectionOutcome::Reconnect;
            }
        };

        match event {
            GatewayEvent::Hello { heartbeat_interval_ms } => {
                seam.arm_heartbeat(Duration::from_millis(heartbeat_interval_ms));
                if send_identify_or_resume(seam, state, token, intents).await.is_err() {
                    warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "DiscordTransport: failed to send identify/resume");
                    return ConnectionOutcome::Reconnect;
                }
            }
            GatewayEvent::HeartbeatSent => {
                if heartbeats.on_heartbeat_sent() {
                    warn!(
                        agent_id = %ctx.agent_id,
                        binding_id = %ctx.binding_id,
                        "DiscordTransport: heartbeat zombie detected (previous beat never acked), reconnecting"
                    );
                    return ConnectionOutcome::Reconnect;
                }
            }
            GatewayEvent::HeartbeatAck => heartbeats.on_ack(),
            GatewayEvent::HeartbeatRequest => {
                let payload = protocol::heartbeat_payload(state.last_seq);
                if seam.send_json(&payload).await.is_err() {
                    return ConnectionOutcome::Reconnect;
                }
                // Same zombie check as the regular `HeartbeatSent` path —
                // an out-of-cycle server-requested beat is still a beat, and
                // must not let a zombie connection go unnoticed just because
                // this particular send was server-triggered.
                if heartbeats.on_heartbeat_sent() {
                    warn!(
                        agent_id = %ctx.agent_id,
                        binding_id = %ctx.binding_id,
                        "DiscordTransport: heartbeat zombie detected (previous beat never acked), reconnecting"
                    );
                    return ConnectionOutcome::Reconnect;
                }
            }
            GatewayEvent::Reconnect => {
                debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "DiscordTransport: gateway requested a reconnect");
                return ConnectionOutcome::Reconnect;
            }
            GatewayEvent::InvalidSession { resumable } => {
                if !resumable {
                    state.clear_session();
                }
                if wait_or_cancelled(cancel, invalid_session_delay()).await {
                    return ConnectionOutcome::Cancelled;
                }
                if send_identify_or_resume(seam, state, token, intents).await.is_err() {
                    return ConnectionOutcome::Reconnect;
                }
            }
            GatewayEvent::Dispatch { seq, kind } => {
                state.last_seq = Some(seq);
                match kind {
                    DispatchKind::Ready { session_id, resume_gateway_url, own_user_id } => {
                        debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "DiscordTransport: gateway READY");
                        state.session_id = Some(session_id);
                        state.resume_gateway_url = Some(resume_gateway_url);
                        state.own_user_id = Some(own_user_id);
                        ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Connected);
                        persist_cursor(ctx, state, seen_ids).await;
                    }
                    DispatchKind::Resumed => {
                        debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "DiscordTransport: gateway RESUMED");
                    }
                    DispatchKind::MessageCreate(msg) => {
                        if let Some(outcome) = handle_message_create(
                            msg,
                            state,
                            seen_ids,
                            ctx,
                            in_flight,
                            http,
                            channel_meta_cache,
                            engagement,
                            token,
                        )
                        .await
                        {
                            return outcome;
                        }
                    }
                    DispatchKind::Other => {}
                }
            }
            GatewayEvent::Unknown => {}
        }
    }
}

/// Sends `RESUME` when [`GatewayConnectionState::can_resume`] says a
/// still-usable session is on hand, `IDENTIFY` otherwise. Used both right
/// after `HELLO` and after a resumable `InvalidSession`'s reconnect delay.
async fn send_identify_or_resume(
    seam: &mut TungsteniteGatewaySeam,
    state: &GatewayConnectionState,
    token: &str,
    intents: u32,
) -> Result<(), GatewaySeamError> {
    let payload = if state.can_resume() {
        let session_id = state.session_id.as_deref().expect("can_resume() implies Some");
        let seq = state.last_seq.expect("can_resume() implies Some");
        protocol::resume_payload(token, session_id, seq)
    } else {
        protocol::identify_payload(token, intents)
    };
    seam.send_json(&payload).await
}

/// A short random delay (1-5s) before retrying `IDENTIFY`/`RESUME` after
/// `op9 InvalidSession`, per the Gateway spec's guidance to avoid hammering
/// straight back into another rejection.
fn invalid_session_delay() -> Duration {
    let span_ms = INVALID_SESSION_MAX_DELAY_MS - INVALID_SESSION_MIN_DELAY_MS;
    Duration::from_millis(INVALID_SESSION_MIN_DELAY_MS + (super::jitter_unit() * span_ms as f64) as u64)
}

/// Handles one already-deduped-or-not `MESSAGE_CREATE`, then persists the
/// durable cursor (`seen_ids` + `state.session_id`/`state.last_seq`) exactly
/// once, after the message has been fully handled — whatever the outcome
/// (dropped as a duplicate, unauthorized, or delivered). Persisting only
/// after full handling (rather than immediately on the dedup insert) means
/// the worst case on a crash mid-handling is that this one message gets
/// re-processed on the next connection — bounded to a single message, never
/// unbounded — rather than a delivered message silently failing to persist
/// as seen. Returns `Some` only when the binding has vanished out from under
/// this connection (the caller must stop entirely); `None` covers every
/// other outcome.
#[allow(clippy::too_many_arguments)]
async fn handle_message_create(
    msg: MessageCreateEvent,
    state: &GatewayConnectionState,
    seen_ids: &mut SeenMessageIds,
    ctx: &ChannelRunContext,
    in_flight: &InFlightChannels,
    http: &reqwest::Client,
    channel_meta_cache: &ChannelMetaCache,
    engagement: &EngagementTracker,
    token: &str,
) -> Option<ConnectionOutcome> {
    if !seen_ids.insert_is_new(&msg.id) {
        debug!(agent_id = %ctx.agent_id, message_id = %msg.id, "DiscordTransport: dropping a RESUME-replayed message already delivered");
        return None;
    }

    let outcome = handle_message_create_inner(
        msg,
        state,
        ctx,
        in_flight,
        http,
        channel_meta_cache,
        engagement,
        token,
    )
    .await;
    persist_cursor(ctx, state, seen_ids).await;
    outcome
}

/// The part of [`handle_message_create`] specific to one message: identity
/// checks, allow-list authorization, the engagement (mention/warm-thread)
/// gate, history backfill, and delivery. Split out so the caller can
/// persist the cursor exactly once after this returns, regardless of which
/// of this function's early returns was taken.
///
/// Two independent gates run in a fixed order: [`security::is_allowed`]
/// (authorization) always runs first and is fail-closed — nothing below it
/// can ever widen who gets through. [`EngagementTracker::decide`]
/// (engagement — is this a message the bot should actually respond to,
/// given mention/warm-thread state) only ever narrows further, on top of an
/// already-authorized message; it never runs, and never could run, before
/// authorization has passed.
#[allow(clippy::too_many_arguments)]
async fn handle_message_create_inner(
    msg: MessageCreateEvent,
    state: &GatewayConnectionState,
    ctx: &ChannelRunContext,
    in_flight: &InFlightChannels,
    http: &reqwest::Client,
    channel_meta_cache: &ChannelMetaCache,
    engagement: &EngagementTracker,
    token: &str,
) -> Option<ConnectionOutcome> {
    let Some(own_user_id) = state.own_user_id.as_deref() else {
        warn!(agent_id = %ctx.agent_id, "DiscordTransport: MESSAGE_CREATE arrived before READY, dropping");
        return None;
    };
    if security::should_ignore_author(&msg.author.id, msg.author.bot, own_user_id) {
        return None;
    }

    // Re-read the agent profile fresh so a mid-connection allow-list edit
    // takes effect on the very next message.
    let profile = match ctx.persistence.agents.get(&ctx.agent_id).await {
        Ok(Some(profile)) => profile,
        Ok(None) => return Some(ConnectionOutcome::Stop),
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, "DiscordTransport: failed to re-read agent profile: {e}");
            return None;
        }
    };
    let Some(binding) = profile.channels.iter().find(|b| b.binding_id == ctx.binding_id) else {
        return Some(ConnectionOutcome::Stop);
    };
    if !binding.enabled {
        return Some(ConnectionOutcome::Stop);
    }
    let ChannelKindConfig::Discord {
        allowed_users,
        allowed_roles,
        allowed_channels,
        dm_role_auth_guild,
        require_mention,
        thread_follow,
        thread_idle_timeout_minutes,
        thread_message_budget,
        backfill_limit,
    } = &binding.kind_config
    else {
        return Some(ConnectionOutcome::Stop);
    };

    let is_dm = msg.guild_id.is_none();
    let role_auth_enabled = security::role_auth_enabled(is_dm, dm_role_auth_guild.as_deref());

    let member_roles: Vec<String> = if is_dm {
        if role_auth_enabled && !allowed_roles.is_empty() {
            let guild_id = dm_role_auth_guild.as_deref().expect("role_auth_enabled(true, _) implies Some");
            resolve_dm_member_roles(http, token, guild_id, &msg.author.id).await
        } else {
            Vec::new()
        }
    } else {
        msg.member.as_ref().map(|m| m.roles.clone()).unwrap_or_default()
    };

    // Resolved ahead of the authorization check itself: a Discord thread
    // carries its own channel id, distinct from its parent's, so
    // `security::is_allowed`'s channel allow-list check needs
    // `is_thread`/`parent_id` on hand to fall back to the parent when a
    // thread's own id isn't the one listed.
    let meta = resolve_channel_meta(http, token, channel_meta_cache, &msg.channel_id).await;

    let auth_ctx = security::AuthContext {
        author_id: &msg.author.id,
        author_username: &msg.author.username,
        is_dm,
        channel_id: &msg.channel_id,
        member_roles: &member_roles,
        role_auth_enabled,
        is_thread: meta.is_thread,
        parent_channel_id: meta.parent_id.as_deref(),
    };
    if !security::is_allowed(&auth_ctx, allowed_users, allowed_roles, allowed_channels) {
        debug!(agent_id = %ctx.agent_id, author_id = %msg.author.id, "DiscordTransport: dropping unauthorized inbound message");
        return None;
    }

    // From here on, the message is authorized — everything below only ever
    // narrows further (the engagement gate) or enriches the text handed to
    // the agent (backfill), never widens who gets through.
    let mentioned_user_ids: Vec<String> = msg.mentions.iter().map(|m| m.id.clone()).collect();
    let mention_ctx = security::MentionContext {
        own_user_id: Some(own_user_id),
        mentioned_user_ids: &mentioned_user_ids,
        content: &msg.content,
    };
    // `require_mention: false` is the escape hatch documented on
    // `ChannelKindConfig::Discord` — every message counts as mentioned for
    // engagement purposes, reverting a guild channel to respond-to-everyone.
    // Short-circuits before calling `is_bot_mentioned` at all in that case.
    let bot_mentioned = !require_mention || security::is_bot_mentioned(&mention_ctx);

    let engagement_input = EngagementInput {
        conversation_id: &msg.channel_id,
        is_dm,
        is_thread: meta.is_thread,
        thread_owner_is_bot: meta.owner_id.as_deref() == state.own_user_id.as_deref(),
        bot_mentioned,
        now: Utc::now(),
    };
    let engagement_params = EngagementParams {
        idle_timeout: Duration::from_secs(u64::from(*thread_idle_timeout_minutes) * 60),
        message_budget: *thread_message_budget,
        thread_follow: *thread_follow,
    };
    let engagement_outcome = engagement.decide(&engagement_input, &engagement_params);
    if engagement_outcome.decision == EngagementDecision::Ignore {
        debug!(
            agent_id = %ctx.agent_id,
            channel_id = %msg.channel_id,
            "DiscordTransport: engagement gate declined to respond, dropping"
        );
        return None;
    }

    // Fires at most once per conversation on the COLD->WARM transition
    // (thread backfill), or once per triggering mention that lands as a
    // reply outside a thread (reply-chain backfill) — never on every
    // message. Either fetch's own failure path already resolves to an empty
    // result, so `backfill_block` is simply empty (no-op) rather than ever
    // blocking delivery. `backfill_limit == 0` disables both modes outright,
    // per the binding's own config.
    let backfill_block = if *backfill_limit == 0 {
        String::new()
    } else if engagement_outcome.became_warm {
        let messages = fetch_thread_backfill(http, token, &msg.channel_id, &msg.id, *backfill_limit).await;
        format_backfill(&messages)
    } else if !meta.is_thread && bot_mentioned {
        match msg.message_reference.as_ref() {
            Some(reference) => {
                let messages = fetch_reply_chain_backfill(http, token, &msg.channel_id, reference).await;
                format_backfill(&messages)
            }
            None => String::new(),
        }
    } else {
        String::new()
    };
    let text =
        if backfill_block.is_empty() { msg.content.clone() } else { format!("{backfill_block}\n\n{}", msg.content) };

    // Resolved only now, after every narrowing gate above (auth,
    // engagement) has already passed — mirrors
    // `slack::runner::resolve_bridge_thread`'s placement, so a dropped or
    // unauthorized message never mints a per-conversation thread it will
    // never use.
    let Some(thread_id) = resolve_discord_conversation_thread(ctx, &msg.channel_id, Utc::now()).await else {
        warn!(agent_id = %ctx.agent_id, channel_id = %msg.channel_id, "DiscordTransport: failed to resolve a per-conversation bridge thread, dropping message");
        return None;
    };

    in_flight.record(&thread_id, msg.channel_id.clone(), ctx.binding_id.clone(), is_dm);
    // Title from the actual current message (`msg.content`), never `text` —
    // `text` may carry a backfill block prefixed ahead of it above, and that
    // block isn't this message's own content.
    if let Err(e) = submit_inbound_message(
        ctx,
        &profile,
        &thread_id,
        ChannelKind::Discord,
        &msg.channel_id,
        &msg.author.id,
        Some(msg.author.username.as_str()),
        &text,
        Some(clean_discord_markup(&msg.content)),
    )
    .await
    {
        warn!(agent_id = %ctx.agent_id, "DiscordTransport: failed to deliver inbound message: {e}");
    }
    None
}

/// Resolves the conversation→thread registry row for a Discord
/// `channel_id`, lazily minting a fresh Launchpad bridge thread on first
/// contact — the Discord analogue of
/// [`super::super::slack::runner::resolve_bridge_thread`], but keyed on
/// Discord's own per-conversation id rather than Slack's
/// `(team_id, channel, thread_ts)` triple: a DM and a guild channel/thread
/// are already each their own distinct `channel_id`
/// (`super::channel_meta`), so no composite key is needed.
///
/// Runs the gc-and-release pass for this binding first, so an
/// idle-evicted conversation's `LeaseGate` state clears before this
/// message's own resolve/mint runs — if this message's own conversation was
/// the one just evicted, it simply re-mints a fresh thread below, exactly
/// as a returning sender safely would. Returns `None` (logged) only on a
/// persistence failure — a message this connection cannot resolve a thread
/// for is dropped rather than mis-routed.
///
/// A freshly minted thread is left untitled (`title: None`), exactly like
/// Slack's own fresh-mint branch: the caller's own `submit_inbound_message`
/// call already derives `auto_title` from this same message's cleaned
/// content generically (see that function's doc), so no special-cased
/// "is this the first message" titling logic is needed here.
async fn resolve_discord_conversation_thread(
    ctx: &ChannelRunContext,
    channel_id: &str,
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
        warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "DiscordTransport: conversation registry gc failed: {e}");
    }

    let key = ConversationKey::new(channel_id);
    let mut minted_thread: Option<Thread> = None;
    let mint = || {
        let mut thread = ctx.persistence.threads.build_fresh_thread(&ctx.agent_id, None);
        thread.channel_origin =
            Some(ChannelBridgeOrigin { kind: ChannelKind::Discord, binding_id: ctx.binding_id.clone() });
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
            warn!(agent_id = %ctx.agent_id, "DiscordTransport: failed to read the conversation registry: {e}");
            return None;
        }
    };

    if let Some(thread) = minted_thread {
        if let Err(e) = ctx.persistence.threads.create(thread.clone()).await {
            warn!(agent_id = %ctx.agent_id, "DiscordTransport: failed to create a per-conversation bridge thread: {e}");
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
    // `slack::runner::resolve_bridge_thread`'s same reasoning.
    ctx.lease_gate.mark_active(&ctx.binding_id, &row.thread_id);
    Some(row.thread_id)
}

/// Fetches `user_id`'s roles in `guild_id` via the REST API — the only way
/// to resolve a DM author's guild roles, since a DM's `MESSAGE_CREATE` has
/// no inline `member` object (a DM has no guild of its own to be a member
/// of). Returns an empty list on any failure (network error, non-2xx,
/// unparseable body) rather than propagating one — an unresolved lookup
/// fails role auth closed for this message rather than blocking delivery
/// outright; the caller still authorizes on `allowed_users` alone.
async fn resolve_dm_member_roles(http: &reqwest::Client, token: &str, guild_id: &str, user_id: &str) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct MemberResponse {
        #[serde(default)]
        roles: Vec<String>,
    }

    let url = format!("https://discord.com/api/v10/guilds/{guild_id}/members/{user_id}");
    let response = match http.get(&url).header("Authorization", format!("Bot {token}")).send().await {
        Ok(response) => response,
        Err(e) => {
            warn!("DiscordTransport: guild member lookup failed: {e}");
            return Vec::new();
        }
    };
    if !response.status().is_success() {
        warn!(status = %response.status(), "DiscordTransport: guild member lookup returned a non-success status");
        return Vec::new();
    }
    match response.json::<MemberResponse>().await {
        Ok(body) => body.roles,
        Err(e) => {
            warn!("DiscordTransport: guild member lookup response did not parse: {e}");
            Vec::new()
        }
    }
}

/// Persists the current dedup cursor (`seen_ids` + the session identifiers
/// `state` carries) for `(ctx.agent_id, ctx.binding_id)`, logging and
/// swallowing any store failure — a failed persist just means the next
/// crash's re-delivery window is slightly wider, never a hard error worth
/// tearing down the connection over.
async fn persist_cursor(ctx: &ChannelRunContext, state: &GatewayConnectionState, seen_ids: &SeenMessageIds) {
    let cursor = ChannelCursor::Discord {
        seen_message_ids: seen_ids.snapshot(),
        session_id: state.session_id.clone(),
        seq: state.last_seq,
    };
    if let Err(e) = ctx.persistence.channel_cursors.set(&ctx.agent_id, &ctx.binding_id, &cursor).await {
        warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "DiscordTransport: failed to persist cursor: {e}");
    }
}

/// Sleeps for `dur` unless `cancel` fires first. Returns `true` if cancelled.
async fn wait_or_cancelled(cancel: &CancellationToken, dur: Duration) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => true,
        _ = tokio::time::sleep(dur) => false,
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use ao_protocol::agent::{
        AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig, CliProviderConfig, InputMode, OutputFormat,
        ProviderConfig, ThreadFollowMode,
    };
    use ao_protocol::error::AoError;
    use ao_protocol::event::RunEndReason;

    use crate::agent_runner::{AgentRunRequest, AgentRunner, AgentRunnerMode, RunComplete, RunnerDispatcher};
    use crate::channels::connection_state::ConnectionStateRegistry;
    use crate::channels::relay::lease_gate::LeaseGate;
    use crate::event_bus::EventBus;
    use crate::instance_registry::InstanceRegistry;
    use crate::queue_manager::QueueManagerRegistry;

    use super::super::protocol::MessageAuthor;
    use super::*;

    const AGENT_ID: &str = "agent-discord-test";
    const BINDING_ID: &str = "discord";
    // Author of the inbound DMs below — a DM's engagement/auth path never
    // needs a resolvable guild, so a fixed snowflake-shaped id is enough.
    const AUTHOR_ID: &str = "222222222222222222";

    /// A stub [`AgentRunner`] that completes immediately without spawning a
    /// real process — same pattern `slack::runner`'s own test module uses so
    /// `submit_inbound_message`'s real `QueueManagerRegistry` path can run
    /// inside a unit test.
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

    /// Shared backing state for one test's [`ChannelRunContext`] — mirrors
    /// `slack::runner::tests::TestHarness`.
    struct TestHarness {
        persistence: Arc<ao_persistence::PersistenceLayer>,
        event_bus: Arc<EventBus>,
        queue_registry: Arc<QueueManagerRegistry>,
        connection_state: Arc<ConnectionStateRegistry>,
        lease_gate: Arc<LeaseGate>,
        _tmp: tempfile::TempDir,
    }

    impl TestHarness {
        fn ctx(&self) -> ChannelRunContext {
            ChannelRunContext {
                agent_id: AGENT_ID.to_string(),
                binding_id: BINDING_ID.to_string(),
                persistence: Arc::clone(&self.persistence),
                queue_registry: Arc::clone(&self.queue_registry),
                connection_state: Arc::clone(&self.connection_state),
                lease_gate: Arc::clone(&self.lease_gate),
                event_bus: Arc::clone(&self.event_bus),
            }
        }
    }

    async fn make_test_harness(agent: AgentProfile) -> TestHarness {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
        let persistence =
            Arc::new(ao_persistence::PersistenceLayer::init_with_root(data_root).await.expect("init persistence"));
        persistence.agents.create(&agent).await.expect("create agent");

        let event_bus = Arc::new(EventBus::new(64));
        let instance_registry = Arc::new(InstanceRegistry::new());
        let runner: Arc<dyn AgentRunner> = Arc::new(StubRunner);
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(Arc::clone(&runner), runner));
        let queue_registry = Arc::new(QueueManagerRegistry::new(
            dispatcher,
            instance_registry,
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
        ));
        let connection_state = Arc::new(ConnectionStateRegistry::new());
        let lease_gate = Arc::new(LeaseGate::new());

        TestHarness { persistence, event_bus, queue_registry, connection_state, lease_gate, _tmp: tmp }
    }

    /// A DM-capable Discord binding: `allowed_users` names [`AUTHOR_ID`]
    /// directly (no role resolution, no channel allow-list — a DM skips the
    /// channel check entirely), and `require_mention: false` so engagement
    /// never depends on parsing a mention out of the test message text.
    /// `backfill_limit: 0` disables the history-backfill fetch outright, so
    /// the only network call this test's `handle_message_create_inner` call
    /// makes is the mandatory channel-meta lookup `resolve_channel_meta`
    /// always performs — production discord.com/api target, gracefully
    /// falling back to [`super::super::channel_meta::ChannelMeta::unresolved`]
    /// on any non-2xx/network failure, exactly as it would with no live
    /// bot token. Either outcome resolves to `is_thread: false`, which is
    /// what this DM scenario needs regardless.
    fn make_test_agent() -> AgentProfile {
        AgentProfile {
            id: AGENT_ID.to_string(),
            name: "Discord Test Agent".to_string(),
            description: String::new(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: Default::default(),
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
            env: Default::default(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            native_provider: None,
            thinking: None,
            enabled_plugins: Default::default(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: Default::default(),
            owning_team_id: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![ChannelBinding {
                binding_id: BINDING_ID.to_string(),
                kind: ChannelKind::Discord,
                enabled: true,
                bridge_thread_id: None,
                allowed_senders: vec![],
                pending_pairing_code: None,
                kind_config: ChannelKindConfig::Discord {
                    allowed_users: vec![AUTHOR_ID.to_string()],
                    allowed_roles: vec![],
                    allowed_channels: vec![],
                    dm_role_auth_guild: None,
                    require_mention: false,
                    thread_follow: ThreadFollowMode::Always,
                    thread_idle_timeout_minutes: 30,
                    thread_message_budget: 25,
                    backfill_limit: 0,
                },
            }],
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            max_turns: None,
}
    }

    fn make_dm_message_in_channel(id: &str, channel_id: &str, content: &str) -> MessageCreateEvent {
        MessageCreateEvent {
            id: id.to_string(),
            channel_id: channel_id.to_string(),
            guild_id: None,
            content: content.to_string(),
            author: MessageAuthor { id: AUTHOR_ID.to_string(), username: "alice".to_string(), bot: false },
            member: None,
            mentions: vec![],
            mention_everyone: false,
            mention_roles: vec![],
            message_reference: None,
        }
    }

    /// Shared plumbing every resolve test below needs — a `state` with an
    /// already-known `own_user_id` (so `handle_message_create_inner` never
    /// hits its own "arrived before READY" early return) plus empty
    /// in-flight/meta-cache/engagement trackers.
    struct ResolveDeps {
        state: GatewayConnectionState,
        in_flight: InFlightChannels,
        http: reqwest::Client,
        channel_meta_cache: ChannelMetaCache,
        engagement: EngagementTracker,
    }

    fn make_resolve_deps() -> ResolveDeps {
        ResolveDeps {
            state: GatewayConnectionState { own_user_id: Some("bot-id".to_string()), ..Default::default() },
            in_flight: InFlightChannels::new(),
            http: reqwest::Client::new(),
            channel_meta_cache: ChannelMetaCache::new(),
            engagement: EngagementTracker::new(),
        }
    }

    /// SHARE-WITHIN, combined with the pre-existing
    /// auto-title guarantee: repeated inbound messages on the *same*
    /// `channel_id` resolve to the *same* per-conversation thread, and only
    /// the first message's content ever seeds `auto_title`.
    #[tokio::test]
    async fn share_within_same_channel_reuses_the_same_thread_and_sets_auto_title_once() {
        let agent = make_test_agent();
        let harness = make_test_harness(agent).await;
        let ctx = harness.ctx();
        let deps = make_resolve_deps();

        let first = make_dm_message_in_channel("msg-1", "channel-a", "<@999999999999999999> please help with the deploy");
        handle_message_create_inner(
            first,
            &deps.state,
            &ctx,
            &deps.in_flight,
            &deps.http,
            &deps.channel_meta_cache,
            &deps.engagement,
            "fake-token",
        )
        .await;

        let row = ctx
            .persistence
            .conversation_registry
            .get(AGENT_ID, BINDING_ID, &ConversationKey::new("channel-a"))
            .await
            .expect("read registry")
            .expect("row exists after the first inbound message");
        let thread_id = row.thread_id.clone();

        let thread = ctx.persistence.threads.get(&thread_id).await.expect("read thread").expect("thread exists");
        assert_eq!(
            thread.auto_title.as_deref(),
            Some("please help with the deploy"),
            "auto_title must be derived from the first inbound message's cleaned content, mention markup dropped"
        );
        assert!(thread.title.is_none(), "auto-titling must never set the explicit, non-renamable `title` field");

        let second = make_dm_message_in_channel("msg-2", "channel-a", "totally unrelated follow-up text");
        handle_message_create_inner(
            second,
            &deps.state,
            &ctx,
            &deps.in_flight,
            &deps.http,
            &deps.channel_meta_cache,
            &deps.engagement,
            "fake-token",
        )
        .await;

        let row_again = ctx
            .persistence
            .conversation_registry
            .get(AGENT_ID, BINDING_ID, &ConversationKey::new("channel-a"))
            .await
            .expect("read registry")
            .expect("row still exists");
        assert_eq!(row_again.thread_id, thread_id, "the same channel_id must reuse the same per-conversation thread");

        let thread = ctx.persistence.threads.get(&thread_id).await.expect("read thread").expect("thread exists");
        assert_eq!(
            thread.auto_title.as_deref(),
            Some("please help with the deploy"),
            "a later message must never overwrite the auto_title set from the first one"
        );
    }

    /// ISOLATE-ACROSS: two distinct
    /// `channel_id`s mint two distinct threads, and the second conversation's
    /// transcript never carries the first's content — the actual security
    /// guarantee this whole phase exists to prove.
    #[tokio::test]
    async fn isolate_across_different_channels_mint_distinct_threads_with_no_shared_context() {
        let agent = make_test_agent();
        let harness = make_test_harness(agent).await;
        let ctx = harness.ctx();
        let deps = make_resolve_deps();

        let joans_secret = "joans-secret-token-12345";
        let joan = make_dm_message_in_channel("msg-joan", "channel-joan", joans_secret);
        handle_message_create_inner(
            joan,
            &deps.state,
            &ctx,
            &deps.in_flight,
            &deps.http,
            &deps.channel_meta_cache,
            &deps.engagement,
            "fake-token",
        )
        .await;

        let mathew = make_dm_message_in_channel("msg-mathew", "channel-mathew", "hey, what's up?");
        handle_message_create_inner(
            mathew,
            &deps.state,
            &ctx,
            &deps.in_flight,
            &deps.http,
            &deps.channel_meta_cache,
            &deps.engagement,
            "fake-token",
        )
        .await;

        let joan_row = ctx
            .persistence
            .conversation_registry
            .get(AGENT_ID, BINDING_ID, &ConversationKey::new("channel-joan"))
            .await
            .expect("read registry")
            .expect("joan's row exists");
        let mathew_row = ctx
            .persistence
            .conversation_registry
            .get(AGENT_ID, BINDING_ID, &ConversationKey::new("channel-mathew"))
            .await
            .expect("read registry")
            .expect("mathew's row exists");

        assert_ne!(joan_row.thread_id, mathew_row.thread_id, "different channel_ids must mint distinct threads");

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
    /// `LeaseGate` from Discord's own dispatch, not rely on
    /// `ChannelBridge::reconcile`'s placeholder — mirrors the equivalent
    /// Slack guarantee for `resolve_bridge_thread`.
    #[tokio::test]
    async fn resolving_a_conversation_marks_its_thread_active_in_the_lease_gate() {
        let agent = make_test_agent();
        let harness = make_test_harness(agent).await;
        let ctx = harness.ctx();
        let deps = make_resolve_deps();

        let msg = make_dm_message_in_channel("msg-1", "channel-lease", "hello");
        handle_message_create_inner(
            msg,
            &deps.state,
            &ctx,
            &deps.in_flight,
            &deps.http,
            &deps.channel_meta_cache,
            &deps.engagement,
            "fake-token",
        )
        .await;

        let row = ctx
            .persistence
            .conversation_registry
            .get(AGENT_ID, BINDING_ID, &ConversationKey::new("channel-lease"))
            .await
            .expect("read registry")
            .expect("row exists");

        assert!(ctx.lease_gate.is_active(&row.thread_id), "resolving a conversation must mark its thread active");
    }
}
