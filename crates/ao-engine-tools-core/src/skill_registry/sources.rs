use std::collections::HashMap;
use std::path::Path;

use tracing::warn;

use ao_protocol::agent::PluginEnablement;

use super::frontmatter::parse_frontmatter;
use super::{SkillEntry, SkillSource};

/// Load all user-pool skills for the given allowlist.
///
/// Each entry in `skills_allowlist` maps to `<data_dir>/skills/<name>/SKILL.md`.
/// Returns `(canonical_name, entry)` pairs in allowlist order.
pub fn load_user_pool(data_dir: &Path, skills_allowlist: &[String]) -> Vec<(String, SkillEntry)> {
    skills_allowlist
        .iter()
        .map(|name| {
            let skill_path = data_dir.join("skills").join(name).join("SKILL.md");
            let entry = match std::fs::read_to_string(&skill_path) {
                Ok(content) => match parse_frontmatter(&content) {
                    Ok(mut record) => {
                        record.source = SkillSource::User;
                        SkillEntry::Ok(record)
                    }
                    Err(e) => SkillEntry::Err(e.to_string()),
                },
                Err(e) => SkillEntry::Err(format!("could not read {}: {e}", skill_path.display())),
            };
            (name.clone(), entry)
        })
        .collect()
}

/// Load all plugin-pool skills for the given enabled-plugin map.
///
/// Each enabled plugin contributes skills from
/// `<data_dir>/plugins/<plugin>/skills/<name>/SKILL.md`.
/// If `enablement.enabled_skills` is `Some(list)`, only those names are included.
/// Returns `(canonical_name, entry)` pairs in filesystem-walk order per plugin.
pub fn load_plugin_pool(
    data_dir: &Path,
    enabled_plugins: &HashMap<String, PluginEnablement>,
) -> Vec<(String, SkillEntry)> {
    let mut out = Vec::new();

    for (plugin_name, enablement) in enabled_plugins {
        if !enablement.enabled {
            continue;
        }

        let plugin_skills_dir = data_dir.join("plugins").join(plugin_name).join("skills");
        if !plugin_skills_dir.exists() {
            continue;
        }

        let dir_iter = match std::fs::read_dir(&plugin_skills_dir) {
            Ok(iter) => iter,
            Err(e) => {
                warn!("failed to read plugin skills dir {}: {e}", plugin_skills_dir.display());
                continue;
            }
        };

        for dir_entry in dir_iter.flatten() {
            if !dir_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }

            let skill_name = dir_entry.file_name().to_string_lossy().to_string();

            if let Some(allowed) = &enablement.enabled_skills {
                if !allowed.contains(&skill_name) {
                    continue;
                }
            }

            let skill_path = dir_entry.path().join("SKILL.md");
            let entry = match std::fs::read_to_string(&skill_path) {
                Ok(content) => match parse_frontmatter(&content) {
                    Ok(mut record) => {
                        record.source = SkillSource::Plugin { plugin_name: plugin_name.clone() };
                        SkillEntry::Ok(record)
                    }
                    Err(e) => SkillEntry::Err(e.to_string()),
                },
                Err(e) => {
                    SkillEntry::Err(format!("could not read {}: {e}", skill_path.display()))
                }
            };

            out.push((skill_name, entry));
        }
    }

    out
}

/// Compiled-in content for the `create-workflow` built-in skill. `include_str!`
/// resolves at compile time relative to this file, so the markdown becomes
/// part of the linked binary — no runtime file read, no dependency on
/// `<data_dir>` existing or being populated.
const BUILTIN_CREATE_WORKFLOW: &str = include_str!("builtin/create-workflow.md");

/// Registry key used only if `BUILTIN_CREATE_WORKFLOW` ever fails to parse
/// (a build-time content bug, not something that can happen from user
/// input) — `parse_frontmatter` needs the record's own `name` field to key
/// the registry, which isn't available when parsing itself is what failed.
const BUILTIN_CREATE_WORKFLOW_FALLBACK_NAME: &str = "create-workflow";

/// Load the built-in skill pool: markdown guides compiled directly into the
/// binary, unlike the user/plugin pools above which read from disk at
/// runtime. Exists to ship first-party guidance (e.g. how to author a saved
/// Workflow script) that every agent gets for free, with no install step.
///
/// Unlike [`load_user_pool`] and [`load_plugin_pool`], this pool is not
/// gated by any allowlist — `SkillRegistry::load` includes it
/// unconditionally for every profile, because there is nothing to "enable":
/// the content ships with the binary, not with a user's data directory.
pub fn load_builtin_pool() -> Vec<(String, SkillEntry)> {
    let entry = match parse_frontmatter(BUILTIN_CREATE_WORKFLOW) {
        Ok(mut record) => {
            record.source = SkillSource::BuiltIn;
            let name = record.name.clone();
            (name, SkillEntry::Ok(record))
        }
        Err(e) => (
            BUILTIN_CREATE_WORKFLOW_FALLBACK_NAME.to_string(),
            SkillEntry::Err(format!("built-in skill 'create-workflow' failed to parse: {e}")),
        ),
    };
    vec![entry]
}
