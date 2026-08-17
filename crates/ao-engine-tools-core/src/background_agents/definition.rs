use serde::{Deserialize, Serialize};

/// Opaque model identifier string passed through to the provider layer.
///
/// A `None` model override means the spawned child inherits the parent's
/// configured model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

impl ModelId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Sentinel entry in [`SubagentDefinition::allowed_tools`] that grants the
/// spawned child the parent's full tool registry instead of a filtered
/// subset. Resolved in
/// [`SubagentSpawner::build_child_context`](crate::background_agents::SubagentSpawner::build_child_context).
pub const ALL_TOOLS_WILDCARD: &str = "*";

/// Describes a subagent variant: its identity, the tools it is permitted to
/// use, a system-prompt fragment appended to the child's resolved prompt, and
/// an optional model override.
///
/// `SubagentDefinition` is intentionally a strict subset of the full agent
/// profile — it carries no mailbox, team, or scheduled-task coupling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentDefinition {
    /// Stable unique identifier for this subagent type (e.g. `"Explore"`).
    pub id: String,
    /// Human-readable description shown in tool-use output and logs.
    pub description: String,
    /// Tool names this subagent is allowed to invoke. Names must match the
    /// `name()` return value of a registered engine tool in the parent registry.
    pub allowed_tools: Vec<String>,
    /// Prompt fragment appended after the parent system prompt and memory blob.
    /// Instructs the child on its role and expected output format.
    pub system_prompt_fragment: String,
    /// Optional model override. When `None` the child inherits the parent's
    /// configured model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<ModelId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_id_display() {
        let mid = ModelId::new("claude-opus-4-7");
        assert_eq!(mid.to_string(), "claude-opus-4-7");
    }

    #[test]
    fn subagent_definition_serde_roundtrip() {
        let def = SubagentDefinition {
            id: "TestAgent".to_string(),
            description: "A test subagent".to_string(),
            allowed_tools: vec!["Read".to_string(), "Glob".to_string()],
            system_prompt_fragment: "Be concise.".to_string(),
            model_override: Some(ModelId::new("claude-sonnet-4-6")),
        };
        let json = serde_json::to_string(&def).expect("serialize");
        let decoded: SubagentDefinition = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.id, "TestAgent");
        assert_eq!(decoded.allowed_tools, vec!["Read", "Glob"]);
        assert_eq!(
            decoded.model_override.as_ref().map(|m| m.as_str()),
            Some("claude-sonnet-4-6")
        );
    }

    #[test]
    fn subagent_definition_no_model_override_omitted_in_json() {
        let def = SubagentDefinition {
            id: "MinimalAgent".to_string(),
            description: "Minimal".to_string(),
            allowed_tools: vec![],
            system_prompt_fragment: String::new(),
            model_override: None,
        };
        let json = serde_json::to_string(&def).expect("serialize");
        assert!(!json.contains("model_override"), "None should be omitted");
    }
}
