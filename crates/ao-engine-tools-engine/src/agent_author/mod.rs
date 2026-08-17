mod prompt;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ao_engine_tools_core::{AgentProfileCacheInvalidator, EngineTool, LoadPolicy, RunnerContext, ToolOutput};
use ao_persistence::profiles::AgentProfileStore;
use ao_persistence::snapshot::SnapshotStore;
use ao_protocol::{
    agent::{
        AgentProfile, AgentRunnerMode, CliProviderConfig, InputMode, NativeProvider, OutputFormat,
        PluginEnablement, ProviderConfig, WorkflowBinding,
    },
    agent_home::ensure_agent_home,
    error::AoError,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use uuid::Uuid;

/// Agent-facing tool for creating and editing `AgentProfile`s — most
/// importantly, an agent's own `persona` and `special_instructions`.
///
/// Dependencies are `Option` so `register_all` can install a stub (the name
/// is present in the catalog, but every op fails with a clear error) before
/// the fully-wired instance replaces it at `AppState` construction time —
/// the same pattern [`crate::Delegate`] uses for its own store injection.
/// Constructor injection is required here because [`RunnerContext`] does not
/// expose persistence, snapshots, or the context cache directly.
pub struct AgentAuthor {
    store: Option<Arc<AgentProfileStore>>,
    snapshots: Option<Arc<SnapshotStore>>,
    cache: Option<Arc<dyn AgentProfileCacheInvalidator>>,
}

impl AgentAuthor {
    pub fn new() -> Self {
        Self {
            store: None,
            snapshots: None,
            cache: None,
        }
    }

    pub fn with_deps(
        store: Arc<AgentProfileStore>,
        snapshots: Arc<SnapshotStore>,
        cache: Arc<dyn AgentProfileCacheInvalidator>,
    ) -> Self {
        Self {
            store: Some(store),
            snapshots: Some(snapshots),
            cache: Some(cache),
        }
    }
}

impl Default for AgentAuthor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EngineTool for AgentAuthor {
    fn name(&self) -> &str {
        "AgentAuthor"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::input_schema()
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }

    fn mutates_filesystem(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let (store, snapshots, cache) = match (&self.store, &self.snapshots, &self.cache) {
            (Some(s), Some(sn), Some(c)) => (s, sn, c),
            _ => {
                return Ok(ToolOutput::error(
                    "AgentAuthor requires an agent store (none configured in this context)",
                    false,
                ))
            }
        };

        let op = match input.get("op").and_then(|v| v.as_str()) {
            Some(o) => o,
            None => return Ok(ToolOutput::error("missing required field: op", true)),
        };

        match op {
            "create" => create_agent(store, snapshots, &input).await,
            "update" => update_agent(store, snapshots, cache, ctx, &input).await,
            "get" => get_agent(store, &input).await,
            "list" => list_agents(store).await,
            other => Ok(ToolOutput::error(
                format!("unknown op '{other}': must be create, update, get, or list"),
                true,
            )),
        }
    }
}

async fn create_agent(
    store: &Arc<AgentProfileStore>,
    snapshots: &Arc<SnapshotStore>,
    input: &Value,
) -> Result<ToolOutput, AoError> {
    let name = match input.get("name").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return Ok(ToolOutput::error("create requires a non-empty 'name'", true)),
    };
    let description = match input.get("description").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return Ok(ToolOutput::error("create requires a 'description'", true)),
    };

    let template = input
        .get("template")
        .and_then(|v| v.as_str())
        .unwrap_or("claude");
    let provider = match template_provider(template) {
        Some(p) => p,
        None => {
            return Ok(ToolOutput::error(
                format!(
                    "unknown template '{template}': must be claude, cursor, codex, or antigravity"
                ),
                true,
            ))
        }
    };

    let runner_mode = match input.get("runner_mode").and_then(|v| v.as_str()) {
        Some("cli") | None => AgentRunnerMode::Cli,
        Some("api") => AgentRunnerMode::Api,
        Some(other) => {
            return Ok(ToolOutput::error(
                format!("unknown runner_mode '{other}': must be cli or api"),
                true,
            ))
        }
    };

    let native_provider = match input.get("native_provider").and_then(|v| v.as_str()) {
        None => None,
        Some("anthropic") => Some(NativeProvider::Anthropic),
        Some("openai") => Some(NativeProvider::Openai),
        Some("openrouter") => Some(NativeProvider::OpenRouter),
        Some(other) => {
            return Ok(ToolOutput::error(
                format!("unknown native_provider '{other}': must be anthropic, openai, or openrouter"),
                true,
            ))
        }
    };

    let profile = AgentProfile {
        id: Uuid::new_v4().to_string(),
        name,
        description,
        emoji: input.get("emoji").and_then(|v| v.as_str()).map(str::to_string),
        provider: ProviderConfig::Cli(provider),
        model: input.get("model").and_then(|v| v.as_str()).map(str::to_string),
        skills: Vec::new(),
        system_prompt: None,
        tools: None,
        env: HashMap::new(),
        max_instances: 1,
        timeout_seconds: 300,
        max_turns: None,
        working_dir: None,
        home_dir: None,
        serialize: true,
        workflows: None,
        template: Some(template.to_string()),
        runner_mode,
        enabled_plugins: HashMap::new(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
        owning_team_id: None,
        native_provider,
        thinking: None,
        // Not exposed as AgentAuthor tool inputs (same as `thinking` above) —
        // an author-created agent gets these knobs from the persisted
        // provider-config/hardcoded-default tiers of `resolve_*` until a
        // dedicated input is added.
        max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
        delegates_to: Vec::new(),
        persona: input.get("persona").and_then(|v| v.as_str()).map(str::to_string),
        special_instructions: input
            .get("special_instructions")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        legacy_system_prompt: None,
        max_delegation_depth: None,
        channels: vec![],
    };

    if let Err(e) = store.create(&profile).await {
        return Ok(ToolOutput::error(
            format!("failed to create agent profile: {e}"),
            true,
        ));
    }

    let agent_home = profile
        .home_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| store.data_root().agent_home_dir(&profile.id));
    if let Err(e) = ensure_agent_home(&agent_home).await {
        return Ok(ToolOutput::error(
            format!("agent profile created but failed to scaffold its home directory: {e}"),
            false,
        ));
    }

    if let Err(e) = sync_snapshot_entry(snapshots, &profile).await {
        return Ok(ToolOutput::error(
            format!("agent profile created but failed to update its snapshot entry: {e}"),
            false,
        ));
    }

    Ok(ToolOutput::structured(json!({
        "id": profile.id,
        "name": profile.name,
        "description": profile.description,
    })))
}

