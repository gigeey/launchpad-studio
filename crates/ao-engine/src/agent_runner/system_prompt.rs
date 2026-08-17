/// Legacy system-prompt helpers shared by the CLI runner.
///
/// The canonical system prompt is now produced by `crate::system_prompt_composer`.
/// This module retains `load_context_blocks`, `render_delegate_targets`,
/// `build_workflows_in_scope_block`, and related helpers that the CLI runner
/// still uses; they will be retired.

use std::collections::{HashMap, HashSet};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use ao_protocol::agent::{AgentId, AgentProfile, WorkflowBinding};
use ao_protocol::workflow::WorkflowSummary;

use crate::agent_context::{AgentHomeContext, WorkspaceContext};

#[cfg(test)]
const DEFAULT_INSTRUCTION_FILENAME: &str = "CLAUDE.md";

// ─── Delegate Targets block ────────────────────────────────────────────────

/// Returns `"L"` for level 0 (leaf) and `"C{n}"` for n ≥ 1.
pub fn level_badge(level: u8) -> String {
    if level == 0 {
        "L".to_string()
    } else {
        format!("C{level}")
    }
}

fn coordinator_level_inner(
    profile: &AgentProfile,
    profile_index: &HashMap<AgentId, AgentProfile>,
    visited: &mut HashSet<String>,
) -> u8 {
    if profile.delegates_to.is_empty() {
        return 0;
    }
    if !visited.insert(profile.id.clone()) {
        // Already on the current recursion path — cycle, treat as leaf.
        return 0;
    }
    let mut max_sub: u8 = 0;
    for entry in &profile.delegates_to {
        if let Some(child) = profile_index.get(&entry.target_agent_id) {
            let sub = coordinator_level_inner(child, profile_index, visited);
            if sub > max_sub {
                max_sub = sub;
            }
        }
        // Orphan entries contribute 0 to the level.
    }
    visited.remove(&profile.id);
    1 + max_sub
}

/// Compute the coordinator level for a profile.
/// Level 0 = leaf (no delegation); level n = deepest delegation chain of length n.
/// A visited-set guard ensures A↔B cycles terminate without panicking.
pub fn coordinator_level(
    profile: &AgentProfile,
    profile_index: &HashMap<AgentId, AgentProfile>,
) -> u8 {
    coordinator_level_inner(profile, profile_index, &mut HashSet::new())
}

/// Render the `# Delegate Targets` system-reminder block for the given profile.
/// Returns `None` when `profile.delegates_to` is empty (no header rendered).
pub fn render_delegate_targets(
    profile: &AgentProfile,
    profile_index: &HashMap<AgentId, AgentProfile>,
) -> Option<String> {
    if profile.delegates_to.is_empty() {
        return None;
    }

    let mut lines = vec![
        "<system-reminder>".to_string(),
        "# Delegate Targets".to_string(),
        "The following agents are available as delegate targets for this session. \
         Use the `Delegate` tool to hand off work to them."
            .to_string(),
        String::new(),
    ];

    for entry in &profile.delegates_to {
        match profile_index.get(&entry.target_agent_id) {
            Some(target_profile) => {
                let badge = level_badge(coordinator_level(target_profile, profile_index));
                let fork = if entry.share_context_allowed { " (fork allowed)" } else { "" };
                lines.push(format!(
                    "- **{}** ({}) — {}{}",
                    entry.name, badge, entry.purpose, fork
                ));
            }
            None => {
                lines.push(format!(
                    "- **{}** ([target profile not found; entry stale]) — {}",
                    entry.name, entry.purpose
                ));
            }
        }
    }

    lines.push("</system-reminder>".to_string());
    Some(lines.join("\n"))
}

/// Derive the agent home directory from the profile or the data dir fallback.
#[cfg(test)]
fn resolve_agent_home(agent: &AgentProfile, data_dir: &Path) -> PathBuf {
    agent
        .home_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("agent_homes").join(&agent.id))
}

