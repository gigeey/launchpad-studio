//! Per-agent Telegram bot token endpoints.
//!
//! The bot token never round-trips through the regular agent-update route —
//! it's write-only here, validated against the Telegram Bot API before
//! anything is stored, and never echoed back over HTTP. Non-secret Telegram
//! config (`AgentProfile.telegram`) still travels through the existing
//! `PUT /agents/{id}` route; these endpoints only own the secret and the
//! `bot_username` cached from `getMe`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use ao_engine::telegram::TelegramClient;
use ao_engine::AppState;
use ao_engine_tools_provider_config::{TelegramTokenStore, TelegramTokenStoreError};
use ao_protocol::agent::{
    AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig, PairingCode, TelegramConfig,
    TelegramThreadMode,
};
use ao_protocol::error::AoError;

use crate::channel_provisioning::provision_bridge_thread;
use crate::error::AppError;

/// Returns a mutable reference to `profile`'s Telegram binding, inserting a
/// disabled, unprovisioned one first if it doesn't have one yet.
fn telegram_binding_mut_or_default(profile: &mut AgentProfile) -> &mut ChannelBinding {
    if profile.channel_of_kind(ChannelKind::Telegram).is_none() {
        profile.channels.push(ChannelBinding {
            binding_id: "telegram".to_string(),
            kind: ChannelKind::Telegram,
            enabled: false,
            bridge_thread_id: None,
            allowed_senders: Vec::new(),
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Telegram { bot_username: None, thread_mode: TelegramThreadMode::default() },
        });
    }
    profile
        .channel_of_kind_mut(ChannelKind::Telegram)
        .expect("just inserted above if missing")
}

fn map_store_err(e: TelegramTokenStoreError) -> AppError {
    AppError(AoError::Internal(format!("telegram token store: {e}")))
}

fn open_token_store() -> Result<TelegramTokenStore, AppError> {
    TelegramTokenStore::open().map_err(map_store_err)
}

#[derive(Debug, Deserialize)]
pub struct SetTelegramTokenRequest {
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct SetTelegramTokenResponse {
    pub bot_username: String,
}

/// `PUT /agents/{agent_id}/telegram/token` — validate and store a bot token.
///
/// Calls Telegram's `getMe` before writing anything, so an invalid token is
/// rejected without touching the token store or the agent profile. On
/// success the resolved username is cached onto the agent's Telegram
/// `ChannelBinding` so the frontend can render it without ever holding the
/// secret. Also provisions the binding's dedicated bridge thread in the same
/// request if it isn't provisioned yet — enabling a channel must atomically
/// provision its bridge thread, not leave that to a later, unrelated profile
/// save (see `provision_bridge_thread`).
pub async fn set_telegram_token(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(body): Json<SetTelegramTokenRequest>,
) -> Result<Json<SetTelegramTokenResponse>, AppError> {
    let token = body.token.trim();
    if token.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "token must not be empty".to_string(),
        )));
    }

    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let bot_info = TelegramClient::new().get_me(token).await.map_err(|_| {
        AppError(AoError::ValidationError(
            "invalid Telegram bot token".to_string(),
        ))
    })?;

    let store = open_token_store()?;
    store.set(&agent_id, token).map_err(map_store_err)?;

    let binding = telegram_binding_mut_or_default(&mut profile);
    if let ChannelKindConfig::Telegram { bot_username, .. } = &mut binding.kind_config {
        *bot_username = Some(bot_info.username.clone());
    }
    binding.enabled = true;
    provision_bridge_thread(&state, &agent_id, binding).await?;

    state.persistence.agents.update(&profile).await?;

    Ok(Json(SetTelegramTokenResponse {
        bot_username: bot_info.username,
    }))
}