async fn update_agent(
    store: &Arc<AgentProfileStore>,
    snapshots: &Arc<SnapshotStore>,
    cache: &Arc<dyn AgentProfileCacheInvalidator>,
    ctx: &RunnerContext,
    input: &Value,
) -> Result<ToolOutput, AoError> {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return Ok(ToolOutput::error("update requires a non-empty 'id'", true)),
    };

    let mut profile = match store.get(&id).await {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(ToolOutput::error(format!("agent '{id}' not found"), true)),
        Err(e) => {
            return Ok(ToolOutput::error(
                format!("failed to load agent profile: {e}"),
                false,
            ))
        }
    };

    // Self-edit safety: archive whatever persona/special_instructions (or,
    // failing that, the legacy system_prompt blob) currently render for this
    // agent BEFORE either field is overwritten, so the prior behavior is
    // always one more `update` away.
    let touches_persona = input.get("persona").is_some();
    let touches_special_instructions = input.get("special_instructions").is_some();
    if touches_persona || touches_special_instructions {
        let archived = archive_blob(profile.persona.as_deref(), profile.special_instructions.as_deref())
            .or_else(|| profile.system_prompt.clone());
        if let Some(archived) = archived {
            profile.legacy_system_prompt = Some(archived);
        }
    }

    if let Some(v) = input.get("name").and_then(|v| v.as_str()) {
        profile.name = v.to_string();
    }
    if let Some(v) = input.get("description").and_then(|v| v.as_str()) {
        profile.description = v.to_string();
    }
    if input.get("emoji").is_some() {
        profile.emoji = input.get("emoji").and_then(|v| v.as_str()).map(str::to_string);
    }
    if input.get("model").is_some() {
        profile.model = input.get("model").and_then(|v| v.as_str()).map(str::to_string);
    }
    if touches_persona {
        profile.persona = input.get("persona").and_then(|v| v.as_str()).map(str::to_string);
    }
    if touches_special_instructions {
        profile.special_instructions = input
            .get("special_instructions")
            .and_then(|v| v.as_str())
            .map(str::to_string);
    }

    let allow_capability_changes = input
        .get("allow_capability_changes")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if allow_capability_changes {
        if let Some(skills_val) = input.get("skills") {
            match apply_skills(&mut profile, skills_val, ctx) {
                Ok(()) => {}
                Err(out) => return Ok(out),
            }
        }
        if let Some(workflows_val) = input.get("workflows") {
            match serde_json::from_value::<WorkflowBinding>(workflows_val.clone()) {
                Ok(binding) => profile.workflows = Some(binding),
                Err(e) => {
                    return Ok(ToolOutput::error(format!("invalid 'workflows': {e}"), true))
                }
            }
        }
        if let Some(plugins_val) = input.get("enabled_plugins") {
            match serde_json::from_value::<HashMap<String, PluginEnablement>>(plugins_val.clone())
            {
                Ok(map) => profile.enabled_plugins = map,
                Err(e) => {
                    return Ok(ToolOutput::error(
                        format!("invalid 'enabled_plugins': {e}"),
                        true,
                    ))
                }
            }
        }
    }

    if let Err(e) = store.update(&profile).await {
        return Ok(ToolOutput::error(
            format!("failed to update agent profile: {e}"),
            true,
        ));
    }

    cache.invalidate(&id).await;

    if let Err(e) = sync_snapshot_entry(snapshots, &profile).await {
        return Ok(ToolOutput::error(
            format!("agent profile updated but failed to update its snapshot entry: {e}"),
            false,
        ));
    }

    Ok(ToolOutput::structured(
        serde_json::to_value(&profile).map_err(|e| AoError::Json(e.to_string()))?,
    ))
}

