//! The Socket Mode connect/dispatch/reconnect loop — Slack's analogue of
//! `crate::channels::discord::runner::run_discord_gateway_loop`, but a
//! materially different shape because Socket Mode's protocol is materially
//! different: no resumable session, no client-driven heartbeat, and no
//! backfill/engagement layer. A dead connection always
//! reconnects from scratch — `connections.open` mints a fresh one-shot URL
//! every time, there is nothing to resume.
//!
//! # Warm rotation (no in-repo precedent)
//!
//! Slack proactively rotates a Socket Mode connection roughly every hour —
//! and ahead of a forced close too — by sending a `disconnect` envelope
//! (`refresh_requested` or `warning`) a short while before the socket
//! actually drops. Reacting to that by closing the
//! current socket and then reconnecting leaves a gap: any envelope Slack
//! delivers between "you're about to lose me" and "the old socket is
//! actually gone" has nowhere to land. The fix carried out here is to
//! briefly hold **two** sockets rather than one:
//!
//! 1. **Steady state** is a single `active` socket, exactly like a fresh
//!    connect.
//! 2. A `disconnect` envelope on `active` does **not** close it. Instead a
//!    second socket (`incoming`) is opened via the same seam's
//!    `connections.open` call, and both sockets are read concurrently.
//! 3. Every envelope off *either* socket runs through the **same** single
//!    [`SeenEventIds`] instance. This is what makes the overlap safe, not
//!    ack timing: if Slack redelivers one event on both sockets during the
//!    handover, the second copy is simply a duplicate and is dropped by
//!    dedup like any other redelivery. The ack itself is still sent back
//!    on whichever socket actually delivered the envelope, immediately.
//! 4. The moment `incoming` delivers its own `hello`, it is promoted:
//!    it becomes the new `active`, and the previous `active` becomes the
//!    one being retired.
//! 5. The retired socket is **grace-drained** — kept open and still
//!    read+acked for a short window, in case Slack had a little more
//!    in-flight traffic queued for it — then closed.
//! 6. If `incoming` never reaches `hello` (a failed connect, or it times
//!    out), it is discarded outright rather than promoted. Nothing about
//!    `active` changes in that case; the connection simply falls back to
//!    an ordinary reconnect-from-scratch whenever `active` itself
//!    eventually hard-closes.
//!
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use ao_protocol::agent::{ChannelKind, ChannelKindConfig};
use ao_protocol::channel_connection_state::ChannelConnectionState;
use ao_protocol::channel_cursor::ChannelCursor;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::slack_conversation_registry::SlackConversationRow;
use ao_protocol::thread::ChannelBridgeOrigin;

use crate::channels::relay::correlation_map::CorrelationMap;
use crate::channels::{submit_inbound_message, ChannelRunContext};

use super::filter::{self, Trigger};
use super::protocol::{self, DisconnectSeverity, SlackEvent, SocketModeEvent};
use super::security;
use super::session::SeenEventIds;
use super::socket_seam::{SlackSocketSeam, SlackSocketSeamError, SocketFrame};
use super::title;
use super::SlackOrigin;

/// Bound on the dedup set — comfortably larger than any realistic
/// redelivery window (a slow ack or a warm-rotation overlap only ever
/// replays a short recent backlog, never a connection's full history).
/// Mirrors `discord::runner::SEEN_IDS_CAPACITY`.
const SEEN_IDS_CAPACITY: usize = 4096;

/// First retry delay after a failed `connections.open`/socket connect.
const BASE_BACKOFF: Duration = Duration::from_secs(1);
/// Ceiling the doubling backoff never exceeds — keeps a persistently
/// unhealthy binding (revoked app token, Slack outage) from ever going
/// longer than a minute between attempts, while still not hot-looping.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Fixed pause after a transient failure to re-read the agent's own profile
/// — unrelated to socket connectivity, so it doesn't participate in the
/// connect backoff's doubling.
const PROFILE_READ_ERROR_BACKOFF: Duration = Duration::from_secs(5);
/// How long a retired (post-promotion) socket is kept open, still being
/// read and acked, before it is closed outright. A few seconds is enough
/// to drain whatever Slack had already queued for it without holding a
/// dead-weight connection open indefinitely.
const GRACE_DRAIN_WINDOW: Duration = Duration::from_secs(5);
/// How long a newly opened `incoming` socket is given to deliver its own
/// `hello` before it is discarded as a failed rotation attempt.
const INCOMING_HELLO_TIMEOUT: Duration = Duration::from_secs(20);

/// Produces one fresh, not-yet-connected socket seam — called once for the
/// initial connect, once per reconnect-from-scratch, and once more whenever a
/// warm rotation opens its second socket. A factory (rather than a bare
/// seam instance) because warm rotation needs to hold two live seams at
/// once, each independently connected via its own `connections.open` call.
/// Production wires this to `TungsteniteSlackSocketSeam::new`; tests wire it
/// to a scripted queue of [`super::socket_seam::FakeSlackSocketSeam`]s.
pub type SlackSeamFactory = Arc<dyn Fn() -> Box<dyn SlackSocketSeam> + Send + Sync>;

/// What one connection's lifetime (from a successful `connect` to its end)
/// decided the outer reconnect loop should do next.
enum ConnectionOutcome {
    Cancelled,
    /// The binding itself is gone (disabled, removed, agent deleted).
    Stop,
    Reconnect,
    /// `active` reported a disconnect reason Slack documents as fatal to
    /// the current credentials (`link_disabled`) — reconnecting with the
    /// same app token will just fail again, so this backs off at the
    /// ceiling rather than the doubling schedule's early, short delays.
    ReconnectAfterHardError,
}

/// Which role a secondary socket is currently playing alongside `active`.
#[derive(Clone, Copy)]
enum SecondaryRole {
    /// Just opened in reaction to a `disconnect` on `active`; waiting for
    /// its own `hello` before it can be promoted.
    Incoming,
    /// Already promoted away from; being grace-drained before it closes.
    Draining,
}

/// The second socket held alongside `active` during a warm rotation, plus
/// the deadline that ends its current role (the hello timeout for
/// [`SecondaryRole::Incoming`], the drain window for
/// [`SecondaryRole::Draining`]).
struct Secondary {
    seam: Box<dyn SlackSocketSeam>,
    role: SecondaryRole,
    deadline: tokio::time::Instant,
}

