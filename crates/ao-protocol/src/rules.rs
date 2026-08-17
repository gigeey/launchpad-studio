use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Source that introduced a skill or rule into the agent's library.
///
/// Shared between the skills and rules subsystems — rules reuse the same
/// provenance taxonomy. Keep this enum in one place so the on-disk JSON
/// serialization (lowercase tags) stays identical for both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AddedBy {
    User,
    Agent,
    Github,
    Link,
}

/// Full wire-level representation of a rule file, including its markdown body.
///
/// Mirrors `SkillDto` shape plus a `content` field — rules load their full
/// body into the agent context snapshot, whereas skills only expose metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuleDto {
    pub id: String,
    pub title: String,
    pub description: String,
    pub added_by: AddedBy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub auto_sync: bool,
    pub enabled: bool,
    pub updated_on: DateTime<Utc>,
    pub added_on: DateTime<Utc>,
    pub content: String,
}

/// Sidecar manifest persisted next to a rule file / bundle.
///
/// Top-level bundles carry `.manifest.json` with all fields populated;
/// nested rules write a sibling `<filename>.manifest.json` containing just
/// the `enabled` override and inherit the rest from their parent bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuleManifest {
    pub added_by: AddedBy,
    pub enabled: bool,
    pub auto_sync: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub imported_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dto() -> RuleDto {
        let ts = DateTime::parse_from_rfc3339("2026-04-19T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        RuleDto {
            id: "my-bundle/inner/strict.md".to_string(),
            title: "Strict Mode".to_string(),
            description: "Require explicit confirmation.".to_string(),
            added_by: AddedBy::Github,
            source_url: Some("https://github.com/example/rules".to_string()),
            auto_sync: true,
            enabled: true,
            updated_on: ts,
            added_on: ts,
            content: "# Strict Mode\n\nAlways confirm.".to_string(),
        }
    }

    #[test]
    fn rule_dto_json_round_trip() {
        let dto = sample_dto();
        let json = serde_json::to_string(&dto).expect("serialize");
        let back: RuleDto = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(dto, back);
    }

    #[test]
    fn rule_manifest_json_round_trip() {
        let ts = DateTime::parse_from_rfc3339("2026-04-19T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let manifest = RuleManifest {
            added_by: AddedBy::User,
            enabled: false,
            auto_sync: false,
            source_url: None,
            imported_at: ts,
        };
        let json = serde_json::to_string(&manifest).expect("serialize");
        let back: RuleManifest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(manifest, back);
    }

    #[test]
    fn added_by_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&AddedBy::User).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&AddedBy::Agent).unwrap(), "\"agent\"");
        assert_eq!(serde_json::to_string(&AddedBy::Github).unwrap(), "\"github\"");
        assert_eq!(serde_json::to_string(&AddedBy::Link).unwrap(), "\"link\"");
    }
}
