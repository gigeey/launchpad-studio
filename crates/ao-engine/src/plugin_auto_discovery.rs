//! Convention-based discovery for repos that omit a plugin manifest.
//!
//! Used as the fallback path when [`plugin_resolver::resolve_manifest`] returns
//! [`plugin_resolver::ResolveError::ManifestMissing`]. A repo qualifies for
//! auto-discovery if it has a top-level `skills/` or `rules/` directory; root
//! `.md` files are ignored so README/CHANGELOG don't get imported as rules.
//!
//! When neither directory exists the caller must surface
//! [`DiscoveryError::NothingDiscovered`] — we never do a silent wholesale
//! import of an arbitrary repo.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Directories we never descend into during discovery.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target"];

/// Skill folders and rule files discovered by convention.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiscoveryResult {
    /// Absolute paths of directories (under `<repo>/skills/`) that directly
    /// contain a `SKILL.md` file.
    pub skill_folders: Vec<PathBuf>,
    /// Absolute paths of `.md` rule files found under `<repo>/rules/`.
    pub rule_files: Vec<PathBuf>,
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error(
        "auto-discovery: repo has no top-level skills/ or rules/ directory (nothing to import)"
    )]
    NothingDiscovered,
}

/// Scan a repo root for convention-placed skills and rules.
///
/// Returns [`DiscoveryError::NothingDiscovered`] when neither `<repo>/skills/`
/// nor `<repo>/rules/` exists. An existing-but-empty directory returns success
/// with empty vectors — an explicit (if empty) import beats silent failure.
pub fn auto_discover(repo_root: &Path) -> Result<DiscoveryResult, DiscoveryError> {
    let skills_root = repo_root.join("skills");
    let rules_root = repo_root.join("rules");

    let skills_exists = skills_root.is_dir();
    let rules_exists = rules_root.is_dir();

    if !skills_exists && !rules_exists {
        return Err(DiscoveryError::NothingDiscovered);
    }

    let mut skill_folders = Vec::new();
    if skills_exists {
        collect_skill_folders(&skills_root, &mut skill_folders);
    }

    let mut rule_files = Vec::new();
    if rules_exists {
        collect_rule_files(&rules_root, &mut rule_files);
    }

    skill_folders.sort();
    rule_files.sort();

    Ok(DiscoveryResult {
        skill_folders,
        rule_files,
    })
}

fn is_skipped_dir_name(name: &std::ffi::OsStr) -> bool {
    let lossy = name.to_string_lossy();
    SKIP_DIRS.iter().any(|s| lossy == *s)
}

/// Recursively walk `dir`, pushing each subdirectory that directly contains a
/// `SKILL.md` file. Does NOT include `dir` itself — skill folders must live
/// under the top-level `skills/` directory, not be that directory.
fn collect_skill_folders(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        if is_skipped_dir_name(name) {
            continue;
        }
        if path.join("SKILL.md").is_file() {
            out.push(path.clone());
        }
        collect_skill_folders(&path, out);
    }
}

