//! On-disk plugin registry: `<plugins-root>/registry.json`.
//!
//! Tracks every globally-installed plugin with metadata the UI and the
//! auto-update scheduler need (source, versions, timestamps, per-plugin
//! auto-update opt-out, which manifest convention matched).
//!
//! Corrupted or missing files are treated as an empty registry — the ops
//! layer is trusted to rebuild state from the plugin directories on disk,
//! and a garbage registry file should not brick Collections.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::plugin_paths::plugins_registry_path;

/// Where a plugin was installed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum PluginSource {
    #[serde(rename = "github_url")]
    GitHubUrl(String),
    LocalPath(PathBuf),
}

/// Which convention resolved a plugin's manifest, or `AutoDiscovered` when
/// no manifest existed and the fallback located content by layout.
///
/// Stored as a kind tag (no path) since the concrete on-disk path is
/// redundant once the plugin has been copied into its canonical slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestLocationKind {
    LaunchpadNative,
    Override,
    ClaudeCode,
    AutoDiscovered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRegistryEntry {
    pub name: String,
    pub version: String,
    pub source: PluginSource,
    pub installed_at: DateTime<Utc>,
    pub last_updated_at: DateTime<Utc>,
    #[serde(default = "default_true")]
    pub auto_update_enabled: bool,
    pub manifest_location: ManifestLocationKind,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRegistry {
    #[serde(default)]
    pub entries: Vec<PluginRegistryEntry>,
}

impl PluginRegistry {
    pub fn get(&self, name: &str) -> Option<&PluginRegistryEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn upsert(&mut self, entry: PluginRegistryEntry) {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.name == entry.name) {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }

    /// Returns `true` when an entry with `name` was present and removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.name != name);
        before != self.entries.len()
    }
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("plugin registry: I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("plugin registry: path resolution failed: {0}")]
    Path(#[from] ao_protocol::error::AoError),
}

/// Load the registry. A missing file OR an unparseable one both produce an
/// empty registry (logged via `tracing::warn!` on corruption); this never
/// panics.
pub fn load_registry() -> Result<PluginRegistry, RegistryError> {
    let path = plugins_registry_path()?;
    if !path.is_file() {
        return Ok(PluginRegistry::default());
    }
    let bytes = std::fs::read(&path)?;
    match serde_json::from_slice::<PluginRegistry>(&bytes) {
        Ok(reg) => Ok(reg),
        Err(e) => {
            tracing::warn!(
                "plugin registry at {} is unreadable ({}); treating as empty",
                path.display(),
                e
            );
            Ok(PluginRegistry::default())
        }
    }
}

/// Atomically save the registry: write to a sibling tmp file, fsync, and
/// rename into place so a crash can never leave a half-written file.
pub fn save_registry(registry: &PluginRegistry) -> Result<(), RegistryError> {
    let path = plugins_registry_path()?;
    let json = serde_json::to_vec_pretty(registry).expect("PluginRegistry always serializes");
    let dir = path
        .parent()
        .expect("plugins_registry_path always has a parent");
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".registry-")
        .suffix(".json.tmp")
        .tempfile_in(dir)?;
    use std::io::Write;
    tmp.as_file_mut().write_all(&json)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(&path).map_err(|e| RegistryError::Io(e.error))?;
    Ok(())
}

/// Insert-or-update an entry by name.
pub fn upsert_entry(entry: PluginRegistryEntry) -> Result<(), RegistryError> {
    let mut registry = load_registry()?;
    registry.upsert(entry);
    save_registry(&registry)
}

/// Remove the entry with `name`. Returns `true` if one was removed.
pub fn remove_entry(name: &str) -> Result<bool, RegistryError> {
    let mut registry = load_registry()?;
    let removed = registry.remove(name);
    if removed {
        save_registry(&registry)?;
    }
    Ok(removed)
}

/// Look up a single entry by name.
pub fn get_entry(name: &str) -> Result<Option<PluginRegistryEntry>, RegistryError> {
    Ok(load_registry()?.get(name).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    // Reuse the shared env-var mutex from plugin_paths so tests across
    // these sibling modules serialize on the same lock.
    use crate::plugin_paths::tests::with_temp_root as paths_with_temp_root;

    fn with_temp_root<F: FnOnce()>(f: F) {
        paths_with_temp_root(|_| f());
    }

    fn fixture_entry(name: &str) -> PluginRegistryEntry {
        let t = Utc::now();
        PluginRegistryEntry {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            source: PluginSource::GitHubUrl(format!("https://github.com/example/{name}")),
            installed_at: t,
            last_updated_at: t,
            auto_update_enabled: true,
            manifest_location: ManifestLocationKind::LaunchpadNative,
        }
    }

    #[test]
    fn load_registry_returns_empty_when_file_missing() {
        with_temp_root(|| {
            let reg = load_registry().expect("load_registry");
            assert!(reg.entries.is_empty());
        });
    }

    #[test]
    fn upsert_entry_adds_new_entry() {
        with_temp_root(|| {
            upsert_entry(fixture_entry("superpowers")).expect("upsert");
            let reg = load_registry().expect("load");
            assert_eq!(reg.entries.len(), 1);
            assert_eq!(reg.entries[0].name, "superpowers");
        });
    }

    #[test]
    fn upsert_entry_replaces_existing_entry_by_name() {
        with_temp_root(|| {
            upsert_entry(fixture_entry("superpowers")).expect("first upsert");

            let mut updated = fixture_entry("superpowers");
            updated.version = "0.2.0".to_string();
            updated.auto_update_enabled = false;
            upsert_entry(updated).expect("second upsert");

            let reg = load_registry().expect("load");
            assert_eq!(reg.entries.len(), 1, "should replace, not append");
            assert_eq!(reg.entries[0].version, "0.2.0");
            assert!(!reg.entries[0].auto_update_enabled);
        });
    }

    #[test]
    fn upsert_entry_keeps_siblings_with_different_names() {
        with_temp_root(|| {
            upsert_entry(fixture_entry("superpowers")).expect("upsert a");
            upsert_entry(fixture_entry("karpathy")).expect("upsert b");
            let reg = load_registry().expect("load");
            assert_eq!(reg.entries.len(), 2);
        });
    }

    #[test]
    fn get_entry_returns_none_for_missing_name() {
        with_temp_root(|| {
            upsert_entry(fixture_entry("superpowers")).expect("upsert");
            assert!(get_entry("nope").expect("get_entry").is_none());
        });
    }

    #[test]
    fn get_entry_returns_cloned_entry_when_present() {
        with_temp_root(|| {
            upsert_entry(fixture_entry("superpowers")).expect("upsert");
            let got = get_entry("superpowers").expect("get_entry");
            assert!(got.is_some());
            assert_eq!(got.unwrap().name, "superpowers");
        });
    }

    #[test]
    fn remove_entry_returns_true_when_present() {
        with_temp_root(|| {
            upsert_entry(fixture_entry("superpowers")).expect("upsert");
            assert!(remove_entry("superpowers").expect("remove"));
            assert!(load_registry().expect("load").entries.is_empty());
        });
    }

    #[test]
    fn remove_entry_returns_false_when_missing() {
        with_temp_root(|| {
            assert!(!remove_entry("nope").expect("remove"));
        });
    }

    #[test]
    fn corrupted_registry_is_treated_as_empty() {
        with_temp_root(|| {
            let path = plugins_registry_path().expect("path");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"{ this is not valid json").unwrap();

            let reg = load_registry().expect("load_registry should not error on corruption");
            assert!(reg.entries.is_empty());
        });
    }

    #[test]
    fn save_then_load_roundtrips_all_fields() {
        with_temp_root(|| {
            let installed = DateTime::parse_from_rfc3339("2026-04-22T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc);
            let updated = DateTime::parse_from_rfc3339("2026-04-22T11:30:00Z")
                .unwrap()
                .with_timezone(&Utc);

            let entry = PluginRegistryEntry {
                name: "local-one".to_string(),
                version: "1.2.3".to_string(),
                source: PluginSource::LocalPath(PathBuf::from("/tmp/local-one")),
                installed_at: installed,
                last_updated_at: updated,
                auto_update_enabled: false,
                manifest_location: ManifestLocationKind::ClaudeCode,
            };

            let mut reg = PluginRegistry::default();
            reg.upsert(entry.clone());
            save_registry(&reg).expect("save");

            let loaded = load_registry().expect("load");
            assert_eq!(loaded.entries, vec![entry]);
        });
    }

    #[test]
    fn save_is_atomic_and_leaves_no_tmp_siblings() {
        with_temp_root(|| {
            upsert_entry(fixture_entry("superpowers")).expect("upsert");
            let dir = plugins_registry_path().unwrap();
            let dir = dir.parent().unwrap().to_path_buf();

            let leftovers: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(".registry-")
                })
                .collect();
            assert!(
                leftovers.is_empty(),
                "unexpected leftover tmp files: {leftovers:?}"
            );
        });
    }
}
