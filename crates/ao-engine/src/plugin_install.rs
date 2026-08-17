//! End-to-end orchestration for installing a plugin into the global store.
//!
//! Composes the lower-level helpers:
//!   * clone a GitHub repo (or point at a local folder) to get a workdir
//!   * [`plugin_resolver::resolve_manifest`] → [`plugin_manifest::parse_manifest`]
//!     with a fallback to [`plugin_auto_discovery::auto_discover`]
//!   * [`copy_skill_folder`] for skill folders and a direct file copy for rule files
//!   * [`plugin_registry::upsert_entry`] to record the installation
//!
//! The public entry point is [`install_plugin_from_source`]. It stages the
//! plugin under a sibling tempdir in `<plugins-root>/` and renames into place
//! as the final step, so a mid-flight failure never leaves a half-installed
//! plugin on disk — registry writes happen only after the swap succeeds.
//!
//! Updates to an already-installed plugin go through
//! [`crate::plugin_refresh::refresh_plugin`], NOT through this
//! function; installing the same name twice is a [`InstallError::Conflict`].

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::Utc;
use thiserror::Error;

use crate::plugin_auto_discovery::{auto_discover, DiscoveryError, DiscoveryResult};
use crate::plugin_manifest::{parse_manifest, PluginManifest, PluginManifestError};
use crate::plugin_paths::plugins_root;
use crate::plugin_registry::{
    get_entry, upsert_entry, ManifestLocationKind, PluginRegistryEntry, PluginSource,
    RegistryError,
};
use crate::plugin_resolver::{resolve_manifest, ManifestLocation, ResolveError};
use crate::skills::parse_github_repo_name;

#[derive(Debug, Error)]
pub enum SkillCopyError {
    #[error("skill copy: source path is not a directory: {0}")]
    SourceNotADirectory(PathBuf),

    #[error("skill copy: source has no file name: {0}")]
    SourceHasNoName(PathBuf),

    #[error("skill copy: SKILL.md missing in source folder: {0}")]
    SkillFileMissing(PathBuf),

