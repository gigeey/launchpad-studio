//! Generic per-agent channel-binding endpoints, plus the Email-specific
//! config/secret/delete surface.
//!
//! Telegram keeps its own dedicated routes (`routes::telegram`) — this
//! module only adds the pieces Telegram doesn't need: a unified read of
//! every binding on an agent, and the setup surface for an Email binding.
//! Both live here rather than growing Telegram's file into a mixed-kind one.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use ao_engine::channels::slack::test_connection::run_test_connection;
use ao_engine::channels::slack::web_api_seam::{ReqwestSlackApiSeam, SlackApiSeam};
use ao_engine::telegram::ChannelBridge;
use ao_engine::AppState;
use ao_engine_tools_provider_config::{
    ChannelSecretStore, ChannelSecretStoreError, TelegramTokenStore, TelegramTokenStoreError,
    DISCORD_TOKEN_SECRET_ROLE, EMAIL_PASSWORD_SECRET_ROLE, SLACK_APP_TOKEN_SECRET_ROLE,
    SLACK_BOT_TOKEN_SECRET_ROLE,
};
use ao_persistence::linked_sender_store::LinkedSenderStore;
use ao_protocol::agent::{
    AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig, SlackConversationMode, ThreadFollowMode,
};
use ao_protocol::channel_connection_state::ChannelConnectionState;
use ao_protocol::error::AoError;
use ao_protocol::linked_sender_list::LinkedSenderList;
use ao_protocol::slack_connection::SlackConnection;
use ao_protocol::slack_test_connection::SlackTestConnectionReport;

use crate::channel_provisioning::provision_bridge_thread;
use crate::error::AppError;

/// Deterministic id for an agent's (at most one, today) Email binding,
/// mirroring Telegram's fixed `"telegram"` binding id.
const EMAIL_BINDING_ID: &str = "email";

/// Deterministic id for an agent's (at most one, today) Discord binding,
/// same convention as [`EMAIL_BINDING_ID`]. The underlying secret store is
/// keyed by `binding_id` (so a future multi-bot-per-agent surface is not
/// precluded at the storage layer), but this HTTP surface — mirroring
/// Email's — only manages a single Discord binding per agent today.
const DISCORD_BINDING_ID: &str = "discord";

/// Deterministic id for an agent's (at most one, today) Slack binding, same
/// convention as [`DISCORD_BINDING_ID`] / [`EMAIL_BINDING_ID`].
const SLACK_BINDING_ID: &str = "slack";

/// Required prefix for a Slack bot token (`xoxb-…`), checked by
/// [`set_slack_channel_secret`] before it's ever stored.
const SLACK_BOT_TOKEN_PREFIX: &str = "xoxb-";

/// Required prefix for a Slack app-level token (`xapp-…`), checked by
/// [`set_slack_channel_secret`] before it's ever stored.
const SLACK_APP_TOKEN_PREFIX: &str = "xapp-";

/// Default poll interval for a freshly created, not-yet-configured Email
/// binding. Overwritten by the first `PUT .../channels/email`.
const DEFAULT_POLL_SECS: u32 = 300;

/// The `kind_config` an Email binding gets before `upsert_email_channel`
/// has ever saved real IMAP/SMTP settings. Shared by
/// [`email_binding_mut_or_default`] (which persists a binding with these
/// defaults) and `set_email_channel_secret`'s no-binding-yet response
/// (which reports these defaults back without persisting anything), so the
/// two "what does an unconfigured Email binding look like" answers can't
/// drift apart.
fn default_email_kind_config() -> ChannelKindConfig {
    ChannelKindConfig::Email {
        address: String::new(),
        imap_host: String::new(),
        imap_port: 0,
        smtp_host: String::new(),
        smtp_port: 0,
        poll_secs: DEFAULT_POLL_SECS,
        require_auth_results: true,
    }
}

/// The `kind_config` a Discord binding gets before `upsert_discord_channel`
/// has ever saved real config — mirrors [`default_email_kind_config`].
/// Every engagement-gating field here reproduces the exact defaults
/// `ChannelKindConfig::Discord`'s own `#[serde(default = ...)]` attributes
/// use, so a freshly-inserted binding and one deserialized from a config
/// predating these fields behave identically.
fn default_discord_kind_config() -> ChannelKindConfig {
    ChannelKindConfig::Discord {
        allowed_users: Vec::new(),
        allowed_roles: Vec::new(),
        allowed_channels: Vec::new(),
        dm_role_auth_guild: None,
        require_mention: default_true(),
        thread_follow: ThreadFollowMode::default(),
        thread_idle_timeout_minutes: default_thread_idle_timeout_minutes(),
        thread_message_budget: default_thread_message_budget(),
        backfill_limit: default_backfill_limit(),
    }
}

/// The `kind_config` a Slack binding gets before `upsert_slack_channel` has
/// ever saved real config — mirrors [`default_discord_kind_config`].
/// `connection_id` starts `None`: it's only ever set by a future Test
/// Connection flow, never by this route.
fn default_slack_kind_config() -> ChannelKindConfig {
    ChannelKindConfig::Slack {
        allowed_channels: Vec::new(),
        allowed_users: Vec::new(),
        connection_id: None,
        conversation_mode: SlackConversationMode::default(),
    }
}

