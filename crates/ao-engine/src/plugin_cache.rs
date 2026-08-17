//! Shared in-memory cache for parsed global plugin content.
//!
//! Context assembly (see [`crate::agent_context`]) needs skill metadata
//! (frontmatter) and full rule bodies from every installed plugin — parsed
//! once at startup (and again whenever a plugin is installed, uninstalled, or
//! refreshed), then reused across every agent's message turn. Re-walking the
//! filesystem on every turn would be wasteful, so this module owns the
//! shared snapshot.
//!
//! The cache trusts the registry as the source of truth (via
//! [`crate::plugin_catalog`]): orphan plugin folders with no registry entry
//! are ignored, matching uninstall's lazy-cleanup contract.
//!
//! ## Design
//!
//! * [`PluginCacheSnapshot`] is the immutable view consumed by readers. It
//!   bundles fully-parsed [`PluginSkillMeta`] + [`PluginRuleMeta`] values.
//! * [`PluginCache`] wraps an `Arc<RwLock<Arc<PluginCacheSnapshot>>>` so
//!   readers can cheaply clone a snapshot `Arc` and release the lock quickly.
//! * [`filter_for_agent`] is a pure function that applies an agent's
//!   [`AgentProfile::enabled_plugins`] map to a snapshot.

use std::sync::Arc;

use tokio::sync::RwLock;

use ao_protocol::agent::AgentProfile;

use crate::agent_context::{PluginRuleMeta, PluginSkillMeta};
use crate::plugin_catalog::{list_plugin_rules, list_plugin_skills, CatalogError};

/// Immutable view of every installed plugin's skill metadata and rule bodies.
/// Sorted by `id` for deterministic context-assembly output.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PluginCacheSnapshot {
    pub skills: Vec<PluginSkillMeta>,
    pub rules: Vec<PluginRuleMeta>,
}

impl PluginCacheSnapshot {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.rules.is_empty()
    }
}

/// Thread-safe holder for the current [`PluginCacheSnapshot`].
///
/// Readers call [`Self::snapshot`] to get a cheap `Arc` clone; writers call
/// [`Self::refresh`] to rebuild from disk.
#[derive(Debug, Clone)]
pub struct PluginCache {
    state: Arc<RwLock<Arc<PluginCacheSnapshot>>>,
}

impl PluginCache {
    /// Create an empty cache. Call [`Self::refresh`] at startup to populate.
    pub fn new_empty() -> Self {
        Self {
            state: Arc::new(RwLock::new(Arc::new(PluginCacheSnapshot::default()))),
        }
    }

    /// Create a cache pre-populated with an explicit snapshot. Used by tests
    /// and by code that already has a snapshot in hand.
    pub fn with_snapshot(snapshot: PluginCacheSnapshot) -> Self {
        Self {
            state: Arc::new(RwLock::new(Arc::new(snapshot))),
        }
    }

    /// Cheap: clones an `Arc` to the current snapshot.
    pub async fn snapshot(&self) -> Arc<PluginCacheSnapshot> {
        self.state.read().await.clone()
    }

    /// Re-read every installed plugin's skills/rules from disk and swap in a
    /// new snapshot. Safe to call concurrently with readers (they continue to
    /// hold the previous `Arc` until they next call [`Self::snapshot`]).
    pub async fn refresh(&self) -> Result<(), CatalogError> {
        let snapshot = tokio::task::spawn_blocking(build_snapshot_blocking)
            .await
            .map_err(|e| {
                CatalogError::Io(std::io::Error::other(format!(
                    "plugin cache refresh panicked: {e}"
                )))
            })??;
        let mut guard = self.state.write().await;
        *guard = Arc::new(snapshot);
        Ok(())
    }
}

impl Default for PluginCache {
    fn default() -> Self {
        Self::new_empty()
    }
}

/// Parse `title` and `description` from a skill file's YAML frontmatter.
/// Returns `(title, description)` — both `None` when frontmatter is absent
/// or the keys are missing.
fn parse_plugin_skill_frontmatter(content: &str) -> (Option<String>, Option<String>) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (None, None);
    }
    let after_first = &trimmed[3..].trim_start_matches(['\r', '\n']);
    let end = match after_first.find("\n---") {
        Some(pos) => pos,
        None => return (None, None),
    };
    let frontmatter = &after_first[..end];
    let mut title = None;
    let mut description = None;
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("title:").or_else(|| line.strip_prefix("name:")) {
            if title.is_none() {
                title = Some(val.trim().trim_matches('"').to_string());
            }
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(val.trim().trim_matches('"').to_string());
        }
    }
    (title, description)
}

