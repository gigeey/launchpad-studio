use serde::{Deserialize, Serialize};

/// Pre-loaded workspace context passed to the canonical system-prompt composer.
///
/// All fields are loaded by the caller before invoking compose_system_prompt().
/// The composer itself performs no file I/O.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceContext {
    /// Absolute path to the workspace root directory.
    pub root_path: String,
    /// Contents of the workspace CLAUDE.md file, if present.
    #[serde(default)]
    pub claude_md_content: Option<String>,
    /// Loaded rule file contents from the workspace rules/ directory.
    #[serde(default)]
    pub rules: Vec<String>,
}

/// Pre-loaded agent home context passed to the canonical system-prompt composer.
///
/// All fields are loaded by the caller before invoking compose_system_prompt().
/// The composer itself performs no file I/O.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentHomeContext {
    /// Contents of the agent home CLAUDE.md file, if present.
    #[serde(default)]
    pub claude_md_content: Option<String>,
    /// Loaded rule file contents from the agent home rules/ directory.
    #[serde(default)]
    pub rules: Vec<String>,
    /// Loaded skill definition contents from the agent home skills/ directory.
    /// Legacy: per-agent skills are no longer the source of truth — the unified
    /// skill listing now arrives pre-rendered in `skills_block`. Retained for
    /// the rare agent that still ships a local `skills/` directory and consumed
    /// only as a fallback when `skills_block` is None.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Pre-rendered "# Studio Skills" listing block, built by the runner from
    /// the unified skill registry (user pool + enabled plugins, plus the MCP
    /// overlay where available). When present this is the authoritative skill
    /// listing and supersedes `skills`. The runner — not the composer — builds
    /// it, because the registry lives above this crate's dependency layer and
    /// the listing must reflect the same pools `RunSkill` dispatches against.
    #[serde(default)]
    pub skills_block: Option<String>,
}