/// Returns a mutable reference to `profile`'s Email binding, inserting a
/// disabled, unconfigured one first if it doesn't have one yet. Defaults
/// `require_auth_results` to `true` — an unconfigured inbox should reject
/// unauthenticated senders once it does start accepting mail, not silently
/// accept spoofable ones.
///
/// Only call this where fabricating *and persisting* a binding is actually
/// wanted (today: [`upsert_email_channel`], which immediately overwrites
/// every field including `allowed_senders` with the caller-supplied config).
/// A caller that only needs to *check for* an existing binding — without
/// ever persisting a freshly-defaulted, empty-allow-list one — should use
/// `profile.channel_of_kind_mut(ChannelKind::Email)` directly instead, the
/// same way `set_email_channel_secret` does.
fn email_binding_mut_or_default(profile: &mut AgentProfile) -> &mut ChannelBinding {
    if profile.channel_of_kind(ChannelKind::Email).is_none() {
        profile.channels.push(ChannelBinding {
            binding_id: EMAIL_BINDING_ID.to_string(),
            kind: ChannelKind::Email,
            enabled: false,
            bridge_thread_id: None,
            allowed_senders: Vec::new(),
            pending_pairing_code: None,
            kind_config: default_email_kind_config(),
        });
    }
    profile
        .channel_of_kind_mut(ChannelKind::Email)
        .expect("just inserted above if missing")
}

/// Returns a mutable reference to `profile`'s Discord binding, inserting a
/// disabled, unconfigured one first if it doesn't have one yet. Empty
/// `allowed_users`/`allowed_roles` on the fresh default fails closed (no
/// sender is authorized) exactly like `security::is_allowed` requires.
fn discord_binding_mut_or_default(profile: &mut AgentProfile) -> &mut ChannelBinding {
    if profile.channel_of_kind(ChannelKind::Discord).is_none() {
        profile.channels.push(ChannelBinding {
            binding_id: DISCORD_BINDING_ID.to_string(),
            kind: ChannelKind::Discord,
            enabled: false,
            bridge_thread_id: None,
            allowed_senders: Vec::new(),
            pending_pairing_code: None,
            kind_config: default_discord_kind_config(),
        });
    }
    profile
        .channel_of_kind_mut(ChannelKind::Discord)
        .expect("just inserted above if missing")
}

/// Returns a mutable reference to `profile`'s Slack binding, inserting a
/// disabled, unconfigured one first if it doesn't have one yet. Empty
/// `allowed_channels`/`allowed_users` on the fresh default fails closed, same
/// as Discord's binding-mut-or-default helper.
fn slack_binding_mut_or_default(profile: &mut AgentProfile) -> &mut ChannelBinding {
    if profile.channel_of_kind(ChannelKind::Slack).is_none() {
        profile.channels.push(ChannelBinding {
            binding_id: SLACK_BINDING_ID.to_string(),
            kind: ChannelKind::Slack,
            enabled: false,
            bridge_thread_id: None,
            allowed_senders: Vec::new(),
            pending_pairing_code: None,
            kind_config: default_slack_kind_config(),
        });
    }
    profile
        .channel_of_kind_mut(ChannelKind::Slack)
        .expect("just inserted above if missing")
}

fn map_secret_store_err(e: ChannelSecretStoreError) -> AppError {
    AppError(AoError::Internal(format!("channel secret store: {e}")))
}

fn map_telegram_store_err(e: TelegramTokenStoreError) -> AppError {
    AppError(AoError::Internal(format!("telegram token store: {e}")))
}

/// Whether a secret is on file for `binding`, without ever reading its
/// value back into a response. Each channel kind currently keeps its
/// secret(s) in its own store — Telegram predates [`ChannelSecretStore`]
/// and still uses [`TelegramTokenStore`] — so this dispatches on kind
/// rather than assuming one shared backend.
fn secret_stored_for(agent_id: &str, binding: &ChannelBinding) -> Result<bool, AppError> {
    match binding.kind {
        ChannelKind::Telegram => {
            let store = TelegramTokenStore::open().map_err(map_telegram_store_err)?;
            Ok(store.get(agent_id).map_err(map_telegram_store_err)?.is_some())
        }
        ChannelKind::Email => {
            let store = ChannelSecretStore::open().map_err(map_secret_store_err)?;
            Ok(store
                .get(agent_id, &binding.binding_id, EMAIL_PASSWORD_SECRET_ROLE)
                .map_err(map_secret_store_err)?
                .is_some())
        }
        ChannelKind::Discord => {
            let store = ChannelSecretStore::open().map_err(map_secret_store_err)?;
            Ok(store
                .get(agent_id, &binding.binding_id, DISCORD_TOKEN_SECRET_ROLE)
                .map_err(map_secret_store_err)?
                .is_some())
        }
        // Slack holds two secrets under distinct roles (bot + app token).
        // `secret_stored` is a single boolean, so it reports "fully
        // configured" (both present) rather than "at least one present" —
        // partial credentials can't open a Socket Mode connection, and a
        // partial-but-true reading here would tell the UI Slack is ready
        // when it isn't.
        ChannelKind::Slack => {
            let store = ChannelSecretStore::open().map_err(map_secret_store_err)?;
            let bot_token_stored = store
                .get(agent_id, &binding.binding_id, SLACK_BOT_TOKEN_SECRET_ROLE)
                .map_err(map_secret_store_err)?
                .is_some();
            let app_token_stored = store
                .get(agent_id, &binding.binding_id, SLACK_APP_TOKEN_SECRET_ROLE)
                .map_err(map_secret_store_err)?
                .is_some();
            Ok(bot_token_stored && app_token_stored)
        }
        // No secret-backed transport exists yet for these kinds.
        ChannelKind::WhatsApp | ChannelKind::Webhook => Ok(false),
    }
}

