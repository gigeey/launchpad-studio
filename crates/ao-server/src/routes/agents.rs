use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;

use ao_engine::agent_runner::system_prompt::coordinator_level;
use ao_engine::instructions::resolve_agent_home_dir;
use ao_engine::prompt_sections::COPILOT_PROFILE_ID;
use ao_engine::system_prompt_composer::compose_system_prompt;
use ao_engine::system_prompt_composer::loader::{load_agent_home_context, load_workspace_context};
use ao_engine::AppState;
use ao_protocol::agent::{AgentProfile, ChannelKind};
use ao_protocol::error::AoError;

use crate::channel_provisioning::provision_bridge_thread;
use crate::error::AppError;

/// POST /agents — create a new agent profile.
pub async fn create_agent(
    State(state): State<Arc<AppState>>,
    Json(profile): Json<AgentProfile>,
) -> Result<Json<AgentProfile>, AppError> {
    state.persistence.agents.create(&profile).await?;

    // Scaffold agent home directory (use custom home_dir if set)
    let agent_home = profile.home_dir.as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| state.persistence.data_root.agent_home_dir(&profile.id));
    ao_protocol::agent_home::ensure_agent_home(&agent_home)
        .await
        .map_err(|e| AoError::Internal(format!("Failed to create agent home: {e}")))?;

    // Update snapshot with new agent entry
    let name = profile.name.clone();
    let emoji = profile.emoji.clone();
    let file_caps = profile.file_capabilities_supported();
    let owning_team_id = profile.owning_team_id.clone();
    state
        .persistence
        .snapshots
        .update_agent_entry(&profile.id, |entry| {
            entry.name = name;
            entry.emoji = emoji;
            entry.file_capabilities_supported = file_caps;
            entry.owning_team_id = owning_team_id;
        })
        .await?;

    Ok(Json(profile))
}

/// Query parameters for `GET /agents`.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ListAgentsQuery {
    /// When `true`, include inline team coordinators (agents with `owning_team_id` set).
    /// Defaults to `false` so chat surfaces don't see team-only agents.
    #[serde(default)]
    pub include_team_coordinators: bool,
    /// When `true`, include tasklist co-pilot agents (template = `tasklist-copilot`).
    /// Defaults to `false` so the chat sidebar doesn't surface per-tasklist
    /// co-pilot threads — those are reached via the tasklist overlay UI instead.
    #[serde(default)]
    pub include_copilots: bool,
}

/// GET /agents — list all agents from snapshot.
///
/// By default, agents with `owning_team_id` set (inline team coordinators) and
/// agents whose template is `tasklist-copilot` (per-tasklist co-pilots) are
/// excluded so they don't appear in chat surfaces. Pass
/// `?include_team_coordinators=true` and/or `?include_copilots=true` to
/// receive the unfiltered list.
///
/// The `has_active_run` and `queue_depth` fields are NOT read from disk —
/// they're overlaid at response time from the live `InstanceRegistry` and
/// `QueueManagerRegistry`. The on-disk snapshot only carries durable fields
/// (name, last_message, message_count, etc.); persisting runtime state used
/// to require six separate cleanup ladders, any one of which could leave the
/// sidebar typing dot wedged.
pub async fn list_agents(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<ListAgentsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let snapshot = state.persistence.snapshots.get().await;

    // Load all full profiles once. We need them both to:
    //   1. Exclude co-pilot agents (template field lives only in the profile, not the snapshot).
    //   2. Compute each agent's coordinator level for the sidebar badge.
    let all_profiles: Vec<AgentProfile> = state
        .persistence
        .agents
        .list()
        .await
        .unwrap_or_default();

    // Build a profile index keyed by agent ID for O(1) lookup during level computation.
    let profile_index: HashMap<String, AgentProfile> = all_profiles
        .iter()
        .map(|p| (p.id.clone(), p.clone()))
        .collect();

    let copilot_ids: HashSet<String> = if query.include_copilots {
        HashSet::new()
    } else {
        all_profiles
            .iter()
            .filter(|p| p.template.as_deref() == Some(COPILOT_PROFILE_ID))
            .map(|p| p.id.clone())
            .collect()
    };

    let mut agents: Vec<ao_persistence::snapshot::AgentSnapshot> = snapshot
        .agents
        .values()
        .filter(|a| !a.agent_id.starts_with("task:"))
        .filter(|a| query.include_team_coordinators || a.owning_team_id.is_none())
        .filter(|a| query.include_copilots || !copilot_ids.contains(&a.agent_id))
        .cloned()
        .collect();

    // Overlay live runtime fields plus the computed coordinator level.
    // The instance-registry is the sole writer for "is this agent currently running";
    // the queue-manager registry owns the in-actor queue depth.
    for entry in agents.iter_mut() {
        entry.has_active_run =
            state.instance_registry.running_count(&entry.agent_id).await > 0;
        // Standalone-scope only, same as `has_active_run` above — team/
        // project/task-scoped runs are tracked under different registry keys.
        entry.running_thread_ids =
            state.instance_registry.running_thread_ids(&entry.agent_id).await;
        entry.queue_depth = state.queue_managers.queue_depth_for(&entry.agent_id).await;
        if let Some(profile) = profile_index.get(&entry.agent_id) {
            entry.coordinator_level = coordinator_level(profile, &profile_index);
        }
        for form in entry.pending_forms.iter_mut() {
            form.is_latest_in_thread = is_pending_form_latest_in_thread(
                &state.persistence.threads,
                &state.persistence.transcripts,
                &entry.agent_id,
                form,
            )
            .await;
        }
    }

    Ok(Json(serde_json::to_value(agents).map_err(|e| AoError::Json(e.to_string()))?))
}

