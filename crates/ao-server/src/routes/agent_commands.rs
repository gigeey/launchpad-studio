use axum::extract::Query;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Debug, Serialize)]
pub struct AgentCommand {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub source_type: String,
    pub scope: String,
}

#[derive(Debug, Serialize)]
pub struct AgentCommandsResponse {
    pub commands: Vec<AgentCommand>,
}

#[derive(Debug, Deserialize)]
pub struct AgentCommandsQuery {
    pub command: String,
    pub working_dir: Option<String>,
}

/// YAML frontmatter fields we care about.
#[derive(Debug, Deserialize, Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// Parse YAML frontmatter from a markdown file's content.
fn parse_frontmatter(content: &str) -> Frontmatter {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Frontmatter::default();
    }
    // Find the closing ---
    if let Some(end) = trimmed[3..].find("\n---") {
        let yaml_block = &trimmed[3..3 + end].trim();
        serde_yaml::from_str(yaml_block).unwrap_or_default()
    } else {
        Frontmatter::default()
    }
}

/// Scan `dir/commands/*.md` files (Claude's legacy command format).
fn scan_commands_dir(dir: &Path, scope: &str, results: &mut HashMap<String, AgentCommand>) {
    let commands_dir = dir.join("commands");
    let entries = match std::fs::read_dir(&commands_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !file_name.ends_with(".md") || file_name.starts_with('.') {
            continue;
        }
        let slug = file_name.trim_end_matches(".md").to_string();
        if results.contains_key(&slug) {
            continue; // project-level already registered
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to read command file {}: {}", path.display(), e);
                continue;
            }
        };
        let fm = parse_frontmatter(&content);
        results.insert(
            slug.clone(),
            AgentCommand {
                name: fm.name.unwrap_or_else(|| slug.clone()),
                description: fm.description,
                slug,
                source_type: "command".to_string(),
                scope: scope.to_string(),
            },
        );
    }
}

/// Scan `dir/skills/*/SKILL.md` (shared skill format used by Claude, Cursor, Codex).
fn scan_skills_dir(dir: &Path, scope: &str, results: &mut HashMap<String, AgentCommand>) {
    let skills_dir = dir.join("skills");
    let entries = match std::fs::read_dir(&skills_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let folder_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if folder_name.starts_with('.') {
            continue;
        }
        let skill_file = path.join("SKILL.md");
        if !skill_file.is_file() {
            continue;
        }
        let slug = folder_name;
        if results.contains_key(&slug) {
            continue;
        }
        let content = match std::fs::read_to_string(&skill_file) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to read skill file {}: {}", skill_file.display(), e);
                continue;
            }
        };
        let fm = parse_frontmatter(&content);
        results.insert(
            slug.clone(),
            AgentCommand {
                name: fm.name.unwrap_or_else(|| slug.clone()),
                description: fm.description,
                slug,
                source_type: "skill".to_string(),
                scope: scope.to_string(),
            },
        );
    }
}

/// Return the scan directories for a given CLI agent command.
/// Returns (project_dirs, user_dirs) where each entry is (base_path, scan_type).
///
/// scan_type: "commands" means scan commands/*.md, "skills" means scan skills/*/SKILL.md
fn get_scan_paths(
    command: &str,
    working_dir: Option<&str>,
    home: &Path,
) -> (Vec<(PathBuf, &'static str)>, Vec<(PathBuf, &'static str)>) {
    let mut project = Vec::new();
    let mut user = Vec::new();

    match command {
        "claude" => {
            if let Some(wd) = working_dir {
                let base = PathBuf::from(wd).join(".claude");
                project.push((base.clone(), "commands"));
                project.push((base, "skills"));
            }
            let base = home.join(".claude");
            user.push((base.clone(), "commands"));
            user.push((base, "skills"));
        }
        "cursor-agent" => {
            if let Some(wd) = working_dir {
                let base = PathBuf::from(wd).join(".cursor");
                project.push((base, "skills"));
            }
            // Cursor's user-level skills are directly skill folders under skills-cursor/
            // so we treat skills-cursor as the parent that contains skill dirs
            user.push((home.join(".cursor"), "skills-cursor"));
        }
        "codex" => {
            if let Some(wd) = working_dir {
                let base = PathBuf::from(wd).join(".agents");
                project.push((base, "skills"));
            }
            let base = home.join(".codex");
            user.push((base, "skills"));
        }
        _ => {}
    }

    (project, user)
}

/// Scan a base path with the given scan type.
fn scan_path(base: &Path, scan_type: &str, scope: &str, results: &mut HashMap<String, AgentCommand>) {
    match scan_type {
        "commands" => scan_commands_dir(base, scope, results),
        "skills" => scan_skills_dir(base, scope, results),
        // Cursor user-level: ~/.cursor/skills-cursor/*/SKILL.md
        // The base is ~/.cursor and scan_type is "skills-cursor"
        "skills-cursor" => {
            let skills_dir = base.join("skills-cursor");
            let entries = match std::fs::read_dir(&skills_dir) {
                Ok(e) => e,
                Err(_) => return,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let folder_name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                if folder_name.starts_with('.') {
                    continue;
                }
                let skill_file = path.join("SKILL.md");
                if !skill_file.is_file() {
                    continue;
                }
                let slug = folder_name;
                if results.contains_key(&slug) {
                    continue;
                }
                let content = match std::fs::read_to_string(&skill_file) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("Failed to read skill file {}: {}", skill_file.display(), e);
                        continue;
                    }
                };
                let fm = parse_frontmatter(&content);
                results.insert(
                    slug.clone(),
                    AgentCommand {
                        name: fm.name.unwrap_or_else(|| slug.clone()),
                        description: fm.description,
                        slug,
                        source_type: "skill".to_string(),
                        scope: scope.to_string(),
                    },
                );
            }
        }
        _ => {}
    }
}

pub async fn list_agent_commands(
    Query(params): Query<AgentCommandsQuery>,
) -> Json<AgentCommandsResponse> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => {
            warn!("Could not determine home directory");
            return Json(AgentCommandsResponse {
                commands: Vec::new(),
            });
        }
    };

    let mut results: HashMap<String, AgentCommand> = HashMap::new();

    let (project_paths, user_paths) =
        get_scan_paths(&params.command, params.working_dir.as_deref(), &home);

    // Scan project-level first so they take precedence
    for (base, scan_type) in &project_paths {
        scan_path(base, scan_type, "project", &mut results);
    }

    // Then user-level (skipped if slug already exists from project)
    for (base, scan_type) in &user_paths {
        scan_path(base, scan_type, "user", &mut results);
    }

    let mut commands: Vec<AgentCommand> = results.into_values().collect();
    commands.sort_by(|a, b| a.slug.cmp(&b.slug));

    Json(AgentCommandsResponse { commands })
}
