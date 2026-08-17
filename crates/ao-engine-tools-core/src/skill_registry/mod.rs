pub mod dispatch;
pub mod frontmatter;
pub mod loader;
pub mod report;
pub mod search_index;
pub mod sources;
pub mod usage;
#[cfg(test)]
mod tests;

pub use frontmatter::{
    clear_retired, parse_frontmatter, set_body, set_description, set_disable_model_invocation,
    set_distilled_from, set_distilled_origin, set_retired, set_version, FrontmatterError,
};
pub use report::{build_report, format_report, rank, SkillUsageReport, SkillUsageStats};
pub use search_index::{reindex_skills, skill_index_records};

/// Whether a skill originates from the user pool, a named plugin, or an MCP server prompt.
#[derive(Debug, Clone, PartialEq)]
pub enum SkillSource {
    User,
    Plugin { plugin_name: String },
    /// Skill was sourced from an MCP server's `prompts/list` + `prompts/get` at startup.
    Mcp { server_name: String },
    /// Skill is compiled into the binary via `include_str!` (see
    /// `skill_registry::sources::load_builtin_pool`) rather than read from
    /// `<data_dir>` at runtime. Ships with every build, needs no install
    /// step, and is not gated by an agent's `skills`/`enabled_plugins`
    /// allowlist — `SkillRegistry::load` always includes it.
    BuiltIn,
}

/// Execution context for a skill invocation.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ContextMode {
    /// Skill body is injected as a follow-up user message in the current runner.
    #[default]
    Inline,
    /// Skill is dispatched as a synchronous child runner via SubagentSpawner.
    Fork,
}

/// A named argument declared in a skill's frontmatter.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillArgument {
    pub name: String,
    pub required: bool,
}

/// Who/what authored a skill's body — orthogonal to [`SkillSource`], which
/// only says *where on disk* a skill was loaded from (user pool vs. plugin
/// vs. MCP server), never who wrote it.
///
/// Read from the frontmatter `origin` key: `set_distilled_origin` is the
/// only writer of `origin: distilled` today, so `Distilled`
/// is the one recognized non-default value. An absent `origin` key, or any
/// value other than `"distilled"`, is treated as
/// [`SkillProvenance::UserAuthored`] — a deliberately conservative default.
/// The lifecycle sweeps (`ao_engine_tools_engine::skill::{consolidation,
/// retirement}`) only ever auto-act on a skill whose provenance is
/// unambiguously *not* the user's own; an unrecognized or missing marker
/// never clears that bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkillProvenance {
    #[default]
    UserAuthored,
    Distilled,
}

/// Parsed and validated skill metadata plus body content.
#[derive(Debug, Clone)]
pub struct SkillRecord {
    pub name: String,
    pub description: String,
    pub context: ContextMode,
    pub agent: Option<String>,
    pub allowed_tools: Vec<String>,
    pub arguments: Vec<SkillArgument>,
    pub body: String,
    pub source: SkillSource,
    /// Additional discovery hint appended to this skill's listing entry.
    pub when_to_use: Option<String>,
    /// Model identifier override applied when this skill is fork-dispatched.
    pub model: Option<String>,
    /// If true, `RunSkill` declines model-issued invocations of this skill.
    pub disable_model_invocation: bool,
    /// Who/what authored this skill's body. See [`SkillProvenance`].
    pub provenance: SkillProvenance,
    /// True once a consolidation or retirement sweep has tombstoned this
    /// skill. Distinct from `disable_model_invocation` alone, which is also
    /// `true` for a brand-new skill still pending its first confirmation —
    /// `retired` marks *why* invocation is disabled: an automated lifecycle
    /// sweep decided this skill's time was up, not that it never got
    /// approved in the first place.
    pub retired: bool,
    /// Human-readable reason a retired skill was retired (e.g. `"unused"`,
    /// `"consolidated"`). `None` unless `retired` is `true`.
    pub retired_reason: Option<String>,
    /// For a consolidation retirement: the name of the skill this one
    /// was merged into. `None` for a usage-based retirement (nothing
    /// superseded it — it just went quiet) or when `retired` is `false`.
    pub superseded_by: Option<String>,
    /// Reflection-candidate ids (`ReflectionCandidate::id`) the
    /// distillation pass folded into this skill when it generalized a
    /// repeated procedure into a template. Empty for a manually authored
    /// skill; only ever non-empty when `provenance ==
    /// SkillProvenance::Distilled`. Read from the frontmatter
    /// `distilled-from` key; see [`set_distilled_from`].
    pub distilled_from: Vec<String>,
    /// Monotonic version counter. A skill starts at 1 the
    /// first time it is written. `SkillRegister` bumps it by 1 whenever a
    /// name is re-registered over an existing skill; the consolidation
    /// sweep (`ao_engine_tools_engine::skill::consolidation`) bumps the
    /// winning skill's version by 1 whenever it absorbs a near-duplicate.
    /// Skills persisted before this field existed load as `1` (serde
    /// default) — the same value a brand-new skill starts at, so there is no
    /// observable difference between "never versioned" and "written once,
    /// never bumped". See [`set_version`].
    pub version: u32,
}

/// A registry entry: either a successfully parsed skill or a load error.
#[derive(Debug, Clone)]
pub enum SkillEntry {
    Ok(SkillRecord),
    Err(String),
}

/// Ordered in-memory collection of skills loaded from user and plugin pools.
#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    pub(crate) entries: Vec<SkillEntry>,
    /// Canonical skill names in insertion order, parallel to `entries`.
    pub(crate) names: Vec<String>,
    /// Maps canonical skill name → index in `entries` for O(1) lookup.
    name_index: std::collections::HashMap<String, usize>,
}

impl SkillRegistry {
    /// Return an empty registry with zero entries.
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
            names: Vec::new(),
            name_index: std::collections::HashMap::new(),
        }
    }

    /// Returns true if the registry contains no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of entries in the registry.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return the entry for a given skill name, or `None` if absent.
    pub fn get(&self, name: &str) -> Option<&SkillEntry> {
        self.name_index.get(name).map(|&idx| &self.entries[idx])
    }

    /// Yield (name, entry) pairs in insertion order.
    pub fn all_visible(&self) -> impl Iterator<Item = (&str, &SkillEntry)> {
        self.names.iter().map(String::as_str).zip(self.entries.iter())
    }

    /// Insert a named entry into the registry (used by the loader and tests).
    pub fn insert(&mut self, name: String, entry: SkillEntry) {
        let idx = self.entries.len();
        self.name_index.insert(name.clone(), idx);
        self.names.push(name);
        self.entries.push(entry);
    }
}
