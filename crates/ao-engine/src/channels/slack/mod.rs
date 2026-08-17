//! Slack's [`crate::channels::ChannelTransport`] implementation, plus the
//! setup-time surface it depends on — app manifest generation and the Test Connection checks a
//! user runs after pasting their two tokens back in.
//!
//! `web_api_seam` is the HTTP boundary — [`web_api_seam::SlackApiSeam`] lets
//! [`test_connection::run_test_connection`] and this module's own outbound
//! relay run against a scripted fake in tests, exactly as
//! [`crate::channels::discord::gateway_seam::GatewaySeam`] does for
//! Discord's inbound connection. `fake_seam` is that fake, exported
//! unconditionally (not `#[cfg(test)]`-gated) so `ao-server`'s own
//! integration tests can drive the same Test Connection logic without a
//! live call to `slack.com` — mirroring `ao_process::mock::MockProcessSupervisor`.
//! `test_connection` is the pure per-check orchestration; the manifest
//! generator itself (`ao_protocol::slack_manifest`) needs no HTTP seam at
//! all and lives in `ao-protocol` so `ao-server` can call it directly.
//!
//! `socket_seam` is the Socket Mode network I/O boundary — it owns
//! `apps.connections.open` itself to obtain the `wss://` URL a live
//! connection uses, unlike `web_api_seam::SlackApiSeam::connections_open`'s
//! one-shot setup-time handshake check, which deliberately discards it.
//!
//! `protocol` is the pure wire-format layer on top of `socket_seam`: Socket
//! Mode envelope parsing, the `message`/`app_mention` inbound event shapes,
//! the outbound envelope acknowledgement, and disconnect-reason
//! classification — the Slack analogue of `discord::protocol`.
//!
//! `session` is the pure, bounded `event_id` de-dup set — the in-memory live
//! mirror of the durable `ChannelCursor::Slack` cursor, analogous to the
//! `SeenMessageIds` half of `discord::session` (Socket Mode has no client
//! heartbeat, so there is no zombie-detection counterpart).
//!
//! `security` is the pure fail-closed allow-list check — the Slack analogue
//! of `discord::security::is_allowed`, minus the guild/role concept Slack
//! has no equivalent of.
//!
//! `filter` is the single, pure inbound trigger decision — the "one legible
//! place." It runs on every parsed `protocol::SocketModeEvent` and
//! folds together the bot-echo guard (load-bearing under
//! one-app-per-agent identity) and the trigger scope (`app_mention`, DM,
//! and participating-thread reply — and deliberately nothing else). Both
//! rules sit here together on purpose so a future engagement layer is an
//! insertion into this module, not a rewrite of the runner.
//!
//! `title` is the pure derivation of a fresh bridge thread's `auto_title`
//! from the raw text of the inbound message that minted it — Slack `mrkdwn`
//! markup cleanup on top of the same whitespace-collapse/truncation rule
//! `ao_protocol::thread::derive_auto_title` already applies everywhere else.
//!
//! `runner` is the connect/dispatch/reconnect loop wiring `socket_seam`,
//! `protocol`, `session`, `security`, and `filter` together into one inbound
//! path that hands every admitted event to
//! `crate::channels::submit_inbound_message` — the same shared dispatch call
//! every other channel transport uses. It also owns the conversation
//! registry lookups that resolve which Launchpad thread a Slack conversation
//! maps onto, and records each dispatch into [`SlackTransport`]'s
//! correlation map so the outbound relay below can find its way back.
//! Unlike `discord::runner`, a dead connection is always reconnected from
//! scratch (Socket Mode has no resumable session), and a proactive
//! `disconnect` envelope triggers a brief two-socket warm rotation rather
//! than a naive close-then-reconnect — see the module's own doc comment for
//! why.
//!
//! [`SlackTransport`] is this module's [`crate::channels::ChannelTransport`]
//! impl, tying the pieces above together: [`SlackTransport::fingerprint`]
//! folds the binding's two resolved secrets (`SLACK_BOT_TOKEN_SECRET_ROLE` /
//! `SLACK_APP_TOKEN_SECRET_ROLE`) and config into one change-detection
//! string, [`SlackTransport::spawn`] resolves the workspace connection
//! record and launches `runner::run_slack_socket_mode_loop`,
//! and [`SlackTransport::spawn_outbound_observer`] hands
//! [`SlackTransport`]'s correlation map straight to the fully shared
//! [`crate::channels::relay::observer::run_relay_observer`] — Slack needs no
//! per-channel outer loop of its own ("Slack is then the first
//! consumer of a proven path"), just [`SlackRelaySink`] as the thin adapter
//! that resolves a bot token and sends through `web_api_seam`'s chunked
//! `chat.postMessage`.

