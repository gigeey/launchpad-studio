//! Discord's implementation of the channel-agnostic
//! [`crate::channels::ChannelTransport`] trait — a handrolled Discord
//! Gateway v10 WebSocket client, mirroring
//! [`crate::channels::email::EmailTransport`]'s shape and
//! [`crate::telegram::transport::TelegramTransport`]'s `InFlightChats`-style
//! outbound-relay bridging (here, [`InFlightChannels`]).
//!
//! `protocol` carries the wire format: opcode envelope parsing, payload
//! construction (`IDENTIFY`/`RESUME`/heartbeat), intent-bit computation, and
//! close-code classification — all pure and independent of any live socket.
//! `security` is the inbound authorization decision (`allowed_users`/
//! `allowed_roles`/`allowed_channels`/`dm_role_auth_guild`), also pure.
//! `session` holds the two pieces of connection state that need dedicated
//! logic to get right: bounded message-id de-dup (for `RESUME` replay) and
//! heartbeat-ack tracking (zombie-connection detection). `gateway_seam` is
//! the only place that actually opens a socket — the
//! [`gateway_seam::GatewaySeam`] trait lets `runner`'s connect/reconnect
//! state machine run against a scripted fake in tests, exactly as
//! [`crate::channels::email::imap_seam::MailSource`] does for the email
//! transport. `runner` is the actual async loop wiring all of the above
//! together.
//!
//! Unlike email, Discord is a synchronous chat channel exactly like
//! Telegram, so its outbound side follows [`crate::telegram::outbound`]'s
//! reply-to-origin-per-turn observer model rather than email's send-tool
//! model: `outbound` is a single shared `EventBus` observer that relays a
//! bridge thread's finished reply back to the channel it arrived on, and
//! `outbound_seam` is the REST-send network boundary it drives (the
//! outbound analogue of `gateway_seam` for the inbound connection).
//! [`InFlightChannels`] is what makes that possible: it stashes which
//! channel (and whether it was a DM) each bridge thread's most recent
//! inbound message came from, peeked — never consumed — by `outbound` at
//! every turn's end.
//!
//! `channel_meta` fills in the one thing `MESSAGE_CREATE` never carries: a
//! channel's `type` — specifically, whether it's a THREAD, and if so its
//! parent and creator. [`channel_meta::resolve_channel_meta`] resolves that
//! lazily over REST and caches it in a [`channel_meta::ChannelMetaCache`]
//! threaded down alongside `http`, since it never changes for a channel's
//! life.
//!
//! `engagement` decides, given `security`'s mention detection and
//! `channel_meta`'s thread/owner facts, whether a shared channel or thread
//! is currently COLD (mention-only) or WARM (respond to everything) — see
//! [`engagement::EngagementTracker`] for the full cold/warm/decay rule set.
//!
//! `backfill` fills in what mention-gating costs the bot: once it no longer
//! reads every message, it needs a deliberate history fetch the moment it
//! *is* pulled in, run before the agent turn starts rather than left to the
//! model to request. [`backfill::fetch_thread_backfill`] pulls a flat window
//! on the COLD->WARM transition; [`backfill::fetch_reply_chain_backfill`]
//! instead walks a reply chain upward when a mention lands outside a thread,
//! where a time window would pull in unrelated channel chatter.
//! [`backfill::format_backfill`] renders either result into the
//! clearly-delimited block injected ahead of the triggering message.

mod backfill;
mod channel_meta;
mod engagement;
mod gateway_seam;
mod outbound_seam;
mod protocol;
mod runner;
mod security;
mod session;
mod title;

pub(crate) mod outbound;

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use ao_engine_tools_provider_config::{ChannelSecretStore, ChannelSecretStoreError, DISCORD_TOKEN_SECRET_ROLE};
use ao_persistence::PersistenceLayer;
use ao_protocol::agent::{AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig};

use crate::channels::relay::correlation_map::CorrelationMap;
use crate::channels::relay::lease_gate::LeaseGate;
use crate::channels::{ChannelRunContext, ChannelTransport};
use crate::event_bus::EventBus;

use channel_meta::ChannelMetaCache;
use engagement::EngagementTracker;

/// A cheap, non-cryptographic random value in `[0, 1)`, derived from the
/// current instant's sub-second timing. Used only for jitter (the Gateway
/// spec's "jitter the first heartbeat" and the invalid-session reconnect
/// delay) — never for anything security-sensitive, so timing-derived
/// entropy is adequate and avoids pulling in a dedicated `rand` dependency
/// this crate doesn't otherwise need.
pub(super) fn jitter_unit() -> f64 {
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    (nanos % 1_000_000) as f64 / 1_000_000.0
}

/// The origin a bridge thread's most recently dispatched inbound message
/// came from — which Discord channel, whether it was a DM (a DM reply goes
/// back through a user-DM channel, not a guild channel), and which binding
/// delivered it. `binding_id` matters for the reply: unlike Telegram (one
/// bot token per agent), a Discord bot token is scoped per binding — an
/// agent can run more than one Discord bot — so [`super::outbound`] needs it
/// to resolve the right token back out of [`DiscordTransport`]'s secret
/// store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelOrigin {
    pub(crate) channel_id: String,
    pub(crate) binding_id: String,
    pub(crate) is_dm: bool,
}