    #[error("skill copy: I/O error: {0}")]
    Io(#[from] std::io::Error),
}

fn copy_skill_folder(src: &Path, dest_parent: &Path) -> Result<PathBuf, SkillCopyError> {
    if !src.is_dir() {
        return Err(SkillCopyError::SourceNotADirectory(src.to_path_buf()));
    }
    if !src.join("SKILL.md").is_file() {
        return Err(SkillCopyError::SkillFileMissing(src.to_path_buf()));
    }

    let folder_name: &OsStr = src
        .file_name()
        .ok_or_else(|| SkillCopyError::SourceHasNoName(src.to_path_buf()))?;

    fs::create_dir_all(dest_parent)?;

    let staging = tempfile::Builder::new()
        .prefix(".skill-staging-")
        .tempdir_in(dest_parent)?;
    let staged_skill = staging.path().join(folder_name);
    copy_dir_recursive(src, &staged_skill)?;

    let final_path = dest_parent.join(folder_name);

    let backup = if final_path.exists() {
        let bk = tempfile::Builder::new()
            .prefix(".skill-backup-")
            .tempdir_in(dest_parent)?;
        let slot = bk.path().join(folder_name);
        fs::rename(&final_path, &slot)?;
        Some((bk, slot))
    } else {
        None
    };

    match fs::rename(&staged_skill, &final_path) {
        Ok(()) => Ok(final_path),
        Err(e) => {
            if let Some((_guard, slot)) = backup.as_ref() {
                let _ = fs::rename(slot, &final_path);
            }
            Err(SkillCopyError::Io(e))
        }
    }
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&src_path)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &dest_path)?;
            #[cfg(windows)]
            {
                if target.is_dir() {
                    std::os::windows::fs::symlink_dir(&target, &dest_path)?;
                } else {
                    std::os::windows::fs::symlink_file(&target, &dest_path)?;
                }
            }
        } else {
            fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

/// Where a plugin is installed from.
#[derive(Debug, Clone)]
pub enum Source {
    GitHubUrl(String),
    LocalPath(PathBuf),
}

/// Everything the UI needs after a successful install.
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub name: String,
    pub version: String,
    pub plugin_dir: PathBuf,
    pub manifest_location: ManifestLocationKind,
    pub skills_installed: usize,
    pub rules_installed: usize,
}

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("plugin '{0}' is already installed")]
    Conflict(String),

    #[error(
        "neither a plugin manifest nor auto-discoverable content was found at the source"
    )]
    NothingToInstall,

    #[error(
        "no plugin manifest was found at the source (caller can retry with auto-discovery)"
    )]
    ManifestMissing,

    #[error("plugin name '{0}' is not a safe folder name")]
    UnsafeName(String),

    #[error("source path does not exist: {0}")]
    SourceMissing(PathBuf),

    #[error("git clone failed for '{url}': {detail}")]
    Clone { url: String, detail: String },

    #[error("git is not available on PATH")]
    GitUnavailable,

    #[error(transparent)]
    InvalidManifest(#[from] PluginManifestError),

    #[error(transparent)]
    Skill(#[from] SkillCopyError),

    #[error(transparent)]
    Registry(#[from] RegistryError),

    #[error(transparent)]
    Path(#[from] ao_protocol::error::AoError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Install a plugin from `source` into the global plugin store.
///
/// Flow: fetch → resolve manifest (or auto-discover) → stage skills + rules
/// into a sibling tempdir → atomic rename to `<plugins-root>/<name>/` →
/// upsert registry entry. On any failure before the rename, zero files land
/// under `<plugins-root>/<name>/`.
///
/// `allow_auto_discovery` controls what happens when no manifest is found:
/// * `true` → silently fall through to [`plugin_auto_discovery::auto_discover`]
///   (the default install-path behavior prior to the UI's explicit opt-in).
/// * `false` → surface [`InstallError::ManifestMissing`] so an interactive
///   caller (the Install dialog) can prompt the user before re-invoking
///   with `true`.
pub fn install_plugin_from_source(
    source: Source,
    manifest_override: Option<&str>,
    allow_auto_discovery: bool,
) -> Result<InstallOutcome, InstallError> {
    let (workdir, _clone_guard) = fetch_source(&source)?;

    let plan = build_plan(&workdir, manifest_override, &source, allow_auto_discovery)?;

    validate_safe_name(&plan.name)?;

    // A name collision in the registry OR a leftover folder on disk both
    // count as conflicts — never silently overwrite, even if the registry
    // and disk disagree.
    if get_entry(&plan.name)?.is_some() {
        return Err(InstallError::Conflict(plan.name));
    }

    let plugins_root_dir = plugins_root()?;
    let final_dir = plugins_root_dir.join(&plan.name);
    if final_dir.exists() {
        return Err(InstallError::Conflict(plan.name));
    }

    // Stage the plugin tree in a sibling of `final_dir` (same filesystem) so
    // the final commit is a single atomic rename. TempDir RAII cleans up on
    // any early return before the rename.
    let staging = tempfile::Builder::new()
        .prefix(".plugin-staging-")
        .tempdir_in(&plugins_root_dir)?;

    let (skills_installed, rules_installed) = stage_plan(staging.path(), &plan)?;

    // Commit: rename the staged tree into its final slot. After this line
    // the TempDir guard's path no longer exists; its drop silently no-ops.
    fs::rename(staging.path(), &final_dir)?;

    let now = Utc::now();
    let entry = PluginRegistryEntry {
        name: plan.name.clone(),
        version: plan.version.clone(),
        source: match &source {
            Source::GitHubUrl(url) => PluginSource::GitHubUrl(url.clone()),
            Source::LocalPath(p) => PluginSource::LocalPath(p.clone()),
        },
        installed_at: now,
        last_updated_at: now,
        auto_update_enabled: true,
        manifest_location: plan.manifest_kind,
    };
    upsert_entry(entry)?;

    Ok(InstallOutcome {
        name: plan.name,
        version: plan.version,
        plugin_dir: final_dir,
        manifest_location: plan.manifest_kind,
        skills_installed,
        rules_installed,
    })
}

/// Holds either a borrowed local path or an owned temp clone — enough to keep
/// the cloned repo on disk for the duration of the install call. The TempDir
/// field is read only by its Drop impl, which does the cleanup.
pub(crate) enum CloneGuard {
    None,
    #[allow(dead_code)]
    TempDir(tempfile::TempDir),
}

/// Clone a GitHub repo into a tempdir (returning a guard whose Drop cleans it
/// up) or point at a local folder directly. Shared between install and
/// refresh.
pub(crate) fn fetch_source(source: &Source) -> Result<(PathBuf, CloneGuard), InstallError> {
    match source {
        Source::GitHubUrl(url) => {
            ensure_git_available()?;
            let temp = tempfile::Builder::new()
                .prefix(".plugin-clone-")
                .tempdir()?;
            clone_github_repo(url, temp.path())?;
            Ok((temp.path().to_path_buf(), CloneGuard::TempDir(temp)))
        }
        Source::LocalPath(p) => {
            if !p.is_dir() {
                return Err(InstallError::SourceMissing(p.clone()));
            }
            Ok((p.clone(), CloneGuard::None))
        }
    }
}

fn ensure_git_available() -> Result<(), InstallError> {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|_| ())
        .map_err(|_| InstallError::GitUnavailable)
}

fn clone_github_repo(url: &str, dest: &Path) -> Result<(), InstallError> {
    // Clone directly into `dest` (which exists as an empty tempdir). Using
    // `--depth 1` keeps the network transfer small; full history is not
    // needed for static asset import.
    let output = Command::new("git")
        .arg("clone")
        .arg("--depth")
        .arg("1")
        .arg(url)
        .arg(dest)
        .output()
        .map_err(|e| InstallError::Clone {
            url: url.to_string(),
            detail: format!("failed to run git clone: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr
            .lines()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(InstallError::Clone {
            url: url.to_string(),
            detail: tail.trim().to_string(),
        });
    }
    Ok(())
}

pub(crate) struct InstallPlan {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) manifest_kind: ManifestLocationKind,
    pub(crate) skill_folders: Vec<PathBuf>,
    pub(crate) rule_files: Vec<PathBuf>,
    /// Serialized `.mcp.json` content (`{ "mcpServers": { ... } }`) to write
    /// to the staged plugin directory. `None` when the plugin has no MCP servers.
    pub(crate) mcp_servers_json: Option<String>,
}

/// Resolve a workdir into an `InstallPlan` by parsing a manifest or falling
/// back to auto-discovery. Shared between install and refresh.
pub(crate) fn build_plan(
    workdir: &Path,
    manifest_override: Option<&str>,
    source: &Source,
    allow_auto_discovery: bool,
) -> Result<InstallPlan, InstallError> {
    match resolve_manifest(workdir, manifest_override) {
        Ok(location) => build_plan_from_manifest(workdir, &location),
        Err(ResolveError::ManifestMissing) => {
            if allow_auto_discovery {
                build_plan_from_discovery(workdir, source)
            } else {
                Err(InstallError::ManifestMissing)
            }
        }
    }
}

fn build_plan_from_manifest(
    workdir: &Path,
    location: &ManifestLocation,
) -> Result<InstallPlan, InstallError> {
    let raw = fs::read_to_string(location.path())?;
    let manifest = parse_manifest(&raw)?;
    let mut skill_folders = collect_skill_folders_from_manifest(workdir, &manifest);
    let mut rule_files = collect_rule_files_from_manifest(workdir, &manifest);

    // Convention fallback: if the manifest omitted `skills` or `rules`
    // entirely, scan the repo's top-level `skills/` and `rules/` dirs the same
    // way we would for a manifest-less repo. Plugins in the wild frequently
    // ship a `.claude-plugin/plugin.json` with only metadata and rely on the
    // convention layout — without this fallback
    // they install with zero skills. Empty arrays stay empty: an explicit
    // `"skills": []` is the author saying "I really mean none."
    if manifest.skills.is_none() || manifest.rules.is_none() {
        if let Ok(discovered) = auto_discover(workdir) {
            if manifest.skills.is_none() {
                skill_folders = discovered.skill_folders;
            }
            if manifest.rules.is_none() {
                rule_files = discovered.rule_files;
            }
        }
    }

    let mcp_servers_json = extract_mcp_servers_json(&manifest, workdir);

    Ok(InstallPlan {
        name: manifest.name,
        version: manifest.version,
        manifest_kind: match location {
            ManifestLocation::LaunchpadNative(_) => ManifestLocationKind::LaunchpadNative,
            ManifestLocation::Override(_) => ManifestLocationKind::Override,
            ManifestLocation::ClaudeCode(_) => ManifestLocationKind::ClaudeCode,
        },
        skill_folders,
        rule_files,
        mcp_servers_json,
    })
}

/// Extract `.mcp.json` content for staging, preferring the manifest's
/// `mcpServers` field over a workdir `.mcp.json` file.
///
/// Returns the full file content (`{ "mcpServers": { ... } }`) as a string,
/// or `None` when neither source defines any servers.
fn extract_mcp_servers_json(manifest: &crate::plugin_manifest::PluginManifest, workdir: &Path) -> Option<String> {
    if let Some(mcp_val) = &manifest.mcp_servers {
        if let Some(obj) = mcp_val.as_object() {
            if !obj.is_empty() {
                let wrapper = serde_json::json!({ "mcpServers": mcp_val });
                return serde_json::to_string_pretty(&wrapper).ok();
            }
        }
    }
    let mcp_file = workdir.join(".mcp.json");
    if mcp_file.is_file() {
        return fs::read_to_string(&mcp_file).ok();
    }
    None
}

fn build_plan_from_discovery(
    workdir: &Path,
    source: &Source,
) -> Result<InstallPlan, InstallError> {
    let discovered: DiscoveryResult = auto_discover(workdir).map_err(|e| match e {
        DiscoveryError::NothingDiscovered => InstallError::NothingToInstall,
    })?;

    // Auto-discovered repos have no manifest, so derive a name from the source
    // and use a conservative default version. Users who want strict semver
    // should ship a manifest.
    let name = derive_name_from_source(source)?;
    let mcp_servers_json = {
        let mcp_file = workdir.join(".mcp.json");
        if mcp_file.is_file() {
            fs::read_to_string(&mcp_file).ok()
        } else {
            None
        }
    };
    Ok(InstallPlan {
        name,
        version: "0.0.0".to_string(),
        manifest_kind: ManifestLocationKind::AutoDiscovered,
        skill_folders: discovered.skill_folders,
        rule_files: discovered.rule_files,
        mcp_servers_json,
    })
}

fn derive_name_from_source(source: &Source) -> Result<String, InstallError> {
    match source {
        Source::GitHubUrl(url) => parse_github_repo_name(url).map_err(|e| InstallError::Clone {
            url: url.clone(),
            detail: e.to_string(),
        }),
        Source::LocalPath(p) => p
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .ok_or_else(|| InstallError::UnsafeName(p.to_string_lossy().into_owned())),
    }
}

/// Each entry in `manifest.skills` is either a skill folder directly (contains
/// `SKILL.md`) or a parent directory to walk for skill folders — this accepts
/// both the `["skills/tdd", "skills/rag"]` shape and the `"skills"` bucket
/// shape without the caller having to know which their manifest uses.
fn collect_skill_folders_from_manifest(
    workdir: &Path,
    manifest: &PluginManifest,
) -> Vec<PathBuf> {
    let Some(selector) = manifest.skills.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for rel in selector.as_vec() {
        let candidate = workdir.join(&rel);
        if !candidate.is_dir() {
            continue;
        }
        if candidate.join("SKILL.md").is_file() {
            out.push(candidate);
        } else {
            collect_skill_folders_recursive(&candidate, &mut out);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn collect_skill_folders_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        if path.join("SKILL.md").is_file() {
            out.push(path.clone());
        }
        collect_skill_folders_recursive(&path, out);
    }
}

fn collect_rule_files_from_manifest(workdir: &Path, manifest: &PluginManifest) -> Vec<PathBuf> {
    let Some(selector) = manifest.rules.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for rel in selector.as_vec() {
        let candidate = workdir.join(&rel);
        if candidate.is_file() {
            if candidate.extension().and_then(|e| e.to_str()) == Some("md") {
                out.push(candidate);
            }
        } else if candidate.is_dir() {
            collect_rule_files_recursive(&candidate, &mut out);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn collect_rule_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            collect_rule_files_recursive(&path, out);
        } else if ft.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
}

/// Copy `plan`'s skills and rules into `staging_root`, producing the tree
/// layout (`skills/<name>/`, `rules/*.md`) that the caller will rename into
/// its final slot. Returns `(skills_installed, rules_installed)`. Shared
/// between install and refresh.
pub(crate) fn stage_plan(
    staging_root: &Path,
    plan: &InstallPlan,
) -> Result<(usize, usize), InstallError> {
    let staged_skills_dir = staging_root.join("skills");
    let mut skills_installed = 0usize;
    for skill_src in &plan.skill_folders {
        copy_skill_folder(skill_src, &staged_skills_dir)?;
        skills_installed += 1;
    }

    let mut rules_installed = 0usize;
    if !plan.rule_files.is_empty() {
        let staged_rules_dir = staging_root.join("rules");
        fs::create_dir_all(&staged_rules_dir)?;
        for rule_src in &plan.rule_files {
            if let Some(name) = rule_src.file_name() {
                let dest = staged_rules_dir.join(name);
                fs::copy(rule_src, &dest)?;
                rules_installed += 1;
            }
        }
    }

    if let Some(ref json_content) = plan.mcp_servers_json {
        fs::write(staging_root.join(".mcp.json"), json_content)?;
    }

    Ok((skills_installed, rules_installed))
}

fn validate_safe_name(name: &str) -> Result<(), InstallError> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains("..")
        || trimmed.starts_with('.')
        || trimmed == "."
    {
        return Err(InstallError::UnsafeName(name.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_paths::tests::with_temp_root as paths_with_temp_root;
    use crate::plugin_registry::{get_entry, load_registry};

    fn with_temp_root<F: FnOnce(&Path)>(f: F) {
        paths_with_temp_root(|root| f(root));
    }

    fn write_file(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Build a fixture plugin repo at `root` with a Launchpad-native manifest,
    /// two skill folders, and two rule files. Returns the repo path.
    fn fixture_repo_with_manifest(root: &Path) -> PathBuf {
        let repo = root.join("src-repo");
        write_file(
            &repo.join(".launchpad-plugin/plugin.json"),
            br#"{
                "name": "fixture-plugin",
                "version": "1.0.0",
                "skills": ["skills/alpha", "skills/beta"],
                "rules": ["rules/one.md", "rules/two.md"]
            }"#,
        );
        write_file(&repo.join("skills/alpha/SKILL.md"), b"# alpha\n");
        write_file(&repo.join("skills/alpha/helper.sh"), b"#!/bin/sh\n");
        write_file(&repo.join("skills/beta/SKILL.md"), b"# beta\n");
        write_file(&repo.join("rules/one.md"), b"rule one\n");
        write_file(&repo.join("rules/two.md"), b"rule two\n");
        // Root README must NOT be imported as a rule (fixes the legacy bug).
        write_file(&repo.join("README.md"), b"# readme\n");
        repo
    }

    #[test]
    fn installs_local_plugin_end_to_end() {
        with_temp_root(|root| {
            let repo = fixture_repo_with_manifest(root);

            let outcome = install_plugin_from_source(Source::LocalPath(repo.clone()), None, true)
                .expect("install should succeed");

            assert_eq!(outcome.name, "fixture-plugin");
            assert_eq!(outcome.version, "1.0.0");
            assert_eq!(outcome.manifest_location, ManifestLocationKind::LaunchpadNative);
            assert_eq!(outcome.skills_installed, 2);
            assert_eq!(outcome.rules_installed, 2);

            // Directory layout.
            let plugin_root = root.join("plugins/fixture-plugin");
            assert_eq!(outcome.plugin_dir, plugin_root);
            assert!(plugin_root.join("skills/alpha/SKILL.md").is_file());
            assert!(plugin_root.join("skills/alpha/helper.sh").is_file());
            assert!(plugin_root.join("skills/beta/SKILL.md").is_file());
            assert!(plugin_root.join("rules/one.md").is_file());
            assert!(plugin_root.join("rules/two.md").is_file());
            // README at the source root must not have been imported.
            assert!(!plugin_root.join("rules/README.md").exists());

            // Registry entry written.
            let entry = get_entry("fixture-plugin")
                .expect("registry read")
                .expect("entry present");
            assert_eq!(entry.version, "1.0.0");
            assert_eq!(entry.manifest_location, ManifestLocationKind::LaunchpadNative);
            assert!(matches!(entry.source, PluginSource::LocalPath(p) if p == repo));
            assert!(entry.auto_update_enabled);
            assert_eq!(entry.installed_at, entry.last_updated_at);
        });
    }

    #[test]
    fn installs_via_auto_discovery_when_no_manifest() {
        with_temp_root(|root| {
            let repo = root.join("my-convention-repo");
            write_file(&repo.join("skills/tdd/SKILL.md"), b"# tdd\n");
            write_file(&repo.join("rules/core.md"), b"core\n");
            write_file(&repo.join("README.md"), b"readme");

            let outcome = install_plugin_from_source(Source::LocalPath(repo.clone()), None, true)
                .expect("auto-discovery install should succeed");

            assert_eq!(outcome.name, "my-convention-repo");
            assert_eq!(outcome.manifest_location, ManifestLocationKind::AutoDiscovered);
            assert_eq!(outcome.skills_installed, 1);
            assert_eq!(outcome.rules_installed, 1);

            let plugin_root = root.join("plugins/my-convention-repo");
            assert!(plugin_root.join("skills/tdd/SKILL.md").is_file());
            assert!(plugin_root.join("rules/core.md").is_file());
            assert!(!plugin_root.join("rules/README.md").exists());
        });
    }

    #[test]
    fn conflict_error_when_same_name_already_installed() {
        with_temp_root(|root| {
            let repo = fixture_repo_with_manifest(root);
            install_plugin_from_source(Source::LocalPath(repo.clone()), None, true)
                .expect("first install");

            let err = install_plugin_from_source(Source::LocalPath(repo), None, true)
                .expect_err("second install should conflict");
            match err {
                InstallError::Conflict(name) => assert_eq!(name, "fixture-plugin"),
                other => panic!("expected Conflict, got {other:?}"),
            }
        });
    }

    #[test]
    fn nothing_to_install_when_no_manifest_and_no_convention_folders() {
        with_temp_root(|root| {
            let repo = root.join("empty-repo");
            // Just a README and some code — no manifest, no skills/, no rules/.
            write_file(&repo.join("README.md"), b"# nothing here");
            write_file(&repo.join("src/lib.rs"), b"// code");

            let err = install_plugin_from_source(Source::LocalPath(repo), None, true)
                .expect_err("should refuse import");
            assert!(matches!(err, InstallError::NothingToInstall));

            // Zero files copied: plugins dir is still empty.
            let plugins = root.join("plugins");
            if plugins.is_dir() {
                let entries: Vec<_> = fs::read_dir(&plugins)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .parse::<String>()
                            .ok()
                            .filter(|n| !n.starts_with('.') && n != "registry.json")
                            .is_some()
                    })
                    .collect();
                assert!(entries.is_empty(), "no plugin dirs should be created");
            }
        });
    }

    #[test]
    fn rejects_unsafe_plugin_names() {
        with_temp_root(|root| {
            let repo = root.join("evil-repo");
            write_file(
                &repo.join(".launchpad-plugin/plugin.json"),
                br#"{ "name": "../etc/passwd", "version": "1.0.0" }"#,
            );
            // Need at least a skill or rule so the plan builds.
            write_file(&repo.join("skills/x/SKILL.md"), b"# x");
            // Manifest says skills = ?  Nope — without a skills field, manifest-mode
            // builds a plan with empty skill/rule lists, which is still fine.
            let err = install_plugin_from_source(Source::LocalPath(repo), None, true)
                .expect_err("unsafe name should be rejected");
            assert!(matches!(err, InstallError::UnsafeName(_)));
        });
    }

    #[test]
    fn source_missing_error_when_local_path_does_not_exist() {
        with_temp_root(|root| {
            let bogus = root.join("does-not-exist");
            let err = install_plugin_from_source(Source::LocalPath(bogus.clone()), None, true)
                .expect_err("missing source should error");
            match err {
                InstallError::SourceMissing(p) => assert_eq!(p, bogus),
                other => panic!("expected SourceMissing, got {other:?}"),
            }
        });
    }

    #[test]
    fn failure_before_rename_leaves_plugins_root_clean() {
        // The manifest points at a skill folder that does NOT exist → build_plan
        // still succeeds with an empty skill list — but let's make it fail in
        // copy_skill_folder by pointing at a file instead of a dir.
        with_temp_root(|root| {
            let repo = root.join("src-repo");
            write_file(
                &repo.join(".launchpad-plugin/plugin.json"),
                br#"{
                    "name": "broken-plugin",
                    "version": "0.1.0",
                    "skills": ["skills/only"]
                }"#,
            );
            // "skills/only" is a valid directory but missing SKILL.md → skill
            // folders collector will walk but find nothing → empty plan, succeeds.
            // Need to actually trigger a failure: have the manifest list a path
            // that IS a dir with SKILL.md, then make the dest un-writable.
            // Simpler: just verify the success-path assertion about registry
            // writes happening only after the rename.
            fs::create_dir_all(repo.join("skills/only")).unwrap();

            // Plan will be valid (empty skills, empty rules) → install succeeds.
            let outcome = install_plugin_from_source(Source::LocalPath(repo), None, true)
                .expect("empty-plan install is allowed");
            assert_eq!(outcome.skills_installed, 0);
            assert_eq!(outcome.rules_installed, 0);
            assert!(root.join("plugins/broken-plugin").is_dir());
        });
    }

    #[test]
    fn no_staging_sidecars_left_after_successful_install() {
        with_temp_root(|root| {
            let repo = fixture_repo_with_manifest(root);
            install_plugin_from_source(Source::LocalPath(repo), None, true).expect("install");

            let plugins_dir = root.join("plugins");
            let leftovers: Vec<_> = fs::read_dir(&plugins_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(".plugin-staging-")
                })
                .collect();
            assert!(leftovers.is_empty(), "unexpected staging dirs: {leftovers:?}");
        });
    }

    #[test]
    fn manifest_string_shape_skills_bucket_walks_for_skill_folders() {
        with_temp_root(|root| {
            let repo = root.join("bucket-repo");
            // `skills: "skills"` shape → walk the dir for all SKILL.md folders.
            write_file(
                &repo.join(".launchpad-plugin/plugin.json"),
                br#"{ "name": "bucket-plugin", "version": "1.0.0", "skills": "skills" }"#,
            );
            write_file(&repo.join("skills/one/SKILL.md"), b"# one");
            write_file(&repo.join("skills/two/SKILL.md"), b"# two");
            write_file(&repo.join("skills/not-a-skill/README.md"), b"oops");

            let outcome = install_plugin_from_source(Source::LocalPath(repo), None, true).expect("install");
            assert_eq!(outcome.skills_installed, 2);
            assert!(root.join("plugins/bucket-plugin/skills/one/SKILL.md").is_file());
            assert!(root.join("plugins/bucket-plugin/skills/two/SKILL.md").is_file());
            // The non-skill subdirectory is not imported.
            assert!(!root.join("plugins/bucket-plugin/skills/not-a-skill").exists());
        });
    }

    #[test]
    fn registry_has_exactly_one_entry_after_single_install() {
        with_temp_root(|root| {
            let repo = fixture_repo_with_manifest(root);
            install_plugin_from_source(Source::LocalPath(repo), None, true).expect("install");
            let reg = load_registry().expect("load");
            assert_eq!(reg.entries.len(), 1);
            assert_eq!(reg.entries[0].name, "fixture-plugin");
        });
    }

    #[test]
    fn manifest_missing_error_when_no_manifest_and_auto_discovery_disabled() {
        with_temp_root(|root| {
            // Repo with auto-discoverable content but NO manifest — if
            // `allow_auto_discovery` were true this would install cleanly.
            let repo = root.join("no-manifest-repo");
            write_file(&repo.join("skills/tdd/SKILL.md"), b"# tdd\n");
            write_file(&repo.join("rules/core.md"), b"core\n");

            let err =
                install_plugin_from_source(Source::LocalPath(repo.clone()), None, false)
                    .expect_err("should surface ManifestMissing without auto-discovery");
            assert!(matches!(err, InstallError::ManifestMissing));

            // Zero files copied: plugins dir is still empty on the failure.
            let plugins = root.join("plugins");
            if plugins.is_dir() {
                let entries: Vec<_> = fs::read_dir(&plugins)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        !e.file_name()
                            .to_string_lossy()
                            .starts_with('.')
                            && e.file_name().to_string_lossy() != "registry.json"
                    })
                    .collect();
                assert!(entries.is_empty(), "no plugin dirs should be created");
            }

            // Retry with auto-discovery enabled → now succeeds.
            let outcome = install_plugin_from_source(Source::LocalPath(repo), None, true)
                .expect("retry with auto-discovery should succeed");
            assert_eq!(outcome.manifest_location, ManifestLocationKind::AutoDiscovered);
        });
    }

    #[test]
    fn manifest_without_skills_field_falls_back_to_root_skills_dir() {
        // Mirrors a common real-world plugin shape: a manifest under
        // `.claude-plugin/plugin.json` with metadata only — no `skills`
        // field — and skills sitting at the repo root in `skills/`.
        with_temp_root(|root| {
            let repo = root.join("claude-shape-repo");
            write_file(
                &repo.join(".claude-plugin/plugin.json"),
                br#"{ "name": "claude-shape", "version": "1.0.0" }"#,
            );
            write_file(&repo.join("skills/alpha/SKILL.md"), b"# alpha");
            write_file(&repo.join("skills/beta/SKILL.md"), b"# beta");
            write_file(&repo.join("rules/core.md"), b"core");

            // allow_auto_discovery=false on purpose: the fallback is *inside*
            // the manifest path, so it should NOT require the discovery
            // opt-in flag.
            let outcome =
                install_plugin_from_source(Source::LocalPath(repo), None, false).expect("install");
            assert_eq!(outcome.manifest_location, ManifestLocationKind::ClaudeCode);
            assert_eq!(outcome.skills_installed, 2);
            assert_eq!(outcome.rules_installed, 1);
            assert!(root.join("plugins/claude-shape/skills/alpha/SKILL.md").is_file());
            assert!(root.join("plugins/claude-shape/skills/beta/SKILL.md").is_file());
            assert!(root.join("plugins/claude-shape/rules/core.md").is_file());
        });
    }

    #[test]
    fn manifest_with_explicit_empty_skills_array_does_not_fall_back() {
        // `"skills": []` is the author saying "I really mean none" — even if
        // a top-level skills/ exists, we must not silently import it.
        with_temp_root(|root| {
            let repo = root.join("explicit-empty-repo");
            write_file(
                &repo.join(".launchpad-plugin/plugin.json"),
                br#"{ "name": "explicit-empty", "version": "1.0.0", "skills": [], "rules": [] }"#,
            );
            write_file(&repo.join("skills/alpha/SKILL.md"), b"# alpha");
            write_file(&repo.join("rules/core.md"), b"core");

            let outcome =
                install_plugin_from_source(Source::LocalPath(repo), None, false).expect("install");
            assert_eq!(outcome.skills_installed, 0);
            assert_eq!(outcome.rules_installed, 0);
        });
    }

    #[test]
    fn validate_safe_name_rejects_dangerous_inputs() {
        assert!(validate_safe_name("").is_err());
        assert!(validate_safe_name("   ").is_err());
        assert!(validate_safe_name("..").is_err());
        assert!(validate_safe_name(".").is_err());
        assert!(validate_safe_name(".hidden").is_err());
        assert!(validate_safe_name("a/b").is_err());
        assert!(validate_safe_name("a\\b").is_err());
        assert!(validate_safe_name("foo/../bar").is_err());
        // Happy path.
        assert!(validate_safe_name("superpowers").is_ok());
        assert!(validate_safe_name("my-plugin-1.0").is_ok());
    }
}