pub mod fake_seam;
pub mod filter;
pub mod protocol;
pub mod runner;
pub mod security;
pub mod socket_seam;
pub mod session;
pub mod test_connection;
pub mod title;
pub mod web_api_seam;

#[cfg(test)]
mod end_to_end;

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use ao_engine_tools_provider_config::{
    ChannelSecretStore, ChannelSecretStoreError, SLACK_APP_TOKEN_SECRET_ROLE, SLACK_BOT_TOKEN_SECRET_ROLE,
};
use ao_persistence::PersistenceLayer;
use ao_protocol::agent::{AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig};

use crate::channels::relay::correlation_map::CorrelationMap;
use crate::channels::relay::lease_gate::LeaseGate;
use crate::channels::relay::observer::{run_relay_observer, RelaySink};
use crate::channels::{ChannelRunContext, ChannelTransport};
use crate::event_bus::EventBus;

use socket_seam::{SlackSocketSeam, TungsteniteSlackSocketSeam};
use web_api_seam::{ReqwestSlackApiSeam, SlackApiSeam};

/// The Slack conversation a bridge thread's most recently dispatched inbound
/// message came from — recorded by `runner`'s inbound dispatch and peeked
/// (never consumed — see [`CorrelationMap`]'s module doc) by the outbound
/// relay at every turn's end. Mirrors `discord::ChannelOrigin`.
///
/// `thread_ts` is `None` only for a DM: a DM has no Slack
/// thread of its own, so a DM reply posts at the top level. Every other
/// trigger (`@mention`, thread reply) always carries the conversation's
/// thread root, so the agent's reply lands in-thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackOrigin {
    pub(crate) channel: String,
    pub(crate) thread_ts: Option<String>,
    /// Unlike Telegram (one bot token per agent), a Slack bot token is
    /// scoped per *binding* — needed here so the outbound relay can resolve
    /// the right token back out of [`SlackTransport`]'s secret store.
    pub(crate) binding_id: String,
}

/// Slack's [`ChannelTransport`] implementation. One instance serves every
/// Slack binding on every agent — bindings are distinguished by
/// `ChannelRunContext::binding_id`, mirroring `DiscordTransport`.
pub struct SlackTransport {
    /// Opened lazily, the first time a fingerprint or spawn call actually
    /// needs a secret — see [`Self::secret_store`]. An install with no Slack
    /// agents configured never touches the OS keychain.
    secret_store: OnceLock<ChannelSecretStore>,
    in_flight: Arc<CorrelationMap<SlackOrigin>>,
}

impl SlackTransport {
    pub fn new() -> Self {
        Self { secret_store: OnceLock::new(), in_flight: Arc::new(CorrelationMap::new()) }
    }

    fn secret_store(&self) -> Result<&ChannelSecretStore, ChannelSecretStoreError> {
        if let Some(store) = self.secret_store.get() {
            return Ok(store);
        }
        let store = ChannelSecretStore::open()?;
        // Mirrors `DiscordTransport::secret_store`'s race handling: at most
        // one caller's `store` wins `set`, everyone reads it back via `get`,
        // and a losing `set` is just a discarded value, not an error.
        let _ = self.secret_store.set(store);
        Ok(self.secret_store.get().expect("secret store was just initialized above"))
    }

