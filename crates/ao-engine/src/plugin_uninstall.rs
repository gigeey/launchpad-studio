//! Uninstall a plugin from the global plugin store.
//!
//! Removes both the plugin's directory under `<plugins-root>/<name>/` and its
//! entry in the on-disk registry. Idempotent: a missing directory or missing
//! registry entry is treated as success.
//!
//! Per-agent configuration is NOT touched by this function. Agent-side cleanup
//! happens lazily via enablement filtering — enabled plugin entries
//! that no longer resolve to an installed plugin simply contribute nothing
//! during context assembly.

use std::fs;

use thiserror::Error;

use crate::plugin_paths::plugins_root;
use crate::plugin_registry::{remove_entry, RegistryError};

#[derive(Debug, Error)]
pub enum UninstallError {
    #[error("plugin name '{0}' is not a safe folder name")]
    UnsafeName(String),

    #[error(transparent)]
    Registry(#[from] RegistryError),

    #[error(transparent)]
    Path(#[from] ao_protocol::error::AoError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// What happened during uninstall. All variants are success; the caller can
/// use the bools for UI messaging ("already gone" vs "removed cleanly").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UninstallOutcome {
    pub directory_removed: bool,
    pub registry_entry_removed: bool,
}

/// Delete `<plugins-root>/<name>/` and drop the registry entry for `name`.
///
/// Idempotent: succeeds cleanly when either the directory or the registry
/// entry (or both) is already missing.
pub fn uninstall_plugin(name: &str) -> Result<UninstallOutcome, UninstallError> {
    validate_safe_name(name)?;

    let plugin_dir = plugins_root()?.join(name);
    let directory_removed = if plugin_dir.exists() {
        fs::remove_dir_all(&plugin_dir)?;
        true
    } else {
        false
    };

    let registry_entry_removed = remove_entry(name)?;

    Ok(UninstallOutcome {
        directory_removed,
        registry_entry_removed,
    })
}

/// Reject names that could escape `<plugins-root>/` or target hidden state.
/// Mirrors the check in `plugin_install::validate_safe_name` so install and
/// uninstall agree on what counts as a legal plugin name.
fn validate_safe_name(name: &str) -> Result<(), UninstallError> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains("..")
        || trimmed.starts_with('.')
        || trimmed == "."
    {
        return Err(UninstallError::UnsafeName(name.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_paths::tests::with_temp_root as paths_with_temp_root;
    use crate::plugin_registry::{
        get_entry, load_registry, upsert_entry, ManifestLocationKind, PluginRegistryEntry,
        PluginSource,
    };
    use chrono::Utc;
    use std::path::Path;

    fn with_temp_root<F: FnOnce(&Path)>(f: F) {
        paths_with_temp_root(|root| f(root));
    }

    fn install_fixture(name: &str) {
        // Create a populated plugin directory and a matching registry entry
        // the same way a real install would have left the store.
        let plugins = plugins_root().expect("plugins_root");
        let dir = plugins.join(name);
        fs::create_dir_all(dir.join("skills/alpha")).unwrap();
        fs::write(dir.join("skills/alpha/SKILL.md"), b"# alpha\n").unwrap();
        fs::create_dir_all(dir.join("rules")).unwrap();
        fs::write(dir.join("rules/core.md"), b"core\n").unwrap();

        let now = Utc::now();
        upsert_entry(PluginRegistryEntry {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            source: PluginSource::LocalPath("/tmp/unused".into()),
            installed_at: now,
            last_updated_at: now,
            auto_update_enabled: true,
            manifest_location: ManifestLocationKind::LaunchpadNative,
        })
        .expect("upsert");
    }

    #[test]
    fn uninstall_removes_directory_and_registry_entry() {
        with_temp_root(|root| {
            install_fixture("superpowers");
            let plugin_dir = root.join("plugins/superpowers");
            assert!(plugin_dir.is_dir(), "precondition: plugin dir exists");
            assert!(
                get_entry("superpowers").unwrap().is_some(),
                "precondition: registry entry exists"
            );

            let outcome = uninstall_plugin("superpowers").expect("uninstall");
            assert!(outcome.directory_removed);
            assert!(outcome.registry_entry_removed);

            assert!(!plugin_dir.exists(), "plugin dir should be gone");
            assert!(
                get_entry("superpowers").unwrap().is_none(),
                "registry entry should be gone"
            );
        });
    }

    #[test]
    fn uninstall_is_idempotent_when_nothing_is_installed() {
        with_temp_root(|_root| {
            let outcome = uninstall_plugin("never-installed").expect("uninstall");
            assert!(!outcome.directory_removed);
            assert!(!outcome.registry_entry_removed);
        });
    }

    #[test]
    fn uninstall_when_only_directory_exists_still_cleans_up() {
        with_temp_root(|root| {
            // Directory present, registry empty — a prior install crashed
            // between rename and registry write, for example.
            let plugin_dir = root.join("plugins/orphan");
            fs::create_dir_all(&plugin_dir).unwrap();
            fs::write(plugin_dir.join("marker"), b"x").unwrap();

            let outcome = uninstall_plugin("orphan").expect("uninstall");
            assert!(outcome.directory_removed);
            assert!(!outcome.registry_entry_removed);
            assert!(!plugin_dir.exists());
        });
    }

    #[test]
    fn uninstall_when_only_registry_entry_exists_still_cleans_up() {
        with_temp_root(|_root| {
            // Registry carries a stale entry — a user deleted the folder by
            // hand. Uninstall should still succeed and purge the entry.
            let now = Utc::now();
            upsert_entry(PluginRegistryEntry {
                name: "stale".to_string(),
                version: "0.1.0".to_string(),
                source: PluginSource::LocalPath("/tmp/unused".into()),
                installed_at: now,
                last_updated_at: now,
                auto_update_enabled: true,
                manifest_location: ManifestLocationKind::LaunchpadNative,
            })
            .expect("upsert");

            let outcome = uninstall_plugin("stale").expect("uninstall");
            assert!(!outcome.directory_removed);
            assert!(outcome.registry_entry_removed);
            assert!(get_entry("stale").unwrap().is_none());
        });
    }

    #[test]
    fn uninstall_does_not_touch_sibling_plugins() {
        with_temp_root(|root| {
            install_fixture("keep-me");
            install_fixture("drop-me");

            uninstall_plugin("drop-me").expect("uninstall");

            assert!(root.join("plugins/keep-me/skills/alpha/SKILL.md").is_file());
            assert!(get_entry("keep-me").unwrap().is_some());

            assert!(!root.join("plugins/drop-me").exists());
            assert!(get_entry("drop-me").unwrap().is_none());

            let reg = load_registry().expect("load");
            assert_eq!(reg.entries.len(), 1);
            assert_eq!(reg.entries[0].name, "keep-me");
        });
    }

    #[test]
    fn uninstall_rejects_unsafe_names() {
        with_temp_root(|_root| {
            assert!(matches!(
                uninstall_plugin(""),
                Err(UninstallError::UnsafeName(_))
            ));
            assert!(matches!(
                uninstall_plugin(".."),
                Err(UninstallError::UnsafeName(_))
            ));
            assert!(matches!(
                uninstall_plugin("a/b"),
                Err(UninstallError::UnsafeName(_))
            ));
            assert!(matches!(
                uninstall_plugin(".hidden"),
                Err(UninstallError::UnsafeName(_))
            ));
        });
    }
}
