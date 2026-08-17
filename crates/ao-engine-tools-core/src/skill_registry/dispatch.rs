use std::path::{Path, PathBuf};

use ao_protocol::agent::AgentProfile;

use super::frontmatter::FrontmatterError;
use super::{SkillEntry, SkillRecord, SkillRegistry, SkillSource};

/// Perform variable substitution on a skill body.
///
/// Replace static vars first, then `$ARGUMENTS` last so user-supplied args
/// cannot inject `${SESSION_ID}` or similar placeholders.
pub fn substitute_skill_vars(
    body: &str,
    args: &str,
    data_dir: &Path,
    skill_dir: &Path,
    session_id: &str,
    agent_id: &str,
) -> String {
    let data_dir_str = data_dir.to_string_lossy().into_owned();
    let skill_dir_str = format!("{}/", skill_dir.to_string_lossy());

    body.replace("${LAUNCHPAD_DATA_DIR}", &data_dir_str)
        .replace("${LAUNCHPAD_SKILL_DIR}", &skill_dir_str)
        .replace("${SESSION_ID}", session_id)
        .replace("${AGENT_ID}", agent_id)
        .replace("$ARGUMENTS", args)
}

/// Compute the absolute skill directory for a given record.
///
/// MCP-sourced and built-in skills have no on-disk directory; both return the
/// data-dir root so any `${LAUNCHPAD_SKILL_DIR}` substitution in their body
/// resolves to a safe, existing path rather than a phantom directory.
pub fn skill_dir_for_record(record: &SkillRecord, skill_name: &str, data_dir: &Path) -> PathBuf {
    match &record.source {
        SkillSource::User => data_dir.join("skills").join(skill_name),
        SkillSource::Plugin { plugin_name } => data_dir
            .join("plugins")
            .join(plugin_name)
            .join("skills")
            .join(skill_name),
        SkillSource::Mcp { .. } => data_dir.to_path_buf(),
        SkillSource::BuiltIn => data_dir.to_path_buf(),
    }
}

/// Validate a skill name per SkillRegister rules: `[a-z0-9_-]`, no `/`, no
/// leading `.`, max 64 chars.
pub fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('.')
        || name.contains('/')
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        Err(format!(
            "invalid skill name '{}': must match [a-z0-9_-], not start with '.', not contain '/', max 64 chars",
            name
        ))
    } else {
        Ok(())
    }
}

/// Validate a skill description: 1-240 chars.
pub fn validate_skill_description(description: &str) -> Result<(), String> {
    if description.is_empty() || description.len() > 240 {
        Err("description must be 1-240 characters".to_string())
    } else {
        Ok(())
    }
}

/// Errors from [`write_skill_to_user_pool`].
#[derive(Debug)]
pub enum SkillWriteError {
    SkillExists,
    SkillCollidesWithPlugin,
    ProfileNotFound,
    Io(std::io::Error),
    Yaml(String),
}

impl std::fmt::Display for SkillWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillWriteError::SkillExists => write!(f, "skill already exists (SkillExists)"),
            SkillWriteError::SkillCollidesWithPlugin => {
                write!(f, "collides with a plugin-pool skill (SkillCollidesWithPlugin)")
            }
            SkillWriteError::ProfileNotFound => write!(f, "agent profile not found"),
            SkillWriteError::Io(e) => write!(f, "io error: {}", e),
            SkillWriteError::Yaml(s) => write!(f, "yaml error: {}", s),
        }
    }
}

/// Whether a [`write_skill_to_user_pool`] call created or updated the file.
#[derive(Debug, Clone, PartialEq)]
pub enum SkillWriteOutcomeType {
    Created,
    Updated,
}