/// The outer "never return except on cancel or the binding disappearing"
/// loop. Restores the durable `event_id` cursor once up front, then
/// repeatedly connects (or reconnects) and hands the live connection to
/// [`run_connection`] until that returns a terminal outcome.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_slack_socket_mode_loop(
    ctx: ChannelRunContext,
    app_token: String,
    team_id: String,
    bot_user_id: String,
    in_flight: Arc<CorrelationMap<SlackOrigin>>,
    seam_factory: SlackSeamFactory,
    cancel: CancellationToken,
) {
    let mut seen_ids = SeenEventIds::new(SEEN_IDS_CAPACITY);
    match ctx.persistence.channel_cursors.get(&ctx.agent_id, &ctx.binding_id).await {
        Ok(Some(ChannelCursor::Slack { seen_event_ids })) => {
            seen_ids = SeenEventIds::from_snapshot(&seen_event_ids, SEEN_IDS_CAPACITY);
        }
        Ok(Some(other)) => {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, ?other, "SlackTransport: persisted cursor is not a Slack cursor, starting fresh");
        }
        Ok(None) => {}
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: failed to load persisted cursor, starting fresh: {e}");
        }
    }

    let mut attempt: u32 = 0;

    loop {
        let profile = match ctx.persistence.agents.get(&ctx.agent_id).await {
            Ok(Some(profile)) => profile,
            Ok(None) => {
                debug!(agent_id = %ctx.agent_id, "SlackTransport: agent no longer exists, stopping socket mode task");
                return;
            }
            Err(e) => {
                warn!(agent_id = %ctx.agent_id, "SlackTransport: failed to re-read agent profile: {e}");
                if wait_or_cancelled(&cancel, PROFILE_READ_ERROR_BACKOFF).await {
                    return;
                }
                continue;
            }
        };
        let Some(binding) = profile.channels.iter().find(|b| b.binding_id == ctx.binding_id) else {
            debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: binding removed, stopping socket mode task");
            return;
        };
        if !binding.enabled {
            debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: binding disabled, stopping socket mode task");
            return;
        }
        if !matches!(binding.kind_config, ChannelKindConfig::Slack { .. }) {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: binding kind_config is not Slack, stopping socket mode task");
            return;
        }

        let mut active = seam_factory();
        let connected = tokio::select! {
            _ = cancel.cancelled() => return,
            result = active.connect(&app_token) => result,
        };
        if let Err(e) = connected {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: socket connect failed: {e}");
            ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Reconnecting);
            attempt += 1;
            if wait_or_cancelled(&cancel, capped_exponential_backoff(attempt)).await {
                return;
            }
            continue;
        }
        attempt = 0;

        let outcome = run_connection(
            active,
            &mut seen_ids,
            &ctx,
            &app_token,
            &team_id,
            &bot_user_id,
            &in_flight,
            &seam_factory,
            &cancel,
        )
        .await;

        let backoff = match outcome {
            ConnectionOutcome::Cancelled | ConnectionOutcome::Stop => return,
            ConnectionOutcome::Reconnect => {
                attempt += 1;
                capped_exponential_backoff(attempt)
            }
            ConnectionOutcome::ReconnectAfterHardError => MAX_BACKOFF,
        };
        ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Reconnecting);
        if wait_or_cancelled(&cancel, backoff).await {
            return;
        }
    }
}

/// Drives one already-connected `active` socket through steady state and,
/// as needed, a full warm-rotation cycle (see the module doc), until the
/// connection needs a fresh reconnect-from-scratch, the binding disappears, or
/// `cancel` fires. Never closes `active` itself on the way out — mirroring
/// `discord::runner`, that's centralized in the caller so every exit path
/// (including ones added later) can't forget it.
#[allow(clippy::too_many_arguments)]
async fn run_connection(
    mut active: Box<dyn SlackSocketSeam>,
    seen_ids: &mut SeenEventIds,
    ctx: &ChannelRunContext,
    app_token: &str,
    team_id: &str,
    bot_user_id: &str,
    in_flight: &CorrelationMap<SlackOrigin>,
    seam_factory: &SlackSeamFactory,
    cancel: &CancellationToken,
) -> ConnectionOutcome {
    let mut secondary: Option<Secondary> = None;

    loop {
        // Computed up front, before `tokio::select!` constructs any branch
        // futures, specifically so the `sleep_until_secondary_deadline`
        // branch below never needs its own borrow of `secondary` — `Instant`
        // is `Copy`, so this carries no reference that could conflict with
        // the `recv_secondary` branch's `&mut secondary` in the same
        // `select!` statement (see that function's doc for why the two
        // would otherwise alias).
        let secondary_deadline = secondary.as_ref().map(|sec| sec.deadline);

        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = active.close().await;
                if let Some(mut sec) = secondary.take() {
                    let _ = sec.seam.close().await;
                }
                return ConnectionOutcome::Cancelled;
            }

            result = active.recv() => {
                match result {
                    Ok(SocketFrame::Text(text)) => {
                        match handle_frame(&text, &mut active, seen_ids, ctx, team_id, bot_user_id, in_flight).await {
                            FrameOutcome::Hello => {
                                ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Connected);
                            }
                            FrameOutcome::Disconnect(DisconnectSeverity::HardError) => {
                                warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: active socket reported a hard-error disconnect");
                                if let Some(mut sec) = secondary.take() {
                                    let _ = sec.seam.close().await;
                                }
                                return ConnectionOutcome::ReconnectAfterHardError;
                            }
                            FrameOutcome::Disconnect(DisconnectSeverity::RoutineRefresh | DisconnectSeverity::Warning) => {
                                if secondary.is_none() {
                                    if let Some(seam) = open_incoming(seam_factory, app_token, cancel).await {
                                        secondary = Some(Secondary {
                                            seam,
                                            role: SecondaryRole::Incoming,
                                            deadline: tokio::time::Instant::now() + INCOMING_HELLO_TIMEOUT,
                                        });
                                    }
                                    // A failed `open_incoming` just means no rotation happens this
                                    // time — `active` keeps running and Slack's own eventual hard
                                    // close will drive a normal reconnect-from-scratch.
                                } else {
                                    debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: ignoring an extra disconnect while a rotation is already in flight");
                                }
                            }
                            FrameOutcome::Stop => return ConnectionOutcome::Stop,
                            FrameOutcome::Continue => {}
                        }
                    }
                    Err(e) => {
                        warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: active socket read failed: {e}");
                        match secondary.take() {
                            Some(Secondary { seam, role: SecondaryRole::Incoming, .. }) => {
                                // The old socket died before the rotation finished; fall back to
                                // treating the not-yet-promoted incoming socket as our only one.
                                // It hasn't said hello yet, so the next iteration's Hello arm
                                // still needs to fire before it's considered ready. The dead old
                                // socket must still be closed here -- it's about to be dropped by
                                // the `active = seam` reassignment below, and nothing else on this
                                // path would otherwise call `close()` on it.
                                let _ = active.close().await;
                                active = seam;
                            }
                            Some(Secondary { mut seam, role: SecondaryRole::Draining, .. }) => {
                                // The just-promoted active died with an old socket still
                                // draining — there is no good connection left. Give up on both
                                // and let the outer loop reconnect from scratch.
                                let _ = seam.close().await;
                                return ConnectionOutcome::Reconnect;
                            }
                            None => return ConnectionOutcome::Reconnect,
                        }
                    }
                }
            }

            result = recv_secondary(&mut secondary) => {
                // `recv_secondary` only ever resolves when `secondary` was
                // `Some` at the moment it did — it pends forever otherwise
                // (see the function doc) — so `secondary` is guaranteed
                // `Some` here, and nothing else in this single-threaded loop
                // body could have changed that in between.
                let role = secondary.as_ref().expect("recv_secondary only resolves when secondary is Some").role;
                match result {
                    Ok(SocketFrame::Text(text)) => {
                        let outcome = handle_frame(&text, &mut secondary.as_mut().unwrap().seam, seen_ids, ctx, team_id, bot_user_id, in_flight).await;
                        match outcome {
                            FrameOutcome::Hello => match role {
                                SecondaryRole::Incoming => {
                                    let sec = secondary.take().expect("secondary present in this arm");
                                    let old_active = std::mem::replace(&mut active, sec.seam);
                                    secondary = Some(Secondary {
                                        seam: old_active,
                                        role: SecondaryRole::Draining,
                                        deadline: tokio::time::Instant::now() + GRACE_DRAIN_WINDOW,
                                    });
                                    ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Connected);
                                }
                                SecondaryRole::Draining => {
                                    debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: unexpected hello on a draining socket, ignoring");
                                }
                            },
                            FrameOutcome::Disconnect(_) => {
                                debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: ignoring a disconnect envelope on the secondary socket");
                            }
                            FrameOutcome::Stop => return ConnectionOutcome::Stop,
                            FrameOutcome::Continue => {}
                        }
                    }
                    Err(e) => {
                        let mut sec = secondary.take().expect("secondary present in this arm");
                        let _ = sec.seam.close().await;
                        match sec.role {
                            SecondaryRole::Incoming => warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: incoming socket failed before reaching hello: {e}"),
                            SecondaryRole::Draining => debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: old socket closed during grace-drain: {e}"),
                        }
                    }
                }
            }

            _ = sleep_until_secondary_deadline(secondary_deadline) => {
                let mut sec = secondary.take().expect("secondary present in this arm");
                let _ = sec.seam.close().await;
                match sec.role {
                    SecondaryRole::Incoming => warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: incoming socket timed out waiting for hello, discarding"),
                    SecondaryRole::Draining => debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: grace-drain window elapsed, closed old socket"),
                }
            }
        }
    }
}

