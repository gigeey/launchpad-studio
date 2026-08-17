//! Tauri command bridge for the global plugin store.
//!
//! Wraps the stateless `ao_engine::plugin_*` library functions as Tauri
//! commands, exposes serializable DTOs to the frontend, and converts the
//! library's typed errors into a single `PluginCommandError` enum the UI can
//! discriminate on (`type` + `detail` tagged enum).
//!
//! Mutation commands (`install_plugin`, `uninstall_plugin`, `refresh_plugin`,
//! `set_plugin_auto_update`) call `plugin_cache.refresh()` after the on-disk
//! change so the next agent message turn sees the new content. Agent-profile
//! mutation commands (`set_agent_plugin_enabled`, `set_agent_skill_subset`)
//! persist via the shared `PersistenceLayer` and invalidate the per-agent
//! `context_cache` so the next turn re-reads.

use std::path::PathBuf;
use std::sync::Arc;

use ao_engine::plugin_catalog::{
    list_plugin_rules, list_plugin_skills, CatalogError, PluginRuleEntry, PluginSkillEntry,
};
use ao_engine::plugin_install::{install_plugin_from_source, InstallError, Source};
use ao_engine::plugin_refresh::{refresh_plugin as engine_refresh_plugin, RefreshError};
use ao_engine::plugin_registry::{
    load_registry, save_registry, ManifestLocationKind, PluginRegistryEntry, PluginSource,
    RegistryError,
};
use ao_engine::plugin_uninstall::{uninstall_plugin as engine_uninstall_plugin, UninstallError};
use ao_engine::AppState;
use ao_protocol::agent::AgentProfile;
use ao_protocol::error::AoError;
use serde::{Deserialize, Serialize};

// ===== DTOs =====

/// Source for `install_plugin`. Mirrors `ao_engine::plugin_install::Source`
/// but uses `String` for `LocalPath` so the wire format is JSON-friendly.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum SourceDto {
    #[serde(rename = "github_url")]
    GitHubUrl(String),
    LocalPath(String),
}

impl From<SourceDto> for Source {
    fn from(s: SourceDto) -> Self {
        match s {
            SourceDto::GitHubUrl(url) => Source::GitHubUrl(url),
            SourceDto::LocalPath(p) => Source::LocalPath(PathBuf::from(p)),
        }
    }
}

/// Public-facing plugin entry. Re-exports `PluginRegistryEntry`'s shape with
/// `String` paths so the frontend doesn't have to deal with platform PathBufs.
#[derive(Debug, Clone, Serialize)]
pub struct PluginEntryDto {
    pub name: String,
    pub version: String,
    pub source: PluginSource,
    pub installed_at: String,
    pub last_updated_at: String,
    pub auto_update_enabled: bool,
    pub manifest_location: ManifestLocationKind,
}