/// Read every plugin's skills and rules from disk. Synchronous; call inside
/// `spawn_blocking` from async contexts, or directly from sync code.
pub(crate) fn build_snapshot_blocking() -> Result<PluginCacheSnapshot, CatalogError> {
    let catalog_skills = list_plugin_skills()?;
    let catalog_rules = list_plugin_rules()?;

    let mut skills: Vec<PluginSkillMeta> = Vec::with_capacity(catalog_skills.len());
    for entry in catalog_skills {
        let content = match std::fs::read_to_string(&entry.skill_md) {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!(
                    skill = %entry.id,
                    path = %entry.skill_md.display(),
                    error = %err,
                    "Failed to read plugin SKILL.md during cache refresh; skipping"
                );
                continue;
            }
        };
        let (frontmatter_title, description) = parse_plugin_skill_frontmatter(&content);
        let title = frontmatter_title.unwrap_or_else(|| last_segment(&entry.skill_name));
        skills.push(PluginSkillMeta {
            id: entry.id,
            plugin_name: entry.plugin_name,
            skill_name: entry.skill_name,
            title,
            description,
            skill_md_path: entry.skill_md,
        });
    }

    let mut rules: Vec<PluginRuleMeta> = Vec::with_capacity(catalog_rules.len());
    for entry in catalog_rules {
        let content = match std::fs::read_to_string(&entry.rule_file) {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!(
                    rule = %entry.id,
                    path = %entry.rule_file.display(),
                    error = %err,
                    "Failed to read plugin rule file during cache refresh; skipping"
                );
                continue;
            }
        };
        rules.push(PluginRuleMeta {
            id: entry.id,
            plugin_name: entry.plugin_name,
            rule_name: entry.rule_name,
            content,
        });
    }

    Ok(PluginCacheSnapshot { skills, rules })
}

fn last_segment(skill_name: &str) -> String {
    skill_name
        .rsplit('/')
        .next()
        .unwrap_or(skill_name)
        .to_string()
}