/// How many trailing entries of a thread's transcript to read when deciding
/// whether a pending form is still the last thing that happened there.
/// Comfortably covers a handful of hidden synthetic entries (skill-body
/// loads, etc.) landing after the `form_request` row without reading the
/// whole file.
const LATEST_FORM_LOOKBACK: usize = 8;

/// True iff `form`'s own `form_request` transcript entry (matched by
/// `form_id`) is still the last non-`hidden_from_user` entry in its thread —
/// i.e. nothing (a message the operator sent past it, an agent reply, a
/// stopped run) has landed in that thread since it was posted. Drives
/// `PendingForm::is_latest_in_thread`, which the `?` activity badge on
/// background/collapsed threads gates on (see
/// `frontend/.../ThreadActivityBadge.tsx`'s `resolveThreadActivity`).
///
/// Reads only a short tail of the relevant transcript file
/// (`LATEST_FORM_LOOKBACK` entries via `read_recent`/`read_recent_at`, the
/// same helpers `TranscriptStore` already exposes for other tail reads) —
/// not the full thread. `read_recent_at` still loads that file's full bytes
/// into memory before slicing off the tail, matching the cost profile of
/// every other `read_recent*` call site in this codebase; there is no
/// cheaper "read just the last line" primitive today, so this is the
/// lightest correct option ao-server has DataRoot for the shape.
/// Runs once per pending form per `GET /agents` call — at most one form per
/// thread, and pending forms are rare, so this stays proportional to "how
/// many forms are actually waiting," not to agent/thread count.
///
/// Falls back to `true` (badge stays visible) whenever the thread lookup or
/// transcript read can't produce an answer — a still-open question should
/// never silently disappear because of an I/O hiccup or a thread that was
/// deleted out from under it.
///
/// Takes the two stores directly rather than `&AppState` so it's unit
/// testable without standing up the rest of the process's dependency graph
/// (`AppState` wires dozens of unrelated services) — same reasoning as
/// `form_answers.rs`'s pure helper functions.
async fn is_pending_form_latest_in_thread(
    threads: &ao_persistence::thread_store::ThreadStore,
    transcripts: &ao_persistence::transcript::TranscriptStore,
    agent_id: &str,
    form: &ao_persistence::snapshot::PendingForm,
) -> bool {
    let recent = match form.thread_id.as_deref() {
        None => transcripts.read_recent(agent_id, LATEST_FORM_LOOKBACK).await,
        Some(thread_id) => match threads.get(thread_id).await {
            Ok(Some(thread)) if thread.kind != ao_protocol::thread::ThreadKind::Default => {
                let path = std::path::PathBuf::from(&thread.transcript_path);
                transcripts.read_recent_at(&path, LATEST_FORM_LOOKBACK).await
            }
            // Default-kind row (or lookup came back empty/erroring): the
            // form's own thread_id was already `None` in that case upstream,
            // but stay defensive and fall back to the agent-keyed file.
            _ => transcripts.read_recent(agent_id, LATEST_FORM_LOOKBACK).await,
        },
    };

    let Ok(entries) = recent else {
        return true;
    };
    let Some(last_visible) = entries.iter().rev().find(|e| !e.hidden_from_user) else {
        return true;
    };
    last_visible.event_type == ao_engine_tools_core::form_events::FORM_REQUEST
        && last_visible
            .metadata
            .as_ref()
            .and_then(|m| m.get("form_id"))
            .and_then(|v| v.as_str())
            == Some(form.form_id.as_str())
}

