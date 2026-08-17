//! Refresh installed plugins from their original source.
//!
//! Powers the Collections UI's Refresh button and the daily auto-update
//! background task. Re-fetches a plugin's source, stages the new tree in a
//! sibling tempdir under `<plugins-root>/`, moves the live directory aside
//! into a backup tempdir, renames the staged tree into place, and updates
//! the registry entry's `last_updated_at` + version. On any failure before
//! the rename, the backup is renamed back — the existing plugin stays on
//! disk and the registry is unchanged, so the next auto-update tick will
//! retry.
//!
//! See [`refresh_plugin`] for the single-plugin entry point and
//! [`auto_update_tick`] for the iterate-all-stale-plugins driver.
//!
//! ## Interaction with the shared plugin cache
//!
//! Callers that care about in-memory state (context assembly via
//! [`crate::plugin_cache::PluginCache`]) should call `plugin_cache.refresh()`
//! after a successful refresh so subsequent message turns see the new
//! content. This module does not touch the cache directly — it owns the
//! on-disk store only.
//!
//! ## Relationship to install
//!
//! [`refresh_plugin`] reuses [`plugin_install::fetch_source`],
//! [`plugin_install::build_plan`], and [`plugin_install::stage_plan`] so the
//! fetch/manifest-resolve/stage pipeline is identical to install. The only
//! differences are (a) refresh swaps instead of rejects on existing
//! `<plugins-root>/<name>/` and (b) refresh preserves `installed_at` and
//! `auto_update_enabled` from the registry entry.

use std::fs;
use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use thiserror::Error;

use crate::plugin_install::{
    build_plan, fetch_source, stage_plan, CloneGuard, InstallError, InstallPlan, Source,
};
use crate::plugin_paths::plugins_root;
use crate::plugin_registry::{
    get_entry, load_registry, upsert_entry, ManifestLocationKind, PluginRegistryEntry,
    PluginSource, RegistryError,
};

/// How long since `last_updated_at` before a plugin is considered stale and
/// eligible for auto-refresh. Per PRD: 24 hours.
const AUTO_UPDATE_THRESHOLD_HOURS: i64 = 24;

fn auto_update_threshold() -> Duration {
    Duration::hours(AUTO_UPDATE_THRESHOLD_HOURS)
}

/// Pure predicate: is this entry due for an auto-refresh `now`?
///
/// A plugin is stale when (a) `auto_update_enabled` is true AND (b) at least
/// [`auto_update_threshold`] has elapsed since `last_updated_at`. Split out as
/// a free function so it can be unit-tested without touching the filesystem.
pub fn is_stale(entry: &PluginRegistryEntry, now: DateTime<Utc>) -> bool {
    entry.auto_update_enabled && (now - entry.last_updated_at) >= auto_update_threshold()
}

/// What [`refresh_plugin`] reports back on success.
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub name: String,
    pub version: String,
    pub plugin_dir: PathBuf,
    pub manifest_location: ManifestLocationKind,
    pub skills_installed: usize,
    pub rules_installed: usize,
    pub last_updated_at: DateTime<Utc>,
}

/// Summary returned by [`auto_update_tick`].
#[derive(Debug, Default, Clone)]
pub struct TickOutcome {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: Vec<String>,
}

#[derive(Debug, Error)]
pub enum RefreshError {
    #[error("plugin '{0}' is not installed")]
    NotFound(String),

