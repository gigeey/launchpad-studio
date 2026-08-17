/// Load context from an agent's home directory (skills, rules, instructions).
///
/// Skills are loaded from the global user/plugin pool via SkillRegistry.
/// Rules and instructions contribute full file contents so the snapshot
/// template can inline them directly into the system prompt.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ao_engine_tools_core::skill_registry::{SkillEntry, SkillRegistry, SkillSource};
use ao_engine_tools_runner::mcp::McpManager;
use ao_protocol::agent::AgentProfile;
use ao_protocol::agent_home;
use ao_protocol::instruction_file::InstructionFilePattern;

use crate::instructions::scan_instructions;
use crate::rules::scan_rules_dir;

/// Builds the runtime skill registry shared into a [`RunnerContext`].
///
/// Loads the agent's user-pool and enabled-plugin skills from
/// `<data_dir>/skills/` and `<data_dir>/plugins/<p>/skills/`, plus a
/// build-time-embedded built-in pool that every agent gets regardless of its
/// allowlist, then overlays any MCP-server prompt-sourced skills. On a name
/// collision the earlier pool wins in this order: user, plugin, built-in —
/// MCP prompt skills only fill gaps left by all three. Passing `None` for
/// `mcp_manager` skips the overlay (callers without an MCP manager wired in).
///
/// Every path that constructs a `RunnerContext` for a run must build the
/// registry through this function. `RunSkill` and `SkillRegister` resolve
/// against `ctx.skill_registry`; if a context-builder forgets to populate it,
/// the registry defaults to empty and *every* skill resolves as "not found" —
/// including ones the system prompt advertises as enabled. Centralizing the
/// load here makes that omission impossible to reintroduce silently.
pub fn build_skill_registry(
    data_dir: &Path,
    profile: &AgentProfile,
    mcp_manager: Option<&McpManager>,
) -> Arc<SkillRegistry> {
    let mut registry = SkillRegistry::load(data_dir, profile);
    if let Some(manager) = mcp_manager {
        manager.extend_skill_registry(&mut registry);
    }
    Arc::new(registry)
}

/// Metadata extracted from a skill file's YAML frontmatter.
/// Retained for backward compatibility with context_cache and agent_runner.
#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub path: String,
    pub id: String,
    pub title: String,
    pub description: Option<String>,
}

/// A single rule file's relative path and full markdown body.
#[derive(Debug, Clone)]
pub struct RuleMeta {
    /// Path relative to the rules root, e.g. `"bundle/inner/strict.md"`.
    pub path: String,
    /// Full markdown body.
    pub content: String,
}

/// A single instruction file's filename and full markdown body.
#[derive(Debug, Clone)]
pub struct InstructionMeta {
    /// On-disk filename (case preserved).
    pub name: String,
    /// Full markdown body.
    pub content: String,
}

/// A skill sourced from a globally-installed plugin, with its prefix-namespaced
/// id and absolute on-disk path. Retained for backward compatibility with
/// agent_runner plugin injection; not rendered by `to_prompt_sections` (plugin
/// skills are now surfaced via the unified `SkillRegistry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSkillMeta {
    pub id: String,
    pub plugin_name: String,
    pub skill_name: String,
    pub title: String,
    pub description: Option<String>,
    pub skill_md_path: std::path::PathBuf,
}

/// A rule sourced from a globally-installed plugin, with its prefix-namespaced
/// id and full markdown body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRuleMeta {
    pub id: String,
    pub plugin_name: String,
    pub rule_name: String,
    pub content: String,
}

/// Loaded context from an agent's home directory.
#[derive(Debug, Default, Clone)]
pub struct AgentHomeContext {
    /// Skills loaded from the global user/plugin pool via SkillRegistry.
    pub skill_registry: SkillRegistry,
    /// Full content of every enabled rule discovered recursively under
    /// `{agent_home}/rules/`.
    pub rules: Vec<RuleMeta>,
    /// Full content of every enabled instruction file at the root of
    /// `{agent_home}/` matching the user-configured filename patterns.
    pub instructions: Vec<InstructionMeta>,
    /// Legacy field: per-agent flat skill metadata (no longer populated).
    /// Kept for compatibility with context_cache and any agent_runner code
    /// that may read it; always empty now.
    pub skills: Vec<SkillMeta>,
    /// Plugin skills injected by agent_runner from the plugin cache. Not
    /// rendered in `to_prompt_sections` — plugin skills surface via
    /// `skill_registry` instead.
    pub plugin_skills: Vec<PluginSkillMeta>,
    /// Rules contributed by globally-installed plugins.
    pub plugin_rules: Vec<PluginRuleMeta>,
}

/// Loaded context from a workspace directory (effective_cwd).
#[derive(Debug, Default, Clone)]
pub struct WorkspaceContext {
    /// Content loaded from {effective_cwd}/CLAUDE.md (or configured instruction file)
    pub instruction: Option<String>,
    /// Content loaded from {effective_cwd}/.claude/rules/*.md
    pub rules: Vec<(String, String)>,
}

