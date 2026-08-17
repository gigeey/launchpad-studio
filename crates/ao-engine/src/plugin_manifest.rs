use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A path selector in the manifest: either a single string (folder path) or an
/// explicit list of folders/files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PathSelector {
    Single(String),
    Multiple(Vec<String>),
}

impl PathSelector {
    pub fn as_vec(&self) -> Vec<String> {
        match self {
            PathSelector::Single(s) => vec![s.clone()],
            PathSelector::Multiple(v) => v.clone(),
        }
    }
}

/// Parsed plugin manifest. Supports the widely-used `.claude-plugin/plugin.json`
/// layout so the importer can consume both Launchpad-native and
/// third-party-compatible manifests.
///
/// Unknown top-level fields are ignored (forward-compatible).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<serde_json::Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<PathSelector>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<PathSelector>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<PathSelector>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<PathSelector>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<serde_json::Value>,

    /// Optional inline MCP server declarations. Same shape as a `.mcp.json`
    /// `mcpServers` object: each key is a server name, each value is either a
    /// stdio entry (`command`, `args`, `env`) or an HTTP entry
    /// (`type: "http"`, `url`). When present, this field takes precedence over
    /// any `.mcp.json` file at the plugin root.
    #[serde(rename = "mcpServers", default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<serde_json::Value>,
}

#[derive(Debug, Error)]
pub enum PluginManifestError {
    #[error("plugin manifest: invalid JSON: {0}")]
    InvalidJson(String),

    #[error("plugin manifest: missing required field `{0}`")]
    MissingField(&'static str),

    #[error("plugin manifest: field `{field}` must be {expected}")]
    InvalidField {
        field: &'static str,
        expected: &'static str,
    },
}

pub fn parse_manifest(source: &str) -> Result<PluginManifest, PluginManifestError> {
    let value: serde_json::Value =
        serde_json::from_str(source).map_err(|e| PluginManifestError::InvalidJson(e.to_string()))?;

    let obj = value
        .as_object()
        .ok_or(PluginManifestError::InvalidField {
            field: "<root>",
            expected: "a JSON object",
        })?;

    let name = obj
        .get("name")
        .ok_or(PluginManifestError::MissingField("name"))?
        .as_str()
        .ok_or(PluginManifestError::InvalidField {
            field: "name",
            expected: "a string",
        })?;
    if name.trim().is_empty() {
        return Err(PluginManifestError::InvalidField {
            field: "name",
            expected: "a non-empty string",
        });
    }

    let version = obj
        .get("version")
        .ok_or(PluginManifestError::MissingField("version"))?
        .as_str()
        .ok_or(PluginManifestError::InvalidField {
            field: "version",
            expected: "a string",
        })?;
    if version.trim().is_empty() {
        return Err(PluginManifestError::InvalidField {
            field: "version",
            expected: "a non-empty string",
        });
    }

    serde_json::from_value::<PluginManifest>(value)
        .map_err(|e| PluginManifestError::InvalidJson(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_array_skills_shape_like_andrej_karpathy_skills() {
        let src = r#"{
            "name": "andrej-karpathy-skills",
            "version": "0.1.0",
            "description": "Karpathy's skills",
            "skills": ["skills/tokenization", "skills/backprop", "skills/attention"]
        }"#;
        let m = parse_manifest(src).expect("should parse");
        assert_eq!(m.name, "andrej-karpathy-skills");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(
            m.skills.as_ref().map(PathSelector::as_vec),
            Some(vec![
                "skills/tokenization".to_string(),
                "skills/backprop".to_string(),
                "skills/attention".to_string(),
            ])
        );
    }

    #[test]
    fn parses_string_plus_array_shape_like_superpowers() {
        let src = r#"{
            "name": "superpowers",
            "version": "1.2.3",
            "description": "Superpowers plugin",
            "skills": "skills",
            "rules": ["rules/core", "rules/style", "rules/safety"]
        }"#;
        let m = parse_manifest(src).expect("should parse");
        assert_eq!(m.name, "superpowers");
        assert_eq!(
            m.skills.as_ref().map(PathSelector::as_vec),
            Some(vec!["skills".to_string()])
        );
        assert_eq!(
            m.rules.as_ref().map(PathSelector::as_vec),
            Some(vec![
                "rules/core".to_string(),
                "rules/style".to_string(),
                "rules/safety".to_string(),
            ])
        );
    }

    #[test]
    fn rejects_malformed_json() {
        let src = r#"{ "name": "broken", "version": "1.0" "#; // missing closing brace
        let err = parse_manifest(src).expect_err("should fail");
        assert!(matches!(err, PluginManifestError::InvalidJson(_)));
    }

    #[test]
    fn rejects_missing_name() {
        let src = r#"{ "version": "1.0.0" }"#;
        let err = parse_manifest(src).expect_err("should fail");
        match err {
            PluginManifestError::MissingField(f) => assert_eq!(f, "name"),
            other => panic!("expected MissingField(name), got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_version() {
        let src = r#"{ "name": "x" }"#;
        let err = parse_manifest(src).expect_err("should fail");
        match err {
            PluginManifestError::MissingField(f) => assert_eq!(f, "version"),
            other => panic!("expected MissingField(version), got {other:?}"),
        }
    }

    #[test]
    fn error_message_is_descriptive() {
        let src = r#"{ "version": "1.0.0" }"#;
        let err = parse_manifest(src).expect_err("should fail");
        let msg = format!("{err}");
        assert!(msg.contains("name"), "error message should mention `name`: {msg}");
        assert!(
            msg.to_lowercase().contains("missing") || msg.to_lowercase().contains("required"),
            "error should describe the problem: {msg}"
        );
    }

    #[test]
    fn ignores_unknown_fields() {
        let src = r#"{
            "name": "forward-compat",
            "version": "0.1.0",
            "futureField": 42,
            "anotherUnknown": { "nested": true }
        }"#;
        let m = parse_manifest(src).expect("unknown fields should be ignored");
        assert_eq!(m.name, "forward-compat");
        assert_eq!(m.version, "0.1.0");
    }

    #[test]
    fn accepts_all_optional_fields() {
        let src = r#"{
            "name": "full",
            "version": "0.1.0",
            "description": "desc",
            "author": { "name": "Ada" },
            "skills": "skills",
            "rules": ["rules/a.md"],
            "agents": "agents",
            "commands": "commands",
            "hooks": { "pre": ["cmd"] }
        }"#;
        let m = parse_manifest(src).expect("should parse full manifest");
        assert!(m.description.is_some());
        assert!(m.author.is_some());
        assert!(m.agents.is_some());
        assert!(m.commands.is_some());
        assert!(m.hooks.is_some());
    }

    #[test]
    fn rejects_empty_name() {
        let src = r#"{ "name": "", "version": "1.0.0" }"#;
        let err = parse_manifest(src).expect_err("empty name should fail");
        assert!(matches!(err, PluginManifestError::InvalidField { field: "name", .. }));
    }

    #[test]
    fn rejects_non_string_name() {
        let src = r#"{ "name": 42, "version": "1.0.0" }"#;
        let err = parse_manifest(src).expect_err("non-string name should fail");
        assert!(matches!(err, PluginManifestError::InvalidField { field: "name", .. }));
    }
}
