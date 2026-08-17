use std::path::Path;

use tracing::warn;

use ao_protocol::agent::AgentProfile;

use super::sources::{load_builtin_pool, load_plugin_pool, load_user_pool};
use super::{SkillEntry, SkillRegistry};

impl SkillRegistry {
    /// Walk the user pool, plugin pool, and built-in pool for `profile`,
    /// returning an ordered registry.
    ///
    /// Resolution order: user pool first, plugin pool second, built-in pool
    /// last. Same-name collision: the earlier pool always wins; a later
    /// duplicate is dropped with a warning. The built-in pool is not gated
    /// by `profile.skills` or `profile.enabled_plugins` — it has no install
    /// step, so it's always present regardless of the profile's allowlist.
    pub fn load(data_dir: &Path, profile: &AgentProfile) -> SkillRegistry {
        let user_entries = load_user_pool(data_dir, &profile.skills);
        let plugin_entries = load_plugin_pool(data_dir, &profile.enabled_plugins);
        let builtin_entries = load_builtin_pool();

        let mut registry = SkillRegistry::empty();

        for (name, entry) in user_entries {
            registry.insert(name, entry);
        }

        for (name, entry) in plugin_entries {
            if registry.name_index.contains_key(&name) {
                let shadowing_source = match &entry {
                    SkillEntry::Ok(r) => format!("{:?}", r.source),
                    SkillEntry::Err(_) => "plugin (load error)".to_string(),
                };
                warn!(
                    skill = %name,
                    shadowing_source = %shadowing_source,
                    "skill from plugin pool shadowed by user-pool entry; plugin version ignored"
                );
                continue;
            }
            registry.insert(name, entry);
        }

        for (name, entry) in builtin_entries {
            if registry.name_index.contains_key(&name) {
                let shadowing_source = match &entry {
                    SkillEntry::Ok(r) => format!("{:?}", r.source),
                    SkillEntry::Err(_) => "built-in (load error)".to_string(),
                };
                warn!(
                    skill = %name,
                    shadowing_source = %shadowing_source,
                    "skill from built-in pool shadowed by user/plugin-pool entry; built-in version ignored"
                );
                continue;
            }
            registry.insert(name, entry);
        }

        registry
    }

    /// Convenience alias for `load` — used by registry refresh calls.
    pub fn refresh(data_dir: &Path, profile: &AgentProfile) -> SkillRegistry {
        Self::load(data_dir, profile)
    }
}