/// Render the unified "# Studio Skills" listing block from a skill registry.
///
/// Produces the `<system-reminder># Studio Skills …</system-reminder>` block that
/// advertises every visible skill (user pool + enabled plugins + any MCP overlay)
/// to the model. Returns `None` when the registry is empty so callers can omit
/// the section entirely.
///
/// This is the single source of truth for the skill listing, shared by both
/// runners. The listing it produces must reflect exactly the pools that
/// `RunSkill` dispatches against (see `build_skill_registry`); building both
/// from the same registry is what keeps "what the model is told exists" in lock
/// step with "what it can actually invoke".
///
/// The header also instructs the model to treat a `/<slug>` prefix in the
/// user's message as an explicit RunSkill request. This is what makes the
/// frontend's slash-command popover work for Studio skills: selecting a skill
/// there inserts literal `/<slug> ` text into the compose box (the same
/// mechanism already used for CLI-native commands), and this instruction is
/// what teaches the model to act on it. No message content is rewritten or
/// hidden — the visible sent text and the model's interpretation stay in sync.
///
/// When `cli_precedence` is true an extra directive is appended instructing the
/// model to prefer Studio skills over the host CLI binary's own skill ecosystem
/// on a name collision. Native runs pass `false`: there is no competing external
/// binary, so the directive would be noise.
pub fn render_studio_skills_block(
    registry: &SkillRegistry,
    cli_precedence: bool,
) -> Option<String> {
    if registry.is_empty() {
        return None;
    }

    // Maximum total characters (header + entries + footer) before we fall back
    // to a compact names-only listing.
    const SKILL_LISTING_CHAR_BUDGET: usize = 8_000;
    // Per-entry description character cap; long descriptions are truncated.
    const SKILL_LISTING_ENTRY_DESC_CAP: usize = 250;

    // Distinctive wording — the surrounding CLI binary (e.g. claude) may inject
    // its own system-reminder advertising native skills under the heading "The
    // following skills are available for use with the Skill tool:". The phrasing
    // below is intentionally different so the model can tell the two ecosystems
    // apart: Studio skills live here and are invoked via `RunSkill`; the binary's
    // native skills are invoked via the binary's own tool.
    let mut header = String::from(
        "<system-reminder>\n# Studio Skills\n\
         The following Studio skills are available. Invoke them via the `RunSkill` tool. \
         (Your CLI binary may advertise a separate native skill ecosystem under its own \
         `Skill` tool — those are NOT invoked via `RunSkill`; the two ecosystems coexist.)\n\
         If the user's message starts with `/<slug>` matching one of the skill names below, \
         treat that as an explicit request to run it: call `RunSkill` with that skill as your \
         first action, passing any text after the slug as `args`.\n",
    );
    if cli_precedence {
        // CLI mode only: the host binary ships its own skills and nothing else
        // tells the model which ecosystem to favour. Make Studio authoritative
        // so an enabled Studio skill is never shadowed by a same-named native one.
        header.push_str(
            "When a capability is offered by BOTH a Studio skill (via `RunSkill`) and your CLI \
             binary's native `Skill` tool, prefer the Studio skill; on a name collision the Studio \
             skill takes precedence.\n",
        );
    }
    header.push('\n');
    let footer = "</system-reminder>";

    // Build the descriptive listing with per-entry description cap applied.
    let mut full_entries = String::new();
    for (name, entry) in registry.all_visible() {
        let line = match entry {
            SkillEntry::Ok(record) => {
                let suffix = match &record.source {
                    SkillSource::User => String::new(),
                    SkillSource::Plugin { plugin_name } => {
                        format!(" [plugin: {}]", plugin_name)
                    }
                    SkillSource::Mcp { server_name } => {
                        format!(" [mcp: {}]", server_name)
                    }
                    SkillSource::BuiltIn => " [built-in]".to_string(),
                };
                // Truncate at char boundary to stay within per-entry cap.
                let desc: String =
                    if record.description.chars().count() > SKILL_LISTING_ENTRY_DESC_CAP {
                        format!(
                            "{}…",
                            record
                                .description
                                .chars()
                                .take(SKILL_LISTING_ENTRY_DESC_CAP)
                                .collect::<String>()
                        )
                    } else {
                        record.description.clone()
                    };
                let when_hint = record
                    .when_to_use
                    .as_deref()
                    .map(|w| format!(" — {}", w))
                    .unwrap_or_default();
                format!(
                    "- **{}** (`{}`) — {}{}{}\n",
                    record.name, record.name, desc, when_hint, suffix
                )
            }
            SkillEntry::Err(reason) => {
                format!("- **{}** (`{}`) — [load error: {}]\n", name, name, reason)
            }
        };
        full_entries.push_str(&line);
    }

    let block = if header.len() + full_entries.len() + footer.len() <= SKILL_LISTING_CHAR_BUDGET {
        format!("{}{}{}", header, full_entries, footer)
    } else {
        // Budget exceeded: fall back to a compact names-only listing so the
        // block stays within the context budget.
        let mut names_entries = String::new();
        for (name, _) in registry.all_visible() {
            names_entries.push_str(&format!("- {}\n", name));
        }
        format!("{}{}{}", header, names_entries, footer)
    };

    Some(block)
}

impl AgentHomeContext {
    /// Returns true if no context was loaded.
    pub fn is_empty(&self) -> bool {
        self.skill_registry.is_empty()
            && self.skills.is_empty()
            && self.rules.is_empty()
            && self.instructions.is_empty()
            && self.plugin_skills.is_empty()
            && self.plugin_rules.is_empty()
    }

    /// Format the loaded context as labeled sections for the system prompt.
    /// Ordering: Skills → Rules → Plugin Rules → Instructions.
    pub fn to_prompt_sections(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }

        let mut sections = Vec::new();

        // Unified # Studio Skills block: user pool + plugin pool from SkillRegistry.
        // No CLI-precedence directive here — `to_prompt_sections` is the legacy
        // in-process renderer; runner-mode-specific wording is applied by the
        // runner via `render_studio_skills_block` directly.
        if let Some(block) = render_studio_skills_block(&self.skill_registry, false) {
            sections.push(block);
        }

