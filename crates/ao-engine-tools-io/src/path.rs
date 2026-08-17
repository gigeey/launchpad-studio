//! Path expansion shared across IO tools.
//!
//! IO tools route every user-supplied path string through [`expand_path`]
//! so behaviour stays consistent: a leading `~` becomes the user's home
//! directory, absolute paths pass through untouched, and everything else
//! is interpreted relative to the runner's current working directory.
//!
//! Keeping the helper in this crate (rather than `ao-engine-tools-core`)
//! lets later stories adopt it from `Read` and `Grep` without crate
//! churn — same import, same semantics, single place to evolve.

use std::path::{Path, PathBuf};

/// Render `path` as a cwd-relative string when it lies under `cwd`,
/// otherwise as the absolute path string unchanged. Saves output tokens
/// for the common case where the search root is inside the runner cwd.
///
/// `cwd` should be pre-canonicalized so symlinks in the cwd chain do not
/// produce false negatives from an `is_prefix` check.
pub fn relativize_path(path: &Path, cwd: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Resolve `input` against `cwd`.
///
/// - Empty input returns `cwd` unchanged.
/// - `~` alone resolves to the user's home directory.
/// - `~/<rest>` resolves to `<home>/<rest>`.
/// - A path that is already absolute (POSIX `/...` or, on Windows, a path
///   with a drive prefix) is returned as-is.
/// - Anything else is joined onto `cwd`.
///
/// If the user's home directory cannot be determined (no `HOME` env var,
/// or `USERPROFILE` on Windows), tilde forms fall back to being joined
/// onto `cwd` so the helper never panics.
pub fn expand_path(input: &str, cwd: &Path) -> PathBuf {
    expand_path_inner(input, cwd, home_dir())
}

fn expand_path_inner(input: &str, cwd: &Path, home: Option<PathBuf>) -> PathBuf {
    if input.is_empty() {
        return cwd.to_path_buf();
    }

    if input == "~" {
        return home.unwrap_or_else(|| cwd.join("~"));
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return match home {
            Some(h) => h.join(rest),
            None => cwd.join(input),
        };
    }

    let candidate = PathBuf::from(input);
    if candidate.is_absolute() {
        return candidate;
    }

    cwd.join(input)
}

fn home_dir() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    #[cfg(windows)]
    {
        if let Ok(h) = std::env::var("USERPROFILE") {
            if !h.is_empty() {
                return Some(PathBuf::from(h));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> PathBuf {
        PathBuf::from("/tmp/launchpad-cwd")
    }

    fn home() -> PathBuf {
        PathBuf::from("/tmp/launchpad-home")
    }

    #[test]
    fn empty_input_returns_cwd() {
        assert_eq!(expand_path_inner("", &cwd(), Some(home())), cwd());
    }

    #[test]
    fn tilde_only_expands_to_home() {
        assert_eq!(expand_path_inner("~", &cwd(), Some(home())), home());
    }

    #[test]
    fn tilde_prefix_expands_to_home_join() {
        assert_eq!(
            expand_path_inner("~/projects/foo", &cwd(), Some(home())),
            home().join("projects/foo"),
        );
    }

    #[test]
    fn tilde_falls_back_to_cwd_when_home_unknown() {
        assert_eq!(expand_path_inner("~", &cwd(), None), cwd().join("~"));
        assert_eq!(expand_path_inner("~/x", &cwd(), None), cwd().join("~/x"),);
    }

    #[test]
    fn absolute_posix_path_passes_through() {
        let abs = PathBuf::from("/etc/hosts");
        assert_eq!(expand_path_inner("/etc/hosts", &cwd(), Some(home())), abs);
    }

    #[cfg(windows)]
    #[test]
    fn absolute_windows_path_passes_through() {
        let p = "C:\\Users\\me";
        assert_eq!(expand_path_inner(p, &cwd(), Some(home())), PathBuf::from(p),);
    }

    #[test]
    fn relative_is_joined_onto_cwd() {
        assert_eq!(
            expand_path_inner("a/b.txt", &cwd(), Some(home())),
            cwd().join("a/b.txt"),
        );
    }

    #[test]
    fn tilde_in_middle_is_not_expanded() {
        // Only a leading `~` or `~/` triggers expansion. A literal `~` in
        // the middle of a relative path is treated as part of the name.
        assert_eq!(
            expand_path_inner("foo/~/bar", &cwd(), Some(home())),
            cwd().join("foo/~/bar"),
        );
    }

    #[test]
    fn public_helper_uses_home_env() {
        // Sanity-check that the public entry point compiles and runs;
        // exact home resolution is covered by `expand_path_inner`.
        let got = expand_path("/abs/path", &cwd());
        assert_eq!(got, PathBuf::from("/abs/path"));
    }

    #[test]
    fn relativize_path_returns_relative_when_under_cwd() {
        let base = PathBuf::from("/tmp/launchpad-cwd");
        let hit = base.join("sub/file.txt");
        assert_eq!(relativize_path(&hit, &base), "sub/file.txt");
    }

    #[test]
    fn relativize_path_returns_absolute_when_outside_cwd() {
        let base = PathBuf::from("/tmp/launchpad-cwd");
        let hit = PathBuf::from("/other/path/file.txt");
        assert_eq!(relativize_path(&hit, &base), "/other/path/file.txt");
    }

    #[test]
    fn relativize_path_returns_relative_for_direct_child() {
        let base = PathBuf::from("/tmp/launchpad-cwd");
        let hit = base.join("file.txt");
        assert_eq!(relativize_path(&hit, &base), "file.txt");
    }
}