/// Validate and apply a capability-gated `skills` patch, rejecting the whole
/// call if any name is absent from the caller's live skill registry.
fn apply_skills(profile: &mut AgentProfile, skills_val: &Value, ctx: &RunnerContext) -> Result<(), ToolOutput> {
    let entries = skills_val
        .as_array()
        .ok_or_else(|| ToolOutput::error("'skills' must be an array of strings", true))?;

    let registry = ctx.skill_registry.read().unwrap();
    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry
            .as_str()
            .ok_or_else(|| ToolOutput::error("'skills' entries must be strings", true))?;
        if registry.get(name).is_none() {
            return Err(ToolOutput::error(
                format!("unknown skill '{name}': not present in this agent's skill registry"),
                true,
            ));
        }
        resolved.push(name.to_string());
    }
    profile.skills = resolved;
    Ok(())
}

async fn get_agent(store: &Arc<AgentProfileStore>, input: &Value) -> Result<ToolOutput, AoError> {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return Ok(ToolOutput::error("get requires a non-empty 'id'", true)),
    };

    match store.get(&id).await {
        Ok(Some(profile)) => Ok(ToolOutput::structured(
            serde_json::to_value(&profile).map_err(|e| AoError::Json(e.to_string()))?,
        )),
        Ok(None) => Ok(ToolOutput::error(format!("agent '{id}' not found"), true)),
        Err(e) => Ok(ToolOutput::error(
            format!("failed to load agent profile: {e}"),
            false,
        )),
    }
}

async fn list_agents(store: &Arc<AgentProfileStore>) -> Result<ToolOutput, AoError> {
    match store.list().await {
        Ok(profiles) => {
            let items: Vec<Value> = profiles
                .iter()
                .map(|p| {
                    json!({
                        "id": p.id,
                        "name": p.name,
                        "description": p.description,
                    })
                })
                .collect();
            Ok(ToolOutput::structured(json!({ "agents": items })))
        }
        Err(e) => Ok(ToolOutput::error(
            format!("failed to list agent profiles: {e}"),
            false,
        )),
    }
}

/// Mirrors the snapshot patch applied by the `create_agent`/`update_agent`
/// HTTP handlers so agents authored or edited through this tool show up in
/// the sidebar the same way ones created through the UI do.
async fn sync_snapshot_entry(snapshots: &Arc<SnapshotStore>, profile: &AgentProfile) -> Result<(), AoError> {
    let name = profile.name.clone();
    let emoji = profile.emoji.clone();
    let file_caps = profile.file_capabilities_supported();
    let owning_team_id = profile.owning_team_id.clone();
    snapshots
        .update_agent_entry(&profile.id, |entry| {
            entry.name = name;
            entry.emoji = emoji;
            entry.file_capabilities_supported = file_caps;
            entry.owning_team_id = owning_team_id;
        })
        .await
}