/// GET /agents/{id} — get full agent profile from YAML.
pub async fn get_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<AgentProfile>, AppError> {
    let profile = state
        .persistence
        .agents
        .get(&id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(id))?;
    Ok(Json(profile))
}

/// Sync every channel binding's `bridge_thread_id` (generalized to any
/// channel kind) as part of a profile save, and
/// enforce that `bridge_thread_id` is server-owned.
///
/// For each binding on the incoming profile, always overwrites its
/// `bridge_thread_id` with whatever is already on record for this agent's
/// same `binding_id` — an incoming PUT can never set or clear it directly.
/// Then delegates to [`provision_bridge_thread`], which is a no-op unless
/// the binding is enabled and nothing was on record yet.
async fn sync_channel_bridge_threads(
    state: &AppState,
    id: &str,
    existing: Option<&AgentProfile>,
    profile: &mut AgentProfile,
) -> Result<(), AppError> {
    for binding in &mut profile.channels {
        binding.bridge_thread_id = existing
            .and_then(|existing| {
                existing.channels.iter().find(|b| b.binding_id == binding.binding_id)
            })
            .and_then(|b| b.bridge_thread_id.clone());

        provision_bridge_thread(state, id, binding).await?;
    }

    Ok(())
}

/// Preserves non-Telegram channel bindings across a general profile save,
/// and — for every binding, Telegram included — refuses to let this save
/// touch `allowed_senders` at all.
///
/// The general AgentProfileForm save path serializes its whole payload as a
/// fresh `AgentProfile`, but the form itself only ever represents the
/// Telegram channel (via the legacy `telegram` field, folded into `channels`
/// by `AgentProfileWire`'s `From` impl). A profile with an Email or Discord
/// binding configured through the dedicated channel routes would otherwise
/// have that binding silently deleted the next time the general form is
/// saved, since `incoming.channels` would be Telegram-only or empty.
///
/// Telegram itself is left as `incoming` has it — including its absence,
/// which represents an intentional disable from the form — since Telegram
/// is the one kind the general form actually edits. `allowed_senders` is the
/// one exception even for Telegram: this whole-document save runs
/// concurrently with the out-of-band Telegram pairing flow, so whatever the
/// client last fetched is potentially already stale by the time this save
/// lands. `ChannelBinding::allowed_senders` is deprecated in favor of
/// `LinkedSenderStore` for exactly this reason — see that field's doc — so
/// this save always keeps the server's existing value (or, for a binding
/// with no server-side counterpart yet, the fail-closed empty list) and
/// never takes the client's copy.
fn merge_preserving_non_telegram_channels(existing: Option<&AgentProfile>, incoming: &mut AgentProfile) {
    let Some(existing) = existing else {
        for binding in &mut incoming.channels {
            binding.allowed_senders = Vec::new();
        }
        return;
    };
    for binding in &existing.channels {
        if binding.kind == ChannelKind::Telegram {
            continue;
        }
        let already_present = incoming.channels.iter().any(|b| b.kind == binding.kind);
        if !already_present {
            incoming.channels.push(binding.clone());
        }
    }
    for binding in &mut incoming.channels {
        binding.allowed_senders = existing
            .channels
            .iter()
            .find(|b| b.binding_id == binding.binding_id)
            .map(|b| b.allowed_senders.clone())
            .unwrap_or_default();
    }
}

/// PUT /agents/{id} — update an existing agent profile.
pub async fn update_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(mut profile): Json<AgentProfile>,
) -> Result<Json<AgentProfile>, AppError> {
    // Ensure path id matches body id
    profile.id = id.clone();

    let existing = state.persistence.agents.get(&id).await?;
    merge_preserving_non_telegram_channels(existing.as_ref(), &mut profile);

    sync_channel_bridge_threads(&state, &id, existing.as_ref(), &mut profile).await?;

    state.persistence.agents.update(&profile).await?;

    // Invalidate context cache for this agent so next run picks up changes
    state.context_cache.invalidate(&id).await;

    // Update snapshot
    let name = profile.name.clone();
    let emoji = profile.emoji.clone();
    let file_caps = profile.file_capabilities_supported();
    let owning_team_id = profile.owning_team_id.clone();
    state
        .persistence
        .snapshots
        .update_agent_entry(&id, |entry| {
            entry.name = name;
            entry.emoji = emoji;
            entry.file_capabilities_supported = file_caps;
            entry.owning_team_id = owning_team_id;
        })
        .await?;

    Ok(Json(profile))
}

