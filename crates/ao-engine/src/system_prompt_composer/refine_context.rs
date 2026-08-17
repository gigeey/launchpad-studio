//! Compact "EXECUTION ENVIRONMENT" catalog injected into the `/prompt-refine`
//! one-shot (`ao_server::routes::prompt_refine`) so the refiner can rewrite a
//! prompt in terms of the executing agent's real tools, skills, workflows,
//! and stated preferences — by exact name. Awareness only: the refine call
//! stays a single tool-less `provider.complete`, no `tools` field and no
//! agent loop. This module only produces prompt TEXT.
//!
//! Tools are uniform across every agent (the whole-server registry), while
//! skills and workflows are scoped to the resolved agent profile — the same
//! split [`super::compose_system_prompt`] draws for the agent's real system
//! prompt. Kept as its own small catalog here rather than reusing that full
//! composed prompt, which carries persona/instructions text that has no
//! bearing on what the refiner needs to know.

use std::path::PathBuf;

use ao_engine_tools_core::skill_registry::SkillEntry;
use ao_engine_tools_core::WorkflowRunnerHandle;
use ao_protocol::agent::{AgentProfile, WorkflowBinding};

use crate::AppState;

const NO_ENVIRONMENT_FALLBACK: &str = "Tools, skills, and workflows will be available when the \
refined prompt executes, but none are currently registered for this agent — rewrite for clarity \
alone.";

const EXECUTION_ENVIRONMENT_FRAMING: &str = "The refined prompt will later execute as a full \
agent turn with the tools, skills, and workflows listed below available, under the user's \
preferences/memories below. Rewrite the assignment prompt to be clearer AND to leverage this \
environment. When a preference or memory expresses a tool/skill/workflow choice, bake that \
choice into the rewrite by exact name (e.g. \"use mcp__launchpad__SendEmail for email\"). \
Reference tools/skills/workflows by their EXACT names only when genuinely relevant to the task \
— do not stuff the prompt with irrelevant names, and never invent a name that is not in the \
lists below.";

/// Gather `profile`'s real execution catalog from live state and render the
/// "EXECUTION ENVIRONMENT" block described in the module docs.
pub async fn build_execution_environment(state: &AppState, profile: &AgentProfile) -> String {
    let tool_names = state.tools_registry.list();

    let skill_registry = crate::agent_context::build_skill_registry(
        state.persistence.data_root.root(),
        profile,
        Some(state.mcp_manager.as_ref()),
    );
    let skills: Vec<(String, String)> = skill_registry
        .all_visible()
        .filter_map(|(name, entry)| match entry {
            SkillEntry::Ok(record) => Some((name.to_string(), first_line(&record.description))),
            SkillEntry::Err(_) => None,
        })
        .collect();

    let workflow_summaries = match &profile.workflows {
        None | Some(WorkflowBinding::None) => vec![],
        Some(WorkflowBinding::All) => state.workflow_runner.get_workflow_summaries(None).await,
        Some(WorkflowBinding::List(ids)) => {
            state.workflow_runner.get_workflow_summaries(Some(ids.as_slice())).await
        }
    };
    let workflows: Vec<(String, String)> = workflow_summaries
        .iter()
        .map(|wf| {
            let description = wf.description.as_deref().map(first_line).unwrap_or_default();
            (wf.name.clone(), description)
        })
        .collect();

    let user_prefs =
        state.persistence.preferences.get().await.unwrap_or(None).unwrap_or_default();
    let user_addressing = super::build_user_addressing(&user_prefs);

    let agent_memories = state.persistence.memory.list(&profile.id).await.unwrap_or_default();
    let global_memories = state.persistence.memory.list_global().await.unwrap_or_default();
    let project_memories = load_project_memories(state, profile).await;
    let memories_block =
        super::build_memories_section(&agent_memories, &project_memories, &global_memories);

    render_execution_environment(
        &tool_names,
        &skills,
        &workflows,
        user_addressing.as_deref(),
        memories_block.as_deref(),
    )
}