/// Combine the current persona/special_instructions into a single archival
/// blob for `legacy_system_prompt`. `None` when neither field is set — the
/// caller falls back to the raw `system_prompt` blob in that case.
fn archive_blob(persona: Option<&str>, special_instructions: Option<&str>) -> Option<String> {
    match (persona, special_instructions) {
        (None, None) => None,
        (Some(p), None) => Some(p.to_string()),
        (None, Some(s)) => Some(s.to_string()),
        (Some(p), Some(s)) => Some(format!("{p}\n\n{s}")),
    }
}

/// Built-in provider presets for `create`'s `template` field. Mirrors the
/// three CLI templates offered by the agent creation UI
/// (`frontend/src/data/agentTemplates.ts`) so a tool-authored agent gets the
/// same working defaults a user would get by hand.
fn template_provider(template: &str) -> Option<CliProviderConfig> {
    match template {
        "claude" => Some(CliProviderConfig {
            command: "claude".to_string(),
            args: vec![
                "--print".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "--include-partial-messages".to_string(),
            ],
            normalizer: Some("Claude".to_string()),
            output_format: OutputFormat::StreamJson,
            input_mode: InputMode::Arg,
            model_arg: Some("--model".to_string()),
            model_aliases: HashMap::new(),
            system_prompt_arg: Some("--append-system-prompt".to_string()),
            session_arg: None,
            resume_args: Vec::new(),
            session_id_fields: Vec::new(),
            clear_env: false,
            no_output_timeout_ms: 30000,
            file_capabilities: None,
        }),
        "cursor" => Some(CliProviderConfig {
            command: "cursor-agent".to_string(),
            args: vec![
                "--print".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--force".to_string(),
                "--approve-mcps".to_string(),
                "--trust".to_string(),
                "--stream-partial-output".to_string(),
            ],
            normalizer: Some("cursor-agent".to_string()),
            output_format: OutputFormat::StreamJson,
            input_mode: InputMode::Arg,
            model_arg: Some("--model".to_string()),
            model_aliases: HashMap::new(),
            system_prompt_arg: None,
            session_arg: None,
            resume_args: Vec::new(),
            session_id_fields: Vec::new(),
            clear_env: false,
            no_output_timeout_ms: 30000,
            file_capabilities: None,
        }),
        "codex" => Some(CliProviderConfig {
            command: "codex".to_string(),
            args: vec![
                "exec".to_string(),
                "--json".to_string(),
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--skip-git-repo-check".to_string(),
            ],
            normalizer: Some("codex".to_string()),
            output_format: OutputFormat::StreamJsonl,
            input_mode: InputMode::Arg,
            model_arg: Some("--model".to_string()),
            model_aliases: HashMap::new(),
            system_prompt_arg: None,
            session_arg: None,
            resume_args: Vec::new(),
            session_id_fields: vec!["thread_id".to_string()],
            clear_env: false,
            no_output_timeout_ms: 30000,
            file_capabilities: None,
        }),
        // v1 scaffold: no UI template picker entry yet (this crate's preset
        // list mirrors `frontend/src/data/agentTemplates.ts`, which hasn't
        // been given an Antigravity option) — an agent using this provider
        // can only be created through this backend `create` path today, not
        // the agent-creation UI. `-p`/MCP-file/API-key wiring live in
        // `CliAgentRunner::build_argv` (`ao-engine`), gated on the `agy`
        // command basename.
        "antigravity" => Some(CliProviderConfig {
            command: "agy".to_string(),
            args: vec![
                "--dangerously-skip-permissions".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
            ],
            normalizer: Some("agy".to_string()),
            output_format: OutputFormat::StreamJson,
            input_mode: InputMode::Arg,
            model_arg: Some("--model".to_string()),
            model_aliases: HashMap::new(),
            system_prompt_arg: None,
            session_arg: None,
            resume_args: Vec::new(),
            session_id_fields: vec!["conversation_id".to_string()],
            clear_env: false,
            no_output_timeout_ms: 30000,
            file_capabilities: None,
        }),
        _ => None,
    }
}