/// POST /agents/{parent_id}/clone — atomically duplicate an agent (profile + home).
pub async fn clone_agent(
    State(state): State<Arc<AppState>>,
    Path(parent_id): Path<String>,
) -> Result<Json<AgentProfile>, AppError> {
    let profile = state.persistence.agents.clone_agent(&parent_id).await?;

    // Update snapshot with the new agent entry (mirrors create_agent).
    let name = profile.name.clone();
    let emoji = profile.emoji.clone();
    let file_caps = profile.file_capabilities_supported();
    let owning_team_id = profile.owning_team_id.clone();
    state
        .persistence
        .snapshots
        .update_agent_entry(&profile.id, |entry| {
            entry.name = name;
            entry.emoji = emoji;
            entry.file_capabilities_supported = file_caps;
            entry.owning_team_id = owning_team_id;
        })
        .await?;

    Ok(Json(profile))
}

/// GET /agents/{id}/compose-prompt — return the canonical composed system prompt
/// for the agent, with an empty volatile tail (no memories, workflows, or
/// delegate targets). Used by the AgentProfileForm Advanced tab to preview
/// what the model will receive.
pub async fn compose_prompt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<String>, AppError> {
    let profile = state
        .persistence
        .agents
        .get(&id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(id))?;

    let user_prefs = state
        .persistence
        .preferences
        .get()
        .await?
        .unwrap_or_default();

    let agent_home = resolve_agent_home_dir(&profile, &state.persistence.data_root);
    let cwd = profile
        .working_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));

    let (workspace_ctx, agent_home_ctx) = tokio::join!(
        load_workspace_context(&cwd),
        load_agent_home_context(&agent_home),
    );

    let date_str = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let prompt = compose_system_prompt(
        &profile,
        &user_prefs,
        &workspace_ctx,
        &agent_home_ctx,
        &[],
        &[],
        &[],
        &[],
        &[],
        &date_str,
        None,
    );

    Ok(Json(prompt))
}