        if !self.rules.is_empty() {
            let mut block = String::from("# Agent Rules\n");
            for rule in &self.rules {
                block.push_str(&format!(
                    "\n## Rule: {}\n\n{}\n",
                    rule.path,
                    rule.content.trim()
                ));
            }
            sections.push(block);
        }

        if !self.plugin_rules.is_empty() {
            let mut block = String::from("# Plugin Rules\n");
            for rule in &self.plugin_rules {
                block.push_str(&format!(
                    "\n## Rule: {}\n\n{}\n",
                    rule.id,
                    rule.content.trim()
                ));
            }
            sections.push(block);
        }

        if !self.instructions.is_empty() {
            let mut block = String::from("# Agent Instructions\n");
            for instruction in &self.instructions {
                block.push_str(&format!(
                    "\n## Instruction: {}\n\n{}\n",
                    instruction.name,
                    instruction.content.trim()
                ));
            }
            sections.push(block);
        }

        if sections.is_empty() {
            None
        } else {
            Some(sections.join("\n\n"))
        }
    }
}

impl WorkspaceContext {
    /// Returns true if no workspace context was loaded.
    pub fn is_empty(&self) -> bool {
        self.instruction.is_none() && self.rules.is_empty()
    }

    /// Format the loaded workspace context as labeled sections for the system prompt.
    pub fn to_prompt_sections(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }

        let mut sections = Vec::new();

        if let Some(ref instruction) = self.instruction {
            if !instruction.trim().is_empty() {
                sections.push(format!(
                    "# Workspace Instructions\n\n{}",
                    instruction.trim()
                ));
            }
        }

        if !self.rules.is_empty() {
            let mut block = String::from("# Workspace Rules\n");
            for (name, content) in &self.rules {
                block.push_str(&format!("\n## {}\n\n{}\n", name, content.trim()));
            }
            sections.push(block);
        }

        if sections.is_empty() {
            None
        } else {
            Some(sections.join("\n\n"))
        }
    }
}

/// Read all `.md` files from a directory, returning (filename_stem, content) pairs.
///
/// Returns an empty vec if the directory doesn't exist or is empty.
async fn read_md_files(dir: &Path) -> Vec<(String, String)> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut results = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                if !content.trim().is_empty() {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    results.push((name, content));
                }
            }
        }
    }

    // Sort by name for deterministic ordering
    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

