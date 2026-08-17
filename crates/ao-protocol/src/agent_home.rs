/// Utilities for agent home directory scaffolding.
///
/// Each agent has a home directory at `~/.launchpad_studio/agent_homes/{agent_id}/`
/// containing:
/// - `skills/` — skill definition files
/// - `rules/` — rule files
/// - `CLAUDE.md` (or configured instruction file) — agent-level instructions

use std::path::{Path, PathBuf};

use crate::instruction_file::InstructionFilePattern;

/// Subdirectory names within an agent home.
pub const SKILLS_DIR: &str = "skills";
pub const RULES_DIR: &str = "rules";

/// Resolve the agent home path for a given agent_id under a data root.
pub fn agent_home_path(data_root: &Path, agent_id: &str) -> PathBuf {
    data_root.join("agent_homes").join(agent_id)
}

/// Resolve the skills directory within an agent home.
pub fn skills_dir(agent_home: &Path) -> PathBuf {
    agent_home.join(SKILLS_DIR)
}

/// Resolve the rules directory within an agent home.
pub fn rules_dir(agent_home: &Path) -> PathBuf {
    agent_home.join(RULES_DIR)
}

/// Resolve every default instruction file path within an agent home.
///
/// Returns one `PathBuf` per configured filename. Callers iterate and read
/// whichever paths exist on disk.
pub fn instruction_file_paths(agent_home: &Path) -> Vec<PathBuf> {
    InstructionFilePattern::default().resolve_all(agent_home)
}

/// Ensure the agent home directory structure exists.
///
/// Creates `{agent_home}/`, `{agent_home}/skills/`, and `{agent_home}/rules/`
/// if they don't already exist. This is idempotent — calling it multiple times
/// is safe.
pub async fn ensure_agent_home(agent_home: &Path) -> Result<(), std::io::Error> {
    tokio::fs::create_dir_all(skills_dir(agent_home)).await?;
    tokio::fs::create_dir_all(rules_dir(agent_home)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_agent_home_path() {
        let root = Path::new("/home/user/.launchpad_studio");
        let path = agent_home_path(root, "my-agent");
        assert_eq!(
            path,
            Path::new("/home/user/.launchpad_studio/agent_homes/my-agent")
        );
    }

    #[test]
    fn test_skills_dir() {
        let home = Path::new("/data/agent_homes/agent-1");
        assert_eq!(skills_dir(home), Path::new("/data/agent_homes/agent-1/skills"));
    }

    #[test]
    fn test_rules_dir() {
        let home = Path::new("/data/agent_homes/agent-1");
        assert_eq!(rules_dir(home), Path::new("/data/agent_homes/agent-1/rules"));
    }

    #[test]
    fn test_instruction_file_paths() {
        let home = Path::new("/data/agent_homes/agent-1");
        assert_eq!(
            instruction_file_paths(home),
            vec![Path::new("/data/agent_homes/agent-1/CLAUDE.md").to_path_buf()]
        );
    }

    #[tokio::test]
    async fn test_ensure_agent_home_creates_directories() {
        let tmp = TempDir::new().unwrap();
        let agent_home = tmp.path().join("agent-1");

        // Directory doesn't exist yet
        assert!(!agent_home.exists());

        ensure_agent_home(&agent_home).await.unwrap();

        assert!(agent_home.exists());
        assert!(agent_home.join("skills").exists());
        assert!(agent_home.join("rules").exists());
    }

    #[tokio::test]
    async fn test_ensure_agent_home_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let agent_home = tmp.path().join("agent-1");

        ensure_agent_home(&agent_home).await.unwrap();
        // Calling again should not error
        ensure_agent_home(&agent_home).await.unwrap();

        assert!(agent_home.join("skills").exists());
        assert!(agent_home.join("rules").exists());
    }
}