/// Write a skill to the user pool and update the agent profile.
///
/// Performs collision detection using `registry`, writes
/// `<data_dir>/skills/<name>/SKILL.md`, appends `name` to
/// `AgentProfile.skills` (deduplicated), and persists the profile.
///
/// Returns `(outcome_type, updated_profile)` on success.
pub async fn write_skill_to_user_pool(
    data_dir: &Path,
    agent_id: &str,
    name: &str,
    body: &str,
    override_existing: bool,
    registry: &SkillRegistry,
) -> Result<(SkillWriteOutcomeType, AgentProfile), SkillWriteError> {
    // Collision detection using the in-memory registry.
    match registry.get(name) {
        Some(SkillEntry::Ok(record)) => {
            if !override_existing {
                return Err(SkillWriteError::SkillExists);
            }
            if matches!(record.source, SkillSource::Plugin { .. }) {
                return Err(SkillWriteError::SkillCollidesWithPlugin);
            }
            // User pool + override:true → proceed
        }
        Some(SkillEntry::Err(_)) => {
            if !override_existing {
                return Err(SkillWriteError::SkillExists);
            }
            // Determine whether this is a user-pool or plugin-pool error entry.
            let user_skill_path = data_dir.join("skills").join(name).join("SKILL.md");
            if !user_skill_path.exists() {
                return Err(SkillWriteError::SkillCollidesWithPlugin);
            }
            // User pool error + override:true → overwrite
        }
        None => {
            // New skill — proceed.
        }
    }

    // Determine outcome type before writing.
    let user_skill_path = data_dir.join("skills").join(name).join("SKILL.md");
    let outcome = if user_skill_path.exists() {
        SkillWriteOutcomeType::Updated
    } else {
        SkillWriteOutcomeType::Created
    };

    // Write SKILL.md to user pool.
    let skill_dir = data_dir.join("skills").join(name);
    tokio::fs::create_dir_all(&skill_dir)
        .await
        .map_err(SkillWriteError::Io)?;
    tokio::fs::write(skill_dir.join("SKILL.md"), body.as_bytes())
        .await
        .map_err(SkillWriteError::Io)?;

    // Read, update, and persist agent profile.
    let profile_path = data_dir.join("agents").join(format!("{}.yaml", agent_id));
    if !profile_path.exists() {
        return Err(SkillWriteError::ProfileNotFound);
    }
    let contents = tokio::fs::read_to_string(&profile_path)
        .await
        .map_err(SkillWriteError::Io)?;
    let mut profile: AgentProfile = serde_yaml::from_str(&contents)
        .map_err(|e| SkillWriteError::Yaml(e.to_string()))?;
    if !profile.skills.contains(&name.to_string()) {
        profile.skills.push(name.to_string());
    }
    let yaml = serde_yaml::to_string(&profile)
        .map_err(|e| SkillWriteError::Yaml(e.to_string()))?;
    tokio::fs::write(&profile_path, yaml.as_bytes())
        .await
        .map_err(SkillWriteError::Io)?;

    Ok((outcome, profile))
}

/// Errors from [`rewrite_user_skill`].
#[derive(Debug)]
pub enum SkillRewriteError {
    /// `name` has no `SKILL.md` under the user pool — either it was never a
    /// user-pool skill (plugin/MCP-sourced) or it has since been removed.
    NotFound,
    Io(std::io::Error),
    Frontmatter(FrontmatterError),
}

impl std::fmt::Display for SkillRewriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillRewriteError::NotFound => write!(f, "no user-pool SKILL.md found for this skill"),
            SkillRewriteError::Io(e) => write!(f, "io error: {}", e),
            SkillRewriteError::Frontmatter(e) => write!(f, "frontmatter error: {}", e),
        }
    }
}

/// Rewrite the on-disk `SKILL.md` for a user-pool skill in place, applying
/// `transform` to its current content and persisting the result.
///
/// Unlike [`write_skill_to_user_pool`], this never creates a skill and never
/// touches an agent profile — it exists purely so the lifecycle
/// sweeps (`ao_engine_tools_engine::skill::{consolidation, retirement}`) can
/// flip a *known-live* skill's trust-gate/retirement frontmatter in place.
/// Callers are responsible for having already confirmed `name` resolves to
/// a [`SkillSource::User`] entry they are allowed to touch — this function
/// only checks that the file exists, not that it is safe to touch (the hard
/// invariant that a non-`Distilled`-provenance skill must never reach this
/// function at all is enforced by those callers, not here).
pub async fn rewrite_user_skill(
    data_dir: &Path,
    name: &str,
    transform: impl FnOnce(&str) -> Result<String, FrontmatterError>,
) -> Result<(), SkillRewriteError> {
    let path = data_dir.join("skills").join(name).join("SKILL.md");
    let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SkillRewriteError::NotFound
        } else {
            SkillRewriteError::Io(e)
        }
    })?;
    let rewritten = transform(&content).map_err(SkillRewriteError::Frontmatter)?;
    tokio::fs::write(&path, rewritten.as_bytes())
        .await
        .map_err(SkillRewriteError::Io)?;
    Ok(())
}