/// Recursively walk `dir`, pushing every `*.md` file found (case-sensitive
/// match on the extension `md`, matching how rules files are written today).
fn collect_rule_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let Some(name) = path.file_name() else {
                continue;
            };
            if is_skipped_dir_name(name) {
                continue;
            }
            collect_rule_files(&path, out);
        } else if file_type.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
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
    fn discovers_skill_folders_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("skills/alpha/SKILL.md"), "# a");
        write_file(&root.join("skills/beta/SKILL.md"), "# b");
        // A non-skill folder under skills/ (no SKILL.md) must be excluded.
        fs::create_dir_all(root.join("skills/gamma")).unwrap();
        write_file(&root.join("skills/gamma/notes.txt"), "");

        let got = auto_discover(root).expect("should discover");
        assert_eq!(
            got.skill_folders,
            vec![root.join("skills/alpha"), root.join("skills/beta")]
        );
        assert!(got.rule_files.is_empty());
    }

    #[test]
    fn discovers_rule_files_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("rules/one.md"), "r1");
        write_file(&root.join("rules/two.md"), "r2");
        // non-md files must be ignored
        write_file(&root.join("rules/notes.txt"), "ignored");

        let got = auto_discover(root).expect("should discover");
        assert_eq!(
            got.rule_files,
            vec![root.join("rules/one.md"), root.join("rules/two.md")]
        );
        assert!(got.skill_folders.is_empty());
    }

    #[test]
    fn discovers_both_skills_and_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("skills/alpha/SKILL.md"), "# a");
        write_file(&root.join("rules/one.md"), "r1");

        let got = auto_discover(root).expect("should discover");
        assert_eq!(got.skill_folders, vec![root.join("skills/alpha")]);
        assert_eq!(got.rule_files, vec![root.join("rules/one.md")]);
    }

    #[test]
    fn errors_when_neither_folder_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Place a README at the root — must not count as a rules import.
        write_file(&root.join("README.md"), "# readme");
        write_file(&root.join("CHANGELOG.md"), "# log");

        let err = auto_discover(root).expect_err("should error");
        assert!(matches!(err, DiscoveryError::NothingDiscovered));
    }

    #[test]
    fn ignores_root_level_md_files() {
        // A repo with a README at the root but a real rules/ folder should
        // return ONLY the rules/* files — never README.md.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("README.md"), "readme");
        write_file(&root.join("CHANGELOG.md"), "changelog");
        write_file(&root.join("rules/style.md"), "r1");

        let got = auto_discover(root).expect("should discover");
        assert_eq!(got.rule_files, vec![root.join("rules/style.md")]);
        assert!(got.skill_folders.is_empty());
    }

    #[test]
    fn skips_git_node_modules_and_target_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Legit skill.
        write_file(&root.join("skills/real/SKILL.md"), "# ok");
        // Skills inside skipped dirs.
        write_file(&root.join("skills/.git/evil/SKILL.md"), "# no");
        write_file(&root.join("skills/node_modules/pkg/SKILL.md"), "# no");
        write_file(&root.join("skills/target/debug/SKILL.md"), "# no");
        // Rules inside skipped dirs.
        write_file(&root.join("rules/good.md"), "good");
        write_file(&root.join("rules/.git/config.md"), "no");
        write_file(&root.join("rules/node_modules/bad.md"), "no");
        write_file(&root.join("rules/target/bad.md"), "no");

        let got = auto_discover(root).expect("should discover");
        assert_eq!(got.skill_folders, vec![root.join("skills/real")]);
        assert_eq!(got.rule_files, vec![root.join("rules/good.md")]);
    }

    #[test]
    fn discovers_nested_skill_folders() {
        // Mirrors the karpathy-skills layout: nested skill folders below a
        // parent that does not itself contain a SKILL.md.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("skills/pack/inner-a/SKILL.md"), "# a");
        write_file(&root.join("skills/pack/inner-b/SKILL.md"), "# b");

        let got = auto_discover(root).expect("should discover");
        assert_eq!(
            got.skill_folders,
            vec![
                root.join("skills/pack/inner-a"),
                root.join("skills/pack/inner-b"),
            ]
        );
    }

    #[test]
    fn rules_walk_is_recursive() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("rules/top.md"), "t");
        write_file(&root.join("rules/sub/nested.md"), "n");

        let got = auto_discover(root).expect("should discover");
        assert_eq!(
            got.rule_files,
            vec![root.join("rules/sub/nested.md"), root.join("rules/top.md")]
        );
    }

    #[test]
    fn empty_skills_dir_is_success_not_error() {
        // An existing-but-empty `skills/` is a valid (if unhelpful) import —
        // only the total absence of both dirs should error.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("skills")).unwrap();

        let got = auto_discover(root).expect("empty skills/ is still a match");
        assert!(got.skill_folders.is_empty());
        assert!(got.rule_files.is_empty());
    }
}
