//! `settings.json` loader for the runner's hook and permission blocks.
//!
//! Two sources are consulted, in the following order:
//!
//! 1. **Project-local** — `<cwd>/.launchpad_studio/settings.json`. Holds
//!    rules that ride with the repository.
//! 2. **User-global** — `<data_root>/settings.json`, where `<data_root>`
//!    is whatever [`ao_protocol::data_root::resolve_data_root_or_cwd`]
//!    returns (honors the `LAUNCHPAD_STUDIO_DATA_DIR` env override).
//!
//! Both files are optional. Missing files yield defaults; malformed JSON
//! produces a [`SettingsError::Parse`] naming the offending path.
//!
//! Merge semantics:
//!
//! - Scalar fields on `permissions` (`concurrent_tool_cap`,
//!   `deny_count_threshold`) — **project-local wins** when both files
//!   provide a value. A field absent from project-local but set in
//!   user-global flows through. A field absent from both falls back to
//!   the documented default.
//! - Vec fields (`hooks.pre_tool_use`, `hooks.post_tool_use`,
//!   `permissions.rules`) are **concatenated** with project entries
//!   first, so users can layer additional rules on top of a base set
//!   without rewriting the global file.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use ao_engine_tools_core::{LoadPolicyOverride, PermissionDecision};
use ao_protocol::data_root;
use serde::{Deserialize, Serialize};

/// Sub-directory under the project root that holds `settings.json`.
const PROJECT_SETTINGS_DIR: &str = ".launchpad_studio";
/// File name of the runner settings document at both sources.
const SETTINGS_FILENAME: &str = "settings.json";

/// Default upper bound on simultaneously in-flight tool invocations.
pub const DEFAULT_CONCURRENT_TOOL_CAP: usize = 10;
/// Default number of `Ask` denials before the runner auto-denies further
/// `Ask` outcomes for the same `(agent, tool)` pair.
pub const DEFAULT_DENY_COUNT_THRESHOLD: u32 = 3;
/// Default per-hook subprocess timeout when an entry omits `timeout_ms`.
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 5000;

/// Top-level shape returned by [`load_runner_settings`]. Combines the
/// merged hook configuration with the merged permissions configuration.
#[derive(Debug, Clone, Default)]
pub struct RunnerSettings {
    pub hooks: HookConfig,
    pub permissions: PermissionsConfig,
    /// Per-tool load-policy overrides from settings.json.
    /// Project-local entries win over user-global for the same tool name.
    pub tool_load_overrides: HashMap<String, LoadPolicyOverride>,
}

/// Hook entries split by phase. Pre-tool-use hooks run before the
/// permission gate; post-tool-use hooks run after a successful
/// invocation.
#[derive(Debug, Clone, Default)]
pub struct HookConfig {
    pub pre_tool_use: Vec<HookEntry>,
    pub post_tool_use: Vec<HookEntry>,
}

/// A single hook command bound to a permission-style match string.
///
/// `match` follows the same `Tool(arg-glob)` grammar as
/// [`crate::permissions::rule`]. `command` is executed via `bash -c` by
/// the hook subprocess runner. `timeout_ms` defaults to
/// [`DEFAULT_HOOK_TIMEOUT_MS`] when the source file omits the field.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HookEntry {
    #[serde(rename = "match")]
    pub r#match: String,
    pub command: String,
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_hook_timeout_ms() -> u64 {
    DEFAULT_HOOK_TIMEOUT_MS
}

/// Permissions block: scalar tuning knobs plus a list of raw rule
/// entries that the permission gate compiles into matchers on demand.
#[derive(Debug, Clone)]
pub struct PermissionsConfig {
    pub concurrent_tool_cap: usize,
    pub deny_count_threshold: u32,
    pub rules: Vec<RawPermissionRule>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            concurrent_tool_cap: DEFAULT_CONCURRENT_TOOL_CAP,
            deny_count_threshold: DEFAULT_DENY_COUNT_THRESHOLD,
            rules: Vec::new(),
        }
    }
}

/// On-disk shape of one entry in `permissions.rules`. The string-form
/// `decision` is mapped into a [`PermissionDecision`] via
/// [`RawPermissionRule::to_decision`]; the loader pre-validates every
/// rule's decision string so unknown values surface as
/// [`SettingsError::UnknownDecision`] before any rule is consumed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RawPermissionRule {
    #[serde(rename = "match")]
    pub r#match: String,
    pub decision: String,
}

