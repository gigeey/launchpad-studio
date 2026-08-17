//! Prefix-namespaced catalog of plugin skills and rules from the global
//! plugin store.
//!
//! Layout on disk is owned by [`plugin_install`](crate::plugin_install):
//!
//! ```text
//! <plugins-root>/<plugin-name>/
//! ├── skills/<skill-path>/SKILL.md
//! └── rules/<rule-path>.md
//! ```
//!
//! This module produces a *logical* view where every entry's ID is
//! `<plugin-name>/<skill-or-rule-name>`. On-disk names are never rewritten —
//! the prefix lives only in the catalog layer, so two plugins that each ship
//! a skill called `tdd` yield distinct IDs (`plugin-a/tdd`, `plugin-b/tdd`)
//! without touching the filesystem.
//!
//! The registry is the source of truth for which plugins exist: orphan plugin
//! folders with no registry entry are ignored, and orphan registry entries
//! with no folder simply contribute zero skills/rules.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::plugin_paths::plugins_root;
use crate::plugin_registry::{load_registry, RegistryError};

/// A plugin skill surfaced through the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSkillEntry {
    /// Prefix-namespaced id: `<plugin-name>/<skill-name>`.
    pub id: String,
    pub plugin_name: String,
    /// Forward-slash path to the skill folder relative to `<plugin>/skills/`.
    pub skill_name: String,
    /// Absolute path to the skill folder on disk.
    pub skill_dir: PathBuf,
    /// Absolute path to the skill's `SKILL.md` file.
    pub skill_md: PathBuf,
}