/// Build the runtime-context block — used only by test helpers in this module.
#[cfg(test)]
fn build_runtime_context_block(cwd: &Path, model_id: Option<&str>) -> String {
    let cwd_str = cwd.to_string_lossy();
    let platform = std::env::consts::OS;
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();

    let mut lines = vec![
        "<run-context>".to_string(),
        format!("  <cwd>{}</cwd>", cwd_str),
        format!("  <platform>{}</platform>", platform),
        format!("  <date>{}</date>", date),
    ];
    if let Some(model) = model_id {
        lines.push(format!("  <model>{}</model>", model));
    }
    lines.push("</run-context>".to_string());
    lines.join("\n")
}

/// Load agent-home and workspace context for a run.
///
/// Shared by `compose` (native path) and `CliAgentRunner` (CLI path) so
/// both runners load context through the same code path. The caller is
/// responsible for merging plugin content into the returned
/// `AgentHomeContext` before rendering it to a string.
///
/// `agent_home` must be pre-computed by the caller (either from
/// `agent.home_dir` or the data-root fallback) so cache-keying and
/// `ensure_agent_home` scaffolding remain under the caller's control.
pub async fn load_context_blocks(
    agent: &AgentProfile,
    data_dir: &Path,
    agent_home: &Path,
    cwd: &Path,
    instruction_filenames: &[String],
) -> (AgentHomeContext, WorkspaceContext) {
    let (home_ctx, workspace_ctx) = tokio::join!(
        crate::agent_context::load_agent_home_context(
            data_dir,
            agent,
            agent_home,
            instruction_filenames,
        ),
        crate::agent_context::load_workspace_context(cwd),
    );

    (home_ctx, workspace_ctx)
}

/// Build the "Workflows in scope" block for the native API system prompt.
///
/// Returns `None` when:
/// - `binding` is `None` or `WorkflowBinding::None`
/// - the resolved workflow list is empty (unknown IDs, empty list)
///
/// The returned block references the WorkflowAction* tool family by name so
/// the agent knows to use tool calls, not XML verbs.
pub fn build_workflows_in_scope_block(
    binding: &Option<WorkflowBinding>,
    summaries: &[WorkflowSummary],
) -> Option<String> {
    let resolved: Vec<&WorkflowSummary> = match binding {
        None | Some(WorkflowBinding::None) => return None,
        Some(WorkflowBinding::All) => summaries.iter().collect(),
        Some(WorkflowBinding::List(ids)) => summaries
            .iter()
            .filter(|s| ids.contains(&s.id))
            .collect(),
    };

    if resolved.is_empty() {
        return None;
    }

    let mut lines = vec!["## Workflows in scope".to_string(), String::new()];
    for s in &resolved {
        let name = s.name.as_str();
        let desc = s.description.as_deref().unwrap_or("").trim_end_matches('.');
        if desc.is_empty() {
            lines.push(format!("- {} (\"{}\")", s.id, name));
        } else {
            lines.push(format!("- {} (\"{}\"): {}.", s.id, name, desc));
        }
    }
    lines.push(String::new());
    lines.push(
        "Use the WorkflowAction* tools (WorkflowActionCreate, WorkflowActionWriteOutput, \
         WorkflowActionCompletePhase, WorkflowActionSkipPhase, WorkflowActionStart, \
         WorkflowActionReadState) to drive these workflows. \
         A workflow task is a state machine — its phases advance only through these \
         tools. Writing a file into the task's output directory with the generic Write \
         tool does NOT register the file with the workflow and will not progress any \
         phase. To pre-fill phases from prior conversation: WorkflowActionCreate → \
         WorkflowActionWriteOutput (per output file) → WorkflowActionCompletePhase \
         (per phase, in declaration order) → WorkflowActionStart. To start a clean \
         run: WorkflowActionCreate → WorkflowActionStart."
            .to_string(),
    );

    Some(lines.join("\n"))
}


// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };
    use tempfile::TempDir;

    use super::*;

    fn blank_profile() -> AgentProfile {
        AgentProfile {
            id: "test-agent".to_string(),
            name: "Test".to_string(),
            description: "".to_string(),
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
            serialize: false,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            max_turns: None,
}
    }

    // ── 1. Empty agent — only env block ───────────────────────────────────────

    #[tokio::test]
    async fn empty_agent_yields_only_env_block() {
        let tmp_data = TempDir::new().unwrap();
        let tmp_cwd = TempDir::new().unwrap();

        let agent = blank_profile();
        let result =
            compose_with_data(&agent, tmp_data.path(), tmp_cwd.path(), None).await;

        // Should start with the run-context block
        assert!(result.starts_with("<run-context>"), "got: {}", result);
        // Should NOT contain any home or workspace sections
        assert!(!result.contains("# Agent Instructions"));
        assert!(!result.contains("# Workspace Instructions"));
        // Should NOT end with a blank line introduced by an absent profile block
        assert!(!result.ends_with("\n\n"));
    }

    // ── 2. Agent with home CLAUDE.md ──────────────────────────────────────────

    #[tokio::test]
    async fn agent_with_home_claude_md_includes_home_block() {
        let tmp_data = TempDir::new().unwrap();
        let tmp_cwd = TempDir::new().unwrap();

        // Set up agent home with a CLAUDE.md
        let agent_homes = tmp_data.path().join("agent_homes").join("test-agent");
        tokio::fs::create_dir_all(&agent_homes).await.unwrap();
        tokio::fs::write(agent_homes.join("CLAUDE.md"), "Be concise.")
            .await
            .unwrap();

        let agent = blank_profile();
        let result =
            compose_with_data(&agent, tmp_data.path(), tmp_cwd.path(), None).await;

        assert!(result.contains("# Agent Instructions"), "got: {}", result);
        assert!(result.contains("Be concise."));
        assert!(!result.contains("# Workspace Instructions"));
    }

    // ── 3. Agent with workspace CLAUDE.md ─────────────────────────────────────

    #[tokio::test]
    async fn agent_with_workspace_claude_md_includes_workspace_block() {
        let tmp_data = TempDir::new().unwrap();
        let tmp_cwd = TempDir::new().unwrap();

        tokio::fs::write(tmp_cwd.path().join("CLAUDE.md"), "No console.log.")
            .await
            .unwrap();

        let agent = blank_profile();
        let result =
            compose_with_data(&agent, tmp_data.path(), tmp_cwd.path(), None).await;

        assert!(result.contains("# Workspace Instructions"), "got: {}", result);
        assert!(result.contains("No console.log."));
        assert!(!result.contains("# Agent Instructions"));
    }

    // ── 4. Full stack ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn full_stack_includes_all_blocks_in_order() {
        let tmp_data = TempDir::new().unwrap();
        let tmp_cwd = TempDir::new().unwrap();

        let agent_homes = tmp_data.path().join("agent_homes").join("test-agent");
        tokio::fs::create_dir_all(&agent_homes).await.unwrap();
        tokio::fs::write(agent_homes.join("CLAUDE.md"), "Agent instructions.")
            .await
            .unwrap();
        tokio::fs::write(tmp_cwd.path().join("CLAUDE.md"), "Workspace instructions.")
            .await
            .unwrap();

        let mut agent = blank_profile();
        agent.system_prompt = Some("You are a helpful assistant.".to_string());

        let result =
            compose_with_data(&agent, tmp_data.path(), tmp_cwd.path(), Some("claude-opus-4-7")).await;

        // All four blocks present
        assert!(result.contains("<run-context>"));
        assert!(result.contains("<model>claude-opus-4-7</model>"));
        assert!(result.contains("# Agent Instructions"));
        assert!(result.contains("Agent instructions."));
        assert!(result.contains("# Workspace Instructions"));
        assert!(result.contains("Workspace instructions."));
        assert!(result.contains("You are a helpful assistant."));

        // Ordering: env before home before workspace before profile
        let env_pos = result.find("<run-context>").unwrap();
        let home_pos = result.find("# Agent Instructions").unwrap();
        let ws_pos = result.find("# Workspace Instructions").unwrap();
        let profile_pos = result.find("You are a helpful assistant.").unwrap();
        assert!(env_pos < home_pos);
        assert!(home_pos < ws_pos);
        assert!(ws_pos < profile_pos);
    }

    // ── 5. {{agent_context}} substitution ─────────────────────────────────────

    #[tokio::test]
    async fn agent_context_placeholder_substituted_inline_not_duplicated() {
        let tmp_data = TempDir::new().unwrap();
        let tmp_cwd = TempDir::new().unwrap();

        let agent_homes = tmp_data.path().join("agent_homes").join("test-agent");
        tokio::fs::create_dir_all(&agent_homes).await.unwrap();
        tokio::fs::write(agent_homes.join("CLAUDE.md"), "Home rule content.")
            .await
            .unwrap();

        let mut agent = blank_profile();
        agent.system_prompt =
            Some("Preamble.\n\n{{agent_context}}\n\nPostamble.".to_string());

        let result =
            compose_with_data(&agent, tmp_data.path(), tmp_cwd.path(), None).await;

        // Home content appears exactly once (inlined in profile block)
        let count = result.matches("Home rule content.").count();
        assert_eq!(count, 1, "home content should appear exactly once; got:\n{}", result);

        // Placeholder was replaced — not present verbatim
        assert!(!result.contains("{{agent_context}}"));

        // Preamble and postamble still present
        assert!(result.contains("Preamble."));
        assert!(result.contains("Postamble."));
    }

    // ── 6. Model id absent ────────────────────────────────────────────────────

    #[tokio::test]
    async fn model_id_absent_omits_model_element() {
        let tmp_data = TempDir::new().unwrap();
        let tmp_cwd = TempDir::new().unwrap();

        let agent = blank_profile();
        let result =
            compose_with_data(&agent, tmp_data.path(), tmp_cwd.path(), None).await;

        assert!(!result.contains("<model>"), "got: {}", result);
        assert!(result.contains("<run-context>"));
    }

    // ── 7. Model id present ───────────────────────────────────────────────────

    #[tokio::test]
    async fn model_id_present_included_in_env_block() {
        let tmp_data = TempDir::new().unwrap();
        let tmp_cwd = TempDir::new().unwrap();

        let agent = blank_profile();
        let result =
            compose_with_data(&agent, tmp_data.path(), tmp_cwd.path(), Some("claude-sonnet-4-6")).await;

        assert!(result.contains("<model>claude-sonnet-4-6</model>"), "got: {}", result);
    }

    /// Testable variant of `compose` that accepts an explicit `data_dir` so
    /// tests do not depend on the process environment.
    async fn compose_with_data(
        agent: &AgentProfile,
        data_dir: &Path,
        cwd: &Path,
        model_id: Option<&str>,
    ) -> String {
        let instruction_filenames = vec![DEFAULT_INSTRUCTION_FILENAME.to_string()];
        let agent_home = resolve_agent_home(agent, data_dir);
        let env_block = build_runtime_context_block(cwd, model_id);

        let (home_ctx, workspace_ctx) =
            load_context_blocks(agent, data_dir, &agent_home, cwd, &instruction_filenames).await;
        let home_block = home_ctx.to_prompt_sections();
        let workspace_block = workspace_ctx.to_prompt_sections();

        let (profile_block, home_inlined) = match agent.system_prompt.as_deref() {
            Some(sp) if sp.contains("{{agent_context}}") => {
                let substituted =
                    sp.replace("{{agent_context}}", home_block.as_deref().unwrap_or(""));
                (Some(substituted), true)
            }
            Some(sp) if !sp.is_empty() => (Some(sp.to_string()), false),
            _ => (None, false),
        };

        let mut blocks: Vec<String> = vec![env_block];
        if !home_inlined {
            if let Some(hb) = home_block {
                blocks.push(hb);
            }
        }
        if let Some(wb) = workspace_block {
            blocks.push(wb);
        }
        if let Some(pb) = profile_block {
            blocks.push(pb);
        }
        // Delegate block: blank profile has delegates_to empty → None → skipped.
        if let Some(dt) = render_delegate_targets(agent, &HashMap::new()) {
            blocks.push(dt);
        }
        blocks.join("\n\n")
    }

    fn make_summary(id: &str, name: &str, desc: Option<&str>) -> WorkflowSummary {
        WorkflowSummary {
            id: id.to_string(),
            name: name.to_string(),
            version: None,
            description: desc.map(|s| s.to_string()),
            phase_count: 2,
            source: ao_protocol::workflow::WorkflowSource::User,
            updated_on: None,
            last_run: None,
        }
    }

    // ── 8. Workflows block: binding present → block appended ──────────────────

    #[test]
    fn workflows_block_binding_present_returns_block() {
        let summaries = vec![
            make_summary("wf-alpha", "Alpha Workflow", Some("Does alpha things")),
            make_summary("wf-beta", "Beta Workflow", None),
        ];
        let binding = Some(WorkflowBinding::All);
        let block = build_workflows_in_scope_block(&binding, &summaries).unwrap();
        assert!(block.contains("## Workflows in scope"), "got: {}", block);
        assert!(block.contains("wf-alpha"), "got: {}", block);
        assert!(block.contains("\"Alpha Workflow\""), "got: {}", block);
        assert!(block.contains("Does alpha things"), "got: {}", block);
        assert!(block.contains("wf-beta"), "got: {}", block);
        assert!(block.contains("WorkflowAction*"), "got: {}", block);
        assert!(block.contains("WorkflowActionCreate"), "got: {}", block);
    }

    // ── 9. Workflows block: binding None → no block ────────────────────────────

    #[test]
    fn workflows_block_binding_none_returns_none() {
        let summaries = vec![make_summary("wf-alpha", "Alpha", Some("desc"))];
        assert!(build_workflows_in_scope_block(&None, &summaries).is_none());
        assert!(
            build_workflows_in_scope_block(&Some(WorkflowBinding::None), &summaries).is_none()
        );
    }

    // ── 10. Workflows block: List with no matching IDs → no block ─────────────

    #[test]
    fn workflows_block_list_no_match_returns_none() {
        let summaries = vec![make_summary("wf-alpha", "Alpha", Some("desc"))];
        let binding = Some(WorkflowBinding::List(vec!["wf-unknown".to_string()]));
        assert!(build_workflows_in_scope_block(&binding, &summaries).is_none());
    }

    // ── 11. Workflows block: List with matching IDs → only listed workflows ───

    #[test]
    fn workflows_block_list_filters_to_bound_ids() {
        let summaries = vec![
            make_summary("wf-alpha", "Alpha", Some("alpha desc")),
            make_summary("wf-beta", "Beta", Some("beta desc")),
            make_summary("wf-gamma", "Gamma", Some("gamma desc")),
        ];
        let binding = Some(WorkflowBinding::List(vec![
            "wf-alpha".to_string(),
            "wf-gamma".to_string(),
        ]));
        let block = build_workflows_in_scope_block(&binding, &summaries).unwrap();
        assert!(block.contains("wf-alpha"), "got: {}", block);
        assert!(block.contains("wf-gamma"), "got: {}", block);
        assert!(!block.contains("wf-beta"), "wf-beta should be absent; got: {}", block);
    }

    // ─── Delegate Targets block tests ─────────────────────────────────────────

    use ao_protocol::agent::DelegateTarget;

    fn make_delegate_target(id: &str, name: &str, purpose: &str, fork: bool) -> DelegateTarget {
        DelegateTarget {
            target_agent_id: id.to_string(),
            name: name.to_string(),
            purpose: purpose.to_string(),
            share_context_allowed: fork,
        }
    }

    // ── 12. Empty delegates_to → None ─────────────────────────────────────────

    #[test]
    fn empty_delegates_to_returns_none() {
        let profile = blank_profile();
        let index: HashMap<String, AgentProfile> = HashMap::new();
        assert!(render_delegate_targets(&profile, &index).is_none());
    }

    // ── 13. Single leaf target → "L" badge ────────────────────────────────────

    #[test]
    fn single_leaf_target_badge_is_l() {
        let mut leaf = blank_profile();
        leaf.id = "leaf-agent".to_string();
        leaf.name = "Leaf Agent".to_string();
        // leaf has no delegates_to → level 0

        let mut host = blank_profile();
        host.delegates_to = vec![make_delegate_target("leaf-agent", "Leaf Agent", "Leaf work", false)];

        let index = HashMap::from([("leaf-agent".to_string(), leaf)]);
        let block = render_delegate_targets(&host, &index).unwrap();
        assert!(block.contains("# Delegate Targets"), "got: {}", block);
        assert!(block.contains("(L)"), "expected L badge; got: {}", block);
        assert!(!block.contains("(fork allowed)"), "got: {}", block);
    }

    // ── 14. Target that itself delegates → "C1" badge ─────────────────────────

    #[test]
    fn target_that_delegates_badge_is_c1() {
        let mut leaf = blank_profile();
        leaf.id = "leaf-agent".to_string();
        // leaf has no delegates_to

        let mut middle = blank_profile();
        middle.id = "middle-agent".to_string();
        middle.name = "Middle".to_string();
        middle.delegates_to = vec![make_delegate_target("leaf-agent", "Leaf", "leaf", false)];
        // middle delegates to leaf → level 1 → C1

        let mut host = blank_profile();
        host.delegates_to = vec![make_delegate_target("middle-agent", "Middle Agent", "Middle work", false)];

        let index = HashMap::from([
            ("leaf-agent".to_string(), leaf),
            ("middle-agent".to_string(), middle),
        ]);
        let block = render_delegate_targets(&host, &index).unwrap();
        assert!(block.contains("(C1)"), "expected C1 badge; got: {}", block);
    }

    // ── 15. Cycle A↔B → renders, level bounded ────────────────────────────────

    #[test]
    fn cycle_a_b_renders_level_bounded() {
        let mut profile_a = blank_profile();
        profile_a.id = "agent-a".to_string();
        profile_a.name = "Agent A".to_string();
        profile_a.delegates_to = vec![make_delegate_target("agent-b", "B", "B work", false)];

        let mut profile_b = blank_profile();
        profile_b.id = "agent-b".to_string();
        profile_b.name = "Agent B".to_string();
        profile_b.delegates_to = vec![make_delegate_target("agent-a", "A", "A work", false)];

        let index = HashMap::from([
            ("agent-a".to_string(), profile_a.clone()),
            ("agent-b".to_string(), profile_b),
        ]);

        let block = render_delegate_targets(&profile_a, &index);
        assert!(block.is_some(), "should render even with cycle");
        let block = block.unwrap();
        assert!(block.contains("# Delegate Targets"), "got: {}", block);
        // Level must be some finite badge (L, C1, C2, …) — just not an infinite loop.
        assert!(block.contains("(L)") || block.contains("(C"), "expected a badge; got: {}", block);
    }

    // ── 16. Orphan entry → stale message ──────────────────────────────────────

    #[test]
    fn orphan_target_shows_stale_message() {
        let mut host = blank_profile();
        host.delegates_to = vec![make_delegate_target("nonexistent", "Missing Agent", "Does stuff", false)];

        let index: HashMap<String, AgentProfile> = HashMap::new();
        let block = render_delegate_targets(&host, &index).unwrap();
        assert!(
            block.contains("[target profile not found; entry stale]"),
            "got: {}", block
        );
        assert!(block.contains("Missing Agent"), "got: {}", block);
    }

    // ── 17. share_context_allowed: true → " (fork allowed)" suffix ───────────

    #[test]
    fn share_context_allowed_shows_fork_suffix() {
        let mut leaf = blank_profile();
        leaf.id = "leaf-agent".to_string();

        let mut host = blank_profile();
        host.delegates_to = vec![make_delegate_target("leaf-agent", "Leaf Agent", "Does work", true)];

        let index = HashMap::from([("leaf-agent".to_string(), leaf)]);
        let block = render_delegate_targets(&host, &index).unwrap();
        assert!(block.contains("(fork allowed)"), "got: {}", block);
    }
}