/// Reads the next frame off `secondary`'s socket when a secondary exists,
/// or never resolves (pends forever) when it doesn't. Letting an absent
/// secondary simply never win the surrounding `tokio::select!` — rather
/// than gating this branch with an `if` precondition next to an `.unwrap()`
/// inside the async expression — sidesteps a real footgun: the `if` guard
/// only controls whether the branch is *polled*, not whether its async
/// expression is *constructed*, so an eagerly-evaluated `.unwrap()` there
/// still panics on a `None` even when the guard would have skipped it.
async fn recv_secondary(secondary: &mut Option<Secondary>) -> Result<SocketFrame, SlackSocketSeamError> {
    match secondary {
        Some(sec) => sec.seam.recv().await,
        None => std::future::pending().await,
    }
}

/// Sleeps until `deadline` (the incoming-hello timeout or the grace-drain
/// window, whichever role `secondary` is in), or never resolves when
/// there's no secondary at all. Takes the deadline as a plain `Option<Instant>`
/// (computed by the caller via `secondary.as_ref().map(|sec| sec.deadline)`)
/// rather than borrowing `secondary` itself — `Instant` is `Copy`, so this
/// carries no reference into the `tokio::select!` statement, which matters
/// because the neighboring `recv_secondary` branch already holds `secondary`
/// mutably for the same `select!` call, and Rust can't allow both a `&mut
/// Option<Secondary>` and a `&Option<Secondary>` borrow alive at once, even
/// across two independently-polled branches. See [`recv_secondary`]'s doc
/// for why this is a plain, unguarded async function rather than a
/// `select!` `if` precondition.
async fn sleep_until_secondary_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Opens a brand-new seam and drives its `connect` (a fresh
/// `connections.open` call) to completion, for use as the warm rotation's
/// second socket. Returns `None` (logged) on a cancelled wait or a failed
/// connect — either way the caller just stays on its current `active`
/// alone.
async fn open_incoming(
    seam_factory: &SlackSeamFactory,
    app_token: &str,
    cancel: &CancellationToken,
) -> Option<Box<dyn SlackSocketSeam>> {
    let mut seam = seam_factory();
    let result = tokio::select! {
        _ = cancel.cancelled() => return None,
        result = seam.connect(app_token) => result,
    };
    match result {
        Ok(_url) => Some(seam),
        Err(e) => {
            warn!("SlackTransport: failed to open a warm-rotation incoming socket: {e}");
            None
        }
    }
}

/// What handling one raw frame off a socket decided, from the perspective
/// of [`run_connection`]'s state machine — distinct from
/// [`filter::FilterDecision`], which only concerns one already-parsed
/// `events_api` payload's trigger decision.
enum FrameOutcome {
    /// The socket that delivered this frame is now connected & ready.
    Hello,
    /// Slack is rotating or force-closing the socket that delivered this.
    Disconnect(DisconnectSeverity),
    /// The binding this connection serves no longer exists/is disabled —
    /// the whole connection (not just this one frame) must end.
    Stop,
    /// Handled fully (acked, and dispatched or dropped as appropriate);
    /// nothing further for the caller to do.
    Continue,
}

/// Parses one raw frame and reacts to its envelope type. For anything
/// carrying an `envelope_id` (`events_api`, `slash_commands`, `interactive`)
/// the ack is sent back on `seam` — whichever socket this frame arrived on
/// — immediately, before any dedup/security/filter/dispatch work runs, per
/// Slack's 3-second ack deadline. The ack is best-effort:
/// a failed send is logged and never blocks or aborts the rest of the
/// handling.
async fn handle_frame(
    text: &str,
    seam: &mut Box<dyn SlackSocketSeam>,
    seen_ids: &mut SeenEventIds,
    ctx: &ChannelRunContext,
    team_id: &str,
    bot_user_id: &str,
    in_flight: &CorrelationMap<SlackOrigin>,
) -> FrameOutcome {
    let event = match protocol::parse_envelope(text) {
        Ok(event) => event,
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: failed to parse a socket mode frame: {e}");
            return FrameOutcome::Continue;
        }
    };

    match &event {
        SocketModeEvent::Hello => FrameOutcome::Hello,
        SocketModeEvent::Disconnect { reason } => FrameOutcome::Disconnect(protocol::classify_disconnect(reason)),
        SocketModeEvent::EventsApi { envelope_id, event: payload } => {
            ack(seam, envelope_id, ctx).await;
            let stop = handle_events_api_event(
                &event,
                &payload.event_id,
                &payload.event,
                seen_ids,
                ctx,
                team_id,
                bot_user_id,
                in_flight,
            )
            .await;
            if stop {
                FrameOutcome::Stop
            } else {
                FrameOutcome::Continue
            }
        }
        SocketModeEvent::SlashCommands { envelope_id } | SocketModeEvent::Interactive { envelope_id } => {
            // Parse-and-ignore for now — the ack is the whole
            // obligation; dispatching these is a future phase.
            ack(seam, envelope_id, ctx).await;
            FrameOutcome::Continue
        }
        SocketModeEvent::Unknown => FrameOutcome::Continue,
    }
}

/// Sends the envelope acknowledgement back on `seam`. Best-effort: logs and
/// swallows a failure rather than propagating it, since a missed ack just
/// means Slack redelivers — which dedup (the second line of defence)
/// already handles.
async fn ack(seam: &mut Box<dyn SlackSocketSeam>, envelope_id: &str, ctx: &ChannelRunContext) {
    let payload = protocol::Acknowledge::new(envelope_id).to_json();
    if let Err(e) = seam.send(&payload).await {
        warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, envelope_id = %envelope_id, "SlackTransport: failed to send envelope ack: {e}");
    }
}

/// Dedups on `event_id`, then hands off to [`handle_events_api_event_inner`]
/// for everything else, persisting the cursor exactly once afterward — but
/// only when this event was newly recorded, mirroring
/// `discord::runner::handle_message_create`'s "persist once, after full
/// handling, only on a real state change" shape. Returns whether the
/// binding disappeared out from under this connection (the caller must
/// stop entirely).
#[allow(clippy::too_many_arguments)]
async fn handle_events_api_event(
    envelope: &SocketModeEvent,
    event_id: &str,
    inner: &SlackEvent,
    seen_ids: &mut SeenEventIds,
    ctx: &ChannelRunContext,
    team_id: &str,
    bot_user_id: &str,
    in_flight: &CorrelationMap<SlackOrigin>,
) -> bool {
    if !seen_ids.insert_is_new(event_id) {
        debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, event_id = %event_id, "SlackTransport: dropping a redelivered event_id already dispatched");
        return false;
    }

    let stop = handle_events_api_event_inner(envelope, inner, ctx, team_id, bot_user_id, in_flight).await;
    persist_cursor(ctx, seen_ids).await;
    stop
}

