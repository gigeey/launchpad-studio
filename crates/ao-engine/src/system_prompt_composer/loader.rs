/// File-system loaders for the canonical system-prompt composer.
///
/// These helpers load agent home and workspace context from disk, producing
/// the pure-data structs that compose_system_prompt() consumes. All I/O is
/// isolated here so the composer itself remains synchronous and testable.

use std::path::{Path, PathBuf};

use ao_protocol::system_prompt_context::{AgentHomeContext, WorkspaceContext};

/// Load workspace context from `path` (the effective working directory).
///
/// Reads the first existing instruction file (CLAUDE.md / Cursor.md etc.) from
/// the workspace root and all .md files from `.claude/rules/`. Missing files
/// and directories are handled gracefully.
pub async fn load_workspace_context(path: &Path) -> WorkspaceContext {
    let claude_md_paths = ao_protocol::instruction_file::InstructionFilePattern::default()
        .resolve_all(path);
    let claude_md_content = read_first_existing(&claude_md_paths)
        .await
        .filter(|s| !s.trim().is_empty());

    let rules_dir = path.join(".claude").join("rules");
    let rules = read_md_files_sorted(&rules_dir).await;

    WorkspaceContext {
        root_path: path.to_string_lossy().to_string(),
        claude_md_content,
        rules,
    }
}

/// Load agent home context from `path` (the agent home directory).
///
/// Reads the first existing instruction file, all .md files from `rules/`
/// (recursively), and SKILL.md from each subdirectory under `skills/`.
/// Missing files and directories are handled gracefully.
pub async fn load_agent_home_context(path: &Path) -> AgentHomeContext {
    let claude_md_paths = ao_protocol::agent_home::instruction_file_paths(path);
    let claude_md_content = read_first_existing(&claude_md_paths)
        .await
        .filter(|s| !s.trim().is_empty());

    let rules_dir = ao_protocol::agent_home::rules_dir(path);
    let skills_dir = ao_protocol::agent_home::skills_dir(path);

    let (rules, skills) = tokio::join!(
        read_md_files_sorted(&rules_dir),
        read_skill_files(&skills_dir),
    );

    AgentHomeContext {
        claude_md_content,
        rules,
        skills,
        // The disk loader does not know about the skill registry (user pool +
        // plugins live above this layer). The runner fills `skills_block` from
        // the registry after this returns; leaving it None here also keeps the
        // value out of the context cache, so the registry-derived listing is
        // recomputed fresh each turn rather than frozen at cache-write time.
        skills_block: None,
    }
}

/// Return the content of the first readable file among `paths`.
async fn read_first_existing(paths: &[PathBuf]) -> Option<String> {
    for path in paths {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            return Some(content);
        }
    }
    None
}

/// Recursively collect all non-empty .md file contents under `dir`, sorted
/// by relative path for deterministic ordering.
async fn read_md_files_sorted(dir: &Path) -> Vec<String> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut pairs: Vec<(String, String)> = Vec::new();
        collect_md_files(&dir, &dir, &mut pairs);
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs.into_iter().map(|(_, content)| content).collect()
    })
    .await
    .unwrap_or_default()
}

/// Synchronous recursive collector — called inside spawn_blocking.
fn collect_md_files(dir: &Path, base: &Path, out: &mut Vec<(String, String)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_md_files(&path, base, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if !content.trim().is_empty() {
                    let rel = path
                        .strip_prefix(base)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    out.push((rel, content));
                }
            }
        }
    }
}

/// Collect SKILL.md contents from each subdirectory of `skills_dir`, sorted
/// by skill name.
async fn read_skill_files(skills_dir: &Path) -> Vec<String> {
    let dir = skills_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        let mut pairs: Vec<(String, String)> = Vec::new();
        for entry in entries.flatten() {
            let skill_dir = entry.path();
            if skill_dir.is_dir() {
                let skill_md = skill_dir.join("SKILL.md");
                if let Ok(content) = std::fs::read_to_string(&skill_md) {
                    if !content.trim().is_empty() {
                        let name = skill_dir
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("")
                            .to_string();
                        pairs.push((name, content));
                    }
                }
            }
        }
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs.into_iter().map(|(_, content)| content).collect()
    })
    .await
    .unwrap_or_default()
}