    /// Wraps the full install pipeline's error set (fetch / manifest /
    /// discover / stage failures). The on-disk plugin remains unchanged when
    /// a refresh returns this — see the module docs for why.
    #[error(transparent)]
    Install(#[from] InstallError),

    #[error(transparent)]
    Registry(#[from] RegistryError),

    #[error(transparent)]
    Path(#[from] ao_protocol::error::AoError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Re-fetch `name` from its registered source, replace the on-disk folder
/// atomically, and bump `last_updated_at`. Preserves `installed_at` and
/// `auto_update_enabled` from the existing entry.
///
/// On any failure before the rename (clone error, missing source, stage
/// error, etc.) the existing plugin directory and registry entry stay
/// untouched — callers can assume a failed refresh is a pure no-op from the
/// user's point of view, modulo the logged error.
pub fn refresh_plugin(name: &str) -> Result<RefreshOutcome, RefreshError> {
    let existing = get_entry(name)?.ok_or_else(|| RefreshError::NotFound(name.to_string()))?;

    // Rebuild a Source from the stored registry entry. Refresh does NOT
    // accept a new source — that's uninstall+install territory.
    let source = match &existing.source {
        PluginSource::GitHubUrl(url) => Source::GitHubUrl(url.clone()),
        PluginSource::LocalPath(path) => Source::LocalPath(path.clone()),
    };

    // Fetch + build plan exactly like install. Any failure here means the
    // existing plugin is untouched (nothing has been written yet).
    let (workdir, _clone_guard): (PathBuf, CloneGuard) = fetch_source(&source)?;
    // Refresh runs unattended (including from the 24h auto-update tick), so
    // it always allows auto-discovery — there is no UI to prompt on
    // `ManifestMissing`. That error path is an install-only concern.
    let plan: InstallPlan = build_plan(&workdir, None, &source, true)?;

    // Defensive guard: a manifest could have changed its `name` field since
    // install. Refresh targets a specific folder by name, so a rename would
    // leave the old folder orphaned and install content under the new name
    // silently. Refuse that — uninstall + install is the explicit path.
    if plan.name != existing.name {
        return Err(RefreshError::Install(InstallError::Conflict(format!(
            "source manifest name '{}' does not match installed plugin '{}'",
            plan.name, existing.name
        ))));
    }

    let plugins_root_dir = plugins_root()?;
    let final_dir = plugins_root_dir.join(&existing.name);

    // Stage new content into a sibling tempdir (same filesystem as the final
    // slot, so the rename is a single atomic syscall).
    let staging = tempfile::Builder::new()
        .prefix(".plugin-refresh-staging-")
        .tempdir_in(&plugins_root_dir)?;
    let (skills_installed, rules_installed) = stage_plan(staging.path(), &plan)?;

    // Move the existing plugin dir into a sibling backup tempdir. If the
    // final rename fails, move the backup back. On success, the backup dir
    // (with old content) is cleaned up by TempDir's Drop.
    let backup = if final_dir.exists() {
        let dir = tempfile::Builder::new()
            .prefix(".plugin-refresh-backup-")
            .tempdir_in(&plugins_root_dir)?;
        let stash = dir.path().join(&existing.name);
        fs::rename(&final_dir, &stash)?;
        Some((dir, stash))
    } else {
        None
    };

    if let Err(err) = fs::rename(staging.path(), &final_dir) {
        // Commit failed — restore the backup so the user keeps their prior
        // install. If the restore ALSO fails we surface the original error;
        // a second-order failure here is the rare "filesystem is busted"
        // case, and leaking a backup dir is preferable to claiming success.
        if let Some((_dir, stash)) = &backup {
            let _ = fs::rename(stash, &final_dir);
        }
        return Err(err.into());
    }

    // Rename succeeded — the backup TempDir (now holding the old content)
    // drops at end of scope and `remove_dir_all`'s the stale tree.
    drop(backup);

    let now = Utc::now();
    let updated_entry = PluginRegistryEntry {
        name: existing.name.clone(),
        version: plan.version.clone(),
        // Preserve the original source exactly — refresh never migrates the
        // user's installation to a different URL/path.
        source: existing.source.clone(),
        installed_at: existing.installed_at,
        last_updated_at: now,
        auto_update_enabled: existing.auto_update_enabled,
        manifest_location: plan.manifest_kind,
    };
    upsert_entry(updated_entry)?;

    Ok(RefreshOutcome {
        name: existing.name,
        version: plan.version,
        plugin_dir: final_dir,
        manifest_location: plan.manifest_kind,
        skills_installed,
        rules_installed,
        last_updated_at: now,
    })
}

/// Iterate the registry and refresh every stale plugin. Per-plugin failures
/// are logged via `tracing::warn!` and collected into the returned outcome —
/// one bad plugin never aborts the tick.
///
/// Synchronous; call inside `spawn_blocking` from async contexts (see
/// [`auto_update_tick_async`]).
pub fn auto_update_tick() -> Result<TickOutcome, RefreshError> {
    let registry = load_registry()?;
    let now = Utc::now();
    let mut outcome = TickOutcome::default();

    for entry in &registry.entries {
        if !is_stale(entry, now) {
            continue;
        }
        outcome.attempted += 1;
        match refresh_plugin(&entry.name) {
            Ok(_) => outcome.succeeded += 1,
            Err(err) => {
                tracing::warn!(
                    plugin = %entry.name,
                    error = %err,
                    "plugin auto-update refresh failed; existing install preserved",
                );
                outcome.failed.push(entry.name.clone());
            }
        }
    }

    Ok(outcome)
}

/// Async wrapper around [`auto_update_tick`] suitable for spawning from
/// tokio runtimes (e.g. app startup).
pub async fn auto_update_tick_async() -> Result<TickOutcome, RefreshError> {
    tokio::task::spawn_blocking(auto_update_tick)
        .await
        .map_err(|e| {
            RefreshError::Io(std::io::Error::other(format!(
                "plugin auto-update tick panicked: {e}"
            )))
        })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_paths::tests::with_temp_root as paths_with_temp_root;
    use crate::plugin_registry::{
        get_entry, upsert_entry, ManifestLocationKind, PluginRegistryEntry, PluginSource,
    };
    use std::path::{Path, PathBuf};

    fn with_temp_root<F: FnOnce(&Path)>(f: F) {
        paths_with_temp_root(|root| f(root));
    }

    fn write_file(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn base_entry(name: &str, last_updated_at: DateTime<Utc>) -> PluginRegistryEntry {
        let installed = last_updated_at - Duration::days(7);
        PluginRegistryEntry {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            source: PluginSource::LocalPath(PathBuf::from("/tmp/unused")),
            installed_at: installed,
            last_updated_at,
            auto_update_enabled: true,
            manifest_location: ManifestLocationKind::LaunchpadNative,
        }
    }

    // --- is_stale predicate tests. These don't touch the filesystem.

    #[test]
    fn is_stale_true_when_older_than_threshold_and_auto_update_on() {
        let now = Utc::now();
        let entry = base_entry("p", now - Duration::hours(25));
        assert!(is_stale(&entry, now));
    }

    #[test]
    fn is_stale_true_at_exactly_the_threshold_boundary() {
        // Uses `>=`: a plugin updated exactly 24h ago should refresh on
        // the next tick.
        let now = Utc::now();
        let entry = base_entry("p", now - Duration::hours(24));
        assert!(is_stale(&entry, now));
    }

    #[test]
    fn is_stale_false_when_younger_than_threshold() {
        let now = Utc::now();
        let entry = base_entry("p", now - Duration::hours(23));
        assert!(!is_stale(&entry, now));
    }

    #[test]
    fn is_stale_false_when_auto_update_disabled_regardless_of_age() {
        let now = Utc::now();
        let mut entry = base_entry("p", now - Duration::days(30));
        entry.auto_update_enabled = false;
        assert!(!is_stale(&entry, now));
    }

    #[test]
    fn is_stale_false_when_last_updated_at_is_in_the_future() {
        // A clock skew between installer and tick shouldn't trigger a
        // refresh — `now - last_updated_at` is negative, less than 24h.
        let now = Utc::now();
        let entry = base_entry("p", now + Duration::hours(1));
        assert!(!is_stale(&entry, now));
    }

    // --- refresh_plugin tests

    /// Create a fully installed "local" plugin with the given source path +
    /// known initial rule content. Returns (registry_entry, source_repo).
    fn seed_installed_plugin(
        root: &Path,
        name: &str,
        source_repo_name: &str,
        initial_rule_body: &str,
    ) -> (PluginRegistryEntry, PathBuf) {
        let source_repo = root.join(source_repo_name);
        write_file(
            &source_repo.join(".launchpad-plugin/plugin.json"),
            format!(
                r#"{{ "name": "{name}", "version": "1.0.0", "skills": ["skills/one"], "rules": ["rules/core.md"] }}"#
            )
            .as_bytes(),
        );
        write_file(&source_repo.join("skills/one/SKILL.md"), b"# one original\n");
        write_file(&source_repo.join("rules/core.md"), initial_rule_body.as_bytes());

        // Pretend-install: populate <plugins-root>/<name>/ with the rule body
        // and a skill, then write the registry entry.
        let plugin_dir = root.join(format!("plugins/{name}"));
        write_file(&plugin_dir.join("skills/one/SKILL.md"), b"# one original\n");
        write_file(&plugin_dir.join("rules/core.md"), initial_rule_body.as_bytes());

        // Set last_updated_at old enough that it would be stale.
        let old_time = Utc::now() - Duration::hours(48);
        let entry = PluginRegistryEntry {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            source: PluginSource::LocalPath(source_repo.clone()),
            installed_at: old_time,
            last_updated_at: old_time,
            auto_update_enabled: true,
            manifest_location: ManifestLocationKind::LaunchpadNative,
        };
        upsert_entry(entry.clone()).expect("upsert");
        (entry, source_repo)
    }

    #[test]
    fn refresh_plugin_replaces_content_and_bumps_last_updated_at() {
        with_temp_root(|root| {
            let (original_entry, source_repo) =
                seed_installed_plugin(root, "demo", "src-repo", "old rule");

            // Change the source: bump the manifest version and rewrite the
            // rule body. The refresh should pick both up.
            write_file(
                &source_repo.join(".launchpad-plugin/plugin.json"),
                br#"{ "name": "demo", "version": "2.0.0", "skills": ["skills/one"], "rules": ["rules/core.md"] }"#,
            );
            write_file(&source_repo.join("rules/core.md"), b"new rule body");
            write_file(&source_repo.join("skills/one/SKILL.md"), b"# one UPDATED\n");

            let before_refresh = Utc::now();
            let outcome = refresh_plugin("demo").expect("refresh");
            assert_eq!(outcome.name, "demo");
            assert_eq!(outcome.version, "2.0.0");
            assert_eq!(outcome.skills_installed, 1);
            assert_eq!(outcome.rules_installed, 1);

            // New content landed on disk.
            assert_eq!(
                fs::read_to_string(root.join("plugins/demo/rules/core.md")).unwrap(),
                "new rule body"
            );
            assert!(fs::read_to_string(root.join("plugins/demo/skills/one/SKILL.md"))
                .unwrap()
                .contains("UPDATED"));

            // Registry has bumped version + last_updated_at, preserved
            // installed_at and auto_update_enabled, preserved source.
            let entry = get_entry("demo").unwrap().unwrap();
            assert_eq!(entry.version, "2.0.0");
            assert!(entry.last_updated_at >= before_refresh);
            assert_ne!(entry.last_updated_at, original_entry.last_updated_at);
            assert_eq!(entry.installed_at, original_entry.installed_at);
            assert!(entry.auto_update_enabled);
            assert_eq!(entry.source, original_entry.source);
        });
    }

    #[test]
    fn refresh_plugin_not_found_error_when_name_not_in_registry() {
        with_temp_root(|_root| {
            let err = refresh_plugin("never-installed").expect_err("should error");
            match err {
                RefreshError::NotFound(name) => assert_eq!(name, "never-installed"),
                other => panic!("expected NotFound, got {other:?}"),
            }
        });
    }

    // --- Failure-preserves-existing path: a network/clone failure during
    // refresh leaves the existing plugin in place, logs an error, and leaves
    // last_updated_at unchanged so retry happens on the next tick.
    //
    // LocalPath that no longer exists is the synchronous analog of a failed
    // GitHub clone — fetch_source will return SourceMissing.

    #[test]
    fn refresh_plugin_failure_preserves_existing_install_and_registry() {
        with_temp_root(|root| {
            let (original_entry, source_repo) =
                seed_installed_plugin(root, "demo", "src-repo", "original body");

            // Blow away the source so fetch_source returns SourceMissing —
            // this is the failure path under test.
            fs::remove_dir_all(&source_repo).expect("remove source");

            let err = refresh_plugin("demo").expect_err("refresh should fail");
            assert!(
                matches!(err, RefreshError::Install(InstallError::SourceMissing(_))),
                "got {err:?}"
            );

            // Existing plugin untouched on disk.
            let body = fs::read_to_string(root.join("plugins/demo/rules/core.md")).unwrap();
            assert_eq!(body, "original body", "plugin content must be preserved");
            assert!(root.join("plugins/demo/skills/one/SKILL.md").is_file());

            // Registry untouched: version + last_updated_at unchanged so
            // the next tick will retry.
            let entry = get_entry("demo").unwrap().unwrap();
            assert_eq!(entry.version, original_entry.version);
            assert_eq!(entry.last_updated_at, original_entry.last_updated_at);
            assert_eq!(entry.installed_at, original_entry.installed_at);

            // No leftover staging/backup sidecars in plugins-root.
            let plugins_dir = root.join("plugins");
            let leftovers: Vec<_> = fs::read_dir(&plugins_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.starts_with(".plugin-refresh-staging-")
                        || n.starts_with(".plugin-refresh-backup-")
                })
                .collect();
            assert!(
                leftovers.is_empty(),
                "no staging/backup dirs should linger: {leftovers:?}"
            );
        });
    }

    #[test]
    fn refresh_plugin_rejects_when_manifest_name_changed() {
        with_temp_root(|root| {
            let (_original_entry, source_repo) =
                seed_installed_plugin(root, "demo", "src-repo", "body");

            // Rewrite the manifest to claim a different name — refresh must
            // refuse rather than silently copy under the wrong folder.
            write_file(
                &source_repo.join(".launchpad-plugin/plugin.json"),
                br#"{ "name": "imposter", "version": "2.0.0", "skills": ["skills/one"], "rules": ["rules/core.md"] }"#,
            );

            let err = refresh_plugin("demo").expect_err("should reject rename");
            match err {
                RefreshError::Install(InstallError::Conflict(msg)) => {
                    assert!(msg.contains("demo"));
                    assert!(msg.contains("imposter"));
                }
                other => panic!("expected Install(Conflict), got {other:?}"),
            }

            // Existing plugin untouched.
            let body = fs::read_to_string(root.join("plugins/demo/rules/core.md")).unwrap();
            assert_eq!(body, "body");
        });
    }

    #[test]
    fn refresh_plugin_no_leftover_sidecars_on_success() {
        with_temp_root(|root| {
            seed_installed_plugin(root, "demo", "src-repo", "body");
            refresh_plugin("demo").expect("refresh");

            let plugins_dir = root.join("plugins");
            let leftovers: Vec<_> = fs::read_dir(&plugins_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.starts_with(".plugin-refresh-staging-")
                        || n.starts_with(".plugin-refresh-backup-")
                })
                .collect();
            assert!(leftovers.is_empty(), "found leftovers: {leftovers:?}");
        });
    }

    // --- auto_update_tick tests

    #[test]
    fn auto_update_tick_refreshes_only_stale_enabled_plugins() {
        with_temp_root(|root| {
            // Stale + enabled → should refresh.
            seed_installed_plugin(root, "stale-on", "stale-on-src", "body1");

            // Stale + disabled → should NOT refresh.
            {
                let source_repo = root.join("stale-off-src");
                write_file(
                    &source_repo.join(".launchpad-plugin/plugin.json"),
                    br#"{ "name": "stale-off", "version": "1.0.0", "rules": ["rules/core.md"] }"#,
                );
                write_file(&source_repo.join("rules/core.md"), b"body2");
                let plugin_dir = root.join("plugins/stale-off");
                write_file(&plugin_dir.join("rules/core.md"), b"body2");
                let old = Utc::now() - Duration::hours(48);
                upsert_entry(PluginRegistryEntry {
                    name: "stale-off".to_string(),
                    version: "1.0.0".to_string(),
                    source: PluginSource::LocalPath(source_repo),
                    installed_at: old,
                    last_updated_at: old,
                    auto_update_enabled: false,
                    manifest_location: ManifestLocationKind::LaunchpadNative,
                })
                .expect("upsert");
            }

            // Fresh + enabled → should NOT refresh.
            {
                let source_repo = root.join("fresh-src");
                write_file(
                    &source_repo.join(".launchpad-plugin/plugin.json"),
                    br#"{ "name": "fresh", "version": "1.0.0", "rules": ["rules/core.md"] }"#,
                );
                write_file(&source_repo.join("rules/core.md"), b"body3");
                let plugin_dir = root.join("plugins/fresh");
                write_file(&plugin_dir.join("rules/core.md"), b"body3");
                let recent = Utc::now() - Duration::hours(1);
                upsert_entry(PluginRegistryEntry {
                    name: "fresh".to_string(),
                    version: "1.0.0".to_string(),
                    source: PluginSource::LocalPath(source_repo),
                    installed_at: recent,
                    last_updated_at: recent,
                    auto_update_enabled: true,
                    manifest_location: ManifestLocationKind::LaunchpadNative,
                })
                .expect("upsert");
            }

            let outcome = auto_update_tick().expect("tick");
            assert_eq!(outcome.attempted, 1);
            assert_eq!(outcome.succeeded, 1);
            assert!(outcome.failed.is_empty());

            // Only stale-on's last_updated_at moved.
            let stale_on = get_entry("stale-on").unwrap().unwrap();
            assert!(stale_on.last_updated_at > Utc::now() - Duration::minutes(1));

            let stale_off = get_entry("stale-off").unwrap().unwrap();
            assert!(stale_off.last_updated_at < Utc::now() - Duration::hours(24));

            let fresh = get_entry("fresh").unwrap().unwrap();
            assert!(fresh.last_updated_at > Utc::now() - Duration::hours(2));
            assert!(fresh.last_updated_at < Utc::now() - Duration::minutes(1));
        });
    }

    #[test]
    fn auto_update_tick_logs_and_continues_on_per_plugin_failure() {
        with_temp_root(|root| {
            // One stale plugin whose source has been deleted — refresh will
            // fail but tick should still complete and move on.
            let (original_entry, source_repo) =
                seed_installed_plugin(root, "broken", "broken-src", "preserved");
            fs::remove_dir_all(&source_repo).unwrap();

            // A second stale plugin whose source is still intact.
            seed_installed_plugin(root, "healthy", "healthy-src", "healthy body");

            let outcome = auto_update_tick().expect("tick");
            assert_eq!(outcome.attempted, 2);
            assert_eq!(outcome.succeeded, 1);
            assert_eq!(outcome.failed, vec!["broken".to_string()]);

            // Broken plugin: disk + registry preserved.
            assert_eq!(
                fs::read_to_string(root.join("plugins/broken/rules/core.md")).unwrap(),
                "preserved"
            );
            let still = get_entry("broken").unwrap().unwrap();
            assert_eq!(still.last_updated_at, original_entry.last_updated_at);

            // Healthy plugin: refreshed.
            let healthy = get_entry("healthy").unwrap().unwrap();
            assert!(healthy.last_updated_at > Utc::now() - Duration::minutes(1));
        });
    }

    #[test]
    fn auto_update_tick_is_noop_on_empty_registry() {
        with_temp_root(|_root| {
            let outcome = auto_update_tick().expect("tick");
            assert_eq!(outcome.attempted, 0);
            assert_eq!(outcome.succeeded, 0);
            assert!(outcome.failed.is_empty());
        });
    }
}