/// The part of [`handle_events_api_event`] specific to one not-yet-seen
/// event: authorization, the trigger filter, bridge-thread resolution, and
/// delivery. Split out so the caller can persist the cursor exactly once
/// after this returns, regardless of which early return was taken — same
/// reasoning as `discord::runner::handle_message_create_inner`.
///
/// Two gates run in a fixed order, same as Discord's: [`security::is_allowed`]
/// (fail-closed authorization) always runs first and only ever narrows what
/// follows; [`filter::classify`] (the bot-echo guard + trigger scope)
/// only ever narrows further on top of an already-authorized message.
async fn handle_events_api_event_inner(
    envelope: &SocketModeEvent,
    inner: &SlackEvent,
    ctx: &ChannelRunContext,
    team_id: &str,
    bot_user_id: &str,
    in_flight: &CorrelationMap<SlackOrigin>,
) -> bool {
    let message = match inner {
        SlackEvent::Message(m) | SlackEvent::AppMention(m) => m,
        // Nothing to authorize or dispatch — filter::classify would drop
        // this as NotATrigger anyway, and there's no channel/user id to
        // check against the allow-list in the first place.
        SlackEvent::Other => return false,
    };

    // Re-read the agent profile fresh so a mid-connection allow-list edit
    // takes effect on the very next event, mirroring
    // `discord::runner::handle_message_create_inner`.
    let profile = match ctx.persistence.agents.get(&ctx.agent_id).await {
        Ok(Some(profile)) => profile,
        Ok(None) => return true,
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, "SlackTransport: failed to re-read agent profile: {e}");
            return false;
        }
    };
    let Some(binding) = profile.channels.iter().find(|b| b.binding_id == ctx.binding_id) else {
        return true;
    };
    if !binding.enabled {
        return true;
    }
    let ChannelKindConfig::Slack { allowed_channels, allowed_users, .. } = &binding.kind_config else {
        return true;
    };

    if !security::is_allowed(&message.channel, &message.user, allowed_channels, allowed_users) {
        debug!(agent_id = %ctx.agent_id, author = %message.user, channel = %message.channel, "SlackTransport: dropping unauthorized inbound message");
        return false;
    }

    // Only consulted for a threaded, non-DM reply — `filter::classify`
    // never reaches this closure for a DM or a top-level message. A cheap
    // read either way, computed up front since the closure itself must be
    // synchronous.
    let participates = match message.thread_ts.as_deref() {
        Some(thread_ts) => ctx
            .persistence
            .slack_conversations
            .get(team_id, &message.channel, Some(thread_ts))
            .await
            .ok()
            .flatten()
            .is_some(),
        None => false,
    };

    let Some(trigger) = filter::classify(envelope, bot_user_id, |_, _| participates).trigger() else {
        debug!(agent_id = %ctx.agent_id, channel = %message.channel, "SlackTransport: dropping a non-triggering inbound event");
        return false;
    };

    // The conversation key: a DM collapses to the channel id alone (one
    // persistent thread per DM); a mention or thread reply keys on the
    // channel plus the thread's root ts — which, for a fresh top-level
    // mention, is the mention's own ts (Slack threads a reply against
    // whatever ts it's replying to).
    let (key_channel, key_thread_ts): (&str, Option<&str>) = match trigger {
        Trigger::DirectMessage => (message.channel.as_str(), None),
        Trigger::Mention | Trigger::ThreadReply => {
            let root = message.thread_ts.as_deref().unwrap_or(message.ts.as_str());
            (message.channel.as_str(), Some(root))
        }
    };

    let now = Utc::now();
    let Some(thread_id) = resolve_bridge_thread(ctx, team_id, key_channel, key_thread_ts, &message.text, now).await
    else {
        return false;
    };

    // Recorded before dispatch, mirroring `discord::runner`'s
    // `in_flight.record` call: the outbound relay observer (peek, never
    // take — see `CorrelationMap`'s module doc) resolves this at the run's
    // `RunEnded` to learn which channel and thread to reply into.
    in_flight.record(
        &thread_id,
        SlackOrigin {
            channel: key_channel.to_string(),
            thread_ts: key_thread_ts.map(str::to_string),
            binding_id: ctx.binding_id.clone(),
        },
    );

    // No display-name resolution in Slack v1 (no channel-metadata
    // resolution) — `submit_inbound_message` is passed `None` and simply
    // doesn't prefix the transcript entry with a sender name.
    // Slack titles its bridge thread at mint time in `resolve_bridge_thread`
    // (see `title::derive_slack_channel_title`) rather than through this
    // shared candidate — `None` here keeps that path untouched.
    if let Err(e) = submit_inbound_message(
        ctx,
        &profile,
        &thread_id,
        ChannelKind::Slack,
        key_channel,
        &message.user,
        None,
        &message.text,
        None,
    )
    .await
    {
        warn!(agent_id = %ctx.agent_id, "SlackTransport: failed to deliver inbound message: {e}");
    }

    false
}

/// Resolves the conversation→thread registry row for
/// `(team_id, channel, thread_ts)`, lazily provisioning a fresh Launchpad
/// thread on first contact ("provisioned once, on first inbound").
/// Touches `last_seen_at` on an existing row either way. Returns `None`
/// (logged) only on a persistence failure — a message this connection
/// cannot resolve a thread for is dropped rather than mis-routed.
///
/// `first_message_text` is only consulted on the fresh-mint branch (an
/// existing row's thread already has whatever title/auto_title it's going
/// to have) — it's the raw text of the very message that triggered this
/// resolve, used to seed the new thread's `auto_title` via
/// [`title::derive_slack_channel_title`] so the sidebar shows the
/// conversation's subject without the tab strip ever needing a per-turn
/// "was this the first message" check of its own.
async fn resolve_bridge_thread(
    ctx: &ChannelRunContext,
    team_id: &str,
    channel: &str,
    thread_ts: Option<&str>,
    first_message_text: &str,
    now: DateTime<Utc>,
) -> Option<String> {
    match ctx.persistence.slack_conversations.get(team_id, channel, thread_ts).await {
        Ok(Some(mut row)) => {
            row.last_seen_at = now;
            if let Err(e) = ctx.persistence.slack_conversations.set(team_id, channel, thread_ts, &row, now).await {
                warn!(agent_id = %ctx.agent_id, "SlackTransport: failed to refresh conversation row: {e}");
            }
            // Registered on every resolve, not just on first mint: `LeaseGate`
            // is process-local, so a conversation created by an earlier
            // holder (or an earlier run of this same process) is otherwise
            // unknown to this process's gate until it sees the conversation
            // again — see `LeaseGate`'s module doc.
            ctx.lease_gate.mark_active(&ctx.binding_id, &row.thread_id);
            Some(row.thread_id)
        }
        Ok(None) => {
            // `title` is left unset (unlike the single eager thread
            // `channel_provisioning::provision_bridge_thread` mints for
            // Discord/Telegram/Email, which is deliberately named after the
            // binding once and for all) — a per-conversation Slack thread
            // instead gets its label from the first inbound message below,
            // and stays renamable (`Thread::offers_rename_tool` gates on
            // `title.is_none()`) the way an ordinary chat thread is.
            let mut thread = ctx.persistence.threads.build_fresh_thread(&ctx.agent_id, None);
            // This is the thread's very first message, so there is
            // no unset-vs-already-set race to check here the way
            // `set_auto_title_if_unset` guards against for an ordinary
            // thread's first turn — nothing else could have set
            // `auto_title` on a row that doesn't exist yet. `None` (an
            // attachment-only message, or text that's pure Slack markup)
            // simply leaves it unset, so the channel-kind label shows.
            thread.auto_title = title::derive_slack_channel_title(first_message_text);
            // Slack has no single dedicated `bridge_thread_id` to reverse-look-up
            // from (see `ChannelBridgeOrigin`'s docstring — Slack is one thread
            // per conversation, not one thread per binding), so this is the only
            // place a Slack bridge thread's identity ever gets recorded. Without
            // this, both the composer-gating hint and the backend's
            // `is_channel_bridge_thread` tool-admission gate silently never
            // recognize a real Slack conversation thread.
            thread.channel_origin = Some(ChannelBridgeOrigin {
                kind: ChannelKind::Slack,
                binding_id: ctx.binding_id.clone(),
            });
            let thread_id = thread.id.clone();
            if let Err(e) = ctx.persistence.threads.create(thread.clone()).await {
                warn!(agent_id = %ctx.agent_id, "SlackTransport: failed to create a bridge thread: {e}");
                return None;
            }
            ctx.event_bus
                .emit(
                    &format!("thread:{}", thread_id),
                    &ctx.agent_id,
                    Some(thread_id.clone()),
                    AgentEventPayload::ThreadCreated { thread },
                )
                .await;
            let row = SlackConversationRow {
                agent_id: ctx.agent_id.clone(),
                thread_id: thread_id.clone(),
                created_at: now,
                last_seen_at: now,
            };
            if let Err(e) = ctx.persistence.slack_conversations.set(team_id, channel, thread_ts, &row, now).await {
                warn!(agent_id = %ctx.agent_id, "SlackTransport: failed to persist a new conversation row: {e}");
            }
            ctx.lease_gate.mark_active(&ctx.binding_id, &thread_id);
            Some(thread_id)
        }
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, "SlackTransport: failed to read the conversation registry: {e}");
            None
        }
    }
}