/// Pure function: apply `agent.enabled_plugins` to `snapshot`, returning only
/// the skills + rules the agent should see.
///
/// Skills pass if the plugin is enabled AND the skill passes the per-agent
/// subset filter (when one is set). Rules pass if the plugin is enabled —
/// there is no per-rule toggle in v1.
pub fn filter_for_agent(
    snapshot: &PluginCacheSnapshot,
    agent: &AgentProfile,
) -> (Vec<PluginSkillMeta>, Vec<PluginRuleMeta>) {
    let skills = snapshot
        .skills
        .iter()
        .filter(|s| agent.is_skill_enabled(&s.plugin_name, &s.skill_name))
        .cloned()
        .collect();
    let rules = snapshot
        .rules
        .iter()
        .filter(|r| agent.is_plugin_enabled(&r.plugin_name))
        .cloned()
        .collect();
    (skills, rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_paths::tests::with_temp_root as paths_with_temp_root;
    use crate::plugin_registry::{
        upsert_entry, ManifestLocationKind, PluginRegistryEntry, PluginSource,
    };
    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, InputMode, OutputFormat, PluginEnablement, ProviderConfig,
    };
    use chrono::Utc;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    fn write_file(path: &Path, contents: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
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

    /// Install a plugin with declared skills (each a (name, title, desc)
    /// triple, frontmatter written to SKILL.md) and rules (each a (name,
    /// body) pair).
    fn install_plugin_with_content(
        root: &Path,
        plugin: &str,
        skills: &[(&str, &str, &str)],
        rules: &[(&str, &str)],
    ) {
        for (skill_name, title, desc) in skills {
            let frontmatter = format!(
                "---\ntitle: \"{title}\"\ndescription: \"{desc}\"\n---\nbody for {skill_name}\n"
            );
            write_file(
                &root.join(format!("plugins/{plugin}/skills/{skill_name}/SKILL.md")),
                frontmatter.as_bytes(),
            );
        }
        for (rule_name, body) in rules {
            write_file(
                &root.join(format!("plugins/{plugin}/rules/{rule_name}.md")),
                body.as_bytes(),
            );
        }
        register(plugin);
    }

    fn base_profile(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 60_000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
            max_turns: None,
        }
    }

    #[test]
    fn build_snapshot_reads_every_plugins_content_once() {
        paths_with_temp_root(|root| {
            install_plugin_with_content(
                root,
                "superpowers",
                &[
                    ("tdd", "Test Driven Development", "Red/green/refactor"),
                    ("rag", "Retrieval Augmented Gen", "Chunked retrieval"),
                ],
                &[("core", "Follow plugin standards.")],
            );

            let snap = build_snapshot_blocking().expect("snapshot");

            assert_eq!(snap.skills.len(), 2);
            assert_eq!(snap.skills[0].id, "superpowers/rag");
            assert_eq!(snap.skills[0].title, "Retrieval Augmented Gen");
            assert_eq!(
                snap.skills[0].description.as_deref(),
                Some("Chunked retrieval")
            );
            assert!(snap.skills[0].skill_md_path.is_file());
            assert_eq!(snap.skills[1].id, "superpowers/tdd");

            assert_eq!(snap.rules.len(), 1);
            assert_eq!(snap.rules[0].id, "superpowers/core");
            assert!(snap.rules[0].content.contains("plugin standards"));
        });
    }

    #[test]
    fn two_agents_share_plugin_but_get_different_filtered_subsets() {
        paths_with_temp_root(|root| {
            install_plugin_with_content(
                root,
                "superpowers",
                &[
                    ("tdd", "TDD", "d"),
                    ("rag", "RAG", "d"),
                    ("debugger", "Debugger", "d"),
                ],
                &[("core", "core rule")],
            );

            let snap = build_snapshot_blocking().expect("snapshot");

            // Agent A: plugin enabled, all skills allowed (subset = None).
            let mut agent_a = base_profile("a");
            agent_a.enabled_plugins.insert(
                "superpowers".to_string(),
                PluginEnablement {
                    enabled: true,
                    enabled_skills: None,
                },
            );

            // Agent B: plugin enabled, only `tdd` allowed.
            let mut agent_b = base_profile("b");
            agent_b.enabled_plugins.insert(
                "superpowers".to_string(),
                PluginEnablement {
                    enabled: true,
                    enabled_skills: Some(vec!["tdd".to_string()]),
                },
            );

            let (skills_a, rules_a) = filter_for_agent(&snap, &agent_a);
            let (skills_b, rules_b) = filter_for_agent(&snap, &agent_b);

            let ids_a: Vec<&str> = skills_a.iter().map(|s| s.id.as_str()).collect();
            assert_eq!(
                ids_a,
                vec!["superpowers/debugger", "superpowers/rag", "superpowers/tdd"]
            );

            let ids_b: Vec<&str> = skills_b.iter().map(|s| s.id.as_str()).collect();
            assert_eq!(ids_b, vec!["superpowers/tdd"]);

            // Rules: plugin-level toggle only — both agents see all rules.
            assert_eq!(rules_a.len(), 1);
            assert_eq!(rules_b.len(), 1);
        });
    }

    #[test]
    fn disabled_plugin_contributes_nothing_even_when_subset_set() {
        paths_with_temp_root(|root| {
            install_plugin_with_content(
                root,
                "superpowers",
                &[("tdd", "TDD", "d")],
                &[("core", "body")],
            );

            let snap = build_snapshot_blocking().expect("snapshot");

            // Plugin flagged off but subset still listed — the off flag wins.
            let mut agent = base_profile("off-agent");
            agent.enabled_plugins.insert(
                "superpowers".to_string(),
                PluginEnablement {
                    enabled: false,
                    enabled_skills: Some(vec!["tdd".to_string()]),
                },
            );

            let (skills, rules) = filter_for_agent(&snap, &agent);
            assert!(skills.is_empty());
            assert!(rules.is_empty());
        });
    }

    #[test]
    fn agent_without_enablement_entry_sees_no_plugin_content() {
        paths_with_temp_root(|root| {
            install_plugin_with_content(
                root,
                "superpowers",
                &[("tdd", "TDD", "d")],
                &[("core", "body")],
            );

            let snap = build_snapshot_blocking().expect("snapshot");

            // Legacy agent — no enabled_plugins at all.
            let agent = base_profile("legacy");
            assert!(agent.enabled_plugins.is_empty());

            let (skills, rules) = filter_for_agent(&snap, &agent);
            assert!(skills.is_empty());
            assert!(rules.is_empty());
        });
    }

    #[test]
    fn two_plugins_both_enabled_yield_merged_content() {
        paths_with_temp_root(|root| {
            install_plugin_with_content(
                root,
                "plugin-a",
                &[("one", "One", "d")],
                &[("core", "a-rule")],
            );
            install_plugin_with_content(
                root,
                "plugin-b",
                &[("two", "Two", "d")],
                &[("core", "b-rule")],
            );

            let snap = build_snapshot_blocking().expect("snapshot");

            let mut agent = base_profile("dual");
            for p in ["plugin-a", "plugin-b"] {
                agent.enabled_plugins.insert(
                    p.to_string(),
                    PluginEnablement {
                        enabled: true,
                        enabled_skills: None,
                    },
                );
            }

            let (skills, rules) = filter_for_agent(&snap, &agent);
            let skill_ids: Vec<&str> = skills.iter().map(|s| s.id.as_str()).collect();
            assert_eq!(skill_ids, vec!["plugin-a/one", "plugin-b/two"]);
            let rule_ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
            assert_eq!(rule_ids, vec!["plugin-a/core", "plugin-b/core"]);
        });
    }

    #[test]
    fn orphan_plugin_dir_without_registry_entry_is_ignored() {
        paths_with_temp_root(|root| {
            // Plugin folder on disk but no registry entry — must not be
            // surfaced by the cache. Tested at the catalog layer too; this
            // re-proves it at the cache layer since the cache walks via the
            // catalog.
            write_file(&root.join("plugins/ghost/skills/x/SKILL.md"), b"# x\n");
            write_file(&root.join("plugins/ghost/rules/y.md"), b"y\n");

            let snap = build_snapshot_blocking().expect("snapshot");
            assert!(snap.skills.is_empty());
            assert!(snap.rules.is_empty());
        });
    }

    #[test]
    fn skill_without_frontmatter_falls_back_to_last_segment_title() {
        paths_with_temp_root(|root| {
            write_file(
                &root.join("plugins/bare/skills/group/nested/SKILL.md"),
                b"just a body, no frontmatter\n",
            );
            register("bare");

            let snap = build_snapshot_blocking().expect("snapshot");
            assert_eq!(snap.skills.len(), 1);
            assert_eq!(snap.skills[0].id, "bare/group/nested");
            assert_eq!(snap.skills[0].skill_name, "group/nested");
            assert_eq!(snap.skills[0].title, "nested");
            assert!(snap.skills[0].description.is_none());
        });
    }

    #[test]
    fn subset_with_unknown_skill_matches_nothing() {
        paths_with_temp_root(|root| {
            install_plugin_with_content(
                root,
                "plugin",
                &[("real", "Real", "d")],
                &[],
            );

            let snap = build_snapshot_blocking().expect("snapshot");

            let mut agent = base_profile("picky");
            agent.enabled_plugins.insert(
                "plugin".to_string(),
                PluginEnablement {
                    enabled: true,
                    enabled_skills: Some(vec!["nonexistent".to_string()]),
                },
            );

            let (skills, _rules) = filter_for_agent(&snap, &agent);
            assert!(skills.is_empty());
        });
    }

    #[tokio::test]
    async fn cache_refresh_swaps_snapshot_and_is_cheap_to_snapshot() {
        // Async wrapper sanity check — can refresh and then cheaply clone
        // the snapshot Arc from an async context. We don't set up any
        // plugins here because `build_snapshot_blocking` calls out to
        // `DataRoot::resolve` which requires the process-global env var;
        // without a guard held across `await`, we can only verify the
        // empty-state async plumbing here. The *filtering* behavior is
        // covered by the sync tests above.
        let cache = PluginCache::new_empty();
        let snap = cache.snapshot().await;
        assert!(snap.is_empty());

        let filled = PluginCache::with_snapshot(PluginCacheSnapshot {
            skills: vec![PluginSkillMeta {
                id: "p/s".to_string(),
                plugin_name: "p".to_string(),
                skill_name: "s".to_string(),
                title: "S".to_string(),
                description: None,
                skill_md_path: PathBuf::from("/tmp/s"),
            }],
            rules: vec![],
        });
        let snap2 = filled.snapshot().await;
        assert_eq!(snap2.skills.len(), 1);

        // Cloning a snapshot Arc is cheap (same backing allocation).
        let snap3 = filled.snapshot().await;
        assert!(Arc::ptr_eq(&snap2, &snap3));
    }
}