impl From<&PluginRegistryEntry> for PluginEntryDto {
    fn from(e: &PluginRegistryEntry) -> Self {
        Self {
            name: e.name.clone(),
            version: e.version.clone(),
            source: e.source.clone(),
            installed_at: e.installed_at.to_rfc3339(),
            last_updated_at: e.last_updated_at.to_rfc3339(),
            auto_update_enabled: e.auto_update_enabled,
            manifest_location: e.manifest_location,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallOutcomeDto {
    pub name: String,
    pub version: String,
    pub plugin_dir: String,
    pub manifest_location: ManifestLocationKind,
    pub skills_installed: usize,
    pub rules_installed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct UninstallOutcomeDto {
    pub directory_removed: bool,
    pub registry_entry_removed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefreshOutcomeDto {
    pub name: String,
    pub version: String,
    pub plugin_dir: String,
    pub manifest_location: ManifestLocationKind,
    pub skills_installed: usize,
    pub rules_installed: usize,
    pub last_updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginSkillDto {
    pub id: String,
    pub plugin_name: String,
    pub skill_name: String,
    pub skill_dir: String,
    pub skill_md: String,
}

impl From<PluginSkillEntry> for PluginSkillDto {
    fn from(e: PluginSkillEntry) -> Self {
        Self {
            id: e.id,
            plugin_name: e.plugin_name,
            skill_name: e.skill_name,
            skill_dir: e.skill_dir.to_string_lossy().into_owned(),
            skill_md: e.skill_md.to_string_lossy().into_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginRuleDto {
    pub id: String,
    pub plugin_name: String,
    pub rule_name: String,
    pub rule_file: String,
}

impl From<PluginRuleEntry> for PluginRuleDto {
    fn from(e: PluginRuleEntry) -> Self {
        Self {
            id: e.id,
            plugin_name: e.plugin_name,
            rule_name: e.rule_name,
            rule_file: e.rule_file.to_string_lossy().into_owned(),
        }
    }
}

// ===== Errors =====

/// Tagged error variant the UI can discriminate on without parsing strings.
/// Tauri serializes the `Err` arm of a command's `Result` as JSON, so this
/// enum's serialized shape (`{"type": "...", "detail": ...}`) lands directly
/// in `await invoke(...).catch(e => e)` on the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "detail", rename_all = "snake_case")]
pub enum PluginCommandError {
    /// Install attempted on a name that is already in the registry / on disk.
    Conflict(String),
    /// Refresh / agent-profile lookup targeted a missing entity.
    NotFound(String),
    /// Plugin manifest was unparseable or rejected by the schema.
    ManifestInvalid(String),
    /// The named source has no manifest AND no auto-discoverable content.
    NothingToInstall,
    /// The named source has no manifest and the caller did not opt into
    /// auto-discovery. The UI uses this to prompt the user with a retry
    /// option that re-invokes `install_plugin` with `allowAutoDiscovery: true`.
    ManifestMissing,
    /// Plugin name contained `/`, `..`, etc. — would escape the plugin store.
    UnsafeName(String),
    /// LocalPath source does not exist.
    SourceMissing(String),
    /// `git clone` failed (network, auth, bad URL, etc.).
    NetworkError { url: String, detail: String },
    /// The `git` binary is not on PATH.
    GitUnavailable,
    /// Targeted agent profile does not exist.
    AgentNotFound(String),
    /// Catch-all for unexpected I/O / serde / persistence errors.
    Internal(String),
}

impl From<InstallError> for PluginCommandError {
    fn from(e: InstallError) -> Self {
        match e {
            InstallError::Conflict(n) => PluginCommandError::Conflict(n),
            InstallError::NothingToInstall => PluginCommandError::NothingToInstall,
            InstallError::ManifestMissing => PluginCommandError::ManifestMissing,
            InstallError::UnsafeName(n) => PluginCommandError::UnsafeName(n),
            InstallError::SourceMissing(p) => {
                PluginCommandError::SourceMissing(p.to_string_lossy().into_owned())
            }
            InstallError::Clone { url, detail } => {
                PluginCommandError::NetworkError { url, detail }
            }
            InstallError::GitUnavailable => PluginCommandError::GitUnavailable,
            InstallError::InvalidManifest(err) => PluginCommandError::ManifestInvalid(err.to_string()),
            other => PluginCommandError::Internal(other.to_string()),
        }
    }
}

impl From<UninstallError> for PluginCommandError {
    fn from(e: UninstallError) -> Self {
        match e {
            UninstallError::UnsafeName(n) => PluginCommandError::UnsafeName(n),
            other => PluginCommandError::Internal(other.to_string()),
        }
    }
}

impl From<RefreshError> for PluginCommandError {
    fn from(e: RefreshError) -> Self {
        match e {
            RefreshError::NotFound(n) => PluginCommandError::NotFound(n),
            RefreshError::Install(install_err) => install_err.into(),
            other => PluginCommandError::Internal(other.to_string()),
        }
    }
}

impl From<RegistryError> for PluginCommandError {
    fn from(e: RegistryError) -> Self {
        PluginCommandError::Internal(e.to_string())
    }
}

impl From<CatalogError> for PluginCommandError {
    fn from(e: CatalogError) -> Self {
        PluginCommandError::Internal(e.to_string())
    }
}

impl From<AoError> for PluginCommandError {
    fn from(e: AoError) -> Self {
        match e {
            AoError::AgentNotFound(id) => PluginCommandError::AgentNotFound(id),
            other => PluginCommandError::Internal(other.to_string()),
        }
    }
}

// ===== Library implementations (testable; Tauri commands are thin shims) =====

pub fn list_plugins_impl() -> Result<Vec<PluginEntryDto>, PluginCommandError> {
    let registry = load_registry()?;
    Ok(registry.entries.iter().map(PluginEntryDto::from).collect())
}

pub fn list_global_skills_impl() -> Result<Vec<PluginSkillDto>, PluginCommandError> {
    Ok(list_plugin_skills()?
        .into_iter()
        .map(PluginSkillDto::from)
        .collect())
}

pub fn list_global_rules_impl() -> Result<Vec<PluginRuleDto>, PluginCommandError> {
    Ok(list_plugin_rules()?
        .into_iter()
        .map(PluginRuleDto::from)
        .collect())
}

pub fn install_plugin_impl(
    source: SourceDto,
    manifest_override: Option<String>,
    allow_auto_discovery: bool,
) -> Result<InstallOutcomeDto, PluginCommandError> {
    let outcome = install_plugin_from_source(
        source.into(),
        manifest_override.as_deref(),
        allow_auto_discovery,
    )?;
    Ok(InstallOutcomeDto {
        name: outcome.name,
        version: outcome.version,
        plugin_dir: outcome.plugin_dir.to_string_lossy().into_owned(),
        manifest_location: outcome.manifest_location,
        skills_installed: outcome.skills_installed,
        rules_installed: outcome.rules_installed,
    })
}

pub fn uninstall_plugin_impl(name: &str) -> Result<UninstallOutcomeDto, PluginCommandError> {
    let outcome = engine_uninstall_plugin(name)?;
    Ok(UninstallOutcomeDto {
        directory_removed: outcome.directory_removed,
        registry_entry_removed: outcome.registry_entry_removed,
    })
}

pub fn refresh_plugin_impl(name: &str) -> Result<RefreshOutcomeDto, PluginCommandError> {
    let outcome = engine_refresh_plugin(name)?;
    Ok(RefreshOutcomeDto {
        name: outcome.name,
        version: outcome.version,
        plugin_dir: outcome.plugin_dir.to_string_lossy().into_owned(),
        manifest_location: outcome.manifest_location,
        skills_installed: outcome.skills_installed,
        rules_installed: outcome.rules_installed,
        last_updated_at: outcome.last_updated_at.to_rfc3339(),
    })
}

pub fn set_plugin_auto_update_impl(
    name: &str,
    enabled: bool,
) -> Result<PluginEntryDto, PluginCommandError> {
    let mut registry = load_registry()?;
    let entry = registry
        .entries
        .iter_mut()
        .find(|e| e.name == name)
        .ok_or_else(|| PluginCommandError::NotFound(name.to_string()))?;
    entry.auto_update_enabled = enabled;
    let updated: PluginEntryDto = (&*entry).into();
    save_registry(&registry)?;
    Ok(updated)
}

// Agent-profile mutations need the live PersistenceLayer + ContextCache, so
// they take `&AppState` rather than reaching for global state.

pub async fn set_agent_plugin_enabled_impl(
    state: &AppState,
    agent_id: &str,
    plugin_name: &str,
    enabled: bool,
) -> Result<AgentProfile, PluginCommandError> {
    let mut profile = state
        .persistence
        .agents
        .get(agent_id)
        .await?
        .ok_or_else(|| PluginCommandError::AgentNotFound(agent_id.to_string()))?;

    profile.set_plugin_enabled(plugin_name, enabled);
    state.persistence.agents.update(&profile).await?;
    state.context_cache.invalidate(agent_id).await;
    Ok(profile)
}

pub async fn set_agent_skill_subset_impl(
    state: &AppState,
    agent_id: &str,
    plugin_name: &str,
    subset: Option<Vec<String>>,
) -> Result<AgentProfile, PluginCommandError> {
    let mut profile = state
        .persistence
        .agents
        .get(agent_id)
        .await?
        .ok_or_else(|| PluginCommandError::AgentNotFound(agent_id.to_string()))?;

    profile.set_skill_subset(plugin_name, subset);
    state.persistence.agents.update(&profile).await?;
    state.context_cache.invalidate(agent_id).await;
    Ok(profile)
}

// ===== Tauri command shims =====

#[tauri::command]
pub fn list_plugins() -> Result<Vec<PluginEntryDto>, PluginCommandError> {
    list_plugins_impl()
}

#[tauri::command]
pub fn list_global_skills() -> Result<Vec<PluginSkillDto>, PluginCommandError> {
    list_global_skills_impl()
}

#[tauri::command]
pub fn list_global_rules() -> Result<Vec<PluginRuleDto>, PluginCommandError> {
    list_global_rules_impl()
}

#[tauri::command]
pub async fn install_plugin(
    state: tauri::State<'_, Arc<AppState>>,
    source: SourceDto,
    manifest_override: Option<String>,
    allow_auto_discovery: Option<bool>,
) -> Result<InstallOutcomeDto, PluginCommandError> {
    let app_state = Arc::clone(state.inner());
    // Default `false`: the UI is expected to send `true` only after an
    // explicit user opt-in via the ManifestMissing retry prompt.
    let allow = allow_auto_discovery.unwrap_or(false);
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        install_plugin_impl(source, manifest_override, allow)
    })
    .await
    .map_err(|e| PluginCommandError::Internal(format!("join error: {e}")))??;

    // Connect plugin-bundled MCP servers now that the plugin is on disk.
    let plugin_name = outcome.name.clone();
    let plugin_dir = std::path::PathBuf::from(&outcome.plugin_dir);
    let mcp_entries =
        ao_engine::plugin_mcp::load_plugin_mcp_entries(&plugin_name, &plugin_dir, None);
    for entry in mcp_entries {
        let src = format!("plugin:{plugin_name}");
        if let Err(e) = app_state
            .mcp_manager
            .add_server(entry, Arc::clone(&app_state.tools_registry), src)
            .await
        {
            tracing::warn!("plugin {plugin_name}: failed to connect MCP server: {e}");
        }
    }

    if let Err(err) = app_state.plugin_cache.refresh().await {
        tracing::warn!("plugin cache refresh after install failed: {err}");
    }
    Ok(outcome)
}

#[tauri::command]
pub async fn uninstall_plugin(
    state: tauri::State<'_, Arc<AppState>>,
    name: String,
) -> Result<UninstallOutcomeDto, PluginCommandError> {
    let app_state = Arc::clone(state.inner());

    // Disconnect this plugin's MCP servers before removing it from disk.
    let plugin_source = format!("plugin:{name}");
    let server_names: Vec<String> = app_state
        .mcp_manager
        .server_statuses()
        .await
        .into_iter()
        .filter(|s| s.source == plugin_source)
        .map(|s| s.name)
        .collect();
    for server_name in server_names {
        if let Err(e) = app_state
            .mcp_manager
            .remove_server(&server_name, &app_state.tools_registry)
            .await
        {
            tracing::warn!("uninstall: failed to disconnect MCP server {server_name}: {e}");
        }
    }

    let name_for_closure = name.clone();
    let outcome =
        tauri::async_runtime::spawn_blocking(move || uninstall_plugin_impl(&name_for_closure))
            .await
            .map_err(|e| PluginCommandError::Internal(format!("join error: {e}")))??;

    if let Err(err) = app_state.plugin_cache.refresh().await {
        tracing::warn!("plugin cache refresh after uninstall failed: {err}");
    }
    Ok(outcome)
}

#[tauri::command]
pub async fn refresh_plugin(
    state: tauri::State<'_, Arc<AppState>>,
    name: String,
) -> Result<RefreshOutcomeDto, PluginCommandError> {
    let app_state = Arc::clone(state.inner());
    let outcome = tauri::async_runtime::spawn_blocking(move || refresh_plugin_impl(&name))
        .await
        .map_err(|e| PluginCommandError::Internal(format!("join error: {e}")))??;

    if let Err(err) = app_state.plugin_cache.refresh().await {
        tracing::warn!("plugin cache refresh after refresh failed: {err}");
    }
    Ok(outcome)
}

#[tauri::command]
pub fn set_plugin_auto_update(
    name: String,
    enabled: bool,
) -> Result<PluginEntryDto, PluginCommandError> {
    set_plugin_auto_update_impl(&name, enabled)
}

#[tauri::command]
pub async fn set_agent_plugin_enabled(
    state: tauri::State<'_, Arc<AppState>>,
    agent_id: String,
    plugin_name: String,
    enabled: bool,
) -> Result<AgentProfile, PluginCommandError> {
    set_agent_plugin_enabled_impl(state.inner(), &agent_id, &plugin_name, enabled).await
}

#[tauri::command]
pub async fn set_agent_skill_subset(
    state: tauri::State<'_, Arc<AppState>>,
    agent_id: String,
    plugin_name: String,
    subset: Option<Vec<String>>,
) -> Result<AgentProfile, PluginCommandError> {
    set_agent_skill_subset_impl(state.inner(), &agent_id, &plugin_name, subset).await
}

#[cfg(test)]
mod tests {
    //! Smoke-tests for the bridge layer. The Tauri command shims are
    //! one-liners over `*_impl` fns, so the real coverage is on the impls.
    //!
    //! These tests reuse the `LAUNCHPAD_STUDIO_DATA_DIR`-based test harness
    //! from `ao_engine::plugin_paths::tests`. We can't import it directly
    //! (it's `pub(crate)` to ao-engine), so we replicate the env-var
    //! manipulation here. The mutex in this module is fine because the
    //! tauri crate has no other tests that touch the env var.

    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_temp_root<F: FnOnce(&std::path::Path)>(f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp: TempDir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        f(tmp.path());
        std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");
    }

    #[test]
    fn list_plugins_returns_empty_on_fresh_install() {
        with_temp_root(|_| {
            let result = list_plugins_impl().expect("list_plugins_impl");
            assert!(
                result.is_empty(),
                "fresh install should have no plugins, got {result:?}"
            );
        });
    }

    #[test]
    fn list_global_skills_returns_empty_on_fresh_install() {
        with_temp_root(|_| {
            let result = list_global_skills_impl().expect("list_global_skills_impl");
            assert!(result.is_empty());
        });
    }

    #[test]
    fn list_global_rules_returns_empty_on_fresh_install() {
        with_temp_root(|_| {
            let result = list_global_rules_impl().expect("list_global_rules_impl");
            assert!(result.is_empty());
        });
    }

    #[test]
    fn set_plugin_auto_update_returns_not_found_for_missing_plugin() {
        with_temp_root(|_| {
            let err =
                set_plugin_auto_update_impl("nonexistent", false).expect_err("should fail");
            match err {
                PluginCommandError::NotFound(name) => assert_eq!(name, "nonexistent"),
                other => panic!("expected NotFound, got {other:?}"),
            }
        });
    }

    #[test]
    fn refresh_plugin_returns_not_found_for_missing_plugin() {
        with_temp_root(|_| {
            let err = refresh_plugin_impl("nonexistent").expect_err("should fail");
            match err {
                PluginCommandError::NotFound(name) => assert_eq!(name, "nonexistent"),
                other => panic!("expected NotFound, got {other:?}"),
            }
        });
    }

    #[test]
    fn uninstall_plugin_with_unsafe_name_returns_unsafe_name() {
        with_temp_root(|_| {
            let err = uninstall_plugin_impl("..").expect_err("should fail");
            match err {
                PluginCommandError::UnsafeName(n) => assert_eq!(n, ".."),
                other => panic!("expected UnsafeName, got {other:?}"),
            }
        });
    }

    #[test]
    fn install_plugin_with_unsafe_name_returns_unsafe_name() {
        with_temp_root(|root| {
            let bad_repo = root.join("repo");
            std::fs::create_dir_all(&bad_repo).unwrap();
            let manifest_dir = bad_repo.join(".launchpad-plugin");
            std::fs::create_dir_all(&manifest_dir).unwrap();
            std::fs::write(
                manifest_dir.join("plugin.json"),
                br#"{"name": "../escape", "version": "0.1.0"}"#,
            )
            .unwrap();

            let err = install_plugin_impl(
                SourceDto::LocalPath(bad_repo.to_string_lossy().into_owned()),
                None,
                true,
            )
            .expect_err("should fail");
            match err {
                PluginCommandError::UnsafeName(n) => assert_eq!(n, "../escape"),
                other => panic!("expected UnsafeName, got {other:?}"),
            }
        });
    }

    #[test]
    fn install_plugin_source_missing_returns_source_missing() {
        with_temp_root(|root| {
            let missing = root.join("does-not-exist");
            let err = install_plugin_impl(
                SourceDto::LocalPath(missing.to_string_lossy().into_owned()),
                None,
                true,
            )
            .expect_err("should fail");
            match err {
                PluginCommandError::SourceMissing(p) => {
                    assert!(p.ends_with("does-not-exist"), "got {p:?}")
                }
                other => panic!("expected SourceMissing, got {other:?}"),
            }
        });
    }

    #[test]
    fn install_plugin_no_manifest_no_content_returns_nothing_to_install() {
        with_temp_root(|root| {
            let empty_repo = root.join("empty-repo");
            std::fs::create_dir_all(&empty_repo).unwrap();
            // Add a README to prove root-level .md files don't count as
            // auto-discoverable rules (regression guard).
            std::fs::write(empty_repo.join("README.md"), b"# nope").unwrap();

            let err = install_plugin_impl(
                SourceDto::LocalPath(empty_repo.to_string_lossy().into_owned()),
                None,
                true,
            )
            .expect_err("should fail");
            match err {
                PluginCommandError::NothingToInstall => {}
                other => panic!("expected NothingToInstall, got {other:?}"),
            }
        });
    }

    #[test]
    fn install_plugin_no_manifest_returns_manifest_missing_without_auto_discovery() {
        // Mirrors the backend retry path: the UI first calls install with
        // `allow_auto_discovery = false`, expects `ManifestMissing`, prompts
        // the user, then retries with `true` which succeeds.
        with_temp_root(|root| {
            let repo = root.join("repo");
            let skills_dir = repo.join("skills").join("tdd");
            std::fs::create_dir_all(&skills_dir).unwrap();
            std::fs::write(skills_dir.join("SKILL.md"), b"# tdd").unwrap();
            // No manifest on purpose.

            let err = install_plugin_impl(
                SourceDto::LocalPath(repo.to_string_lossy().into_owned()),
                None,
                false,
            )
            .expect_err("should surface ManifestMissing");
            assert!(matches!(err, PluginCommandError::ManifestMissing));

            // Confirm the retry-with-auto-discovery path still works.
            let outcome = install_plugin_impl(
                SourceDto::LocalPath(repo.to_string_lossy().into_owned()),
                None,
                true,
            )
            .expect("retry with auto-discovery should succeed");
            assert_eq!(outcome.manifest_location, ManifestLocationKind::AutoDiscovered);
        });
    }

    #[test]
    fn install_then_list_plugins_round_trips() {
        with_temp_root(|root| {
            let repo = root.join("repo");
            let skills_dir = repo.join("skills").join("greet");
            std::fs::create_dir_all(&skills_dir).unwrap();
            std::fs::write(
                skills_dir.join("SKILL.md"),
                b"---\ntitle: Greet\n---\nhello",
            )
            .unwrap();
            let manifest_dir = repo.join(".launchpad-plugin");
            std::fs::create_dir_all(&manifest_dir).unwrap();
            std::fs::write(
                manifest_dir.join("plugin.json"),
                br#"{"name": "greeter", "version": "0.1.0", "skills": "skills"}"#,
            )
            .unwrap();

            let outcome = install_plugin_impl(
                SourceDto::LocalPath(repo.to_string_lossy().into_owned()),
                None,
                true,
            )
            .expect("install");
            assert_eq!(outcome.name, "greeter");
            assert_eq!(outcome.skills_installed, 1);

            let listed = list_plugins_impl().expect("list");
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].name, "greeter");
            assert_eq!(listed[0].version, "0.1.0");
            assert!(listed[0].auto_update_enabled);
        });
    }

    #[test]
    fn set_plugin_auto_update_toggles_field_in_registry() {
        with_temp_root(|root| {
            let repo = root.join("repo");
            let skills_dir = repo.join("skills").join("greet");
            std::fs::create_dir_all(&skills_dir).unwrap();
            std::fs::write(skills_dir.join("SKILL.md"), b"hi").unwrap();
            let manifest_dir = repo.join(".launchpad-plugin");
            std::fs::create_dir_all(&manifest_dir).unwrap();
            std::fs::write(
                manifest_dir.join("plugin.json"),
                br#"{"name": "greeter", "version": "0.1.0", "skills": "skills"}"#,
            )
            .unwrap();

            install_plugin_impl(
                SourceDto::LocalPath(repo.to_string_lossy().into_owned()),
                None,
                true,
            )
            .expect("install");

            let updated =
                set_plugin_auto_update_impl("greeter", false).expect("toggle");
            assert!(!updated.auto_update_enabled);

            // Verify on-disk state.
            let listed = list_plugins_impl().expect("list");
            assert_eq!(listed.len(), 1);
            assert!(!listed[0].auto_update_enabled);

            // Toggle back on.
            let updated = set_plugin_auto_update_impl("greeter", true).expect("toggle");
            assert!(updated.auto_update_enabled);
        });
    }

    #[test]
    fn install_outcome_dto_serializes_with_string_path() {
        // Guard the wire format: PathBuf must be flattened to a String for
        // the frontend, never serialized as `{ "path": "...", "components": ... }`
        // or anything platform-dependent.
        let dto = InstallOutcomeDto {
            name: "p".into(),
            version: "0.1.0".into(),
            plugin_dir: "/tmp/p".into(),
            manifest_location: ManifestLocationKind::LaunchpadNative,
            skills_installed: 0,
            rules_installed: 0,
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["plugin_dir"], "/tmp/p");
        assert_eq!(json["manifest_location"], "launchpad_native");
    }

    #[test]
    fn plugin_command_error_serializes_as_tagged_enum() {
        let err = PluginCommandError::Conflict("foo".into());
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["type"], "conflict");
        assert_eq!(json["detail"], "foo");

        let err = PluginCommandError::NetworkError {
            url: "https://example.com".into(),
            detail: "boom".into(),
        };
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["type"], "network_error");
        assert_eq!(json["detail"]["url"], "https://example.com");
        assert_eq!(json["detail"]["detail"], "boom");

        let err = PluginCommandError::NothingToInstall;
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["type"], "nothing_to_install");
        assert!(json.get("detail").is_none() || json["detail"].is_null());

        // ManifestMissing is the retry handshake signal — the UI
        // discriminates on this `type` tag to decide whether to prompt for
        // auto-discovery, so the wire format must stay stable.
        let err = PluginCommandError::ManifestMissing;
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["type"], "manifest_missing");
        assert!(json.get("detail").is_none() || json["detail"].is_null());
    }
}