/// DELETE /agents/{id} — delete an agent profile and its on-disk home
/// directory.
///
/// Order of operations:
///   1. Delete the profile YAML.
///   2. Remove the agent entry from the snapshot.
///   3. Best-effort: recursively delete agent_homes/<id>/. Filesystem failures
///      are logged but do not fail the request — the logical delete has
///      already succeeded.
pub async fn delete_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<axum::http::StatusCode, AppError> {
    // Cascade: cancel in-flight tasks, clean address books, re-classify orphans.
    // Runs before the profile and data-dir deletion so the classifier can still
    // load the parent agent profiles when re-routing orphaned tasks.
    if let Err(e) = state.cascade_service.execute_cascade(&id).await {
        tracing::warn!(
            agent_id = %id,
            error = %e,
            "delete_agent: cascade failed; continuing with profile deletion"
        );
    }

    state.persistence.agents.delete(&id).await?;
    state.persistence.snapshots.remove_agent_entry(&id).await?;

    let home_dir = state.persistence.data_root.agent_home_dir(&id);
    match tokio::fs::remove_dir_all(&home_dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(
                "Failed to remove agent home directory {:?} for agent {}: {}",
                home_dir,
                id,
                e
            );
        }
    }

    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod merge_preserving_non_telegram_channels_tests {
    use super::merge_preserving_non_telegram_channels;
    use ao_protocol::agent::{
        AgentProfile, AgentRunnerMode, ChannelBinding, ChannelKind, ChannelKindConfig,
        CliProviderConfig, InputMode, OutputFormat, ProviderConfig, ThreadFollowMode,
    };
    use std::collections::HashMap;

    fn base_profile() -> AgentProfile {
        AgentProfile {
            id: "a".into(),
            name: "a".into(),
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

    fn email_binding() -> ChannelBinding {
        ChannelBinding {
            binding_id: "email".to_string(),
            kind: ChannelKind::Email,
            enabled: true,
            bridge_thread_id: Some("thread-email".to_string()),
            allowed_senders: vec!["axew@example.com".to_string()],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Email {
                address: "agent@example.com".to_string(),
                imap_host: "imap.example.com".to_string(),
                imap_port: 993,
                smtp_host: "smtp.example.com".to_string(),
                smtp_port: 587,
                poll_secs: 60,
                require_auth_results: true,
            },
        }
    }

    fn discord_binding() -> ChannelBinding {
        ChannelBinding {
            binding_id: "discord".to_string(),
            kind: ChannelKind::Discord,
            enabled: true,
            bridge_thread_id: Some("thread-discord".to_string()),
            allowed_senders: vec![],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Discord {
                allowed_users: vec![],
                allowed_roles: vec![],
                allowed_channels: vec![],
                dm_role_auth_guild: None,
                require_mention: true,
                thread_follow: ThreadFollowMode::default(),
                thread_idle_timeout_minutes: 15,
                thread_message_budget: 10,
                backfill_limit: 20,
            },
        }
    }

    fn telegram_binding(bot_username: &str) -> ChannelBinding {
        ChannelBinding {
            binding_id: "telegram".to_string(),
            kind: ChannelKind::Telegram,
            enabled: true,
            bridge_thread_id: Some("thread-telegram".to_string()),
            allowed_senders: vec![],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Telegram {
                bot_username: Some(bot_username.to_string()),
                thread_mode: Default::default(),
            },
        }
    }

    #[test]
    fn email_preserved_when_body_is_telegram_only() {
        let mut existing = base_profile();
        existing.channels = vec![email_binding(), telegram_binding("@old_bot")];

        let mut incoming = base_profile();
        incoming.channels = vec![telegram_binding("@new_bot")];

        merge_preserving_non_telegram_channels(Some(&existing), &mut incoming);

        assert_eq!(incoming.channels.len(), 2);
        assert!(incoming
            .channels
            .iter()
            .any(|b| b.kind == ChannelKind::Email && b.binding_id == "email"));
    }

    #[test]
    fn telegram_updated_from_body() {
        let mut existing = base_profile();
        existing.channels = vec![telegram_binding("@old_bot")];

        let mut incoming = base_profile();
        incoming.channels = vec![telegram_binding("@new_bot")];

        merge_preserving_non_telegram_channels(Some(&existing), &mut incoming);

        assert_eq!(incoming.channels.len(), 1);
        let ChannelKindConfig::Telegram { bot_username, .. } = &incoming.channels[0].kind_config
        else {
            panic!("expected telegram config");
        };
        assert_eq!(bot_username.as_deref(), Some("@new_bot"));
    }

    #[test]
    fn telegram_dropped_when_body_has_no_telegram() {
        let mut existing = base_profile();
        existing.channels = vec![telegram_binding("@old_bot")];

        let mut incoming = base_profile();
        incoming.channels = vec![];

        merge_preserving_non_telegram_channels(Some(&existing), &mut incoming);

        assert!(incoming.channels.is_empty());
    }

    #[test]
    fn discord_preserved() {
        let mut existing = base_profile();
        existing.channels = vec![discord_binding()];

        let mut incoming = base_profile();
        incoming.channels = vec![telegram_binding("@new_bot")];

        merge_preserving_non_telegram_channels(Some(&existing), &mut incoming);

        assert_eq!(incoming.channels.len(), 2);
        assert!(incoming.channels.iter().any(|b| b.kind == ChannelKind::Discord));
    }

    #[test]
    fn no_existing_profile_is_a_no_op() {
        let mut incoming = base_profile();
        incoming.channels = vec![telegram_binding("@new_bot")];

        merge_preserving_non_telegram_channels(None, &mut incoming);

        assert_eq!(incoming.channels.len(), 1);
    }

    /// Clobber regression for the non-Telegram case: even when the client's
    /// incoming binding carries a stale/empty `allowed_senders`, the merge
    /// must keep the server's existing value rather than taking the
    /// client's — this is the whole-document write that used to erase a
    /// sender linked out-of-band between the client's fetch and its save.
    #[test]
    fn allowed_senders_is_always_taken_from_existing_never_from_a_stale_incoming_value() {
        let mut existing = base_profile();
        let mut existing_email = email_binding();
        existing_email.allowed_senders = vec!["linked@example.com".to_string()];
        existing.channels = vec![existing_email];

        let mut incoming = base_profile();
        let mut incoming_email = email_binding();
        incoming_email.allowed_senders = vec![]; // stale client copy
        incoming.channels = vec![incoming_email];

        merge_preserving_non_telegram_channels(Some(&existing), &mut incoming);

        let merged = incoming
            .channels
            .iter()
            .find(|b| b.kind == ChannelKind::Email)
            .expect("email binding present");
        assert_eq!(
            merged.allowed_senders,
            vec!["linked@example.com".to_string()],
            "the seeded sender must survive a save carrying a stale client value"
        );
    }

    /// Same regression, explicitly for Telegram: unlike every other field on
    /// a Telegram binding (which the general form legitimately updates),
    /// `allowed_senders` must never come from the client's copy, even though
    /// Telegram itself isn't otherwise preserved from `existing`.
    #[test]
    fn telegram_allowed_senders_is_also_forced_from_existing_even_though_the_rest_of_the_binding_updates() {
        let mut existing = base_profile();
        let mut existing_telegram = telegram_binding("@old_bot");
        existing_telegram.allowed_senders = vec!["555".to_string()];
        existing.channels = vec![existing_telegram];

        let mut incoming = base_profile();
        let mut incoming_telegram = telegram_binding("@new_bot");
        incoming_telegram.allowed_senders = vec![]; // stale client copy
        incoming.channels = vec![incoming_telegram];

        merge_preserving_non_telegram_channels(Some(&existing), &mut incoming);

        assert_eq!(incoming.channels.len(), 1);
        let merged = &incoming.channels[0];
        assert_eq!(
            merged.allowed_senders,
            vec!["555".to_string()],
            "a linked Telegram chat must survive a general profile save"
        );
        let ChannelKindConfig::Telegram { bot_username, .. } = &merged.kind_config else {
            panic!("expected telegram config");
        };
        assert_eq!(
            bot_username.as_deref(),
            Some("@new_bot"),
            "the rest of the Telegram binding must still update normally"
        );
    }

    /// A binding with no existing server-side counterpart has no server
    /// value to preserve — it must fail closed to an empty list rather than
    /// trust whatever the client happened to submit.
    #[test]
    fn allowed_senders_defaults_to_empty_for_a_binding_with_no_existing_counterpart() {
        let mut existing = base_profile();
        existing.channels = vec![];

        let mut incoming = base_profile();
        let mut incoming_telegram = telegram_binding("@new_bot");
        incoming_telegram.allowed_senders = vec!["999".to_string()];
        incoming.channels = vec![incoming_telegram];

        merge_preserving_non_telegram_channels(Some(&existing), &mut incoming);

        assert!(
            incoming.channels[0].allowed_senders.is_empty(),
            "a brand new binding must fail closed, not trust the client's submitted allow-list"
        );
    }
}

/// End-to-end clobber regression through the real `update_agent` handler —
/// the unit tests above prove the merge function alone; these prove the
/// whole HTTP save path never reaches into `LinkedSenderStore`, so a sender
/// linked out-of-band survives a concurrent `PUT /agents/{id}` regardless of
/// what the merge does to the deprecated inline field.
#[cfg(test)]
mod update_agent_clobber_tests {
    use super::update_agent;
    use ao_engine::AppState;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::agent::{
        AgentProfile, AgentRunnerMode, ChannelBinding, ChannelKind, ChannelKindConfig, CliProviderConfig,
        InputMode, OutputFormat, ProviderConfig, TelegramThreadMode,
    };
    use axum::extract::{Path, State};
    use axum::Json;
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::error::AppError;

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

    fn telegram_binding(bot_username: &str) -> ChannelBinding {
        ChannelBinding {
            binding_id: "telegram".to_string(),
            kind: ChannelKind::Telegram,
            enabled: true,
            bridge_thread_id: None,
            allowed_senders: vec![],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Telegram {
                bot_username: Some(bot_username.to_string()),
                thread_mode: TelegramThreadMode::default(),
            },
        }
    }

    /// The Telegram case named explicitly by the security fix: a chat
    /// linked through the pairing flow (simulated here the same way the
    /// pairing writer does it — a direct `linked_senders.add_sender` call,
    /// never touching the profile document) must still be admitted after a
    /// concurrent `update_agent` save whose client body predates the link
    /// and so carries an empty `allowed_senders`.
    #[tokio::test]
    async fn a_telegram_sender_linked_out_of_band_survives_a_concurrent_update_agent_save() {
        let (state, _tmp) = setup_state().await;

        let mut profile = base_profile("agent-clobber-tg");
        profile.channels = vec![telegram_binding("@bot")];
        state.persistence.agents.create(&profile).await.unwrap();

        state
            .persistence
            .linked_senders
            .add_sender("agent-clobber-tg", "telegram", "555")
            .await
            .unwrap();

        // The client's copy of the profile is exactly what was fetched
        // before the pairing landed — its Telegram binding's inline
        // `allowed_senders` is still empty.
        let mut stale_body = profile.clone();
        stale_body.channels = vec![telegram_binding("@bot")];

        let updated = unwrap_ok(
            update_agent(
                State(Arc::clone(&state)),
                Path("agent-clobber-tg".to_string()),
                Json(stale_body),
            )
            .await,
        );
        assert_eq!(updated.id, "agent-clobber-tg");

        let senders = state
            .persistence
            .linked_senders
            .get("agent-clobber-tg", "telegram")
            .await
            .unwrap()
            .expect("store must still hold the linked sender")
            .senders;
        assert_eq!(
            senders,
            vec!["555".to_string()],
            "update_agent's whole-document save must never reach into LinkedSenderStore, \
             so the sender linked out-of-band survives regardless of what the client submitted"
        );
    }

    /// Same regression for a non-Telegram binding (Email), whose kind was
    /// already protected against the *binding-disappearing* half of the
    /// clobber before this fix — this proves the *allowed_senders* half is
    /// now closed too.
    #[tokio::test]
    async fn an_email_sender_survives_a_concurrent_update_agent_save_with_a_stale_body() {
        let (state, _tmp) = setup_state().await;

        let mut profile = base_profile("agent-clobber-email");
        let email_binding = ChannelBinding {
            binding_id: "email".to_string(),
            kind: ChannelKind::Email,
            enabled: false,
            bridge_thread_id: None,
            allowed_senders: vec![],
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Email {
                address: "agent@example.com".to_string(),
                imap_host: "imap.example.com".to_string(),
                imap_port: 993,
                smtp_host: "smtp.example.com".to_string(),
                smtp_port: 587,
                poll_secs: 60,
                require_auth_results: true,
            },
        };
        profile.channels = vec![email_binding];
        state.persistence.agents.create(&profile).await.unwrap();

        state
            .persistence
            .linked_senders
            .set(
                "agent-clobber-email",
                "email",
                &ao_protocol::linked_sender_list::LinkedSenderList {
                    senders: vec!["boss@example.com".to_string()],
                },
            )
            .await
            .unwrap();

        // A general-form save that only knows about Telegram (the form's
        // legacy shape) and so submits no Email binding at all.
        let mut stale_body = profile.clone();
        stale_body.channels = vec![];

        let _ = unwrap_ok(
            update_agent(
                State(Arc::clone(&state)),
                Path("agent-clobber-email".to_string()),
                Json(stale_body),
            )
            .await,
        );

        let senders = state
            .persistence
            .linked_senders
            .get("agent-clobber-email", "email")
            .await
            .unwrap()
            .expect("store must still hold the linked sender")
            .senders;
        assert_eq!(senders, vec!["boss@example.com".to_string()]);
    }
}

#[cfg(test)]
mod is_pending_form_latest_in_thread_tests {
    use super::is_pending_form_latest_in_thread;
    use ao_engine_tools_core::form_events::{form_request_entry, FormRequestMeta};
    use ao_persistence::paths::DataRoot;
    use ao_persistence::snapshot::PendingForm;
    use ao_persistence::thread_store::ThreadStore;
    use ao_persistence::transcript::TranscriptStore;
    use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
    use chrono::Utc;
    use serde_json::json;

    async fn stores() -> (tempfile::TempDir, ThreadStore, TranscriptStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path());
        root.ensure_directories().await.unwrap();
        let threads = ThreadStore::load(root.clone()).await.unwrap();
        let transcripts = TranscriptStore::new(root);
        (dir, threads, transcripts)
    }

    fn form_request(form_id: &str) -> TranscriptEntry {
        form_request_entry(
            "agent-1",
            FormRequestMeta {
                form_id: form_id.to_string(),
                spec: json!({ "title": "Q" }),
                mode: "async".to_string(),
            },
            false,
        )
    }

    /// Shape of the entry `LiveFormBridge::ask_form` persists for a sync
    /// form — same `form_request` event type, `mode: "sync"`, but
    /// `hidden_from_user: true` (unlike the async variant above).
    fn sync_form_request(form_id: &str) -> TranscriptEntry {
        form_request_entry(
            "agent-1",
            FormRequestMeta {
                form_id: form_id.to_string(),
                spec: json!({ "title": "Q" }),
                mode: "sync".to_string(),
            },
            true,
        )
    }

    fn user_message(text: &str) -> TranscriptEntry {
        TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::System("user".to_string()),
            content: text.to_string(),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        }
    }

    fn hidden_entry() -> TranscriptEntry {
        TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::System("system".to_string()),
            content: String::new(),
            event_type: "skill_body".to_string(),
            metadata: None,
            hidden_from_user: true,
        }
    }

    fn pending_form(form_id: &str, thread_id: Option<&str>) -> PendingForm {
        PendingForm {
            thread_id: thread_id.map(str::to_string),
            form_id: form_id.to_string(),
            spec: json!({}),
            is_latest_in_thread: true,
            orphaned: false,
        }
    }

    #[tokio::test]
    async fn default_thread_true_when_form_request_is_last_entry() {
        let (_dir, threads, transcripts) = stores().await;
        transcripts.append("agent-1", &form_request("form-1")).await.unwrap();

        let form = pending_form("form-1", None);
        assert!(is_pending_form_latest_in_thread(&threads, &transcripts, "agent-1", &form).await);
    }

    #[tokio::test]
    async fn default_thread_false_once_a_later_message_lands() {
        let (_dir, threads, transcripts) = stores().await;
        transcripts.append("agent-1", &form_request("form-1")).await.unwrap();
        transcripts.append("agent-1", &user_message("never mind")).await.unwrap();

        let form = pending_form("form-1", None);
        assert!(!is_pending_form_latest_in_thread(&threads, &transcripts, "agent-1", &form).await);
    }

    #[tokio::test]
    async fn default_thread_true_when_only_hidden_entries_follow() {
        let (_dir, threads, transcripts) = stores().await;
        transcripts.append("agent-1", &form_request("form-1")).await.unwrap();
        transcripts.append("agent-1", &hidden_entry()).await.unwrap();

        let form = pending_form("form-1", None);
        assert!(is_pending_form_latest_in_thread(&threads, &transcripts, "agent-1", &form).await);
    }

    #[tokio::test]
    async fn non_default_thread_true_when_form_request_is_last_entry() {
        let (_dir, threads, transcripts) = stores().await;
        let fresh = threads.build_fresh_thread("agent-1", Some("Spike".to_string()));
        let fresh = threads.create(fresh).await.unwrap();
        let path = std::path::PathBuf::from(&fresh.transcript_path);
        transcripts.append_at(&path, &form_request("form-a")).await.unwrap();

        let form = pending_form("form-a", Some(&fresh.id));
        assert!(is_pending_form_latest_in_thread(&threads, &transcripts, "agent-1", &form).await);
    }

    #[tokio::test]
    async fn non_default_thread_false_once_a_later_message_lands() {
        let (_dir, threads, transcripts) = stores().await;
        let fresh = threads.build_fresh_thread("agent-1", Some("Spike".to_string()));
        let fresh = threads.create(fresh).await.unwrap();
        let path = std::path::PathBuf::from(&fresh.transcript_path);
        transcripts.append_at(&path, &form_request("form-a")).await.unwrap();
        transcripts.append_at(&path, &user_message("moving on")).await.unwrap();

        let form = pending_form("form-a", Some(&fresh.id));
        assert!(!is_pending_form_latest_in_thread(&threads, &transcripts, "agent-1", &form).await);
    }

    /// A sync form_request entry (`hidden_from_user: true`) posted on the
    /// same thread AFTER an unrelated async form must not make that async
    /// form look stale — the sync entry is invisible to this scan by design
    /// (it exists only so the snapshot can rehydrate the sync form, not
    /// to be counted as timeline activity that supersedes anything).
    #[tokio::test]
    async fn default_thread_true_when_only_a_sync_form_request_follows() {
        let (_dir, threads, transcripts) = stores().await;
        transcripts.append("agent-1", &form_request("form-1")).await.unwrap();
        transcripts.append("agent-1", &sync_form_request("form-2")).await.unwrap();

        let form = pending_form("form-1", None);
        assert!(is_pending_form_latest_in_thread(&threads, &transcripts, "agent-1", &form).await);
    }

    /// A form on thread A must never read thread B's tail — regression guard
    /// against accidentally falling back to the agent-keyed default file for
    /// a resolvable non-default thread.
    #[tokio::test]
    async fn non_default_threads_do_not_leak_into_each_other() {
        let (_dir, threads, transcripts) = stores().await;
        let thread_a = threads.create(threads.build_fresh_thread("agent-1", None)).await.unwrap();
        let thread_b = threads.create(threads.build_fresh_thread("agent-1", None)).await.unwrap();

        transcripts
            .append_at(&std::path::PathBuf::from(&thread_a.transcript_path), &form_request("form-a"))
            .await
            .unwrap();
        // Thread B has unrelated later activity — must not affect thread A's read.
        transcripts
            .append_at(&std::path::PathBuf::from(&thread_b.transcript_path), &user_message("unrelated"))
            .await
            .unwrap();

        let form = pending_form("form-a", Some(&thread_a.id));
        assert!(is_pending_form_latest_in_thread(&threads, &transcripts, "agent-1", &form).await);
    }
}