    /// Resolves one of this binding's two secrets, logging and returning
    /// `None` on any store failure or absence rather than propagating an
    /// error — callers treat "not on file" as "not runnable yet," not a
    /// hard failure. Never logs the secret itself, only the outcome.
    fn resolve_secret(&self, agent_id: &str, binding_id: &str, role: &str) -> Option<String> {
        match self.secret_store() {
            Ok(store) => match store.get(agent_id, binding_id, role) {
                Ok(secret) => secret,
                Err(e) => {
                    warn!(agent_id = %agent_id, binding_id = %binding_id, role, "SlackTransport: failed to read a secret: {e}");
                    None
                }
            },
            Err(e) => {
                warn!(agent_id = %agent_id, binding_id = %binding_id, "SlackTransport: failed to open secret store: {e}");
                None
            }
        }
    }

    /// The bot token (`xoxb-…`) `chat.postMessage` and the rest of the Web
    /// API authenticate with.
    fn resolve_bot_token(&self, agent_id: &str, binding_id: &str) -> Option<String> {
        self.resolve_secret(agent_id, binding_id, SLACK_BOT_TOKEN_SECRET_ROLE)
    }

    /// The app-level token (`xapp-…`) `apps.connections.open` authenticates
    /// with to establish the Socket Mode connection.
    fn resolve_app_token(&self, agent_id: &str, binding_id: &str) -> Option<String> {
        self.resolve_secret(agent_id, binding_id, SLACK_APP_TOKEN_SECRET_ROLE)
    }
}

