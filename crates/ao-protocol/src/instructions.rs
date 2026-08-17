use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Wire-level representation of an instruction file discovered at the root
/// of an agent's home directory.
///
/// `id` carries the actual on-disk filename (case preserved) so the client
/// can round-trip toggle requests without mangling the casing the user
/// chose when they dropped the file in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstructionDto {
    pub id: String,
    pub name: String,
    pub path: String,
    pub enabled: bool,
    pub updated_on: DateTime<Utc>,
    pub content: String,
}

/// Sidecar manifest persisted under `{agent_home}/.instructions/<filename>.manifest.json`.
///
/// Only written when the user toggles the default-`true` enable state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstructionManifest {
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_dto_json_round_trip() {
        let ts = DateTime::parse_from_rfc3339("2026-04-19T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let dto = InstructionDto {
            id: "CLAUDE.md".to_string(),
            name: "CLAUDE.md".to_string(),
            path: "CLAUDE.md".to_string(),
            enabled: true,
            updated_on: ts,
            content: "# Instructions\n\nBe helpful.".to_string(),
        };
        let json = serde_json::to_string(&dto).expect("serialize");
        let back: InstructionDto = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(dto, back);
    }

    #[test]
    fn instruction_manifest_json_round_trip() {
        let manifest = InstructionManifest { enabled: false };
        let json = serde_json::to_string(&manifest).expect("serialize");
        let back: InstructionManifest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(manifest, back);
    }
}