#[derive(Debug, Serialize)]
pub struct ChannelStatusResponse {
    pub binding_id: String,
    pub kind: ChannelKind,
    pub enabled: bool,
    /// Whether the binding's dedicated bridge thread has been provisioned —
    /// deliberately a bool rather than the thread id, since this surface
    /// doesn't need callers navigating to the thread.
    pub bridge_thread_provisioned: bool,
    pub allowed_senders: Vec<String>,
    /// Whether a secret is stored for this binding. Never the secret itself.
    pub secret_stored: bool,
    pub kind_config: ChannelKindConfig,
    /// Honest per-binding connection state — see
    /// [`ChannelConnectionState`]'s doc for what each value means, in
    /// particular why `not-holding-lease` isn't an error state.
    pub connection_state: ChannelConnectionState,
}

/// Builds the status response for one binding. `allowed_senders` is read
/// through [`LinkedSenderStore`] (falling back to — and backfilling from —
/// the deprecated inline `ChannelBinding::allowed_senders` for a binding
/// that predates the store), the same source of truth `get_channel_senders`
/// uses: the inline field is never populated by the current write paths
/// (`upsert_email_channel` et al. write straight to the store, per
/// `ChannelBinding::allowed_senders`'s doc), so reading it here would report
/// an empty allow-list for every binding configured since that migration.
async fn channel_status_response(
    agent_id: &str,
    binding: &ChannelBinding,
    bridge: &ChannelBridge,
    linked_senders: &LinkedSenderStore,
) -> Result<ChannelStatusResponse, AppError> {
    let allowed_senders = linked_senders
        .get_or_backfill(agent_id, &binding.binding_id, &binding.allowed_senders)
        .await?;
    Ok(ChannelStatusResponse {
        binding_id: binding.binding_id.clone(),
        kind: binding.kind,
        enabled: binding.enabled,
        bridge_thread_provisioned: binding.bridge_thread_id.is_some(),
        allowed_senders,
        secret_stored: secret_stored_for(agent_id, binding)?,
        kind_config: binding.kind_config.clone(),
        connection_state: bridge.connection_state(agent_id, &binding.binding_id),
    })
}

/// `GET /agents/{agent_id}/channels` — non-secret status for every channel
/// binding on the agent (Telegram included, for a unified view). Never
/// returns a secret value, only whether one is on file.
pub async fn list_channels(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<ChannelStatusResponse>>, AppError> {
    let profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let mut statuses = Vec::with_capacity(profile.channels.len());
    for binding in &profile.channels {
        statuses.push(
            channel_status_response(&agent_id, binding, &state.telegram_bridge, &state.persistence.linked_senders)
                .await?,
        );
    }

    Ok(Json(statuses))
}

#[derive(Debug, Serialize)]
pub struct SendersResponse {
    pub senders: Vec<String>,
}

/// `GET /agents/{agent_id}/channels/{binding_id}/senders` — one binding's
/// [`LinkedSenderStore`](ao_persistence::linked_sender_store::LinkedSenderStore)
/// allow-list. Falls back to the deprecated inline
/// `ChannelBinding::allowed_senders` for a binding that predates the store
/// and hasn't had an enforcement read backfill it yet — see that field's
/// doc.
pub async fn get_channel_senders(
    State(state): State<Arc<AppState>>,
    Path((agent_id, binding_id)): Path<(String, String)>,
) -> Result<Json<SendersResponse>, AppError> {
    if let Some(list) = state.persistence.linked_senders.get(&agent_id, &binding_id).await? {
        return Ok(Json(SendersResponse { senders: list.senders }));
    }
    let profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;
    let inline = profile
        .channels
        .iter()
        .find(|b| b.binding_id == binding_id)
        .map(|b| b.allowed_senders.clone())
        .unwrap_or_default();
    Ok(Json(SendersResponse { senders: inline }))
}

#[derive(Debug, Deserialize)]
pub struct SetChannelSendersRequest {
    #[serde(default)]
    pub senders: Vec<String>,
}

/// `PUT /agents/{agent_id}/channels/{binding_id}/senders` — clobber-free
/// direct edit of one binding's allow-list. Unlike the general
/// `PUT /agents/{id}` profile save, this never round-trips the whole
/// profile document, so it can't race the Telegram pairing writer (or any
/// other out-of-band linker) the way `ChannelBinding::allowed_senders` used
/// to.
pub async fn set_channel_senders(
    State(state): State<Arc<AppState>>,
    Path((agent_id, binding_id)): Path<(String, String)>,
    Json(body): Json<SetChannelSendersRequest>,
) -> Result<Json<SendersResponse>, AppError> {
    let list = LinkedSenderList { senders: body.senders };
    state.persistence.linked_senders.set(&agent_id, &binding_id, &list).await?;
    Ok(Json(SendersResponse { senders: list.senders }))
}

#[derive(Debug, Deserialize)]
pub struct UpsertEmailChannelRequest {
    pub address: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub poll_secs: u32,
    pub require_auth_results: bool,
    #[serde(default)]
    pub allowed_senders: Vec<String>,
    pub enabled: bool,
}

/// `PUT /agents/{agent_id}/channels/email` — create or update the agent's
/// Email binding config (everything except the password). Enabling it here
/// provisions the binding's dedicated bridge thread in the same request
/// (see [`provision_bridge_thread`]) — the same atomicity Telegram's token
/// endpoint provides, so a binding enabled purely through this route never
/// waits on some later, unrelated profile save to start polling.
pub async fn upsert_email_channel(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(body): Json<UpsertEmailChannelRequest>,
) -> Result<Json<ChannelStatusResponse>, AppError> {
    let address = body.address.trim().to_string();
    if address.is_empty() || !address.contains('@') {
        return Err(AppError(AoError::ValidationError(
            "a valid email address is required".to_string(),
        )));
    }
    let imap_host = body.imap_host.trim().to_string();
    if imap_host.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "imap_host must not be empty".to_string(),
        )));
    }
    let smtp_host = body.smtp_host.trim().to_string();
    if smtp_host.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "smtp_host must not be empty".to_string(),
        )));
    }
    if body.imap_port == 0 {
        return Err(AppError(AoError::ValidationError(
            "imap_port must be nonzero".to_string(),
        )));
    }
    if body.smtp_port == 0 {
        return Err(AppError(AoError::ValidationError(
            "smtp_port must be nonzero".to_string(),
        )));
    }
    if body.poll_secs == 0 {
        return Err(AppError(AoError::ValidationError(
            "poll_secs must be nonzero".to_string(),
        )));
    }

    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let binding = email_binding_mut_or_default(&mut profile);
    binding.kind_config = ChannelKindConfig::Email {
        address,
        imap_host,
        imap_port: body.imap_port,
        smtp_host,
        smtp_port: body.smtp_port,
        poll_secs: body.poll_secs,
        require_auth_results: body.require_auth_results,
    };
    binding.enabled = body.enabled;
    let binding_id = binding.binding_id.clone();
    provision_bridge_thread(&state, &agent_id, binding).await?;

    state.persistence.agents.update(&profile).await?;
    // The submitted allow-list lands in LinkedSenderStore, not the profile
    // document — see `ChannelBinding::allowed_senders`'s doc for why a
    // client-submitted profile save must never be the thing that sets it.
    state
        .persistence
        .linked_senders
        .set(&agent_id, &binding_id, &LinkedSenderList { senders: body.allowed_senders })
        .await?;

    let binding = profile
        .channel_of_kind(ChannelKind::Email)
        .expect("just upserted above");
    Ok(Json(
        channel_status_response(&agent_id, binding, &state.telegram_bridge, &state.persistence.linked_senders).await?,
    ))
}