/// `DELETE /agents/{agent_id}/telegram/token` — clear the stored token and
/// fully reset the bridge for this agent. Removing the token invalidates any
/// chat linkage and pending pairing code, since both were only meaningful
/// while a specific bot owned this agent's chat.
pub async fn delete_telegram_token(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<StatusCode, AppError> {
    let store = open_token_store()?;
    store.delete(&agent_id).map_err(map_store_err)?;

    if let Some(mut profile) = state.persistence.agents.get(&agent_id).await? {
        // Same owned-copy/write-back shim pattern as
        // `set_telegram_token` above.
        let binding_id = profile.telegram_binding().map(|b| b.binding_id.clone());
        if let Some(mut telegram) = profile.telegram_config_view() {
            telegram.bot_username = None;
            telegram.enabled = false;
            telegram.allowed_chat_ids.clear();
            telegram.pending_pairing_code = None;
            // The binding just ended: drop any chat mapping the outbound
            // relay would otherwise use for a completion still in flight on
            // this thread (e.g. an async Delegate spawned before the token
            // was deleted) rather than waiting for the bridge's reconcile
            // loop to notice the token is gone.
            if let Some(bridge_thread_id) = telegram.bridge_thread_id.as_deref() {
                state.telegram_bridge.invalidate_thread(bridge_thread_id);
            }
            profile.set_telegram_config(Some(telegram));
            state.persistence.agents.update(&profile).await?;
        }
        // The token is gone, so every chat previously authorized under it
        // must lose that authorization too — otherwise a sender paired
        // against the deleted token stays authorized (via
        // `LinkedSenderStore`, which drives real enforcement) the moment a
        // new token is ever set for this agent, even though the deprecated
        // inline `allowed_chat_ids` cleared above reads as fully reset.
        if let Some(binding_id) = binding_id.as_deref() {
            state.persistence.linked_senders.clear(&agent_id, binding_id).await?;
        }
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct TelegramStatusResponse {
    pub has_token: bool,
    pub bot_username: Option<String>,
    pub enabled: bool,
    pub linked: bool,
    pub allowed_chat_ids: Vec<i64>,
    pub pending_pairing_code: Option<PairingCode>,
}

/// `GET /agents/{agent_id}/telegram/status` — non-secret status for the
/// setup modal. Never returns the token itself. An expired pending pairing
/// code is reported as `null` rather than the stale code, since callers only
/// care whether it's currently usable.
pub async fn get_telegram_status(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<TelegramStatusResponse>, AppError> {
    let profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let store = open_token_store()?;
    let has_token = store.get(&agent_id).map_err(map_store_err)?.is_some();

    let now_unix = chrono::Utc::now().timestamp();
    // Reads through the `telegram_config_view` shim rather than
    // `profile.channels` directly; ported once this route reads
    // `ChannelBinding` natively.
    let telegram = profile.telegram_config_view();
    Ok(Json(TelegramStatusResponse {
        has_token,
        bot_username: telegram.as_ref().and_then(|t| t.bot_username.clone()),
        enabled: telegram.as_ref().map(|t| t.enabled).unwrap_or(false),
        linked: telegram
            .as_ref()
            .map(|t| !t.allowed_chat_ids.is_empty())
            .unwrap_or(false),
        allowed_chat_ids: telegram
            .as_ref()
            .map(|t| t.allowed_chat_ids.clone())
            .unwrap_or_default(),
        pending_pairing_code: telegram.and_then(|t| {
            t.pending_pairing_code
                .filter(|code| !code.is_expired(now_unix))
        }),
    }))
}

#[derive(Debug, Serialize)]
pub struct CreatePairingCodeResponse {
    pub code: String,
    pub expires_at_unix: i64,
}

/// `POST /agents/{agent_id}/telegram/pairing-code` — mint a fresh pairing
/// code for the user to send the bot from the chat they want to link.
/// Regenerating overwrites any prior pending code, so only the most recently
/// issued code is ever valid.
pub async fn create_pairing_code(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<CreatePairingCodeResponse>, AppError> {
    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let now_unix = chrono::Utc::now().timestamp();
    let pairing_code = PairingCode::generate(now_unix);

    // Same owned-copy/write-back shim pattern as
    // `set_telegram_token` above.
    let mut telegram = profile.telegram_config_view().unwrap_or_else(|| TelegramConfig {
        enabled: false,
        bot_username: None,
        thread_mode: TelegramThreadMode::default(),
        bridge_thread_id: None,
        allowed_chat_ids: Vec::new(),
        pending_pairing_code: None,
    });
    telegram.pending_pairing_code = Some(pairing_code.clone());
    profile.set_telegram_config(Some(telegram));
    state.persistence.agents.update(&profile).await?;

    Ok(Json(CreatePairingCodeResponse {
        code: pairing_code.code,
        expires_at_unix: pairing_code.expires_at_unix,
    }))
}

#[derive(Debug, Serialize)]
pub struct UnlinkChatResponse {
    pub allowed_chat_ids: Vec<i64>,
}

/// `DELETE /agents/{agent_id}/telegram/chats/{chat_id}` — revoke a linked
/// chat's access to this agent. Idempotent: unlinking a chat that isn't
/// linked just returns the (unchanged) allow-list.
///
/// Authorization for this chat is granted by an entry in
/// [`ao_persistence::linked_sender_store::LinkedSenderStore`] (written by the
/// pairing flow in `try_link_chat`), not by the inline `allowed_chat_ids`
/// this handler also maintains for display purposes. This must always remove
/// the chat from that store too — otherwise the sender stays authorized to
/// message the bot after "unlinking", the mirror-image of the whole-document
/// clobber `LinkedSenderStore` was introduced to close.
pub async fn delete_telegram_chat(
    State(state): State<Arc<AppState>>,
    Path((agent_id, chat_id)): Path<(String, i64)>,
) -> Result<Json<UnlinkChatResponse>, AppError> {
    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    // Same owned-copy/write-back shim pattern as
    // `set_telegram_token` above.
    let binding_id = profile.telegram_binding().map(|b| b.binding_id.clone());
    let had_telegram_binding = binding_id.is_some();
    let allowed_chat_ids = if let Some(mut telegram) = profile.telegram_config_view() {
        telegram.allowed_chat_ids.retain(|id| *id != chat_id);
        // Only drop the outbound relay's mapping if it's currently pointed
        // at the chat being unlinked — the dedicated thread may have other
        // still-linked chats sharing it, and an in-flight reply for one of
        // those must not be discarded by unlinking a different chat.
        if let Some(bridge_thread_id) = telegram.bridge_thread_id.as_deref() {
            state
                .telegram_bridge
                .invalidate_thread_for_chat(bridge_thread_id, chat_id);
        }
        let allowed_chat_ids = telegram.allowed_chat_ids.clone();
        profile.set_telegram_config(Some(telegram));
        allowed_chat_ids
    } else {
        Vec::new()
    };

    if let Some(binding_id) = binding_id.as_deref() {
        state
            .persistence
            .linked_senders
            .remove_sender(&agent_id, binding_id, &chat_id.to_string())
            .await?;
    }

    if had_telegram_binding {
        state.persistence.agents.update(&profile).await?;
    }

    Ok(Json(UnlinkChatResponse { allowed_chat_ids }))
}

#[cfg(test)]
mod unlink_revocation_tests {
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
            channels: vec![telegram_binding()],
            max_turns: None,
        }
    }

    fn telegram_binding() -> ChannelBinding {
        ChannelBinding {
            binding_id: "telegram".to_string(),
            kind: ChannelKind::Telegram,
            enabled: true,
            bridge_thread_id: None,
            allowed_senders: vec![],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Telegram {
                bot_username: Some("@bot".to_string()),
                thread_mode: TelegramThreadMode::default(),
            },
        }
    }

    /// Mirrors the shape of `try_link_chat`'s write in
    /// `ao_engine::telegram::transport` — a direct `linked_senders.add_sender`
    /// call, the same way the pairing flow links a chat out-of-band from the
    /// profile document.
    async fn pair_sender(state: &Arc<AppState>, agent_id: &str, chat_id: i64) {
        state
            .persistence
            .linked_senders
            .add_sender(agent_id, "telegram", &chat_id.to_string())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unlinking_a_chat_revokes_it_from_the_linked_sender_store() {
        let (state, _tmp) = setup_state().await;
        let profile = base_profile("agent-unlink-revoke");
        state.persistence.agents.create(&profile).await.unwrap();

        pair_sender(&state, "agent-unlink-revoke", 555).await;
        let linked = state
            .persistence
            .linked_senders
            .get("agent-unlink-revoke", "telegram")
            .await
            .unwrap()
            .expect("pairing must land in the store");
        assert_eq!(
            linked.senders,
            vec!["555".to_string()],
            "the paired chat must be authorized before unlinking"
        );

        let _ = unwrap_ok(
            delete_telegram_chat(
                State(Arc::clone(&state)),
                Path(("agent-unlink-revoke".to_string(), 555)),
            )
            .await,
        );

        let after_unlink = state
            .persistence
            .linked_senders
            .get("agent-unlink-revoke", "telegram")
            .await
            .unwrap()
            .unwrap_or_default();
        assert!(
            !after_unlink.senders.iter().any(|s| s == "555"),
            "unlinking must remove the sender from LinkedSenderStore, not just the inline list, \
             or the chat stays authorized to message the bot after \"unlinking\""
        );
    }

    #[tokio::test]
    async fn unlinking_one_chat_does_not_revoke_a_different_paired_chat() {
        let (state, _tmp) = setup_state().await;
        let profile = base_profile("agent-unlink-selective");
        state.persistence.agents.create(&profile).await.unwrap();

        pair_sender(&state, "agent-unlink-selective", 111).await;
        pair_sender(&state, "agent-unlink-selective", 222).await;

        let _ = unwrap_ok(
            delete_telegram_chat(
                State(Arc::clone(&state)),
                Path(("agent-unlink-selective".to_string(), 111)),
            )
            .await,
        );

        let remaining = state
            .persistence
            .linked_senders
            .get("agent-unlink-selective", "telegram")
            .await
            .unwrap()
            .unwrap()
            .senders;
        assert_eq!(remaining, vec!["222".to_string()]);
    }

    #[tokio::test]
    async fn deleting_the_token_revokes_every_linked_sender_from_the_store() {
        let (state, _tmp) = setup_state().await;
        let profile = base_profile("agent-token-delete-revoke");
        state.persistence.agents.create(&profile).await.unwrap();

        pair_sender(&state, "agent-token-delete-revoke", 111).await;
        pair_sender(&state, "agent-token-delete-revoke", 222).await;

        let _ = unwrap_ok(
            delete_telegram_token(State(Arc::clone(&state)), Path("agent-token-delete-revoke".to_string()))
                .await,
        );

        let remaining = state
            .persistence
            .linked_senders
            .get("agent-token-delete-revoke", "telegram")
            .await
            .unwrap()
            .unwrap_or_default()
            .senders;
        assert!(
            remaining.is_empty(),
            "deleting the token must revoke every sender previously linked under it, \
             or they stay authorized (via LinkedSenderStore) if a new token is set later"
        );
    }
}