/// `bridge_thread_id -> ChannelOrigin` for turns this transport just
/// dispatched. Mirrors [`crate::telegram::outbound::InFlightChats`]'s
/// peek-not-take semantics for the same reason: a reply may need to resolve
/// the origin more than once (e.g. a delegate completion re-entering the
/// same thread later), so reading it must never consume it — only explicit
/// invalidation ([`Self::remove`]) does.
///
/// A thin `ChannelOrigin`-specialized wrapper over the shared
/// [`CorrelationMap`], re-exposing `record` at this type's original
/// unpacked-fields signature.
pub(crate) struct InFlightChannels(CorrelationMap<ChannelOrigin>);

impl InFlightChannels {
    pub(crate) fn new() -> Self {
        Self(CorrelationMap::new())
    }

    /// Called by the inbound gateway loop right before it submits a message
    /// onto `thread_id`.
    pub(crate) fn record(&self, thread_id: &str, channel_id: String, binding_id: String, is_dm: bool) {
        self.0.record(thread_id, ChannelOrigin { channel_id, binding_id, is_dm });
    }

    /// Reads the origin mapped to `thread_id` without removing it — see the
    /// struct doc for why a read must never consume the mapping.
    ///
    /// Test-only, and compiled as such. The relay path does not come through
    /// here: the observer resolves the mapping generically over
    /// [`CorrelationMap`] via [`Self::correlation_map`], so this typed
    /// wrapper has no production caller. It stays because this module's own
    /// tests assert the peek-not-consume contract directly against
    /// `InFlightChannels`.
    #[cfg(test)]
    pub(crate) fn peek(&self, thread_id: &str) -> Option<ChannelOrigin> {
        self.0.peek(thread_id)
    }

    /// Unconditionally drops the mapping for `thread_id`. Called when a
    /// binding ends outright — disabled, token rotated away, or deleted.
    pub(crate) fn remove(&self, thread_id: &str) {
        self.0.remove(thread_id);
    }

    /// Exposes the underlying shared map for [`super::outbound`]'s
    /// [`handle_relay_event`](crate::channels::relay::observer::handle_relay_event)
    /// call, which operates generically over [`CorrelationMap`] rather than
    /// this type's unpacked-fields `record`.
    pub(crate) fn correlation_map(&self) -> &CorrelationMap<ChannelOrigin> {
        &self.0
    }
}

/// Discord's [`ChannelTransport`] implementation. One instance serves every
/// Discord binding on every agent — bindings are distinguished by
/// `ChannelRunContext::binding_id`, mirroring how `EmailTransport` serves
/// every email binding through one struct.
pub struct DiscordTransport {
    /// Opened lazily, the first time a fingerprint or spawn call actually
    /// needs a token — see [`Self::secret_store`]. An install with no
    /// Discord agents configured never touches the OS keychain.
    secret_store: OnceLock<ChannelSecretStore>,
    /// Shared REST client: the occasional guild-member-roles lookup a DM
    /// needs when `dm_role_auth_guild` is set (the gateway connection itself
    /// never goes through this), and — via `outbound::run_outbound_observer`
    /// cloning it into a `ReqwestSendSeam` — every outbound reply send.
    /// Carries a fixed request timeout so a hung connection on either path
    /// can never stall the caller indefinitely: the outbound observer in
    /// particular is a single task that awaits each send inline, so an
    /// unbounded hang there would stall every agent's outbound relay at
    /// once, not just the one send.
    http: reqwest::Client,
    in_flight: Arc<InFlightChannels>,
    /// Shared lazy-resolved channel-metadata cache — see
    /// [`channel_meta::resolve_channel_meta`]. Constructed once alongside
    /// `http` above and threaded down through the same gateway-loop
    /// plumbing.
    channel_meta_cache: ChannelMetaCache,
    /// Shared cold/warm engagement state, one tracker per transport instance
    /// (i.e. shared across every binding) — see
    /// [`engagement::EngagementTracker`]. `Arc`-wrapped the same way
    /// `in_flight` is, since the type itself carries no internal `Arc`.
    engagement: Arc<EngagementTracker>,
}

/// Bound on any single REST call this transport makes (guild-member-roles
/// lookup, outbound message send). Generous relative to Discord's own
/// documented response times, but firm enough that one stalled connection
/// can never block the outbound observer — a single shared task — from
/// relaying every other agent's replies.
const HTTP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

