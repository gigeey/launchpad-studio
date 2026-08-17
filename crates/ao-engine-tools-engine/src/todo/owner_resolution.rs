use std::sync::Arc;

use ao_engine_tools_core::ToolOutput;
use ao_persistence::profiles::AgentProfileStore;

/// Resolve a raw `owner` value from `TodoCreate`/`TodoUpdate` into a canonical
/// agent_id, mirroring how the `Delegate` tool resolves `target`:
///
/// 1. If `raw_owner` already names an existing agent (`{data_root}/agents/{raw_owner}.yaml`
///    exists), it is used as-is.
/// 2. Otherwise `raw_owner` is treated as a display name and looked up in the
///    calling agent's `delegates_to` address book (exact match, same as
///    `Delegate`).
/// 3. If neither resolves, returns a ready-to-return [`ToolOutput::Error`]
///    that names the unresolved value and enumerates the calling agent's
///    available address-book targets, so the caller can self-correct without
///    a round-trip through the dispatcher's later "agent missing" failure.
///
/// `store` is `None` in contexts that don't wire agent-profile persistence
/// (e.g. most unit-test fixtures) — resolution is then skipped entirely and
/// `raw_owner` passes through unchanged, matching the tool's behavior before
/// this resolver existed.
pub(crate) async fn resolve_owner(
    store: Option<&Arc<AgentProfileStore>>,
    caller_agent_id: &str,
    raw_owner: &str,
) -> Result<String, ToolOutput> {
    let Some(store) = store else {
        return Ok(raw_owner.to_string());
    };

    match store.get(raw_owner).await {
        Ok(Some(_)) => return Ok(raw_owner.to_string()),
        Ok(None) => {}
        Err(e) => {
            return Err(ToolOutput::error(
                format!("failed to resolve owner '{raw_owner}': {e}"),
                false,
            ));
        }
    }

    let caller_profile = match store.get(caller_agent_id).await {
        Ok(opt) => opt,
        Err(e) => {
            return Err(ToolOutput::error(
                format!("failed to load calling agent's profile: {e}"),
                false,
            ));
        }
    };

    if let Some(profile) = &caller_profile {
        if let Some(entry) = profile.delegates_to.iter().find(|e| e.name == raw_owner) {
            return Ok(entry.target_agent_id.clone());
        }
    }

    let available: Vec<String> = caller_profile
        .map(|p| {
            p.delegates_to
                .iter()
                .map(|e| format!("{} ({})", e.name, e.target_agent_id))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Err(ToolOutput::error(
        format!(
            "owner '{raw_owner}' could not be resolved: it is not a known agent_id, and no \
             address-book entry named '{raw_owner}' exists on the calling agent. Available \
             address-book targets: {}.",
            if available.is_empty() {
                "(none configured)".to_string()
            } else {
                available.join(", ")
            }
        ),
        true,
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ao_persistence::paths::DataRoot;
    use ao_protocol::agent::{
        AgentProfile, AgentRunnerMode, CliProviderConfig, DelegateTarget, InputMode, OutputFormat,
        ProviderConfig,
    };
    use tempfile::TempDir;

    use super::*;

    fn make_profile(id: &str, name: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: name.to_string(),
            description: "test agent".to_string(),
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
            runner_mode: AgentRunnerMode::default(),
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
            channels: vec![],
            max_turns: None,
        }
    }

    async fn setup_store(tmp: &TempDir, caller: &AgentProfile, target: &AgentProfile) -> Arc<AgentProfileStore> {
        let data_root = DataRoot::new(tmp.path());
        std::fs::create_dir_all(data_root.agents_dir()).unwrap();
        let store = Arc::new(AgentProfileStore::new(data_root));
        store.create(caller).await.unwrap();
        store.create(target).await.unwrap();
        store
    }

    #[tokio::test]
    async fn no_store_passes_raw_value_through() {
        let result = resolve_owner(None, "caller", "some-owner").await;
        assert_eq!(result.unwrap(), "some-owner");
    }

    #[tokio::test]
    async fn existing_agent_id_passes_through_unchanged() {
        let tmp = TempDir::new().unwrap();
        let caller = make_profile("caller", "Caller");
        let target = make_profile("frontend-worker", "Frontend Worker");
        let store = setup_store(&tmp, &caller, &target).await;

        let result = resolve_owner(Some(&store), "caller", "frontend-worker").await;
        assert_eq!(result.unwrap(), "frontend-worker");
    }

    #[tokio::test]
    async fn known_display_name_resolves_to_target_agent_id() {
        let tmp = TempDir::new().unwrap();
        let mut caller = make_profile("caller", "Caller");
        caller.delegates_to = vec![DelegateTarget {
            target_agent_id: "frontend-worker".to_string(),
            name: "Frontend".to_string(),
            purpose: "handle frontend tasks".to_string(),
            share_context_allowed: false,
        }];
        let target = make_profile("frontend-worker", "Frontend Worker");
        let store = setup_store(&tmp, &caller, &target).await;

        let result = resolve_owner(Some(&store), "caller", "Frontend").await;
        assert_eq!(result.unwrap(), "frontend-worker");
    }

    #[tokio::test]
    async fn unknown_owner_fails_fast_and_lists_available_targets() {
        let tmp = TempDir::new().unwrap();
        let mut caller = make_profile("caller", "Caller");
        caller.delegates_to = vec![DelegateTarget {
            target_agent_id: "frontend-worker".to_string(),
            name: "Frontend".to_string(),
            purpose: "handle frontend tasks".to_string(),
            share_context_allowed: false,
        }];
        let target = make_profile("frontend-worker", "Frontend Worker");
        let store = setup_store(&tmp, &caller, &target).await;

        let err = resolve_owner(Some(&store), "caller", "Backend")
            .await
            .expect_err("unresolvable owner must fail fast");
        match err {
            ToolOutput::Error { message, recoverable } => {
                assert!(message.contains("Backend"), "got: {message}");
                assert!(message.contains("Frontend"), "got: {message}");
                assert!(message.contains("frontend-worker"), "got: {message}");
                assert!(recoverable, "unresolvable owner must be a recoverable error");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolution_is_case_sensitive_exact_match_like_delegate() {
        let tmp = TempDir::new().unwrap();
        let mut caller = make_profile("caller", "Caller");
        caller.delegates_to = vec![DelegateTarget {
            target_agent_id: "frontend-worker".to_string(),
            name: "Frontend".to_string(),
            purpose: "handle frontend tasks".to_string(),
            share_context_allowed: false,
        }];
        let target = make_profile("frontend-worker", "Frontend Worker");
        let store = setup_store(&tmp, &caller, &target).await;

        // Differently-cased name must NOT match — same exact-match semantics
        // as Delegate's `profile.delegates_to.iter().find(|e| &e.name == name)`.
        let err = resolve_owner(Some(&store), "caller", "frontend")
            .await
            .expect_err("case-mismatched name must not resolve");
        assert!(matches!(err, ToolOutput::Error { .. }));
    }
}
