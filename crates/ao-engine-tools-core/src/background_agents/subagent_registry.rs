use std::collections::HashMap;

use thiserror::Error;

use super::definition::SubagentDefinition;

/// Error returned by [`SubagentRegistry::lookup_by_id`] when the requested
/// subagent type has not been registered.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown subagent type '{id}'")]
pub struct UnknownSubagentType {
    pub id: String,
}

/// Catalog of known [`SubagentDefinition`] entries — both built-ins and
/// user-supplied.
///
/// This is a distinct type from [`BackgroundAgentRegistry`](super::registry::BackgroundAgentRegistry),
/// which tracks *live* in-flight handles. `SubagentRegistry` is a static
/// lookup table consulted at spawn time to resolve a subagent type name into
/// its definition.
#[derive(Debug, Default, Clone)]
pub struct SubagentRegistry {
    entries: HashMap<String, SubagentDefinition>,
}

impl SubagentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a [`SubagentDefinition`].
    ///
    /// If an entry with the same `id` already exists it is **replaced**.
    /// Callers that want built-in-wins semantics should check
    /// [`contains`](Self::contains) before inserting.
    pub fn register(&mut self, def: SubagentDefinition) {
        self.entries.insert(def.id.clone(), def);
    }

    /// Return `true` if a definition with `id` is already registered.
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// Look up a subagent definition by its type id.
    ///
    /// Returns [`UnknownSubagentType`] when no entry matches — callers must
    /// not panic on an unknown id.
    pub fn lookup_by_id(&self, id: &str) -> Result<&SubagentDefinition, UnknownSubagentType> {
        self.entries.get(id).ok_or_else(|| UnknownSubagentType {
            id: id.to_string(),
        })
    }

    /// Return every registered definition, sorted by `id`.
    ///
    /// Used to enumerate the catalog for the Delegate tool's dynamic
    /// description and for "unknown subagent type" error messages. Sorting
    /// keeps the listing stable across calls regardless of `HashMap` ordering.
    pub fn list(&self) -> Vec<&SubagentDefinition> {
        let mut defs: Vec<&SubagentDefinition> = self.entries.values().collect();
        defs.sort_by(|a, b| a.id.cmp(&b.id));
        defs
    }

    /// Number of registered definitions.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no definitions are registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_by_id_returns_unknown_subagent_type_for_missing_id() {
        let reg = SubagentRegistry::new();
        let err = reg.lookup_by_id("DoesNotExist").expect_err("should be unknown");
        assert_eq!(err.id, "DoesNotExist");
    }

    #[test]
    fn register_custom_definition() {
        let mut reg = SubagentRegistry::new();
        reg.register(SubagentDefinition {
            id: "CustomAgent".to_string(),
            description: "Custom".to_string(),
            allowed_tools: vec!["Read".to_string()],
            system_prompt_fragment: "Be custom.".to_string(),
            model_override: None,
        });
        assert!(reg.lookup_by_id("CustomAgent").is_ok());
    }

    #[test]
    fn contains_returns_false_for_unknown_id() {
        let reg = SubagentRegistry::new();
        assert!(!reg.contains("GhostAgent"));
    }

    #[test]
    fn contains_returns_true_for_registered_id() {
        let mut reg = SubagentRegistry::new();
        reg.register(SubagentDefinition {
            id: "CustomAgent".to_string(),
            description: "Custom".to_string(),
            allowed_tools: vec!["Read".to_string()],
            system_prompt_fragment: "Be custom.".to_string(),
            model_override: None,
        });
        assert!(reg.contains("CustomAgent"));
    }
}