impl Default for SlackTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelTransport for SlackTransport {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Slack
    }

    fn fingerprint(&self, agent: &AgentProfile, binding: &ChannelBinding) -> Option<String> {
        let ChannelKindConfig::Slack { connection_id, .. } = &binding.kind_config else {
            return None;
        };
        // Not runnable until a successful Test Connection has provisioned
        // the workspace connection record — nothing to
        // resolve a team id or bot user id from yet.
        connection_id.as_ref()?;
        // Both tokens are redacted from the fingerprint's tail by
        // construction: neither lives on `ChannelKindConfig` (see its own
        // doc), only in the secret store, so `kind_config`'s Debug output
        // never includes them. A binding needs both to be runnable at all —
        // the TWO-TOKEN SECRETS constraint — so either one rotating or
        // either one going missing must change (or clear) the fingerprint.
        let bot_token = self.resolve_bot_token(&agent.id, &binding.binding_id)?;
        let app_token = self.resolve_app_token(&agent.id, &binding.binding_id)?;
        Some(format!("{bot_token}|{app_token}|{:?}", binding.kind_config))
    }

    fn spawn(&self, ctx: ChannelRunContext, cancel: CancellationToken) -> JoinHandle<()> {
        let bot_token = self.resolve_bot_token(&ctx.agent_id, &ctx.binding_id);
        let app_token = self.resolve_app_token(&ctx.agent_id, &ctx.binding_id);
        let in_flight = Arc::clone(&self.in_flight);

        tokio::spawn(async move {
            // `bot_token` itself is never read by the inbound loop below
            // (only the outbound relay resolves it, per-relay, off
            // `SlackOrigin::binding_id`) — checked here anyway so a binding
            // missing either half of its two-token pair never starts a
            // socket that can only ever fail to reply.
            if bot_token.is_none() {
                warn!(
                    agent_id = %ctx.agent_id,
                    binding_id = %ctx.binding_id,
                    "SlackTransport: bot token unavailable at spawn time, not starting socket mode task"
                );
                return;
            }
            let Some(app_token) = app_token else {
                warn!(
                    agent_id = %ctx.agent_id,
                    binding_id = %ctx.binding_id,
                    "SlackTransport: app token unavailable at spawn time, not starting socket mode task"
                );
                return;
            };
            let Some((team_id, bot_user_id)) = resolve_connection_identity(&ctx).await else {
                return;
            };

            let seam_factory: runner::SlackSeamFactory =
                Arc::new(|| Box::new(TungsteniteSlackSocketSeam::new()) as Box<dyn SlackSocketSeam>);

            runner::run_slack_socket_mode_loop(ctx, app_token, team_id, bot_user_id, in_flight, seam_factory, cancel)
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
        let in_flight = Arc::clone(&self.in_flight);
        let sink: Arc<dyn RelaySink<SlackOrigin>> = Arc::new(SlackRelaySink { transport: self });
        // Slack is thin enough, thanks to that generalization, to skip a
        // hand-rolled outer `EventBus::subscribe()` loop entirely (unlike
        // Telegram's and Discord's, which still own one for their own
        // per-channel behavior — see `channels::relay::mod`'s doc) and hand
        // the sink straight to the fully shared observer. This is also the
        // one call site that inherits the `LeaseGate` check: `run_relay_observer`
        // consults it (via `handle_relay_event`/`recover_lagged_replies`)
        // before every relay, so a standby process holding no lease for a
        // binding can never emit a duplicate reply through this path.
        Some(tokio::spawn(async move {
            run_relay_observer(persistence, lease_gate, in_flight, sink, event_bus, shutdown_rx).await;
        }))
    }
}

/// Resolves the `(team_id, bot_user_id)` a live socket needs from the
/// binding's referenced [`ao_protocol::slack_connection::SlackConnection`]
/// record — the indirection means `spawn` needs one profile
/// re-read plus one connection-store lookup before it can start the loop,
/// rather than reading either off the binding directly. Returns `None`
/// (logged) when the agent, the binding, its `connection_id`, or the
/// connection record itself isn't there to find — each treated the same as
/// a missing token: not runnable yet, try again next reconcile tick.
async fn resolve_connection_identity(ctx: &ChannelRunContext) -> Option<(String, String)> {
    let profile = match ctx.persistence.agents.get(&ctx.agent_id).await {
        Ok(Some(profile)) => profile,
        Ok(None) => {
            debug!(agent_id = %ctx.agent_id, "SlackTransport: agent no longer exists at spawn time");
            return None;
        }
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, "SlackTransport: failed to read agent profile at spawn time: {e}");
            return None;
        }
    };
    let Some(binding) = profile.channels.iter().find(|b| b.binding_id == ctx.binding_id) else {
        debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: binding no longer exists at spawn time");
        return None;
    };
    let ChannelKindConfig::Slack { connection_id, .. } = &binding.kind_config else {
        warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: binding kind_config is not Slack at spawn time");
        return None;
    };
    let Some(connection_id) = connection_id else {
        warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: no connection_id on binding at spawn time");
        return None;
    };
    match ctx.persistence.slack_connections.get(connection_id).await {
        Ok(Some(connection)) => Some((connection.team_id, connection.bot_user_id)),
        Ok(None) => {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, connection_id, "SlackTransport: connection record not found at spawn time");
            None
        }
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "SlackTransport: failed to read connection record: {e}");
            None
        }
    }
}

/// Bridges [`SlackTransport`]'s token resolution to the shared [`RelaySink`]
/// contract [`run_relay_observer`] drives — Slack's analogue of
/// `discord::outbound::DiscordRelaySink`.
struct SlackRelaySink {
    transport: Arc<SlackTransport>,
}

#[async_trait]
impl RelaySink<SlackOrigin> for SlackRelaySink {
    async fn relay(&self, agent_id: &str, origin: &SlackOrigin, text: &str) {
        let seam = ReqwestSlackApiSeam::new();
        relay_reply(&self.transport, &seam, agent_id, origin, text).await;
    }
}