#[derive(Debug, Deserialize)]
pub struct SetEmailChannelSecretRequest {
    pub password: String,
}

/// `PUT /agents/{agent_id}/channels/email/secret` — write-only: stores the
/// IMAP/SMTP password in [`ChannelSecretStore`] and never echoes it back.
/// Also provisions the bridge thread if the binding is already enabled but
/// somehow not yet provisioned, so setting credentials and enabling in one
/// user flow reliably ends with a running poll loop rather than depending
/// on this being called strictly after the enabling PUT.
///
/// Deliberately does **not** call [`email_binding_mut_or_default`]. If no
/// Email binding has been saved yet (i.e. the caller is setting the
/// password before ever PUTting `.../channels/email`), fabricating one here
/// and persisting it via `agents.update` would write a binding with an
/// empty `allowed_senders` to disk — and an empty allow-list is fail-closed
/// (see `ao_engine::channels::email::security`), so it silently black-holes
/// every inbound message. The password itself lives in a separate
/// [`ChannelSecretStore`] keyed by the deterministic [`EMAIL_BINDING_ID`],
/// so storing it never requires a profile binding to exist. When no binding
/// exists yet, this stores the secret and reports it as stored without
/// touching `profile.channels` at all — `upsert_email_channel` remains the
/// only path that creates/persists the binding.
pub async fn set_email_channel_secret(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(body): Json<SetEmailChannelSecretRequest>,
) -> Result<Json<ChannelStatusResponse>, AppError> {
    let password = body.password.trim();
    if password.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "password must not be empty".to_string(),
        )));
    }

    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let store = ChannelSecretStore::open().map_err(map_secret_store_err)?;
    store
        .set(&agent_id, EMAIL_BINDING_ID, EMAIL_PASSWORD_SECRET_ROLE, password)
        .map_err(map_secret_store_err)?;

    match profile.channel_of_kind_mut(ChannelKind::Email) {
        Some(binding) => {
            provision_bridge_thread(&state, &agent_id, binding).await?;
            state.persistence.agents.update(&profile).await?;

            let binding = profile
                .channel_of_kind(ChannelKind::Email)
                .expect("just set secret above");
            Ok(Json(
                channel_status_response(&agent_id, binding, &state.telegram_bridge, &state.persistence.linked_senders)
                    .await?,
            ))
        }
        None => Ok(Json(ChannelStatusResponse {
            binding_id: EMAIL_BINDING_ID.to_string(),
            kind: ChannelKind::Email,
            enabled: false,
            bridge_thread_provisioned: false,
            allowed_senders: Vec::new(),
            secret_stored: true,
            kind_config: default_email_kind_config(),
            connection_state: state.telegram_bridge.connection_state(&agent_id, EMAIL_BINDING_ID),
        })),
    }
}