/// Discovers every enabled rule recursively under `rules_dir`. Wraps the
/// synchronous `rules::scan_rules_dir` in `spawn_blocking` so the tokio
/// executor is not blocked on large rule trees.
async fn load_enabled_rules(rules_dir: PathBuf) -> Vec<RuleMeta> {
    tokio::task::spawn_blocking(move || {
        scan_rules_dir(&rules_dir)
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| RuleMeta {
                path: r.id,
                content: r.content,
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Discovers every enabled instruction file at the root of `agent_home`
/// matching one of `patterns`. Returns an empty vec on I/O errors (already
/// logged inside `scan_instructions`).
async fn load_enabled_instructions(agent_home: PathBuf, patterns: Vec<String>) -> Vec<InstructionMeta> {
    tokio::task::spawn_blocking(move || {
        scan_instructions(&agent_home, &patterns)
            .unwrap_or_default()
            .into_iter()
            .filter(|i| i.enabled)
            .map(|i| InstructionMeta {
                name: i.name,
                content: i.content,
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Load all context from an agent's home directory.
///
/// Skills are loaded from the global user/plugin pool via SkillRegistry
/// (reads `<data_dir>/skills/` and `<data_dir>/plugins/<p>/skills/`).
/// Rules and instructions still come from the agent's home directory.
///
/// Missing directories or files are handled gracefully — no errors if nothing exists.
pub async fn load_agent_home_context(
    data_dir: &Path,
    profile: &AgentProfile,
    agent_home: &Path,
    instruction_filenames: &[String],
) -> AgentHomeContext {
    let rules_dir = agent_home::rules_dir(agent_home);

    let skill_registry = SkillRegistry::load(data_dir, profile);

    let (rules, instructions) = tokio::join!(
        load_enabled_rules(rules_dir),
        load_enabled_instructions(agent_home.to_path_buf(), instruction_filenames.to_vec()),
    );

    tracing::info!(
        data_dir = %data_dir.display(),
        agent_home = %agent_home.display(),
        skill_count = skill_registry.len(),
        rule_count = rules.len(),
        instruction_count = instructions.len(),
        "Loaded agent home context"
    );

    AgentHomeContext {
        skill_registry,
        rules,
        instructions,
        skills: Vec::new(),
        plugin_skills: Vec::new(),
        plugin_rules: Vec::new(),
    }
}

/// Load workspace context from the effective_cwd directory.
///
/// Reads the instruction file from the workspace root and rules from
/// {effective_cwd}/.claude/rules/. Missing files or directories are
/// handled gracefully — no errors if nothing exists.
pub async fn load_workspace_context(effective_cwd: &Path) -> WorkspaceContext {
    let instruction_paths = InstructionFilePattern::default().resolve_all(effective_cwd);
    let rules_dir = effective_cwd.join(".claude").join("rules");

    let (instruction, rules) =
        tokio::join!(read_first_existing(&instruction_paths), read_md_files(&rules_dir),);

    let instruction = instruction.filter(|s| !s.trim().is_empty());

    WorkspaceContext { instruction, rules }
}

/// Returns the contents of the first readable file among `paths`, or None if
/// none exist / are readable. Used to pick an instruction file when multiple
/// candidate filenames are configured.
async fn read_first_existing(paths: &[PathBuf]) -> Option<String> {
    for path in paths {
        if let Ok(contents) = tokio::fs::read_to_string(path).await {
            return Some(contents);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };
    use ao_engine_tools_core::skill_registry::SkillEntry;
    use tempfile::TempDir;

    use super::*;

    fn default_patterns() -> Vec<String> {
        vec!["CLAUDE.md".to_string()]
    }

    fn minimal_profile() -> AgentProfile {
        AgentProfile {
            id: "test-agent".to_string(),
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

    // ─── build_skill_registry tests ──────────────────────────────────────────

    /// The shared helper must populate the registry from the profile's enabled
    /// user-pool skills. An empty result here is the exact failure mode the
    /// helper exists to prevent: when a RunnerContext-builder skips the load,
    /// `ctx.skill_registry` defaults to empty and every skill — including ones
    /// the system prompt advertises — resolves as "not found".
    #[test]
    fn build_skill_registry_loads_profile_enabled_skills() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join("skills").join("ping");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: ping\ndescription: Pong\n---\nbody",
        )
        .unwrap();

        let mut profile = minimal_profile();
        profile.skills = vec!["ping".to_string()];

        // No MCP manager wired — the user-pool load alone must populate the
        // registry, mirroring the native runner when no MCP servers exist.
        let registry = build_skill_registry(tmp.path(), &profile, None);

        assert!(
            matches!(registry.get("ping"), Some(SkillEntry::Ok(_))),
            "expected 'ping' to resolve from the user pool, got {:?}",
            registry.get("ping"),
        );
    }

    /// A profile enabling no user/plugin skills yields a registry containing
    /// only the always-on built-in pool (`create-workflow`) — not a panic,
    /// error, or genuinely-empty registry. This distinguishes "loaded,
    /// nothing user/plugin-enabled" from the bug's "never loaded" so the
    /// positive test above can't pass trivially, while accounting for the
    /// built-in pool's unconditional presence (see `SkillRegistry::load`).
    #[test]
    fn build_skill_registry_empty_profile_yields_builtin_only_registry() {
        let tmp = TempDir::new().unwrap();
        let registry = build_skill_registry(tmp.path(), &minimal_profile(), None);
        assert_eq!(registry.len(), 1);
        assert!(matches!(registry.get("create-workflow"), Some(SkillEntry::Ok(_))));
    }

    // ─── load_agent_home_context tests ───────────────────────────────────────

    #[tokio::test]
    async fn test_load_empty_agent_home() {
        let tmp = TempDir::new().unwrap();
        let agent_home = tmp.path().join("agent-1");
        ao_protocol::agent_home::ensure_agent_home(&agent_home)
            .await
            .unwrap();

        let ctx = load_agent_home_context(tmp.path(), &minimal_profile(), &agent_home, &default_patterns()).await;
        // Nothing was written under the agent home or data dir, so every
        // agent-home-specific field stays empty — but the always-on
        // built-in pool (see `SkillRegistry::load`) still contributes the
        // `create-workflow` skill, so the context as a whole is no longer
        // literally empty and a prompt section is rendered for it.
        assert!(ctx.rules.is_empty());
        assert!(ctx.instructions.is_empty());
        assert!(ctx.skills.is_empty());
        assert!(ctx.plugin_skills.is_empty());
        assert!(ctx.plugin_rules.is_empty());
        assert_eq!(ctx.skill_registry.len(), 1);
        assert!(!ctx.is_empty());
        assert!(ctx.to_prompt_sections().is_some());
    }

    #[tokio::test]
    async fn test_load_nonexistent_agent_home() {
        let tmp = TempDir::new().unwrap();
        let agent_home = tmp.path().join("does-not-exist");

        let ctx = load_agent_home_context(tmp.path(), &minimal_profile(), &agent_home, &default_patterns()).await;
        // Same reasoning as `test_load_empty_agent_home`: a nonexistent
        // agent home yields no rules/instructions, but the built-in pool
        // still populates skill_registry unconditionally.
        assert!(ctx.rules.is_empty());
        assert!(ctx.instructions.is_empty());
        assert_eq!(ctx.skill_registry.len(), 1);
        assert!(!ctx.is_empty());
    }

    #[tokio::test]
    async fn test_registry_skills_render_in_unified_block() {
        let tmp = TempDir::new().unwrap();
        let agent_home = tmp.path().join("agent-1");
        ao_protocol::agent_home::ensure_agent_home(&agent_home).await.unwrap();

        // Write a skill to the user pool
        let skill_dir = tmp.path().join("skills").join("my-skill");
        tokio::fs::create_dir_all(&skill_dir).await.unwrap();
        tokio::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Does cool things\n---\nbody",
        )
        .await
        .unwrap();

        let mut profile = minimal_profile();
        profile.skills = vec!["my-skill".to_string()];

        let ctx = load_agent_home_context(tmp.path(), &profile, &agent_home, &default_patterns()).await;
        assert!(!ctx.is_empty());

        let sections = ctx.to_prompt_sections().unwrap();
        // Heading was renamed from "# Agent Skills" to "# Studio Skills" so
        // the model can distinguish Studio's RunSkill ecosystem from the CLI
        // binary's native `Skill` tool, which injects its own "The following
        // skills are available for use with the Skill tool" reminder block.
        assert!(sections.contains("# Studio Skills"));
        assert!(sections.contains("**my-skill**"));
        assert!(sections.contains("`my-skill`"));
        assert!(sections.contains("Does cool things"));
        assert!(!sections.contains("# Plugin Skills"));
        assert!(sections.contains("<system-reminder>"));
        assert!(sections.contains("</system-reminder>"));
    }

    #[tokio::test]
    async fn test_registry_error_entry_renders_load_error() {
        let tmp = TempDir::new().unwrap();
        let agent_home = tmp.path().join("agent-1");
        ao_protocol::agent_home::ensure_agent_home(&agent_home).await.unwrap();

        // Write a broken SKILL.md (no name/description)
        let skill_dir = tmp.path().join("skills").join("broken");
        tokio::fs::create_dir_all(&skill_dir).await.unwrap();
        tokio::fs::write(skill_dir.join("SKILL.md"), "---\nno_name: true\n---\nbody").await.unwrap();

        let mut profile = minimal_profile();
        profile.skills = vec!["broken".to_string()];

        let ctx = load_agent_home_context(tmp.path(), &profile, &agent_home, &default_patterns()).await;
        let sections = ctx.to_prompt_sections().unwrap();
        assert!(sections.contains("# Studio Skills"));
        assert!(sections.contains("[load error:"));
        assert!(sections.contains("`broken`"));
    }

    #[tokio::test]
    async fn test_no_user_skills_still_renders_builtin_skills_block() {
        let tmp = TempDir::new().unwrap();
        let agent_home = tmp.path().join("agent-1");
        ao_protocol::agent_home::ensure_agent_home(&agent_home).await.unwrap();

        // Add a rule but no user-pool skills.
        tokio::fs::write(agent_home.join("rules/style.md"), "Use 4 spaces.").await.unwrap();

        let ctx = load_agent_home_context(tmp.path(), &minimal_profile(), &agent_home, &default_patterns()).await;
        let sections = ctx.to_prompt_sections().unwrap();
        // No user-pool skill was configured, but the always-on built-in
        // pool (see `SkillRegistry::load`) still contributes
        // `create-workflow`, so the Studio Skills block renders regardless.
        assert!(sections.contains("# Studio Skills"));
        assert!(sections.contains("create-workflow"));
        assert!(sections.contains("# Agent Rules"));
    }

    // ─── Rules and instructions tests (functionality unchanged) ──────────────

    #[tokio::test]
    async fn test_load_instruction_file() {
        let tmp = TempDir::new().unwrap();
        let agent_home = tmp.path().join("agent-1");
        ao_protocol::agent_home::ensure_agent_home(&agent_home).await.unwrap();

        tokio::fs::write(
            agent_home.join("CLAUDE.md"),
            "You are a helpful coding assistant.",
        )
        .await
        .unwrap();

        let ctx = load_agent_home_context(tmp.path(), &minimal_profile(), &agent_home, &default_patterns()).await;
        assert_eq!(ctx.instructions.len(), 1);
        assert_eq!(ctx.instructions[0].name, "CLAUDE.md");
        assert!(ctx.instructions[0].content.contains("helpful coding assistant"));
    }

    #[tokio::test]
    async fn test_load_multiple_instructions_sorted() {
        let tmp = TempDir::new().unwrap();
        let agent_home = tmp.path().join("agent-1");
        ao_protocol::agent_home::ensure_agent_home(&agent_home).await.unwrap();

        tokio::fs::write(agent_home.join("CLAUDE.md"), "Claude content.").await.unwrap();
        tokio::fs::write(agent_home.join("Cursor.md"), "Cursor content.").await.unwrap();

        let patterns = vec!["CLAUDE.md".to_string(), "Cursor.md".to_string()];
        let ctx = load_agent_home_context(tmp.path(), &minimal_profile(), &agent_home, &patterns).await;
        assert_eq!(ctx.instructions.len(), 2);
        assert_eq!(ctx.instructions[0].name, "CLAUDE.md");
        assert_eq!(ctx.instructions[1].name, "Cursor.md");

        let sections = ctx.to_prompt_sections().unwrap();
        assert!(sections.contains("## Instruction: CLAUDE.md"));
        assert!(sections.contains("## Instruction: Cursor.md"));
    }

    #[tokio::test]
    async fn test_disabled_instruction_omitted() {
        let tmp = TempDir::new().unwrap();
        let agent_home = tmp.path().join("agent-1");
        ao_protocol::agent_home::ensure_agent_home(&agent_home).await.unwrap();

        tokio::fs::write(agent_home.join("CLAUDE.md"), "Claude content.").await.unwrap();
        // Disable CLAUDE.md via sidecar manifest.
        let manifest_dir = agent_home.join(".instructions");
        tokio::fs::create_dir_all(&manifest_dir).await.unwrap();
        tokio::fs::write(
            manifest_dir.join("CLAUDE.md.manifest.json"),
            r#"{"enabled":false}"#,
        )
        .await
        .unwrap();

        let ctx = load_agent_home_context(tmp.path(), &minimal_profile(), &agent_home, &default_patterns()).await;
        assert!(ctx.instructions.is_empty());
    }

    #[tokio::test]
    async fn test_load_nested_rules_bundle_includes_every_enabled_file() {
        let tmp = TempDir::new().unwrap();
        let agent_home = tmp.path().join("agent-1");
        ao_protocol::agent_home::ensure_agent_home(&agent_home).await.unwrap();

        let bundle_dir = agent_home.join("rules").join("bundle");
        let inner_dir = bundle_dir.join("inner");
        tokio::fs::create_dir_all(&inner_dir).await.unwrap();

        tokio::fs::write(bundle_dir.join("root.md"), "Root rule content.").await.unwrap();
        tokio::fs::write(inner_dir.join("strict.md"), "Strict rule content.").await.unwrap();

        let ctx = load_agent_home_context(tmp.path(), &minimal_profile(), &agent_home, &default_patterns()).await;
        assert_eq!(ctx.rules.len(), 2);
        let paths: Vec<&str> = ctx.rules.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"bundle/root.md"));
        assert!(paths.contains(&"bundle/inner/strict.md"));

        let sections = ctx.to_prompt_sections().unwrap();
        assert!(sections.contains("## Rule: bundle/root.md"));
        assert!(sections.contains("## Rule: bundle/inner/strict.md"));
        assert!(sections.contains("Root rule content."));
        assert!(sections.contains("Strict rule content."));
    }

    #[tokio::test]
    async fn test_disabled_nested_rule_omitted() {
        let tmp = TempDir::new().unwrap();
        let agent_home = tmp.path().join("agent-1");
        ao_protocol::agent_home::ensure_agent_home(&agent_home).await.unwrap();

        let bundle_dir = agent_home.join("rules").join("bundle");
        tokio::fs::create_dir_all(&bundle_dir).await.unwrap();
        tokio::fs::write(bundle_dir.join("keep.md"), "Keep me.").await.unwrap();
        tokio::fs::write(bundle_dir.join("drop.md"), "Drop me.").await.unwrap();
        tokio::fs::write(
            bundle_dir.join("drop.md.manifest.json"),
            r#"{"added_by":"user","enabled":false,"auto_sync":false,"source_url":null,"imported_at":"2024-01-01T00:00:00Z"}"#,
        )
        .await
        .unwrap();

        let ctx = load_agent_home_context(tmp.path(), &minimal_profile(), &agent_home, &default_patterns()).await;
        let paths: Vec<&str> = ctx.rules.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["bundle/keep.md"]);
    }

    #[tokio::test]
    async fn test_plugin_rules_still_render() {
        // plugin_rules are set externally (agent_runner) and still rendered.
        let ctx = AgentHomeContext {
            skill_registry: SkillRegistry::empty(),
            skills: vec![],
            rules: vec![],
            instructions: vec![],
            plugin_skills: vec![],
            plugin_rules: vec![PluginRuleMeta {
                id: "superpowers/core".to_string(),
                plugin_name: "superpowers".to_string(),
                rule_name: "core".to_string(),
                content: "plugin rule body".to_string(),
            }],
        };

        let sections = ctx.to_prompt_sections().unwrap();
        assert!(sections.contains("# Plugin Rules"));
        assert!(sections.contains("## Rule: superpowers/core"));
        assert!(sections.contains("plugin rule body"));
        assert!(!sections.contains("# Studio Skills"));
        assert!(!sections.contains("# Plugin Skills"));
    }

    #[tokio::test]
    async fn test_registry_skill_name_used_not_title() {
        // New format: name field from registry record, not legacy `title`.
        let mut registry = SkillRegistry::empty();
        use ao_engine_tools_core::skill_registry::{ContextMode, SkillRecord, SkillSource};
        registry.insert(
            "my-skill".to_string(),
            SkillEntry::Ok(SkillRecord {
                name: "my-skill".to_string(),
                description: "A cool skill".to_string(),
                context: ContextMode::Inline,
                agent: None,
                allowed_tools: vec![],
                arguments: vec![],
                body: "body".to_string(),
                source: SkillSource::User,
                when_to_use: None,
                model: None,
                disable_model_invocation: false,
                provenance: ao_engine_tools_core::skill_registry::SkillProvenance::UserAuthored,
                retired: false,
                retired_reason: None,
                superseded_by: None,
                distilled_from: vec![],
                version: 1,
            }),
        );
        let ctx = AgentHomeContext {
            skill_registry: registry,
            ..AgentHomeContext::default()
        };
        let sections = ctx.to_prompt_sections().unwrap();
        // Format: - **my-skill** (`my-skill`) — A cool skill
        assert!(sections.contains("**my-skill** (`my-skill`) — A cool skill"));
    }

    #[tokio::test]
    async fn test_plugin_skill_rendered_with_suffix() {
        use ao_engine_tools_core::skill_registry::{ContextMode, SkillRecord, SkillSource};
        let mut registry = SkillRegistry::empty();
        registry.insert(
            "tdd".to_string(),
            SkillEntry::Ok(SkillRecord {
                name: "tdd".to_string(),
                description: "Red/green/refactor".to_string(),
                context: ContextMode::Inline,
                agent: None,
                allowed_tools: vec![],
                arguments: vec![],
                body: "body".to_string(),
                source: SkillSource::Plugin { plugin_name: "superpowers".to_string() },
                when_to_use: None,
                model: None,
                disable_model_invocation: false,
                provenance: ao_engine_tools_core::skill_registry::SkillProvenance::UserAuthored,
                retired: false,
                retired_reason: None,
                superseded_by: None,
                distilled_from: vec![],
                version: 1,
            }),
        );
        let ctx = AgentHomeContext {
            skill_registry: registry,
            ..AgentHomeContext::default()
        };
        let sections = ctx.to_prompt_sections().unwrap();
        assert!(sections.contains("[plugin: superpowers]"));
        assert!(sections.contains("Red/green/refactor"));
    }

    // ─── WorkspaceContext tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_load_workspace_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let ctx = load_workspace_context(tmp.path()).await;
        assert!(ctx.is_empty());
        assert!(ctx.to_prompt_sections().is_none());
    }

    #[tokio::test]
    async fn test_load_workspace_nonexistent_dir() {
        let ctx = load_workspace_context(Path::new("/nonexistent/workspace")).await;
        assert!(ctx.is_empty());
    }

    #[tokio::test]
    async fn test_load_workspace_instruction_file() {
        let tmp = TempDir::new().unwrap();
        tokio::fs::write(tmp.path().join("CLAUDE.md"), "Project-specific instructions here.")
            .await
            .unwrap();

        let ctx = load_workspace_context(tmp.path()).await;
        assert!(ctx.instruction.is_some());
        assert!(ctx.instruction.unwrap().contains("Project-specific instructions"));
        assert!(ctx.rules.is_empty());
    }

    #[tokio::test]
    async fn test_load_workspace_rules() {
        let tmp = TempDir::new().unwrap();
        let rules_dir = tmp.path().join(".claude").join("rules");
        tokio::fs::create_dir_all(&rules_dir).await.unwrap();
        tokio::fs::write(rules_dir.join("no-console.md"), "Do not use console.log.")
            .await
            .unwrap();
        tokio::fs::write(rules_dir.join("style.md"), "Use 2-space indentation.")
            .await
            .unwrap();

        let ctx = load_workspace_context(tmp.path()).await;
        assert!(ctx.instruction.is_none());
        assert_eq!(ctx.rules.len(), 2);
        assert_eq!(ctx.rules[0].0, "no-console");
        assert_eq!(ctx.rules[1].0, "style");
    }

    #[tokio::test]
    async fn test_load_workspace_combined() {
        let tmp = TempDir::new().unwrap();
        tokio::fs::write(tmp.path().join("CLAUDE.md"), "Workspace instructions.")
            .await
            .unwrap();
        let rules_dir = tmp.path().join(".claude").join("rules");
        tokio::fs::create_dir_all(&rules_dir).await.unwrap();
        tokio::fs::write(rules_dir.join("lint.md"), "Run eslint before committing.")
            .await
            .unwrap();

        let ctx = load_workspace_context(tmp.path()).await;
        assert!(ctx.instruction.is_some());
        assert_eq!(ctx.rules.len(), 1);

        let sections = ctx.to_prompt_sections().unwrap();
        assert!(sections.contains("# Workspace Instructions"));
        assert!(sections.contains("Workspace instructions."));
        assert!(sections.contains("# Workspace Rules"));
        assert!(sections.contains("## lint"));
        assert!(sections.contains("Run eslint before committing."));
    }

    #[tokio::test]
    async fn test_workspace_empty_instruction_file_skipped() {
        let tmp = TempDir::new().unwrap();
        tokio::fs::write(tmp.path().join("CLAUDE.md"), "   \n  ")
            .await
            .unwrap();

        let ctx = load_workspace_context(tmp.path()).await;
        assert!(ctx.instruction.is_none());
        assert!(ctx.is_empty());
    }

    // ─── Skill listing: when_to_use and budget ────────────────────────────────

    fn make_skill_record(name: &str, description: &str, when_to_use: Option<&str>) -> ao_engine_tools_core::skill_registry::SkillRecord {
        use ao_engine_tools_core::skill_registry::{ContextMode, SkillRecord, SkillSource};
        SkillRecord {
            name: name.to_string(),
            description: description.to_string(),
            context: ContextMode::Inline,
            agent: None,
            allowed_tools: vec![],
            arguments: vec![],
            body: String::new(),
            source: SkillSource::User,
            when_to_use: when_to_use.map(str::to_string),
            model: None,
            disable_model_invocation: false,
            provenance: ao_engine_tools_core::skill_registry::SkillProvenance::UserAuthored,
            retired: false,
            retired_reason: None,
            superseded_by: None,
            distilled_from: vec![],
            version: 1,
        }
    }

    #[tokio::test]
    async fn test_when_to_use_appended_in_listing() {
        let mut registry = SkillRegistry::empty();
        registry.insert(
            "my-skill".to_string(),
            SkillEntry::Ok(make_skill_record("my-skill", "Does things", Some("Use when you need to do things"))),
        );
        let ctx = AgentHomeContext { skill_registry: registry, ..AgentHomeContext::default() };
        let sections = ctx.to_prompt_sections().unwrap();
        assert!(sections.contains("Does things"), "description missing");
        assert!(
            sections.contains("Use when you need to do things"),
            "when_to_use hint missing from listing"
        );
    }

    #[tokio::test]
    async fn test_no_when_to_use_leaves_listing_unchanged() {
        let mut registry = SkillRegistry::empty();
        registry.insert(
            "my-skill".to_string(),
            SkillEntry::Ok(make_skill_record("my-skill", "Does things", None)),
        );
        let ctx = AgentHomeContext { skill_registry: registry, ..AgentHomeContext::default() };
        let sections = ctx.to_prompt_sections().unwrap();
        // Entry line ends after description + suffix, no extra " — " hint.
        assert!(sections.contains("**my-skill** (`my-skill`) — Does things"));
        // No dangling " — " from a missing when_to_use.
        let entry_line = sections
            .lines()
            .find(|l| l.contains("my-skill") && l.starts_with('-'))
            .unwrap();
        assert!(
            !entry_line.ends_with("— "),
            "trailing ' — ' should not appear when when_to_use is None"
        );
    }

    #[tokio::test]
    async fn test_long_description_truncated_to_entry_cap() {
        use ao_engine_tools_core::skill_registry::{ContextMode, SkillRecord, SkillSource};
        // 260-char description — 10 over the 250-char cap.
        let long_desc = "x".repeat(260);
        let mut registry = SkillRegistry::empty();
        registry.insert(
            "verbose-skill".to_string(),
            SkillEntry::Ok(SkillRecord {
                name: "verbose-skill".to_string(),
                description: long_desc.clone(),
                context: ContextMode::Inline,
                agent: None,
                allowed_tools: vec![],
                arguments: vec![],
                body: String::new(),
                source: SkillSource::User,
                when_to_use: None,
                model: None,
                disable_model_invocation: false,
                provenance: ao_engine_tools_core::skill_registry::SkillProvenance::UserAuthored,
                retired: false,
                retired_reason: None,
                superseded_by: None,
                distilled_from: vec![],
                version: 1,
            }),
        );
        let ctx = AgentHomeContext { skill_registry: registry, ..AgentHomeContext::default() };
        let sections = ctx.to_prompt_sections().unwrap();
        // The listing must not contain the full 260-char description.
        assert!(
            !sections.contains(&long_desc),
            "full oversized description should be truncated"
        );
        // The truncation marker must be present.
        assert!(sections.contains('…'), "truncation marker '…' should appear");
    }

    #[tokio::test]
    async fn test_budget_exceeded_falls_back_to_names_only() {
        use ao_engine_tools_core::skill_registry::{ContextMode, SkillRecord, SkillSource};
        // Each entry with a 250-char truncated description costs ~285 chars.
        // Budget ceiling is 8 000 chars; we need ceil(7715/285) ≈ 28 entries
        // to exceed it. Use 40 to be well over the limit.
        let mut registry = SkillRegistry::empty();
        for i in 0..40u32 {
            let name = format!("skill-{:03}", i);
            // Description exceeds the 250-char per-entry cap; after truncation
            // each entry is still ~285 chars. 40 × 285 ≈ 11 400 > 8 000.
            let desc = "y".repeat(400);
            registry.insert(
                name.clone(),
                SkillEntry::Ok(SkillRecord {
                    name: name.clone(),
                    description: desc,
                    context: ContextMode::Inline,
                    agent: None,
                    allowed_tools: vec![],
                    arguments: vec![],
                    body: String::new(),
                    source: SkillSource::User,
                    when_to_use: None,
                    model: None,
                    disable_model_invocation: false,
                    provenance: ao_engine_tools_core::skill_registry::SkillProvenance::UserAuthored,
                    retired: false,
                    retired_reason: None,
                    superseded_by: None,
                    distilled_from: vec![],
                    version: 1,
                }),
            );
        }
        let ctx = AgentHomeContext { skill_registry: registry, ..AgentHomeContext::default() };
        let sections = ctx.to_prompt_sections().unwrap();

        // In names-only mode the "yyy…" description content must not appear.
        assert!(
            !sections.contains(&"y".repeat(10)),
            "descriptions should not appear in names-only fallback"
        );
        // All skill names must still appear.
        assert!(sections.contains("skill-000"), "skill-000 missing from names-only listing");
        assert!(sections.contains("skill-039"), "skill-039 missing from names-only listing");
        // The heading and wrapper must still be present.
        assert!(sections.contains("# Studio Skills"));
        assert!(sections.contains("<system-reminder>"));
    }

    // ─── render_studio_skills_block: CLI precedence directive ─────────────────

    #[test]
    fn render_block_empty_registry_returns_none() {
        let registry = SkillRegistry::empty();
        assert!(render_studio_skills_block(&registry, false).is_none());
        assert!(render_studio_skills_block(&registry, true).is_none());
    }

    #[test]
    fn render_block_cli_precedence_directive_present_when_flagged() {
        let mut registry = SkillRegistry::empty();
        registry.insert(
            "my-skill".to_string(),
            SkillEntry::Ok(make_skill_record("my-skill", "Does things", None)),
        );

        // CLI mode: the precedence directive must instruct the model to prefer
        // Studio skills over the host binary's native `Skill` ecosystem.
        let cli_block = render_studio_skills_block(&registry, true).unwrap();
        assert!(
            cli_block.contains("prefer the Studio skill"),
            "CLI precedence directive missing; got:\n{}",
            cli_block
        );
        assert!(
            cli_block.contains("takes precedence"),
            "CLI precedence wording missing; got:\n{}",
            cli_block
        );
        // The skill itself must still be listed.
        assert!(cli_block.contains("**my-skill**"));

        // Native mode: no competing external binary, so the directive is omitted.
        let native_block = render_studio_skills_block(&registry, false).unwrap();
        assert!(
            !native_block.contains("prefer the Studio skill"),
            "native listing must not carry the CLI precedence directive; got:\n{}",
            native_block
        );
        assert!(native_block.contains("**my-skill**"));
    }

    #[test]
    fn render_block_plugin_skill_carries_suffix() {
        use ao_engine_tools_core::skill_registry::{ContextMode, SkillRecord, SkillSource};
        let mut registry = SkillRegistry::empty();
        registry.insert(
            "brainstorming".to_string(),
            SkillEntry::Ok(SkillRecord {
                name: "brainstorming".to_string(),
                description: "Structured idea generation".to_string(),
                context: ContextMode::Inline,
                agent: None,
                allowed_tools: vec![],
                arguments: vec![],
                body: "body".to_string(),
                source: SkillSource::Plugin { plugin_name: "superpowers".to_string() },
                when_to_use: None,
                model: None,
                disable_model_invocation: false,
                provenance: ao_engine_tools_core::skill_registry::SkillProvenance::UserAuthored,
                retired: false,
                retired_reason: None,
                superseded_by: None,
                distilled_from: vec![],
                version: 1,
            }),
        );
        let block = render_studio_skills_block(&registry, true).unwrap();
        assert!(block.contains("**brainstorming**"));
        assert!(block.contains("[plugin: superpowers]"));
    }
}
