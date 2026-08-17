use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub ts: DateTime<Utc>,
    pub role: TranscriptRole,
    pub content: String,
    pub event_type: String,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    /// When true, the UI suppresses this entry (synthetic injections like
    /// skill-body loads). The entry is still persisted so the agent's context
    /// window includes it on the next turn.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden_from_user: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TranscriptRole {
    Agent { agent: String },
    Schedule { task_id: String },
    System(String),
}

/// Which transcript file a [`PaginationCursor`]'s `byte_offset` addresses.
///
/// A branch thread's own file only holds post-fork turns; pre-fork history
/// lives in the SOURCE thread's file instead. `Own` (the default, so cursors
/// minted before this field existed keep working) points into the requested
/// thread's own transcript. `Inherited` means pagination has walked off the
/// start of a branch thread's own file and `byte_offset` now addresses the
/// branch's SOURCE thread transcript instead — the caller must route the
/// next "load older" read to that file, still filtered to `ts <=
/// history_floor_ts` so post-fork source writes never leak into the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorPhase {
    #[default]
    Own,
    Inherited,
}

/// Cursor for byte-offset-based pagination through JSONL transcript files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginationCursor {
    /// Byte offset of the first byte of the oldest returned message line in the file.
    pub byte_offset: u64,
    /// The `ts` field (as ISO8601 string) of the oldest returned message.
    pub last_message_id: String,
    /// Parsed DateTime<Utc> of the oldest returned message.
    pub timestamp: DateTime<Utc>,
    /// Which file `byte_offset` is relative to. Defaults to `Own` on
    /// deserialize so cursors round-tripped by older clients still resolve
    /// correctly (see [`CursorPhase`]).
    #[serde(default)]
    pub phase: CursorPhase,
}

/// Paginated response wrapping entries and an optional cursor for fetching more.
/// When `cursor` is `None`, the start of the file has been reached.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginatedResponse<T> {
    pub entries: Vec<T>,
    pub cursor: Option<PaginationCursor>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_entry() -> TranscriptEntry {
        TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::System("user".to_string()),
            content: "hi".to_string(),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        }
    }

    #[test]
    fn hidden_from_user_defaults_to_false_on_deserialize() {
        let wire = r#"{"ts":"2026-04-20T00:00:00Z","role":"user","content":"hi","event_type":"message","metadata":null}"#;
        let entry: TranscriptEntry = serde_json::from_str(wire).unwrap();
        assert!(!entry.hidden_from_user);
    }

    #[test]
    fn hidden_from_user_false_is_skipped_on_serialize() {
        let json = serde_json::to_string(&base_entry()).unwrap();
        assert!(
            !json.contains("hidden_from_user"),
            "false flag should be omitted to keep the wire format backwards compatible: {json}"
        );
    }

    #[test]
    fn hidden_from_user_true_round_trips() {
        let mut entry = base_entry();
        entry.hidden_from_user = true;
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"hidden_from_user\":true"));
        let rt: TranscriptEntry = serde_json::from_str(&json).unwrap();
        assert!(rt.hidden_from_user);
    }
}