/// `DELETE /agents/{agent_id}/channels/email` — removes the Email binding
/// from `profile.channels`, deletes its stored secret, and best-effort
/// invalidates its bridge thread's outbound-relay mapping. Idempotent: a
/// no-op (still 204) when the agent has no Email binding.
pub async fn delete_email_channel(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<StatusCode, AppError> {
    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    if let Some(binding) = profile.channel_of_kind(ChannelKind::Email).cloned() {
        if let Some(bridge_thread_id) = binding.bridge_thread_id.as_deref() {
            state.telegram_bridge.invalidate_thread(bridge_thread_id);
        }

        let store = ChannelSecretStore::open().map_err(map_secret_store_err)?;
        store
            .delete(&agent_id, &binding.binding_id, EMAIL_PASSWORD_SECRET_ROLE)
            .map_err(map_secret_store_err)?;

        profile.channels.retain(|b| b.kind != ChannelKind::Email);
        state.persistence.agents.update(&profile).await?;
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct UpsertDiscordChannelRequest {
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub allowed_roles: Vec<String>,
    #[serde(default)]
    pub allowed_channels: Vec<String>,
    #[serde(default)]
    pub dm_role_auth_guild: Option<String>,
    /// Same defaults as `ChannelKindConfig::Discord`'s own fields — see
    /// `default_discord_kind_config` for why a request omitting these must
    /// behave exactly like a persisted profile predating them.
    #[serde(default = "default_true")]
    pub require_mention: bool,
    #[serde(default)]
    pub thread_follow: ThreadFollowMode,
    #[serde(default = "default_thread_idle_timeout_minutes")]
    pub thread_idle_timeout_minutes: u32,
    #[serde(default = "default_thread_message_budget")]
    pub thread_message_budget: u32,
    #[serde(default = "default_backfill_limit")]
    pub backfill_limit: u32,
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_thread_idle_timeout_minutes() -> u32 {
    15
}

fn default_thread_message_budget() -> u32 {
    10
}

fn default_backfill_limit() -> u32 {
    20
}

/// `PUT /agents/{agent_id}/channels/discord` — create or update the agent's
/// Discord binding config (everything except the bot token). Enabling it
/// here provisions the binding's dedicated bridge thread in the same
/// request (see [`provision_bridge_thread`]), same atomicity guarantee as
/// Email's and Telegram's equivalent endpoints.
pub async fn upsert_discord_channel(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(body): Json<UpsertDiscordChannelRequest>,
) -> Result<Json<ChannelStatusResponse>, AppError> {
    let dm_role_auth_guild = match body.dm_role_auth_guild {
        Some(guild) => {
            let trimmed = guild.trim().to_string();
            if trimmed.is_empty() {
                return Err(AppError(AoError::ValidationError(
                    "dm_role_auth_guild must not be blank".to_string(),
                )));
            }
            Some(trimmed)
        }
        None => None,
    };

    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let binding = discord_binding_mut_or_default(&mut profile);
    binding.kind_config = ChannelKindConfig::Discord {
        allowed_users: body.allowed_users,
        allowed_roles: body.allowed_roles,
        allowed_channels: body.allowed_channels,
        dm_role_auth_guild,
        require_mention: body.require_mention,
        thread_follow: body.thread_follow,
        thread_idle_timeout_minutes: body.thread_idle_timeout_minutes,
        thread_message_budget: body.thread_message_budget,
        backfill_limit: body.backfill_limit,
    };
    binding.enabled = body.enabled;
    provision_bridge_thread(&state, &agent_id, binding).await?;

    state.persistence.agents.update(&profile).await?;

    let binding = profile
        .channel_of_kind(ChannelKind::Discord)
        .expect("just upserted above");
    Ok(Json(
        channel_status_response(&agent_id, binding, &state.telegram_bridge, &state.persistence.linked_senders).await?,
    ))
}

#[derive(Debug, Deserialize)]
pub struct SetDiscordChannelSecretRequest {
    pub bot_token: String,
}

/// `PUT /agents/{agent_id}/channels/discord/secret` — write-only: stores the
/// bot token in [`ChannelSecretStore`] under [`DISCORD_TOKEN_SECRET_ROLE`]
/// and never echoes it back. Also provisions the bridge thread if the
/// binding is already enabled but somehow not yet provisioned, mirroring
/// Email's secret endpoint.
pub async fn set_discord_channel_secret(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(body): Json<SetDiscordChannelSecretRequest>,
) -> Result<Json<ChannelStatusResponse>, AppError> {
    let bot_token = body.bot_token.trim();
    if bot_token.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "bot_token must not be empty".to_string(),
        )));
    }

    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let binding = discord_binding_mut_or_default(&mut profile);
    let binding_id = binding.binding_id.clone();

    let store = ChannelSecretStore::open().map_err(map_secret_store_err)?;
    store
        .set(&agent_id, &binding_id, DISCORD_TOKEN_SECRET_ROLE, bot_token)
        .map_err(map_secret_store_err)?;

    provision_bridge_thread(&state, &agent_id, binding).await?;
    state.persistence.agents.update(&profile).await?;

    let binding = profile
        .channel_of_kind(ChannelKind::Discord)
        .expect("just set secret above");
    Ok(Json(
        channel_status_response(&agent_id, binding, &state.telegram_bridge, &state.persistence.linked_senders).await?,
    ))
}