impl DiscordTransport {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()
            // `reqwest::Client::builder()` only errors on a broken TLS
            // backend or resolver setup, never on config values like a
            // fixed timeout — the same class of infallible-in-practice
            // construction `reqwest::Client::new()` (which this replaces)
            // already assumed.
            .expect("discord REST client with a fixed timeout must always build");
        Self {
            secret_store: OnceLock::new(),
            http,
            in_flight: Arc::new(InFlightChannels::new()),
            channel_meta_cache: ChannelMetaCache::new(),
            engagement: Arc::new(EngagementTracker::new()),
        }
    }

    fn secret_store(&self) -> Result<&ChannelSecretStore, ChannelSecretStoreError> {
        if let Some(store) = self.secret_store.get() {
            return Ok(store);
        }
        let store = ChannelSecretStore::open()?;
        // Mirrors `EmailTransport::secret_store`'s race handling: at most
        // one caller's `store` wins `set`, everyone reads it back via `get`,
        // and a losing `set` is just a discarded value, not an error.
        let _ = self.secret_store.set(store);
        Ok(self.secret_store.get().expect("secret store was just initialized above"))
    }

    /// Resolves the binding's bot token, logging and returning `None` on any
    /// store failure or absence rather than propagating an error — callers
    /// treat "no token" as "not runnable yet," not a hard failure. Never
    /// logs the token itself, only the outcome.
    fn resolve_token(&self, agent_id: &str, binding_id: &str) -> Option<String> {
        match self.secret_store() {
            Ok(store) => match store.get(agent_id, binding_id, DISCORD_TOKEN_SECRET_ROLE) {
                Ok(token) => token,
                Err(e) => {
                    warn!(agent_id = %agent_id, binding_id = %binding_id, "DiscordTransport: failed to read token: {e}");
                    None
                }
            },
            Err(e) => {
                warn!(agent_id = %agent_id, binding_id = %binding_id, "DiscordTransport: failed to open secret store: {e}");
                None
            }
        }
    }

    /// Ends this transport's outbound-relay binding for `thread_id`
    /// outright. Called whenever a binding is torn down, mirroring
    /// [`crate::telegram::transport::TelegramTransport::invalidate_thread`].
    pub fn invalidate_thread(&self, thread_id: &str) {
        self.in_flight.remove(thread_id);
    }
}

impl Default for DiscordTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelTransport for DiscordTransport {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Discord
    }

    fn fingerprint(&self, agent: &AgentProfile, binding: &ChannelBinding) -> Option<String> {
        let ChannelKindConfig::Discord { .. } = &binding.kind_config else {
            return None;
        };
        // Token is redacted from the fingerprint's tail by construction: the
        // token lives in the secret store, never on `ChannelKindConfig`, so
        // `kind_config`'s Debug output never includes it.
        let token = self.resolve_token(&agent.id, &binding.binding_id)?;
        Some(format!("{token}|{:?}", binding.kind_config))
    }

    fn spawn(&self, ctx: ChannelRunContext, cancel: CancellationToken) -> JoinHandle<()> {
        let token = self.resolve_token(&ctx.agent_id, &ctx.binding_id);
        let http = self.http.clone();
        let in_flight = Arc::clone(&self.in_flight);
        let channel_meta_cache = self.channel_meta_cache.clone();
        let engagement = Arc::clone(&self.engagement);

        tokio::spawn(async move {
            let Some(token) = token else {
                warn!(
                    agent_id = %ctx.agent_id,
                    binding_id = %ctx.binding_id,
                    "DiscordTransport: token unavailable at spawn time, not starting gateway task"
                );
                return;
            };
            runner::run_discord_gateway_loop(ctx, token, http, in_flight, channel_meta_cache, engagement, cancel)
                .await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_peek_does_not_consume_the_mapping() {
        let map = InFlightChannels::new();
        map.record("thread-1", "channel-9".to_string(), "binding-1".to_string(), false);

        let expected = Some(ChannelOrigin {
            channel_id: "channel-9".to_string(),
            binding_id: "binding-1".to_string(),
            is_dm: false,
        });
        assert_eq!(map.peek("thread-1"), expected.clone());
        // Peeking again must return the same value — peek must never consume.
        assert_eq!(map.peek("thread-1"), expected);
    }

    #[test]
    fn peek_on_an_unrecorded_thread_is_none() {
        let map = InFlightChannels::new();
        assert_eq!(map.peek("no-such-thread"), None);
    }

    #[test]
    fn remove_clears_the_mapping() {
        let map = InFlightChannels::new();
        map.record("thread-1", "channel-9".to_string(), "binding-1".to_string(), true);
        map.remove("thread-1");
        assert_eq!(map.peek("thread-1"), None);
    }

    #[test]
    fn recording_again_overwrites_the_prior_mapping_for_the_same_thread() {
        let map = InFlightChannels::new();
        map.record("thread-1", "channel-1".to_string(), "binding-1".to_string(), false);
        map.record("thread-1", "channel-2".to_string(), "binding-2".to_string(), true);
        assert_eq!(
            map.peek("thread-1"),
            Some(ChannelOrigin {
                channel_id: "channel-2".to_string(),
                binding_id: "binding-2".to_string(),
                is_dm: true
            })
        );
    }

    #[test]
    fn jitter_unit_stays_within_the_unit_interval() {
        for _ in 0..20 {
            let v = jitter_unit();
            assert!((0.0..1.0).contains(&v), "jitter_unit produced {v}, expected [0, 1)");
        }
    }
}