impl RawPermissionRule {
    /// Convert the raw `decision` string into a typed
    /// [`PermissionDecision`]. Recognised values (snake_case): `allow`,
    /// `allow_once`, `allow_session`, `deny`, `ask`. Anything else
    /// produces [`SettingsError::UnknownDecision`].
    ///
    /// `Mutate` is intentionally NOT loadable from config — its payload
    /// (`updated_input`) requires a structured value that does not fit
    /// the `String` shape used here. Hooks emit `Mutate` instead.
    pub fn to_decision(&self) -> Result<PermissionDecision, SettingsError> {
        match self.decision.as_str() {
            "allow" => Ok(PermissionDecision::Allow),
            "allow_once" => Ok(PermissionDecision::AllowOnce),
            "allow_session" => Ok(PermissionDecision::AllowSession),
            "deny" => Ok(PermissionDecision::Deny {
                reason: format!("denied by rule '{}'", self.r#match),
            }),
            "ask" => Ok(PermissionDecision::Ask {
                reason: format!("rule '{}' requires confirmation", self.r#match),
            }),
            _ => Err(SettingsError::UnknownDecision {
                rule: self.r#match.clone(),
                decision: self.decision.clone(),
            }),
        }
    }
}

/// Errors returned by [`load_runner_settings`].
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// I/O failure other than `NotFound` (which is treated as "absent",
    /// not an error). Captures the path so callers can pinpoint the
    /// offender.
    #[error("failed to read settings file '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// JSON parse failure. The path is included so test assertions and
    /// user diagnostics can name the offending file.
    #[error("failed to parse settings file '{path}': {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// A `permissions.rules[*].decision` string was not one of the
    /// recognised values. Surfacing this loudly prevents a typo from
    /// silently degrading to a default verdict.
    #[error("settings rule '{rule}' has unknown decision '{decision}'")]
    UnknownDecision { rule: String, decision: String },
}

/// On-disk JSON shape for one `settings.json` document. Mirror of
/// [`RunnerSettings`] except scalars are `Option`s so we can detect
/// absence (and apply project-over-global precedence correctly).
#[derive(Debug, Default, Deserialize)]
struct RawSettings {
    #[serde(default)]
    hooks: RawHookSection,
    #[serde(default)]
    permissions: RawPermissionsSection,
    /// Raw map of tool name → string value from `tool_load_overrides`.
    /// Validated and converted to typed overrides during merge.
    #[serde(default)]
    tool_load_overrides: HashMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawHookSection {
    #[serde(default)]
    pre_tool_use: Vec<HookEntry>,
    #[serde(default)]
    post_tool_use: Vec<HookEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPermissionsSection {
    concurrent_tool_cap: Option<usize>,
    deny_count_threshold: Option<u32>,
    #[serde(default)]
    rules: Vec<RawPermissionRule>,
}

/// Load and merge runner settings from the project-local and
/// user-global sources. See the module-level docs for merge semantics.
///
/// Returns [`RunnerSettings::default`]-equivalent values when both
/// sources are absent.
pub fn load_runner_settings(cwd: &Path) -> Result<RunnerSettings, SettingsError> {
    let project_path = cwd.join(PROJECT_SETTINGS_DIR).join(SETTINGS_FILENAME);
    let global_path = data_root::resolve_data_root_or_cwd().join(SETTINGS_FILENAME);

    let project = read_settings_file(&project_path)?;
    let global = read_settings_file(&global_path)?;

    // Eagerly validate every decision string so a typo in either file
    // fails the whole load instead of waiting until rule evaluation.
    for raw in [project.as_ref(), global.as_ref()].into_iter().flatten() {
        for rule in &raw.permissions.rules {
            let _ = rule.to_decision()?;
        }
    }

    let mut settings = RunnerSettings::default();

    // Permissions scalars: start with global, then let project override.
    let mut cap = global.as_ref().and_then(|g| g.permissions.concurrent_tool_cap);
    let mut threshold = global.as_ref().and_then(|g| g.permissions.deny_count_threshold);
    if let Some(p) = project.as_ref() {
        if p.permissions.concurrent_tool_cap.is_some() {
            cap = p.permissions.concurrent_tool_cap;
        }
        if p.permissions.deny_count_threshold.is_some() {
            threshold = p.permissions.deny_count_threshold;
        }
    }
    settings.permissions.concurrent_tool_cap = cap.unwrap_or(DEFAULT_CONCURRENT_TOOL_CAP);
    settings.permissions.deny_count_threshold =
        threshold.unwrap_or(DEFAULT_DENY_COUNT_THRESHOLD);

    // Concatenate vec fields with project entries first.
    if let Some(p) = project.as_ref() {
        settings
            .permissions
            .rules
            .extend(p.permissions.rules.iter().cloned());
        settings
            .hooks
            .pre_tool_use
            .extend(p.hooks.pre_tool_use.iter().cloned());
        settings
            .hooks
            .post_tool_use
            .extend(p.hooks.post_tool_use.iter().cloned());
    }
    if let Some(g) = global.as_ref() {
        settings
            .permissions
            .rules
            .extend(g.permissions.rules.iter().cloned());
        settings
            .hooks
            .pre_tool_use
            .extend(g.hooks.pre_tool_use.iter().cloned());
        settings
            .hooks
            .post_tool_use
            .extend(g.hooks.post_tool_use.iter().cloned());
    }

    // tool_load_overrides: user-global fills in first, then project-local
    // overwrites (project wins on same key). Invalid values are warned and
    // skipped — they never fail the load.
    if let Some(g) = global.as_ref() {
        for (name, val) in &g.tool_load_overrides {
            match parse_load_policy_override(val) {
                Some(ov) => {
                    settings.tool_load_overrides.insert(name.clone(), ov);
                }
                None => {
                    tracing::warn!(
                        "settings.json: tool_load_overrides['{}'] has unknown value '{}'; skipping",
                        name,
                        val
                    );
                }
            }
        }
    }
    if let Some(p) = project.as_ref() {
        for (name, val) in &p.tool_load_overrides {
            match parse_load_policy_override(val) {
                Some(ov) => {
                    settings.tool_load_overrides.insert(name.clone(), ov);
                }
                None => {
                    tracing::warn!(
                        "settings.json: tool_load_overrides['{}'] has unknown value '{}'; skipping",
                        name,
                        val
                    );
                }
            }
        }
    }

    Ok(settings)
}

fn read_settings_file(path: &Path) -> Result<Option<RawSettings>, SettingsError> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let parsed: RawSettings =
                serde_json::from_str(&text).map_err(|source| SettingsError::Parse {
                    path: path.to_path_buf(),
                    source,
                })?;
            Ok(Some(parsed))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SettingsError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn parse_load_policy_override(s: &str) -> Option<LoadPolicyOverride> {
    match s {
        "always_load" => Some(LoadPolicyOverride::ForceAlwaysLoad),
        "deferred" => Some(LoadPolicyOverride::ForceDeferred),
        _ => None,
    }
}

