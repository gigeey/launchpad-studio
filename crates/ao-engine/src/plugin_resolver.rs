//! Locate a plugin's manifest inside a cloned/extracted repo root.
//!
//! Resolution walks these locations in order:
//!   1. `.launchpad-plugin/plugin.json` (Launchpad-native, preferred)
//!   2. user-provided override path (relative to `repo_root`)
//!   3. `.claude-plugin/plugin.json` (third-party plugin compatibility)
//!
//! The matched location is returned as a typed [`ManifestLocation`] so the UI
//! and telemetry can distinguish which convention a plugin uses. When no
//! manifest is found, [`ResolveError::ManifestMissing`] is returned and the
//! caller decides whether to trigger auto-discovery.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Which manifest location produced the resolved path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestLocation {
    /// `<repo>/.launchpad-plugin/plugin.json`.
    LaunchpadNative(PathBuf),
    /// User-specified override path (relative to `repo_root`).
    Override(PathBuf),
    /// `<repo>/.claude-plugin/plugin.json`.
    ClaudeCode(PathBuf),
}

impl ManifestLocation {
    /// Absolute path to the manifest file on disk.
    pub fn path(&self) -> &Path {
        match self {
            ManifestLocation::LaunchpadNative(p)
            | ManifestLocation::Override(p)
            | ManifestLocation::ClaudeCode(p) => p.as_path(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error(
        "plugin manifest: none found (checked .launchpad-plugin/plugin.json, override, .claude-plugin/plugin.json)"
    )]
    ManifestMissing,
}

/// Locate a plugin manifest inside `repo_root` by priority.
///
/// `override_path` is a caller-supplied relative path (e.g. `"manifest.json"`
/// or `"plugins/foo/plugin.json"`) that is checked only between the
/// Launchpad-native and third-party-compatible defaults — per product spec, the
/// convention-default `.launchpad-plugin/plugin.json` still wins.
pub fn resolve_manifest(
    repo_root: &Path,
    override_path: Option<&str>,
) -> Result<ManifestLocation, ResolveError> {
    let native = repo_root.join(".launchpad-plugin").join("plugin.json");
    if native.is_file() {
        return Ok(ManifestLocation::LaunchpadNative(native));
    }

    if let Some(rel) = override_path {
        let candidate = repo_root.join(rel);
        if candidate.is_file() {
            return Ok(ManifestLocation::Override(candidate));
        }
    }

    let claude = repo_root.join(".claude-plugin").join("plugin.json");
    if claude.is_file() {
        return Ok(ManifestLocation::ClaudeCode(claude));
    }

    Err(ResolveError::ManifestMissing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_file(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn prefers_launchpad_native_over_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join(".launchpad-plugin/plugin.json"), "{}");
        write_file(&root.join(".claude-plugin/plugin.json"), "{}");
        write_file(&root.join("custom.json"), "{}");

        let got = resolve_manifest(root, Some("custom.json")).expect("should resolve");
        match got {
            ManifestLocation::LaunchpadNative(p) => {
                assert_eq!(p, root.join(".launchpad-plugin/plugin.json"));
            }
            other => panic!("expected LaunchpadNative, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_override_when_native_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("custom/manifest.json"), "{}");
        write_file(&root.join(".claude-plugin/plugin.json"), "{}");

        let got =
            resolve_manifest(root, Some("custom/manifest.json")).expect("should resolve override");
        match got {
            ManifestLocation::Override(p) => assert_eq!(p, root.join("custom/manifest.json")),
            other => panic!("expected Override, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_claude_code_when_no_native_and_no_override() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join(".claude-plugin/plugin.json"), "{}");

        let got = resolve_manifest(root, None).expect("should resolve claude-code");
        match got {
            ManifestLocation::ClaudeCode(p) => {
                assert_eq!(p, root.join(".claude-plugin/plugin.json"))
            }
            other => panic!("expected ClaudeCode, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_claude_code_when_override_points_to_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join(".claude-plugin/plugin.json"), "{}");

        let got = resolve_manifest(root, Some("does-not-exist.json"))
            .expect("missing override should fall through to claude-code");
        assert!(matches!(got, ManifestLocation::ClaudeCode(_)));
    }

    #[test]
    fn returns_manifest_missing_when_nothing_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_manifest(tmp.path(), None).expect_err("should be missing");
        assert!(matches!(err, ResolveError::ManifestMissing));
    }

    #[test]
    fn returns_manifest_missing_when_override_also_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_manifest(tmp.path(), Some("nope.json")).expect_err("should be missing");
        assert!(matches!(err, ResolveError::ManifestMissing));
    }

    #[test]
    fn path_accessor_returns_inner_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join(".launchpad-plugin/plugin.json"), "{}");
        let loc = resolve_manifest(root, None).unwrap();
        assert_eq!(loc.path(), root.join(".launchpad-plugin/plugin.json"));
    }

    #[test]
    fn ignores_directory_at_manifest_location() {
        // If `.launchpad-plugin/plugin.json` is a directory (not a file), we
        // should NOT match it — fall through to the next candidate.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".launchpad-plugin/plugin.json")).unwrap();
        write_file(&root.join(".claude-plugin/plugin.json"), "{}");

        let got = resolve_manifest(root, None).expect("should fall through to claude-code");
        assert!(matches!(got, ManifestLocation::ClaudeCode(_)));
    }
}