/// Resolves `origin.binding_id`'s bot token and sends `text` to
/// `origin.channel` (threaded under `origin.thread_ts` when set), chunked to
/// Slack's message-length limit via [`web_api_seam::send_chunked_message`].
/// Mirrors [`crate::channels::discord::outbound::relay_reply`]: any failure
/// — missing token, network error, a rejected chunk — is logged and
/// swallowed here rather than propagated, since [`RelaySink::relay`] must
/// never crash the turn, the thread, or the process. The token itself is
/// never logged, only the outcome. Split out from [`SlackRelaySink::relay`]
/// so tests can drive it directly against a [`fake_seam::FakeSlackApiSeam`]
/// instead of a real [`ReqwestSlackApiSeam`].
async fn relay_reply(transport: &SlackTransport, seam: &dyn SlackApiSeam, agent_id: &str, origin: &SlackOrigin, text: &str) {
    let Some(bot_token) = transport.resolve_bot_token(agent_id, &origin.binding_id) else {
        warn!(agent_id = %agent_id, binding_id = %origin.binding_id, "SlackBridge: no bot token on file, cannot relay reply");
        return;
    };
    if let Err(e) =
        web_api_seam::send_chunked_message(seam, &bot_token, &origin.channel, origin.thread_ts.as_deref(), text).await
    {
        warn!(agent_id = %agent_id, channel = %origin.channel, "SlackBridge: failed to relay reply to Slack: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ao_protocol::agent::SlackConversationMode;

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

    // `LAUNCHPAD_STUDIO_DATA_DIR` is mutated by tests across this crate, so
    // this must serialize through the one crate-wide env lock — see
    // `discord::outbound`'s identical `lock_env` for the full rationale.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        crate::plugin_paths::tests::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_secret(agent_id: &str, binding_id: &str, role: &str, value: &str) {
        ChannelSecretStore::open().expect("secret store opens").set(agent_id, binding_id, role, value).expect("secret stored");
    }

    fn slack_binding(connection_id: Option<&str>) -> ChannelBinding {
        ChannelBinding {
            binding_id: "slack".to_string(),
            kind: ChannelKind::Slack,
            enabled: true,
            bridge_thread_id: None,
            allowed_senders: vec![],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Slack {
                allowed_channels: vec!["C123".to_string()],
                allowed_users: vec![],
                connection_id: connection_id.map(str::to_string),
                conversation_mode: SlackConversationMode::PerConversation,
            },
        }
    }

    fn make_agent(binding: ChannelBinding) -> AgentProfile {
        use ao_protocol::agent::{CliProviderConfig, InputMode, OutputFormat, ProviderConfig};

        AgentProfile {
            id: "agent-1".to_string(),
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
            channels: vec![binding],
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            max_turns: None,
}
    }

    // --- kind / invalidate_thread -------------------------------------------

    #[test]
    fn kind_is_slack() {
        assert_eq!(SlackTransport::new().kind(), ChannelKind::Slack);
    }

    #[test]
    fn invalidate_thread_clears_a_recorded_mapping() {
        let transport = SlackTransport::new();
        transport.in_flight.record(
            "thread-1",
            SlackOrigin { channel: "C123".to_string(), thread_ts: None, binding_id: "slack".to_string() },
        );
        transport.invalidate_thread("thread-1");
        assert_eq!(transport.in_flight.peek("thread-1"), None);
    }

    // --- fingerprint ---------------------------------------------------------

    #[test]
    fn fingerprint_is_none_without_a_connection_id() {
        let _guard = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);

        let agent = make_agent(slack_binding(None));
        let transport = SlackTransport::new();
        assert_eq!(transport.fingerprint(&agent, &agent.channels[0]), None);
    }

    #[test]
    fn fingerprint_is_none_without_either_token_on_file() {
        let _guard = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);

        let agent = make_agent(slack_binding(Some("conn-1")));
        let transport = SlackTransport::new();
        assert_eq!(
            transport.fingerprint(&agent, &agent.channels[0]),
            None,
            "neither token is on file yet — not runnable"
        );

        set_secret("agent-1", "slack", SLACK_BOT_TOKEN_SECRET_ROLE, "xoxb-fake");
        assert_eq!(
            transport.fingerprint(&agent, &agent.channels[0]),
            None,
            "only the bot token is on file — still missing the app token"
        );
    }

    #[test]
    fn fingerprint_is_some_once_both_tokens_are_on_file_and_changes_on_rotation() {
        let _guard = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);
        set_secret("agent-1", "slack", SLACK_BOT_TOKEN_SECRET_ROLE, "xoxb-fake");
        set_secret("agent-1", "slack", SLACK_APP_TOKEN_SECRET_ROLE, "xapp-fake");

        let agent = make_agent(slack_binding(Some("conn-1")));
        let transport = SlackTransport::new();
        let first = transport.fingerprint(&agent, &agent.channels[0]);
        assert!(first.is_some());

        set_secret("agent-1", "slack", SLACK_BOT_TOKEN_SECRET_ROLE, "xoxb-rotated");
        let second = transport.fingerprint(&agent, &agent.channels[0]);
        assert!(second.is_some());
        assert_ne!(first, second, "rotating either token must change the fingerprint");
    }

    // --- relay_reply -----------------------------------------------------

    fn slack_origin(channel: &str, thread_ts: Option<&str>) -> SlackOrigin {
        SlackOrigin { channel: channel.to_string(), thread_ts: thread_ts.map(str::to_string), binding_id: "slack".to_string() }
    }

    #[tokio::test]
    async fn relay_reply_sends_through_the_seam_when_a_token_is_on_file() {
        let _guard = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);
        set_secret("agent-1", "slack", SLACK_BOT_TOKEN_SECRET_ROLE, "xoxb-fake");

        let transport = SlackTransport::new();
        let seam = fake_seam::FakeSlackApiSeam::new(
            Err(web_api_seam::SlackApiCallError::Auth("unused".to_string())),
            Ok(()),
        );

        relay_reply(&transport, &seam, "agent-1", &slack_origin("C123", Some("111.000")), "hello there").await;

        let calls = seam.post_message_calls();
        assert_eq!(calls, vec![("C123".to_string(), Some("111.000".to_string()), "hello there".to_string())]);
    }

    #[tokio::test]
    async fn relay_reply_does_nothing_when_no_token_is_on_file() {
        let _guard = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);

        let transport = SlackTransport::new();
        let seam = fake_seam::FakeSlackApiSeam::new(
            Err(web_api_seam::SlackApiCallError::Auth("unused".to_string())),
            Ok(()),
        );

        relay_reply(&transport, &seam, "agent-1", &slack_origin("C123", None), "hello there").await;

        assert!(seam.post_message_calls().is_empty(), "no token on file must never reach the seam");
    }

    #[tokio::test]
    async fn relay_reply_logs_and_swallows_a_seam_failure() {
        let _guard = lock_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set(&[
            (ao_protocol::data_root::DATA_DIR_ENV_VAR, tmp.path().to_str().unwrap()),
            ("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1"),
        ]);
        set_secret("agent-1", "slack", SLACK_BOT_TOKEN_SECRET_ROLE, "xoxb-fake");

        let transport = SlackTransport::new();
        let seam = fake_seam::FakeSlackApiSeam::new(
            Err(web_api_seam::SlackApiCallError::Auth("unused".to_string())),
            Ok(()),
        )
        .with_post_message_result(Err(web_api_seam::SlackApiCallError::Network("boom".to_string())));

        // Must not panic — the failure is logged and swallowed, same as a
        // missing token.
        relay_reply(&transport, &seam, "agent-1", &slack_origin("C123", None), "hello there").await;
    }
}