/// `DELETE /agents/{agent_id}/channels/discord` — removes the Discord
/// binding from `profile.channels`, deletes its stored bot token, and
/// best-effort invalidates its bridge thread's outbound-relay mapping.
/// Idempotent: a no-op (still 204) when the agent has no Discord binding.
pub async fn delete_discord_channel(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<StatusCode, AppError> {
    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    if let Some(binding) = profile.channel_of_kind(ChannelKind::Discord).cloned() {
        if let Some(bridge_thread_id) = binding.bridge_thread_id.as_deref() {
            state.telegram_bridge.invalidate_thread(bridge_thread_id);
        }

        let store = ChannelSecretStore::open().map_err(map_secret_store_err)?;
        store
            .delete(&agent_id, &binding.binding_id, DISCORD_TOKEN_SECRET_ROLE)
            .map_err(map_secret_store_err)?;

        profile.channels.retain(|b| b.kind != ChannelKind::Discord);
        state.persistence.agents.update(&profile).await?;
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct UpsertSlackChannelRequest {
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub allowed_channels: Vec<String>,
    #[serde(default)]
    pub conversation_mode: SlackConversationMode,
    pub enabled: bool,
}

/// `PUT /agents/{agent_id}/channels/slack` — create or update the agent's
/// Slack binding config (everything except the two tokens). Enabling it
/// here provisions the binding's dedicated bridge thread in the same
/// request (see [`provision_bridge_thread`]), same atomicity guarantee as
/// Email's/Discord's equivalent endpoints.
///
/// Deliberately preserves whatever `connection_id` the binding already had
/// (`None` for a fresh binding) rather than accepting one in the request —
/// that field is the connection reference a future Test Connection flow
/// populates, and this route only ever touches the fields a caller can
/// actually supply here.
pub async fn upsert_slack_channel(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(body): Json<UpsertSlackChannelRequest>,
) -> Result<Json<ChannelStatusResponse>, AppError> {
    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let binding = slack_binding_mut_or_default(&mut profile);
    let connection_id = match &binding.kind_config {
        ChannelKindConfig::Slack { connection_id, .. } => connection_id.clone(),
        _ => None,
    };
    binding.kind_config = ChannelKindConfig::Slack {
        allowed_channels: body.allowed_channels,
        allowed_users: body.allowed_users,
        connection_id,
        conversation_mode: body.conversation_mode,
    };
    binding.enabled = body.enabled;
    provision_bridge_thread(&state, &agent_id, binding).await?;

    state.persistence.agents.update(&profile).await?;

    let binding = profile
        .channel_of_kind(ChannelKind::Slack)
        .expect("just upserted above");
    Ok(Json(
        channel_status_response(&agent_id, binding, &state.telegram_bridge, &state.persistence.linked_senders).await?,
    ))
}

#[derive(Debug, Deserialize)]
pub struct SetSlackChannelSecretRequest {
    pub bot_token: String,
    pub app_token: String,
}

/// `PUT /agents/{agent_id}/channels/slack/secret` — write-only: stores
/// *both* Slack tokens in [`ChannelSecretStore`], the bot token under
/// [`SLACK_BOT_TOKEN_SECRET_ROLE`] and the app-level token under
/// [`SLACK_APP_TOKEN_SECRET_ROLE`], and never echoes either back. Validates
/// each token's prefix before storing anything — pasting the two tokens
/// into the wrong fields is the most likely setup mistake, and a clear 400
/// here is far cheaper than a confusing Socket Mode handshake failure once
/// the transport lands. Also provisions the bridge thread if the binding is
/// already enabled but somehow not yet provisioned, mirroring Discord's and
/// Email's secret endpoints.
pub async fn set_slack_channel_secret(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(body): Json<SetSlackChannelSecretRequest>,
) -> Result<Json<ChannelStatusResponse>, AppError> {
    let bot_token = body.bot_token.trim();
    let app_token = body.app_token.trim();
    if bot_token.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "bot_token must not be empty".to_string(),
        )));
    }
    if app_token.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "app_token must not be empty".to_string(),
        )));
    }
    if !bot_token.starts_with(SLACK_BOT_TOKEN_PREFIX) {
        return Err(AppError(AoError::ValidationError(format!(
            "bot_token must start with '{SLACK_BOT_TOKEN_PREFIX}' — this usually means the \
             bot token and app token were swapped"
        ))));
    }
    if !app_token.starts_with(SLACK_APP_TOKEN_PREFIX) {
        return Err(AppError(AoError::ValidationError(format!(
            "app_token must start with '{SLACK_APP_TOKEN_PREFIX}' — this usually means the \
             bot token and app token were swapped"
        ))));
    }

    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let binding = slack_binding_mut_or_default(&mut profile);
    let binding_id = binding.binding_id.clone();

    let store = ChannelSecretStore::open().map_err(map_secret_store_err)?;
    store
        .set(&agent_id, &binding_id, SLACK_BOT_TOKEN_SECRET_ROLE, bot_token)
        .map_err(map_secret_store_err)?;
    store
        .set(&agent_id, &binding_id, SLACK_APP_TOKEN_SECRET_ROLE, app_token)
        .map_err(map_secret_store_err)?;

    provision_bridge_thread(&state, &agent_id, binding).await?;
    state.persistence.agents.update(&profile).await?;

    let binding = profile
        .channel_of_kind(ChannelKind::Slack)
        .expect("just set secret above");
    Ok(Json(
        channel_status_response(&agent_id, binding, &state.telegram_bridge, &state.persistence.linked_senders).await?,
    ))
}

