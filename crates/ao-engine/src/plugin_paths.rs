//! Canonical on-disk paths for the global plugin store.
//!
//! All plugin install/update/uninstall code should resolve locations through
//! these helpers so reads and writes agree on a single layout:
//!
//! ```text
//! <data-root>/plugins/
//! ├── registry.json         // plugins_registry_path()
//! └── <plugin-name>/        // plugin_dir("<plugin-name>")
//! ```
//!
//! `<data-root>` is resolved via [`DataRoot::resolve`], which honors the
//! `LAUNCHPAD_STUDIO_DATA_DIR` env var and defaults to `~/.launchpad_studio/`.

use std::path::PathBuf;

use ao_persistence::paths::DataRoot;
use ao_protocol::error::AoError;

/// Root directory for all installed plugins: `<data-root>/plugins/`.
///
/// Creates the directory if missing. Idempotent.
pub fn plugins_root() -> Result<PathBuf, AoError> {
    let data_root = DataRoot::resolve()?;
    let path = data_root.root().join("plugins");
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

/// Directory for a specific plugin: `<plugins-root>/<name>/`.
///
/// Creates the directory (and the plugins root) if missing. Idempotent.
pub fn plugin_dir(name: &str) -> Result<PathBuf, AoError> {
    let dir = plugins_root()?.join(name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Path to the plugin registry JSON file: `<plugins-root>/registry.json`.
///
/// Ensures the parent directory exists; does not create the file itself.
pub fn plugins_registry_path() -> Result<PathBuf, AoError> {
    Ok(plugins_root()?.join("registry.json"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    // Crate-wide serialization lock for tests that mutate the process-global
    // LAUNCHPAD_STUDIO_DATA_DIR env var. Every test in `ao-engine` that sets
    // this var MUST hold this single lock for its full duration — the lib
    // tests compile into one binary that runs in parallel, so any test that
    // mutates the var under a *different* mutex can stomp a sibling's temp
    // root mid-run. This is the one and only env lock; do not introduce
    // per-module mutexes for the same var.
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn with_temp_root<F: FnOnce(&std::path::Path)>(f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        f(tmp.path());
        std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");
    }

    #[test]
    fn plugins_root_composes_under_data_root_and_creates_it() {
        with_temp_root(|root| {
            let got = plugins_root().expect("plugins_root");
            assert_eq!(got, root.join("plugins"));
            assert!(got.is_dir(), "plugins root should be created");
        });
    }

    #[test]
    fn plugin_dir_composes_under_plugins_root_and_creates_it() {
        with_temp_root(|root| {
            let got = plugin_dir("superpowers").expect("plugin_dir");
            assert_eq!(got, root.join("plugins").join("superpowers"));
            assert!(got.is_dir(), "plugin dir should be created");
        });
    }

    #[test]
    fn plugins_registry_path_composes_under_plugins_root() {
        with_temp_root(|root| {
            let got = plugins_registry_path().expect("plugins_registry_path");
            assert_eq!(got, root.join("plugins").join("registry.json"));
            // Parent exists; file itself does not.
            assert!(got.parent().unwrap().is_dir(), "parent should exist");
            assert!(!got.exists(), "registry file should not be created");
        });
    }

    #[test]
    fn helpers_are_idempotent() {
        with_temp_root(|_root| {
            let a = plugins_root().expect("first call");
            let b = plugins_root().expect("second call");
            assert_eq!(a, b);

            let c = plugin_dir("x").expect("first plugin_dir");
            let d = plugin_dir("x").expect("second plugin_dir");
            assert_eq!(c, d);
            assert!(c.is_dir());
        });
    }
}