/// A plugin rule surfaced through the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRuleEntry {
    /// Prefix-namespaced id: `<plugin-name>/<rule-name>`. The `.md` extension
    /// is stripped from `rule-name` so ids read cleanly in the UI.
    pub id: String,
    pub plugin_name: String,
    /// Forward-slash path (extension-stripped) relative to `<plugin>/rules/`.
    pub rule_name: String,
    /// Absolute path to the rule `.md` file on disk.
    pub rule_file: PathBuf,
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Path(#[from] ao_protocol::error::AoError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// List every skill from every installed plugin, prefix-namespaced by plugin
/// name. Entries are sorted by id.
pub fn list_plugin_skills() -> Result<Vec<PluginSkillEntry>, CatalogError> {
    let root = plugins_root()?;
    let registry = load_registry()?;
    let mut out = Vec::new();
    for plugin in &registry.entries {
        let plugin_dir = root.join(&plugin.name);
        let skills_dir = plugin_dir.join("skills");
        if !skills_dir.is_dir() {
            continue;
        }
        collect_plugin_skills(&plugin.name, &skills_dir, &mut out);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// List every rule file from every installed plugin, prefix-namespaced by
/// plugin name. Entries are sorted by id.
pub fn list_plugin_rules() -> Result<Vec<PluginRuleEntry>, CatalogError> {
    let root = plugins_root()?;
    let registry = load_registry()?;
    let mut out = Vec::new();
    for plugin in &registry.entries {
        let plugin_dir = root.join(&plugin.name);
        let rules_dir = plugin_dir.join("rules");
        if !rules_dir.is_dir() {
            continue;
        }
        collect_plugin_rules(&plugin.name, &rules_dir, &mut out);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Resolve a prefix-namespaced skill id back to its on-disk entry. Returns
/// `Ok(None)` when the id is malformed (missing `/`, unsafe segments) or when
/// the plugin or skill is not installed.
pub fn lookup_plugin_skill(id: &str) -> Result<Option<PluginSkillEntry>, CatalogError> {
    let Some((plugin_name, rest)) = split_prefixed_id(id) else {
        return Ok(None);
    };
    let root = plugins_root()?;
    let skill_dir = root.join(plugin_name).join("skills").join(rest);
    let skill_md = skill_dir.join("SKILL.md");
    if !skill_md.is_file() {
        return Ok(None);
    }
    Ok(Some(PluginSkillEntry {
        id: id.to_string(),
        plugin_name: plugin_name.to_string(),
        skill_name: rest.to_string(),
        skill_dir,
        skill_md,
    }))
}

/// Resolve a prefix-namespaced rule id back to its on-disk entry. Returns
/// `Ok(None)` when the id is malformed or the rule does not exist.
pub fn lookup_plugin_rule(id: &str) -> Result<Option<PluginRuleEntry>, CatalogError> {
    let Some((plugin_name, rest)) = split_prefixed_id(id) else {
        return Ok(None);
    };
    let root = plugins_root()?;
    let rules_dir = root.join(plugin_name).join("rules");
    let rule_file = rules_dir.join(format!("{rest}.md"));
    if !rule_file.is_file() {
        return Ok(None);
    }
    Ok(Some(PluginRuleEntry {
        id: id.to_string(),
        plugin_name: plugin_name.to_string(),
        rule_name: rest.to_string(),
        rule_file,
    }))
}

/// Split `plugin/rest` into `(plugin, rest)`. Rejects ids with no `/`, empty
/// halves, or `..` components that would escape the plugin root.
fn split_prefixed_id(id: &str) -> Option<(&str, &str)> {
    let (plugin, rest) = id.split_once('/')?;
    if plugin.is_empty() || rest.is_empty() {
        return None;
    }
    if plugin.contains('\\') || rest.contains('\\') {
        return None;
    }
    for segment in rest.split('/') {
        if segment.is_empty() || segment == ".." || segment == "." {
            return None;
        }
    }
    Some((plugin, rest))
}

fn collect_plugin_skills(plugin_name: &str, skills_root: &Path, out: &mut Vec<PluginSkillEntry>) {
    walk_skill_dirs(plugin_name, skills_root, skills_root, out);
}

fn walk_skill_dirs(
    plugin_name: &str,
    skills_root: &Path,
    dir: &Path,
    out: &mut Vec<PluginSkillEntry>,
) {
    // A directory counts as a skill when it contains SKILL.md; walking
    // continues into it so bundles with nested skills all surface.
    let skill_md = dir.join("SKILL.md");
    if dir != skills_root && skill_md.is_file() {
        if let Some(skill_name) = rel_to_forward_slash(dir, skills_root) {
            if !skill_name.is_empty() {
                out.push(PluginSkillEntry {
                    id: format!("{plugin_name}/{skill_name}"),
                    plugin_name: plugin_name.to_string(),
                    skill_name,
                    skill_dir: dir.to_path_buf(),
                    skill_md,
                });
            }
        }
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !should_descend_into(&name) {
            continue;
        }
        walk_skill_dirs(plugin_name, skills_root, &entry.path(), out);
    }
}

fn collect_plugin_rules(plugin_name: &str, rules_root: &Path, out: &mut Vec<PluginRuleEntry>) {
    walk_rule_files(plugin_name, rules_root, rules_root, out);
}

fn walk_rule_files(
    plugin_name: &str,
    rules_root: &Path,
    dir: &Path,
    out: &mut Vec<PluginRuleEntry>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !should_descend_into(&name) {
                continue;
            }
            walk_rule_files(plugin_name, rules_root, &path, out);
        } else if ft.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
            let Ok(rel) = path.strip_prefix(rules_root) else {
                continue;
            };
            // Drop the `.md` extension from the id so ids read as
            // `plugin/rule-name`, not `plugin/rule-name.md`.
            let with_slashes = rel_components_to_string(rel);
            let Some(id_rel) = with_slashes.strip_suffix(".md") else {
                continue;
            };
            if id_rel.is_empty() {
                continue;
            }
            out.push(PluginRuleEntry {
                id: format!("{plugin_name}/{id_rel}"),
                plugin_name: plugin_name.to_string(),
                rule_name: id_rel.to_string(),
                rule_file: path,
            });
        }
    }
}

fn should_descend_into(name: &str) -> bool {
    !(name.starts_with('.') || name == "node_modules" || name == "target")
}

fn rel_to_forward_slash(abs: &Path, root: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    Some(rel_components_to_string(rel))
}

fn rel_components_to_string(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_paths::tests::with_temp_root as paths_with_temp_root;
    use crate::plugin_registry::{
        upsert_entry, ManifestLocationKind, PluginRegistryEntry, PluginSource,
    };
    use chrono::Utc;

    fn with_temp_root<F: FnOnce(&Path)>(f: F) {
        paths_with_temp_root(|root| f(root));
    }

    fn write_file(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn register(name: &str) {
        let now = Utc::now();
        upsert_entry(PluginRegistryEntry {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            source: PluginSource::LocalPath(PathBuf::from("/tmp/src")),
            installed_at: now,
            last_updated_at: now,
            auto_update_enabled: true,
            manifest_location: ManifestLocationKind::LaunchpadNative,
        })
        .unwrap();
    }

    /// Populate `<plugins-root>/<name>/` with a minimal skills+rules layout.
    fn install_fixture(root: &Path, plugin: &str, skills: &[&str], rules: &[&str]) {
        for skill in skills {
            write_file(
                &root.join(format!("plugins/{plugin}/skills/{skill}/SKILL.md")),
                format!("# {skill}\n").as_bytes(),
            );
        }
        for rule in rules {
            write_file(
                &root.join(format!("plugins/{plugin}/rules/{rule}.md")),
                format!("rule {rule}\n").as_bytes(),
            );
        }
        register(plugin);
    }

    #[test]
    fn lists_skills_with_plugin_prefix() {
        with_temp_root(|root| {
            install_fixture(root, "superpowers", &["tdd", "rag"], &[]);

            let skills = list_plugin_skills().expect("list");
            let ids: Vec<&str> = skills.iter().map(|s| s.id.as_str()).collect();
            assert_eq!(ids, vec!["superpowers/rag", "superpowers/tdd"]);

            let tdd = skills.iter().find(|s| s.skill_name == "tdd").unwrap();
            assert_eq!(tdd.plugin_name, "superpowers");
            assert_eq!(tdd.skill_dir, root.join("plugins/superpowers/skills/tdd"));
            assert!(tdd.skill_md.is_file());
        });
    }

    #[test]
    fn lists_rules_with_plugin_prefix_and_no_md_suffix() {
        with_temp_root(|root| {
            install_fixture(root, "standards", &[], &["tone", "structure"]);

            let rules = list_plugin_rules().expect("list");
            let ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
            assert_eq!(ids, vec!["standards/structure", "standards/tone"]);

            let tone = rules.iter().find(|r| r.rule_name == "tone").unwrap();
            assert_eq!(tone.plugin_name, "standards");
            assert_eq!(tone.rule_file, root.join("plugins/standards/rules/tone.md"));
        });
    }

    #[test]
    fn prefix_makes_ids_unique_across_two_plugins() {
        with_temp_root(|root| {
            // Both plugins ship a skill called `tdd` and a rule called `core`.
            install_fixture(root, "plugin-a", &["tdd"], &["core"]);
            install_fixture(root, "plugin-b", &["tdd"], &["core"]);

            let skill_ids: Vec<String> = list_plugin_skills()
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(skill_ids, vec!["plugin-a/tdd", "plugin-b/tdd"]);

            let rule_ids: Vec<String> = list_plugin_rules()
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect();
            assert_eq!(rule_ids, vec!["plugin-a/core", "plugin-b/core"]);
        });
    }

    #[test]
    fn lookup_skill_round_trips_to_listed_path() {
        with_temp_root(|root| {
            install_fixture(root, "superpowers", &["tdd"], &[]);

            let listed = list_plugin_skills().unwrap();
            let from_list = listed.iter().find(|s| s.id == "superpowers/tdd").unwrap();

            let looked_up = lookup_plugin_skill("superpowers/tdd")
                .unwrap()
                .expect("skill should be found");

            assert_eq!(looked_up, *from_list);
            assert_eq!(
                looked_up.skill_md,
                root.join("plugins/superpowers/skills/tdd/SKILL.md")
            );
        });
    }

    #[test]
    fn lookup_rule_round_trips_to_listed_path() {
        with_temp_root(|root| {
            install_fixture(root, "standards", &[], &["tone"]);

            let listed = list_plugin_rules().unwrap();
            let from_list = listed.iter().find(|r| r.id == "standards/tone").unwrap();

            let looked_up = lookup_plugin_rule("standards/tone")
                .unwrap()
                .expect("rule should be found");

            assert_eq!(looked_up, *from_list);
            assert_eq!(
                looked_up.rule_file,
                root.join("plugins/standards/rules/tone.md")
            );
        });
    }

    #[test]
    fn lookup_returns_none_for_missing_or_unsafe_ids() {
        with_temp_root(|root| {
            install_fixture(root, "superpowers", &["tdd"], &["tone"]);

            // Unknown plugin.
            assert!(lookup_plugin_skill("ghost/tdd").unwrap().is_none());
            assert!(lookup_plugin_rule("ghost/tone").unwrap().is_none());

            // Known plugin, unknown item.
            assert!(lookup_plugin_skill("superpowers/missing").unwrap().is_none());
            assert!(lookup_plugin_rule("superpowers/missing").unwrap().is_none());

            // Malformed / unsafe ids.
            assert!(lookup_plugin_skill("no-slash-here").unwrap().is_none());
            assert!(lookup_plugin_skill("").unwrap().is_none());
            assert!(lookup_plugin_skill("superpowers/").unwrap().is_none());
            assert!(lookup_plugin_skill("/tdd").unwrap().is_none());
            assert!(lookup_plugin_skill("superpowers/../tdd").unwrap().is_none());
            assert!(lookup_plugin_skill("superpowers/./tdd").unwrap().is_none());

            // Sanity: a valid skill still resolves.
            assert!(lookup_plugin_skill("superpowers/tdd").unwrap().is_some());
            // Touching `root` keeps the fixture alive.
            let _ = root;
        });
    }

    #[test]
    fn nested_skills_and_rules_keep_their_relative_path_in_the_id() {
        with_temp_root(|root| {
            write_file(
                &root.join("plugins/bundle/skills/group/alpha/SKILL.md"),
                b"# alpha\n",
            );
            write_file(
                &root.join("plugins/bundle/rules/group/policy.md"),
                b"policy\n",
            );
            register("bundle");

            let skills = list_plugin_skills().unwrap();
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].id, "bundle/group/alpha");
            assert_eq!(skills[0].skill_name, "group/alpha");

            let rules = list_plugin_rules().unwrap();
            assert_eq!(rules.len(), 1);
            assert_eq!(rules[0].id, "bundle/group/policy");
            assert_eq!(rules[0].rule_name, "group/policy");

            // Round-trip lookups.
            let looked = lookup_plugin_skill("bundle/group/alpha").unwrap().unwrap();
            assert_eq!(looked.skill_md, skills[0].skill_md);
            let looked = lookup_plugin_rule("bundle/group/policy").unwrap().unwrap();
            assert_eq!(looked.rule_file, rules[0].rule_file);
        });
    }

    #[test]
    fn orphan_plugin_dir_without_registry_entry_is_ignored() {
        with_temp_root(|root| {
            // Files exist on disk but the plugin was never registered — the
            // catalog trusts the registry as the source of truth.
            write_file(
                &root.join("plugins/orphan/skills/x/SKILL.md"),
                b"# x\n",
            );
            write_file(&root.join("plugins/orphan/rules/y.md"), b"y\n");

            let skills = list_plugin_skills().unwrap();
            let rules = list_plugin_rules().unwrap();
            assert!(skills.is_empty());
            assert!(rules.is_empty());
        });
    }

    #[test]
    fn registered_plugin_without_content_contributes_nothing() {
        with_temp_root(|_root| {
            // Registry entry but no skills/ or rules/ folder on disk.
            register("empty");

            let skills = list_plugin_skills().unwrap();
            let rules = list_plugin_rules().unwrap();
            assert!(skills.is_empty());
            assert!(rules.is_empty());
        });
    }

    #[test]
    fn non_md_files_under_rules_are_ignored() {
        with_temp_root(|root| {
            write_file(
                &root.join("plugins/mixed/rules/real.md"),
                b"real rule\n",
            );
            write_file(&root.join("plugins/mixed/rules/README.txt"), b"note\n");
            write_file(
                &root.join("plugins/mixed/rules/image.png"),
                b"\x89PNG\r\n",
            );
            register("mixed");

            let rules = list_plugin_rules().unwrap();
            let ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
            assert_eq!(ids, vec!["mixed/real"]);
        });
    }

    #[test]
    fn hidden_and_vendor_dirs_are_skipped_when_walking() {
        with_temp_root(|root| {
            write_file(&root.join("plugins/vendored/skills/a/SKILL.md"), b"# a\n");
            // These should NOT produce catalog entries even though they match
            // the SKILL.md pattern.
            write_file(
                &root.join("plugins/vendored/skills/.hidden/SKILL.md"),
                b"# h\n",
            );
            write_file(
                &root.join("plugins/vendored/skills/node_modules/junk/SKILL.md"),
                b"# n\n",
            );
            write_file(
                &root.join("plugins/vendored/rules/.hidden/bad.md"),
                b"x",
            );
            register("vendored");

            let skill_ids: Vec<String> = list_plugin_skills()
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(skill_ids, vec!["vendored/a"]);

            let rule_ids: Vec<String> = list_plugin_rules()
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect();
            assert!(rule_ids.is_empty());
        });
    }
}
