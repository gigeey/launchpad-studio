use std::collections::HashMap;
use std::path::Path;

use ao_protocol::error::AoError;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Resolves a canonical project key for `cwd`.
///
/// Algorithm:
/// 1. Spawn `git rev-parse --show-toplevel` with `cwd` as the working directory.
/// 2. On success, use the trimmed stdout as the raw key; canonicalize it.
/// 3. On any failure (not a git repo, git missing, etc.), canonicalize `cwd` directly.
/// 4. Strip trailing slash; on macOS/Windows lowercase the result for
///    case-insensitive filesystem safety.
pub async fn resolve_project_key(cwd: &Path) -> Result<String, AoError> {
    let raw = try_git_toplevel(cwd).await.unwrap_or_default();

    let base = if !raw.is_empty() {
        let git_path = Path::new(&raw);
        std::fs::canonicalize(git_path).unwrap_or_else(|_| git_path.to_path_buf())
    } else {
        std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
    };

    let mut s = base.to_string_lossy().into_owned();

    // Strip trailing slash.
    if s.ends_with('/') || s.ends_with('\\') {
        s.pop();
    }

    // Lowercase on case-insensitive platforms (macOS, Windows).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        s = s.to_lowercase();
    }

    Ok(s)
}

/// Attempts to get the git repository root for `cwd`.
/// Returns `None` on any error (git not found, not a repo, etc.).
async fn try_git_toplevel(cwd: &Path) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .current_dir(cwd)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim().to_string();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

#[derive(Serialize, Deserialize)]
struct ProjectIndexEntry {
    project_key: String,
    last_resolved_at: String,
}

/// Updates `memory/projects/index.json` with the resolved canonical key for `hash`.
/// Called after every successful project key resolution so the index stays current.
pub async fn update_projects_index(
    data_root: &crate::paths::DataRoot,
    hash: &str,
    canonical_key: &str,
) -> Result<(), AoError> {
    let index_path = data_root.memory_projects_index_path();

    let mut index: HashMap<String, ProjectIndexEntry> =
        if tokio::fs::try_exists(&index_path).await.unwrap_or(false) {
            let content = tokio::fs::read_to_string(&index_path).await?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            HashMap::new()
        };

    index.insert(
        hash.to_string(),
        ProjectIndexEntry {
            project_key: canonical_key.to_string(),
            last_resolved_at: Utc::now().to_rfc3339(),
        },
    );

    if let Some(parent) = index_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let tmp_path = index_path.with_extension("json.tmp");
    let content =
        serde_json::to_string_pretty(&index).map_err(|e| AoError::Json(e.to_string()))?;
    tokio::fs::write(&tmp_path, &content).await?;
    tokio::fs::rename(&tmp_path, &index_path).await?;

    Ok(())
}

/// Returns a 32-char lowercase hex string (SHA-256 truncated to 128 bits).
/// Safe for use as a filename on all platforms.
pub fn hash_project_key(canonical_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_key.as_bytes());
    let result = hasher.finalize();
    format!("{:x}", result)[..32].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    #[tokio::test]
    async fn test_git_repo_cwd_returns_toplevel() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_path = tmp.path();

        // Init git repo and create a subdirectory.
        StdCommand::new("git")
            .args(["init"])
            .current_dir(tmp_path)
            .output()
            .expect("git init failed");

        let sub = tmp_path.join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        // Resolving from the subdirectory should return the repo root.
        let key = resolve_project_key(&sub).await.unwrap();
        let expected = std::fs::canonicalize(tmp_path).unwrap().to_string_lossy().into_owned();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let expected = expected.to_lowercase();

        assert_eq!(key, expected);
    }

    #[tokio::test]
    async fn test_non_git_cwd_returns_canonicalized_path() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_path = tmp.path();

        // No git init — not a repo.
        let key = resolve_project_key(tmp_path).await.unwrap();
        let expected = std::fs::canonicalize(tmp_path).unwrap().to_string_lossy().into_owned();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let expected = expected.to_lowercase();

        assert_eq!(key, expected);
        assert!(!key.ends_with('/'));
    }

    #[test]
    fn test_hash_of_same_key_is_stable() {
        let key = "/home/user/my-project";
        let h1 = hash_project_key(key);
        let h2 = hash_project_key(key);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 32);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_hash_different_keys_differ() {
        let h1 = hash_project_key("/home/user/project-a");
        let h2 = hash_project_key("/home/user/project-b");
        assert_ne!(h1, h2);
    }
}