/// Persists the current dedup cursor for `(ctx.agent_id, ctx.binding_id)`,
/// logging and swallowing any store failure — mirrors
/// `discord::runner::persist_cursor`: a failed persist just widens the next
/// crash's re-delivery window, never a reason to tear down the connection.
async fn persist_cursor(ctx: &ChannelRunContext, seen_ids: &SeenEventIds) {
    let cursor = ChannelCursor::Slack { seen_event_ids: seen_ids.snapshot() };
    if let Err(e) = ctx.persistence.channel_cursors.set(&ctx.agent_id, &ctx.binding_id, &cursor).await {
        warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: failed to persist cursor: {e}");
    }
}

/// Doubling backoff starting at [`BASE_BACKOFF`], capped at [`MAX_BACKOFF`].
/// `attempt` is 1-based (the first failure passes `1`).
fn capped_exponential_backoff(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(6);
    let scaled_millis = BASE_BACKOFF.as_millis().saturating_mul(1u128 << exponent);
    Duration::from_millis(scaled_millis.min(MAX_BACKOFF.as_millis()) as u64)
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
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;

    use ao_protocol::agent::{
        AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig, CliProviderConfig, InputMode, OutputFormat,
        ProviderConfig, SlackConversationMode,
    };
    use ao_protocol::error::AoError;
    use ao_protocol::event::RunEndReason;

    use crate::agent_runner::{AgentRunRequest, AgentRunner, AgentRunnerMode, RunComplete, RunnerDispatcher};
    use crate::channels::connection_state::ConnectionStateRegistry;
    use crate::channels::relay::lease_gate::LeaseGate;
    use crate::event_bus::EventBus;
    use crate::instance_registry::InstanceRegistry;
    use crate::queue_manager::QueueManagerRegistry;

    use super::super::socket_seam::{FakeSlackSocketSeam, SlackSocketSeamError};
    use super::*;

    const AGENT_ID: &str = "agent-slack-test";
    const BINDING_ID: &str = "slack";
    const TEAM_ID: &str = "T1";
    const BOT_USER_ID: &str = "U0BOT";

    /// A stub [`AgentRunner`] that completes immediately without spawning a
    /// real process — the same pattern `queue_manager`'s own end-to-end test
    /// uses so `submit_inbound_message`'s real `QueueManagerRegistry` path
    /// can run inside a unit test.
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

    /// A stub [`AgentRunner`] whose `run` sleeps for `delay` before
    /// completing — stands in for a real, multi-second agent turn so a test
    /// can observe what the read loop does *while a dispatch is still in
    /// flight*, without an actual slow process. `completed` flips only after
    /// the sleep, so a test can assert something else happened strictly
    /// before this dispatch finished.
    struct SlowRunner {
        delay: Duration,
        completed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl AgentRunner for SlowRunner {
        fn mode(&self) -> AgentRunnerMode {
            AgentRunnerMode::Cli
        }

        async fn run(&self, req: AgentRunRequest) -> Result<RunComplete, AoError> {
            tokio::time::sleep(self.delay).await;
            self.completed.store(true, Ordering::SeqCst);
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

    /// Shared backing state for one test's [`ChannelRunContext`]s.
    /// `ChannelRunContext` itself isn't `Clone` (its callers each build their
    /// own), and a test needs one instance to move into the spawned runner
    /// task plus a second to read back through for assertions — both
    /// sharing the same underlying stores, via [`Self::ctx`].
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
        make_test_harness_with_runner(agent, Arc::new(StubRunner)).await
    }

    /// Same as [`make_test_harness`], but with a caller-supplied [`AgentRunner`]
    /// wired into the queue manager's dispatcher — used by tests that need to
    /// observe the read loop's behavior *while a dispatch is still in flight*
    /// (e.g. [`SlowRunner`]), rather than one that always completes instantly.
    async fn make_test_harness_with_runner(agent: AgentProfile, runner: Arc<dyn AgentRunner>) -> TestHarness {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
        let persistence =
            Arc::new(ao_persistence::PersistenceLayer::init_with_root(data_root).await.expect("init persistence"));
        persistence.agents.create(&agent).await.expect("create agent");

        let event_bus = Arc::new(EventBus::new(64));
        let instance_registry = Arc::new(InstanceRegistry::new());
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(
            Arc::clone(&runner),
            runner,
        ));
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

    fn make_test_agent(allowed_channels: Vec<String>, allowed_users: Vec<String>) -> AgentProfile {
        AgentProfile {
            id: AGENT_ID.to_string(),
            name: "Slack Test Agent".to_string(),
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
                kind: ChannelKind::Slack,
                enabled: true,
                bridge_thread_id: None,
                allowed_senders: vec![],
                pending_pairing_code: None,
                kind_config: ChannelKindConfig::Slack {
                    allowed_channels,
                    allowed_users,
                    connection_id: None,
                    conversation_mode: SlackConversationMode::PerConversation,
                },
            }],
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            max_turns: None,
}
    }

    /// Wraps a [`FakeSlackSocketSeam`] so a test retains an externally
    /// observable "was `close()` called" flag even after the seam itself
    /// has been boxed and moved into a [`SlackSeamFactory`].
    struct CloseProbe {
        inner: FakeSlackSocketSeam,
        closed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl SlackSocketSeam for CloseProbe {
        async fn connect(&mut self, app_token: &str) -> Result<String, SlackSocketSeamError> {
            self.inner.connect(app_token).await
        }
        async fn send(&mut self, text: &str) -> Result<(), SlackSocketSeamError> {
            self.inner.send(text).await
        }
        async fn recv(&mut self) -> Result<SocketFrame, SlackSocketSeamError> {
            self.inner.recv().await
        }
        async fn close(&mut self) -> Result<(), SlackSocketSeamError> {
            self.closed.store(true, Ordering::SeqCst);
            self.inner.close().await
        }
    }

    fn with_close_probe(seam: FakeSlackSocketSeam) -> (CloseProbe, Arc<AtomicBool>) {
        let closed = Arc::new(AtomicBool::new(false));
        (CloseProbe { inner: seam, closed: Arc::clone(&closed) }, closed)
    }

    /// Wraps a [`FakeSlackSocketSeam`] so a test retains an externally
    /// observable "was `connect()` called yet" flag — used to prove a
    /// warm-rotation `incoming` socket was opened promptly (i.e. the read
    /// loop reached and handled a `disconnect` envelope) rather than only
    /// eventually, after some unrelated in-flight dispatch happened to
    /// finish.
    struct ConnectProbe {
        inner: FakeSlackSocketSeam,
        connected: Arc<AtomicBool>,
    }

    #[async_trait]
    impl SlackSocketSeam for ConnectProbe {
        async fn connect(&mut self, app_token: &str) -> Result<String, SlackSocketSeamError> {
            let result = self.inner.connect(app_token).await;
            if result.is_ok() {
                self.connected.store(true, Ordering::SeqCst);
            }
            result
        }
        async fn send(&mut self, text: &str) -> Result<(), SlackSocketSeamError> {
            self.inner.send(text).await
        }
        async fn recv(&mut self) -> Result<SocketFrame, SlackSocketSeamError> {
            self.inner.recv().await
        }
        async fn close(&mut self) -> Result<(), SlackSocketSeamError> {
            self.inner.close().await
        }
    }

    fn with_connect_probe(seam: FakeSlackSocketSeam) -> (ConnectProbe, Arc<AtomicBool>) {
        let connected = Arc::new(AtomicBool::new(false));
        (ConnectProbe { inner: seam, connected: Arc::clone(&connected) }, connected)
    }

    /// Wraps a [`FakeSlackSocketSeam`] so a test can inspect every frame sent
    /// (in practice, envelope acks) after the seam has been boxed and moved
    /// into a [`SlackSeamFactory`] — used to prove a later envelope is acked
    /// without waiting on an earlier one's (possibly slow) dispatch.
    struct SentFramesProbe {
        inner: FakeSlackSocketSeam,
        sent: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait]
    impl SlackSocketSeam for SentFramesProbe {
        async fn connect(&mut self, app_token: &str) -> Result<String, SlackSocketSeamError> {
            self.inner.connect(app_token).await
        }
        async fn send(&mut self, text: &str) -> Result<(), SlackSocketSeamError> {
            self.sent.lock().unwrap_or_else(|e| e.into_inner()).push(text.to_string());
            self.inner.send(text).await
        }
        async fn recv(&mut self) -> Result<SocketFrame, SlackSocketSeamError> {
            self.inner.recv().await
        }
        async fn close(&mut self) -> Result<(), SlackSocketSeamError> {
            self.inner.close().await
        }
    }

    fn with_sent_probe(seam: FakeSlackSocketSeam) -> (SentFramesProbe, Arc<StdMutex<Vec<String>>>) {
        let sent = Arc::new(StdMutex::new(Vec::new()));
        (SentFramesProbe { inner: seam, sent: Arc::clone(&sent) }, sent)
    }

    fn boxed(seam: FakeSlackSocketSeam) -> Box<dyn SlackSocketSeam> {
        Box::new(seam)
    }

    /// A queue of pre-boxed seams a [`SlackSeamFactory`] hands out one at a
    /// time, in order — one per `connect` attempt the runner makes (the
    /// initial connect, any reconnect-from-scratch, and a warm rotation's
    /// second socket all call the factory once each). Exhausting
    /// the queue yields a seam whose own `connect` fails, so a test that
    /// under-scripts just sees an ordinary failed reconnect rather than a
    /// panic.
    fn seam_factory_from(seams: Vec<Box<dyn SlackSocketSeam>>) -> SlackSeamFactory {
        let queue = Arc::new(StdMutex::new(VecDeque::from(seams)));
        Arc::new(move || -> Box<dyn SlackSocketSeam> {
            let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
            match queue.pop_front() {
                Some(seam) => seam,
                None => Box::new(FakeSlackSocketSeam::new(
                    Err(SlackSocketSeamError::ConnectionsOpen("no more scripted seams".to_string())),
                    vec![],
                )),
            }
        })
    }

    fn hello_frame() -> SocketFrame {
        SocketFrame::Text(r#"{"type":"hello"}"#.to_string())
    }

    fn disconnect_frame(reason: &str) -> SocketFrame {
        SocketFrame::Text(format!(r#"{{"type":"disconnect","reason":"{reason}"}}"#))
    }

    fn app_mention_frame(envelope_id: &str, event_id: &str, channel: &str, user: &str, text: &str) -> SocketFrame {
        SocketFrame::Text(format!(
            r#"{{
                "envelope_id":"{envelope_id}",
                "payload":{{
                    "event":{{"type":"app_mention","channel":"{channel}","user":"{user}","text":"{text}","ts":"1701234567.000100","team":"{TEAM_ID}"}},
                    "type":"event_callback","event_id":"{event_id}"
                }},
                "type":"events_api"
            }}"#
        ))
    }

    /// Same as [`app_mention_frame`], but threaded under `thread_ts` — the
    /// only way a test can drive a `resolve_bridge_thread` *load* of an
    /// already-minted thread rather than a fresh mint (a bare
    /// `app_mention_frame` always keys on its own `ts`, i.e. always mints).
    fn app_mention_in_thread_frame(
        envelope_id: &str,
        event_id: &str,
        channel: &str,
        user: &str,
        text: &str,
        ts: &str,
        thread_ts: &str,
    ) -> SocketFrame {
        SocketFrame::Text(format!(
            r#"{{
                "envelope_id":"{envelope_id}",
                "payload":{{
                    "event":{{"type":"app_mention","channel":"{channel}","user":"{user}","text":"{text}","ts":"{ts}","thread_ts":"{thread_ts}","team":"{TEAM_ID}"}},
                    "type":"event_callback","event_id":"{event_id}"
                }},
                "type":"events_api"
            }}"#
        ))
    }

    /// Polls `check` until it returns `Some`, or gives up after ~2 seconds —
    /// resilient to scheduling jitter without a fixed, arbitrary sleep.
    async fn wait_for<F, Fut, T>(mut check: F) -> Option<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        for _ in 0..200 {
            if let Some(value) = check().await {
                return Some(value);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }

    async fn transcript_text_for(ctx: &ChannelRunContext, thread_id: &str) -> Option<String> {
        let thread = ctx.persistence.threads.get(thread_id).await.ok()??;
        let entries =
            ctx.persistence.transcripts.read_all_at(&std::path::PathBuf::from(&thread.transcript_path)).await.ok()?;
        Some(entries.into_iter().map(|e| e.content).collect::<Vec<_>>().join("\n"))
    }

    // --- Steady state: hello -> event -> dispatched ---

    #[tokio::test]
    async fn hello_then_event_is_dispatched() {
        let agent = make_test_agent(vec!["C123".to_string()], vec![]);
        let harness = make_test_harness(agent).await;

        let active = boxed(FakeSlackSocketSeam::connects_to(
            "wss://example.slack.com/socket",
            vec![hello_frame(), app_mention_frame("env-1", "Ev001", "C123", "U456", "hello there")],
        ));
        let seam_factory = seam_factory_from(vec![active]);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(run_slack_socket_mode_loop(
            harness.ctx(),
            "xapp-fake".to_string(),
            TEAM_ID.to_string(),
            BOT_USER_ID.to_string(),
            Arc::new(CorrelationMap::new()),
            seam_factory,
            cancel.clone(),
        ));

        let ctx = harness.ctx();
        let row = wait_for(|| async {
            ctx.persistence.slack_conversations.get(TEAM_ID, "C123", Some("1701234567.000100")).await.ok().flatten()
        })
        .await
        .expect("conversation row must be provisioned once the mention is dispatched");

        let text = wait_for(|| {
            let ctx = &ctx;
            let thread_id = row.thread_id.clone();
            async move { transcript_text_for(ctx, &thread_id).await }
        })
        .await
        .expect("transcript entry must be written for the dispatched mention");
        assert!(text.contains("hello there"), "transcript must contain the dispatched message text, got: {text}");

        cancel.cancel();
        let _ = handle.await;
    }

    // --- fresh-mint auto_title: set once from the first inbound message,
    //     never overwritten by a later message in the same thread. ---

    #[tokio::test]
    async fn fresh_thread_creation_sets_auto_title_and_a_later_message_does_not_overwrite_it() {
        let agent = make_test_agent(vec!["C123".to_string()], vec![]);
        let harness = make_test_harness(agent).await;

        // `app_mention_frame`'s fixed `ts` doubles as the fresh thread's key
        // (a top-level mention with no `thread_ts` roots on its own `ts`).
        let thread_root = "1701234567.000100";
        let active = boxed(FakeSlackSocketSeam::connects_to(
            "wss://example.slack.com/socket",
            vec![
                hello_frame(),
                app_mention_frame("env-1", "Ev001", "C123", "U456", "please help with the deploy"),
                app_mention_in_thread_frame(
                    "env-2",
                    "Ev002",
                    "C123",
                    "U456",
                    "totally unrelated follow-up text",
                    "1701234568.000200",
                    thread_root,
                ),
            ],
        ));
        let seam_factory = seam_factory_from(vec![active]);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(run_slack_socket_mode_loop(
            harness.ctx(),
            "xapp-fake".to_string(),
            TEAM_ID.to_string(),
            BOT_USER_ID.to_string(),
            Arc::new(CorrelationMap::new()),
            seam_factory,
            cancel.clone(),
        ));

        let ctx = harness.ctx();
        let row = wait_for(|| async {
            ctx.persistence.slack_conversations.get(TEAM_ID, "C123", Some(thread_root)).await.ok().flatten()
        })
        .await
        .expect("conversation row must be provisioned once the mention is dispatched");

        let thread = wait_for(|| {
            let ctx = &ctx;
            let thread_id = row.thread_id.clone();
            async move { ctx.persistence.threads.get(&thread_id).await.ok().flatten().filter(|t| t.auto_title.is_some()) }
        })
        .await
        .expect("auto_title must be populated from the first inbound message on fresh creation");
        assert_eq!(thread.auto_title.as_deref(), Some("please help with the deploy"));
        assert!(thread.title.is_none(), "a fresh Slack channel thread must stay renamable (title unset)");

        // Wait for the second, differently-worded message to actually land
        // in the same thread before asserting the title didn't move.
        wait_for(|| {
            let ctx = &ctx;
            let thread_id = row.thread_id.clone();
            async move { transcript_text_for(ctx, &thread_id).await.filter(|t| t.contains("totally unrelated follow-up text")) }
        })
        .await
        .expect("the second, in-thread message must also be dispatched (a resolve_bridge_thread load, not a mint)");

        let thread = ctx.persistence.threads.get(&row.thread_id).await.expect("read thread").expect("thread exists");
        assert_eq!(
            thread.auto_title.as_deref(),
            Some("please help with the deploy"),
            "a later message in an already-minted thread must never overwrite the auto_title set at creation"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn a_duplicate_event_id_is_dropped_not_dispatched() {
        let agent = make_test_agent(vec!["C123".to_string()], vec![]);
        let harness = make_test_harness(agent).await;

        let active = boxed(FakeSlackSocketSeam::connects_to(
            "wss://example.slack.com/socket",
            vec![
                hello_frame(),
                app_mention_frame("env-1", "Ev001", "C123", "U456", "first"),
                app_mention_frame("env-2", "Ev001", "C123", "U456", "first"),
            ],
        ));
        let seam_factory = seam_factory_from(vec![active]);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(run_slack_socket_mode_loop(
            harness.ctx(),
            "xapp-fake".to_string(),
            TEAM_ID.to_string(),
            BOT_USER_ID.to_string(),
            Arc::new(CorrelationMap::new()),
            seam_factory,
            cancel.clone(),
        ));

        let ctx = harness.ctx();
        let row = wait_for(|| async {
            ctx.persistence.slack_conversations.get(TEAM_ID, "C123", Some("1701234567.000100")).await.ok().flatten()
        })
        .await
        .expect("conversation row must be provisioned for the first delivery");

        // Give the (would-be) duplicate a chance to be processed too.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let text = transcript_text_for(&ctx, &row.thread_id).await.unwrap_or_default();
        let occurrences = text.matches("first").count();
        assert_eq!(occurrences, 1, "a redelivered event_id must be dispatched exactly once, transcript was: {text}");

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn an_unauthorized_channel_is_dropped() {
        // Allow-list only admits C999; the inbound mention is in C123.
        let agent = make_test_agent(vec!["C999".to_string()], vec![]);
        let harness = make_test_harness(agent).await;

        let active = boxed(FakeSlackSocketSeam::connects_to(
            "wss://example.slack.com/socket",
            vec![hello_frame(), app_mention_frame("env-1", "Ev001", "C123", "U456", "not allowed")],
        ));
        let seam_factory = seam_factory_from(vec![active]);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(run_slack_socket_mode_loop(
            harness.ctx(),
            "xapp-fake".to_string(),
            TEAM_ID.to_string(),
            BOT_USER_ID.to_string(),
            Arc::new(CorrelationMap::new()),
            seam_factory,
            cancel.clone(),
        ));

        tokio::time::sleep(Duration::from_millis(150)).await;
        let ctx = harness.ctx();
        let row = ctx.persistence.slack_conversations.get(TEAM_ID, "C123", Some("1701234567.000100")).await.unwrap();
        assert!(row.is_none(), "an unauthorized channel must never provision a conversation row");

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn a_bot_echo_is_dropped() {
        let agent = make_test_agent(vec!["C123".to_string()], vec![]);
        let harness = make_test_harness(agent).await;

        // The event's own user id equals bot_user_id — the echo guard.
        let active = boxed(FakeSlackSocketSeam::connects_to(
            "wss://example.slack.com/socket",
            vec![hello_frame(), app_mention_frame("env-1", "Ev001", "C123", BOT_USER_ID, "echo of myself")],
        ));
        let seam_factory = seam_factory_from(vec![active]);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(run_slack_socket_mode_loop(
            harness.ctx(),
            "xapp-fake".to_string(),
            TEAM_ID.to_string(),
            BOT_USER_ID.to_string(),
            Arc::new(CorrelationMap::new()),
            seam_factory,
            cancel.clone(),
        ));

        tokio::time::sleep(Duration::from_millis(150)).await;
        let ctx = harness.ctx();
        let row = ctx.persistence.slack_conversations.get(TEAM_ID, "C123", Some("1701234567.000100")).await.unwrap();
        assert!(row.is_none(), "a bot echo (own user id) must never be dispatched");

        cancel.cancel();
        let _ = handle.await;
    }

    // --- The read loop must not stall behind a slow dispatch ---

    #[tokio::test]
    async fn a_slow_dispatch_does_not_block_the_read_loop_from_acking_the_next_frame() {
        let agent = make_test_agent(vec!["C123".to_string()], vec![]);
        let completed = Arc::new(AtomicBool::new(false));
        let runner = Arc::new(SlowRunner { delay: Duration::from_secs(3), completed: Arc::clone(&completed) });
        let harness = make_test_harness_with_runner(agent, runner).await;

        let (active_probe, sent) = with_sent_probe(FakeSlackSocketSeam::connects_to(
            "wss://example.slack.com/socket",
            vec![
                hello_frame(),
                app_mention_frame("env-1", "Ev001", "C123", "U456", "first"),
                app_mention_frame("env-2", "Ev002", "C123", "U456", "second"),
            ],
        ));
        let seam_factory = seam_factory_from(vec![Box::new(active_probe)]);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(run_slack_socket_mode_loop(
            harness.ctx(),
            "xapp-fake".to_string(),
            TEAM_ID.to_string(),
            BOT_USER_ID.to_string(),
            Arc::new(CorrelationMap::new()),
            seam_factory,
            cancel.clone(),
        ));

        // The first mention's dispatch runs against a `SlowRunner` that
        // takes 3 seconds to complete. If the read loop awaited that turn
        // inline, the second envelope could never be acked until the first
        // one's agent run finished — so seeing both acks land is only
        // possible if the loop kept reading (and acking) instead of
        // stalling behind the first dispatch.
        let both_acked = wait_for(|| {
            let sent = Arc::clone(&sent);
            async move {
                let count = sent.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|f| f.contains("envelope_id")).count();
                if count >= 2 {
                    Some(true)
                } else {
                    None
                }
            }
        })
        .await;

        assert_eq!(both_acked, Some(true), "both envelopes must be acked, sent so far: {:?}", sent.lock().unwrap());
        assert!(
            !completed.load(Ordering::SeqCst),
            "the first dispatch's 3-second agent run must still be in flight when the second frame is acked \
             — proves the read loop does not stall behind a prior dispatch"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn a_disconnect_during_a_slow_dispatch_still_opens_a_warm_rotation_socket() {
        let agent = make_test_agent(vec!["C123".to_string()], vec![]);
        let completed = Arc::new(AtomicBool::new(false));
        let runner = Arc::new(SlowRunner { delay: Duration::from_secs(3), completed: Arc::clone(&completed) });
        let harness = make_test_harness_with_runner(agent, runner).await;

        // `active`: hello, a mention whose dispatch will hang for 3 seconds,
        // then a routine disconnect right behind it.
        let active = boxed(FakeSlackSocketSeam::connects_to(
            "wss://example.slack.com/socket",
            vec![hello_frame(), app_mention_frame("env-1", "Ev001", "C123", "U456", "slow turn"), disconnect_frame("refresh_requested")],
        ));
        let (incoming_probe, incoming_connected) =
            with_connect_probe(FakeSlackSocketSeam::connects_to("wss://example.slack.com/socket-2", vec![hello_frame()]));

        let seam_factory = seam_factory_from(vec![active, Box::new(incoming_probe)]);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(run_slack_socket_mode_loop(
            harness.ctx(),
            "xapp-fake".to_string(),
            TEAM_ID.to_string(),
            BOT_USER_ID.to_string(),
            Arc::new(CorrelationMap::new()),
            seam_factory,
            cancel.clone(),
        ));

        // The disconnect envelope sits right behind the mention in the same
        // socket's queue. If the read loop awaited the mention's (3-second)
        // dispatch inline, the disconnect could not be read — and the
        // warm-rotation socket could not be opened — until that dispatch
        // finished.
        let rotated = wait_for(|| {
            let flag = Arc::clone(&incoming_connected);
            async move { if flag.load(Ordering::SeqCst) { Some(true) } else { None } }
        })
        .await;

        assert_eq!(
            rotated,
            Some(true),
            "the disconnect envelope must open the warm-rotation socket even while the prior mention's dispatch is still in flight"
        );
        assert!(
            !completed.load(Ordering::SeqCst),
            "the mention's 3-second agent run must still be in flight when warm rotation begins \
             — proves the read loop does not stall behind a prior dispatch"
        );

        cancel.cancel();
        let _ = handle.await;
    }

    // --- Warm rotation ---

    #[tokio::test]
    async fn disconnect_opens_a_second_socket_and_dedup_delivers_exactly_once() {
        let agent = make_test_agent(vec!["C123".to_string()], vec![]);
        let harness = make_test_harness(agent).await;

        // `active`: hello, a routine disconnect (triggers rotation), then the
        // SAME event_id redelivered on this socket too.
        let (active_probe, active_closed) = with_close_probe(FakeSlackSocketSeam::connects_to(
            "wss://example.slack.com/socket",
            vec![
                hello_frame(),
                disconnect_frame("refresh_requested"),
                app_mention_frame("env-dup-active", "Ev-dup", "C123", "U456", "warm rotation payload"),
            ],
        ));
        // `incoming`: its own hello (promotes it), then the SAME event_id
        // again — proving overlap is safe by construction (dedup), not by
        // ack timing.
        let (incoming_probe, incoming_closed) = with_close_probe(FakeSlackSocketSeam::connects_to(
            "wss://example.slack.com/socket-2",
            vec![
                hello_frame(),
                app_mention_frame("env-dup-incoming", "Ev-dup", "C123", "U456", "warm rotation payload"),
            ],
        ));

        let seam_factory = seam_factory_from(vec![Box::new(active_probe), Box::new(incoming_probe)]);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(run_slack_socket_mode_loop(
            harness.ctx(),
            "xapp-fake".to_string(),
            TEAM_ID.to_string(),
            BOT_USER_ID.to_string(),
            Arc::new(CorrelationMap::new()),
            seam_factory,
            cancel.clone(),
        ));

        let ctx = harness.ctx();
        let row = wait_for(|| async {
            ctx.persistence.slack_conversations.get(TEAM_ID, "C123", Some("1701234567.000100")).await.ok().flatten()
        })
        .await
        .expect("conversation row must be provisioned once the mention is dispatched");

        // Let any (would-be) second delivery attempt run too.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let text = transcript_text_for(&ctx, &row.thread_id).await.unwrap_or_default();
        let occurrences = text.matches("warm rotation payload").count();
        assert_eq!(
            occurrences, 1,
            "the same event_id delivered on both the old and new socket must be dispatched exactly once, transcript was: {text}"
        );

        let old_closed = wait_for(|| {
            let flag = Arc::clone(&active_closed);
            async move { if flag.load(Ordering::SeqCst) { Some(true) } else { None } }
        })
        .await;
        assert_eq!(old_closed, Some(true), "the retired socket must be closed after incoming's hello");
        assert!(!incoming_closed.load(Ordering::SeqCst), "the promoted socket (formerly incoming) must stay open");

        cancel.cancel();
        let _ = handle.await;
    }
}