/// Resolve the same `working_dir` → cwd fallback the native runner uses at
/// session-registration time (`agent_runner::native`), so project-scoped
/// memory resolves the same way it would for this agent's real runs. A
/// refine call has no active session/thread, so `working_dir` (or the
/// process cwd, for agents that never set one) is the closest stand-in.
async fn load_project_memories(
    state: &AppState,
    profile: &AgentProfile,
) -> Vec<ao_protocol::memory::MemoryEntry> {
    let cwd = profile
        .working_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    match ao_persistence::project_key::resolve_project_key(&cwd).await {
        Ok(canonical_key) => {
            let hash = ao_persistence::project_key::hash_project_key(&canonical_key);
            state.persistence.memory.list_project(&hash).await.unwrap_or_default()
        }
        Err(_) => vec![],
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

/// Pure renderer — no I/O, deterministic given its inputs. Split out from
/// [`build_execution_environment`] so the "degrade to the generic fallback
/// line" and "known entries survive into the block" behaviors are directly
/// testable against plain data, the same way `compose_system_prompt` is
/// tested elsewhere in this module.
fn render_execution_environment(
    tool_names: &[String],
    skills: &[(String, String)],
    workflows: &[(String, String)],
    user_addressing: Option<&str>,
    memories_block: Option<&str>,
) -> String {
    let mut sections: Vec<String> = Vec::new();

    if !tool_names.is_empty() {
        sections.push(render_tools_section(tool_names));
    }
    if !skills.is_empty() {
        sections.push(render_named_list_section("## This Agent's Skills", skills));
    }
    if !workflows.is_empty() {
        sections.push(render_named_list_section("## This Agent's Workflows", workflows));
    }

    let mut preferences_parts: Vec<&str> = Vec::new();
    if let Some(addressing) = user_addressing {
        preferences_parts.push(addressing);
    }
    if let Some(memories) = memories_block {
        preferences_parts.push(memories);
    }
    if !preferences_parts.is_empty() {
        sections.push(format!("## Preferences + Memories\n\n{}", preferences_parts.join("\n\n")));
    }

    if sections.is_empty() {
        return format!("# EXECUTION ENVIRONMENT\n\n{}", NO_ENVIRONMENT_FALLBACK);
    }

    format!(
        "# EXECUTION ENVIRONMENT\n\n{}\n\n{}",
        EXECUTION_ENVIRONMENT_FRAMING,
        sections.join("\n\n")
    )
}

fn render_tools_section(tool_names: &[String]) -> String {
    let mut native: Vec<&str> = Vec::new();
    let mut by_server: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();

    for name in tool_names {
        match name.strip_prefix("mcp__").and_then(|rest| rest.split("__").next()) {
            Some(server) if !server.is_empty() => {
                by_server.entry(server).or_default().push(name.as_str());
            }
            _ => native.push(name.as_str()),
        }
    }

    let mut lines = vec!["## Available Tools".to_string(), String::new()];
    if !native.is_empty() {
        lines.push(format!("Native: {}", native.join(", ")));
    }
    for (server, names) in by_server {
        lines.push(format!("MCP `{}`: {}", server, names.join(", ")));
    }
    lines.join("\n")
}

fn render_named_list_section(header: &str, entries: &[(String, String)]) -> String {
    let mut lines = vec![header.to_string(), String::new()];
    for (name, description) in entries {
        if description.is_empty() {
            lines.push(format!("- **{}**", name));
        } else {
            lines.push(format!("- **{}** — {}", name, description));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ao_protocol::agent::{CliProviderConfig, InputMode, OutputFormat, ProviderConfig};

    use super::*;

    fn test_profile(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: "Test".to_string(),
            description: "test".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "claude".to_string(),
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
            runner_mode: Default::default(),
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

    #[test]
    fn render_execution_environment_includes_known_tool_and_skill() {
        let tools = vec!["Read".to_string(), "mcp__launchpad__SendEmail".to_string()];
        let skills = vec![("deploy".to_string(), "Deploy the current branch".to_string())];
        let workflows = vec![("release-checklist".to_string(), String::new())];

        let block = render_execution_environment(
            &tools,
            &skills,
            &workflows,
            Some("You are assisting Andrew."),
            Some("[Agent Memories]\n- prefer mcp__launchpad__SendEmail for email"),
        );

        assert!(block.contains("Read"), "expected known tool name in block: {block}");
        assert!(
            block.contains("mcp__launchpad__SendEmail"),
            "expected MCP tool name in block: {block}"
        );
        assert!(block.contains("deploy"), "expected skill name in block: {block}");
        assert!(block.contains("release-checklist"), "expected workflow name in block: {block}");
        assert!(
            block.contains("prefer mcp__launchpad__SendEmail for email"),
            "expected memory entry in block: {block}"
        );
    }

    #[test]
    fn render_execution_environment_empty_context_degrades_to_generic_line() {
        let block = render_execution_environment(&[], &[], &[], None, None);

        assert_eq!(block, format!("# EXECUTION ENVIRONMENT\n\n{}", NO_ENVIRONMENT_FALLBACK));
        assert!(!block.contains("## Available Tools"));
        assert!(!block.contains("## This Agent's Skills"));
    }

    #[tokio::test]
    async fn build_execution_environment_wires_real_registry_skill_and_memory() {
        use ao_process::mock::MockProcessSupervisor;
        use ao_protocol::memory::MemorySource;

        let _guard =
            crate::plugin_paths::tests::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

        let skill_dir = tmp.path().join("skills").join("deploy");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: deploy\ndescription: Deploy the current branch to staging\n---\nbody",
        )
        .unwrap();

        let state = AppState::new_with_mock(MockProcessSupervisor::new(vec![]))
            .await
            .expect("init state");

        let mut profile = test_profile("refine-context-agent");
        profile.skills = vec!["deploy".to_string()];

        state
            .persistence
            .memory
            .add(&profile.id, "prefer RunSkill over ad-hoc bash", MemorySource::Manual)
            .await
            .expect("write agent memory");

        let block = build_execution_environment(&state, &profile).await;

        std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

        assert!(block.contains("Read"), "expected known native tool in block: {block}");
        assert!(block.contains("deploy"), "expected profile-scoped skill in block: {block}");
        assert!(
            block.contains("prefer RunSkill over ad-hoc bash"),
            "expected profile-scoped memory entry in block: {block}"
        );
    }
}