/// `DELETE /agents/{agent_id}/channels/slack` — removes the Slack binding
/// from `profile.channels`, deletes both stored tokens, and best-effort
/// invalidates its bridge thread's outbound-relay mapping. Idempotent: a
/// no-op (still 204) when the agent has no Slack binding.
///
/// Deliberately does **not** touch the `SlackConnection` record the
/// binding's `connection_id` points at: deleting
/// a binding must not be the only path that can delete a workspace's
/// credential, since a future binding may reference the same connection.
pub async fn delete_slack_channel(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<StatusCode, AppError> {
    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    if let Some(binding) = profile.channel_of_kind(ChannelKind::Slack).cloned() {
        if let Some(bridge_thread_id) = binding.bridge_thread_id.as_deref() {
            state.telegram_bridge.invalidate_thread(bridge_thread_id);
        }

        let store = ChannelSecretStore::open().map_err(map_secret_store_err)?;
        store
            .delete(&agent_id, &binding.binding_id, SLACK_BOT_TOKEN_SECRET_ROLE)
            .map_err(map_secret_store_err)?;
        store
            .delete(&agent_id, &binding.binding_id, SLACK_APP_TOKEN_SECRET_ROLE)
            .map_err(map_secret_store_err)?;

        profile.channels.retain(|b| b.kind != ChannelKind::Slack);
        state.persistence.agents.update(&profile).await?;
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct SlackManifestResponse {
    /// Full pasteable text — a leading comment block plus the YAML manifest
    /// itself. See [`ao_protocol::slack_manifest::generate_slack_app_manifest`].
    pub manifest_yaml: String,
}

/// `GET /agents/{agent_id}/channels/slack/manifest` — a prefilled app
/// manifest the user pastes into Slack's "Create app → From an app
/// manifest" flow. Read-only and side-effect free: it needs
/// only the agent's name (for the app's display name) and does not require
/// a Slack binding to exist yet — generating the manifest is the very first
/// step of setup, before any binding or token exists.
pub async fn get_slack_manifest(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<SlackManifestResponse>, AppError> {
    let profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    Ok(Json(SlackManifestResponse {
        manifest_yaml: ao_protocol::slack_manifest::generate_slack_app_manifest(&profile.name),
    }))
}

/// `POST /agents/{agent_id}/channels/slack/test-connection` — runs
/// `auth.test`, a scope diff, and an `apps.connections.open` handshake
/// check against the two stored tokens. The response
/// carries identity and per-check outcomes only — never a token. On a
/// successful `auth.test`, persists the captured identity into the
/// binding's [`ao_protocol::slack_connection::SlackConnection`] record
/// (provisioning one, via a fresh `connection_id`, the first time this
/// succeeds) — `bot_user_id` in particular is load-bearing for the
/// bot-echo guard.
pub async fn test_slack_connection(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<SlackTestConnectionReport>, AppError> {
    let seam = ReqwestSlackApiSeam::new();
    let report = run_slack_test_connection(&state, &agent_id, &seam).await?;
    Ok(Json(report))
}

/// The seam-parameterized logic [`test_slack_connection`] delegates to, kept
/// `pub` (not `pub(crate)`) and separate from the route handler so tests —
/// this crate's own integration suite included — can drive it with
/// [`ao_engine::channels::slack::fake_seam::FakeSlackApiSeam`] instead of a
/// live call to `slack.com`.
pub async fn run_slack_test_connection(
    state: &AppState,
    agent_id: &str,
    seam: &dyn SlackApiSeam,
) -> Result<SlackTestConnectionReport, AppError> {
    let mut profile = state
        .persistence
        .agents
        .get(agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.to_string()))?;

    let binding = slack_binding_mut_or_default(&mut profile);
    let binding_id = binding.binding_id.clone();

    let store = ChannelSecretStore::open().map_err(map_secret_store_err)?;
    let bot_token =
        store.get(agent_id, &binding_id, SLACK_BOT_TOKEN_SECRET_ROLE).map_err(map_secret_store_err)?;
    let app_token =
        store.get(agent_id, &binding_id, SLACK_APP_TOKEN_SECRET_ROLE).map_err(map_secret_store_err)?;
    let (Some(bot_token), Some(app_token)) = (bot_token, app_token) else {
        return Err(AppError(AoError::ValidationError(
            "Slack bot and app tokens must both be stored before Test Connection can run — \
             set them via PUT .../channels/slack/secret first"
                .to_string(),
        )));
    };

    let report = run_test_connection(seam, &bot_token, &app_token).await;

    if let Some(identity) = &report.identity {
        let binding = slack_binding_mut_or_default(&mut profile);
        let connection_id = match &binding.kind_config {
            ChannelKindConfig::Slack { connection_id: Some(existing), .. } => existing.clone(),
            _ => uuid::Uuid::new_v4().to_string(),
        };
        if let ChannelKindConfig::Slack { connection_id: slot, .. } = &mut binding.kind_config {
            *slot = Some(connection_id.clone());
        }
        state.persistence.agents.update(&profile).await?;

        state
            .persistence
            .slack_connections
            .set(
                &connection_id,
                &SlackConnection {
                    team_id: identity.team_id.clone(),
                    team_name: identity.team_name.clone(),
                    bot_user_id: identity.bot_user_id.clone(),
                },
            )
            .await?;
    }

    Ok(report)
}

#[cfg(test)]
mod channel_senders_route_tests {
    use super::*;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::agent::{AgentRunnerMode, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
    use std::collections::HashMap;

    async fn setup_state() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("Failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = ao_persistence::PersistenceLayer::init();
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock).await.expect("AppState init")
        };
        (Arc::new(state), tmp)
    }

    fn unwrap_ok<T>(r: Result<T, AppError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {:?}", e.0),
        }
    }

    fn base_profile(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.into(),
            name: id.into(),
            description: "".into(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "claude".into(),
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
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: AgentRunnerMode::Cli,
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
            max_turns: None,
        }
    }

    #[tokio::test]
    async fn set_then_get_round_trips_through_the_store() {
        let (state, _tmp) = setup_state().await;
        let profile = base_profile("agent-senders-a");
        state.persistence.agents.create(&profile).await.unwrap();

        let set_response = unwrap_ok(
            set_channel_senders(
                State(Arc::clone(&state)),
                Path(("agent-senders-a".to_string(), "telegram".to_string())),
                Json(SetChannelSendersRequest { senders: vec!["555".to_string()] }),
            )
            .await,
        );
        assert_eq!(set_response.senders, vec!["555".to_string()]);

        let get_response = unwrap_ok(
            get_channel_senders(
                State(Arc::clone(&state)),
                Path(("agent-senders-a".to_string(), "telegram".to_string())),
            )
            .await,
        );
        assert_eq!(get_response.senders, vec!["555".to_string()]);
    }

    #[tokio::test]
    async fn get_falls_back_to_the_deprecated_inline_field_for_an_unmigrated_binding() {
        let (state, _tmp) = setup_state().await;
        let mut profile = base_profile("agent-senders-b");
        profile.channels = vec![ChannelBinding {
            binding_id: "telegram".to_string(),
            kind: ChannelKind::Telegram,
            enabled: true,
            bridge_thread_id: None,
            allowed_senders: vec!["999".to_string()],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Telegram { bot_username: Some("@bot".to_string()), thread_mode: Default::default() },
        }];
        state.persistence.agents.create(&profile).await.unwrap();

        let response = unwrap_ok(
            get_channel_senders(
                State(Arc::clone(&state)),
                Path(("agent-senders-b".to_string(), "telegram".to_string())),
            )
            .await,
        );
        assert_eq!(
            response.senders,
            vec!["999".to_string()],
            "an un-migrated binding's store read must fall back to the inline field"
        );
    }

    #[tokio::test]
    async fn set_never_writes_the_profile_document() {
        let (state, _tmp) = setup_state().await;
        let mut profile = base_profile("agent-senders-c");
        profile.channels = vec![ChannelBinding {
            binding_id: "telegram".to_string(),
            kind: ChannelKind::Telegram,
            enabled: true,
            bridge_thread_id: None,
            allowed_senders: vec![],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Telegram { bot_username: Some("@bot".to_string()), thread_mode: Default::default() },
        }];
        state.persistence.agents.create(&profile).await.unwrap();

        let _ = unwrap_ok(
            set_channel_senders(
                State(Arc::clone(&state)),
                Path(("agent-senders-c".to_string(), "telegram".to_string())),
                Json(SetChannelSendersRequest { senders: vec!["123".to_string()] }),
            )
            .await,
        );

        let stored = state.persistence.agents.get("agent-senders-c").await.unwrap().unwrap();
        let stored_binding = stored.telegram_binding().unwrap();
        assert!(
            stored_binding.allowed_senders.is_empty(),
            "this route must edit LinkedSenderStore only, never the profile document"
        );
    }

    #[tokio::test]
    async fn two_agents_sharing_a_binding_id_are_isolated_through_the_route() {
        let (state, _tmp) = setup_state().await;
        state.persistence.agents.create(&base_profile("agent-senders-d1")).await.unwrap();
        state.persistence.agents.create(&base_profile("agent-senders-d2")).await.unwrap();

        let _ = unwrap_ok(
            set_channel_senders(
                State(Arc::clone(&state)),
                Path(("agent-senders-d1".to_string(), "telegram".to_string())),
                Json(SetChannelSendersRequest { senders: vec!["111".to_string()] }),
            )
            .await,
        );
        let _ = unwrap_ok(
            set_channel_senders(
                State(Arc::clone(&state)),
                Path(("agent-senders-d2".to_string(), "telegram".to_string())),
                Json(SetChannelSendersRequest { senders: vec!["222".to_string()] }),
            )
            .await,
        );

        let d1 = unwrap_ok(
            get_channel_senders(
                State(Arc::clone(&state)),
                Path(("agent-senders-d1".to_string(), "telegram".to_string())),
            )
            .await,
        );
        let d2 = unwrap_ok(
            get_channel_senders(
                State(Arc::clone(&state)),
                Path(("agent-senders-d2".to_string(), "telegram".to_string())),
            )
            .await,
        );
        assert_eq!(d1.senders, vec!["111".to_string()]);
        assert_eq!(d2.senders, vec!["222".to_string()]);
    }
}
