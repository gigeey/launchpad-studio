use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;
use crate::tasklist::{TaskId, TasklistId};

/// One line of a tasklist's hidden changelog. Appended every time a CLI
/// agent emits a parsed `<task-item-notification>` block alongside its
/// terminal `<task action="…">` tag. Persists alongside other tasklist
/// outputs so the co-pilot can read recent activity for context injection
/// without re-walking transcripts.
///
/// `status` mirrors the literal status string carried by
/// `<task-item-notification>` (`complete | failed | needs_clarification`)
/// so the changelog round-trips the producing agent's wording rather than
/// re-deriving it. `details` is optional for the same reason — the
/// notification block treats it as optional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangelogEntry {
    pub task_id: TaskId,
    pub tasklist_id: TasklistId,
    pub agent_id: AgentId,
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    pub ts: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changelog_entry_round_trips_with_and_without_details() {
        let with = ChangelogEntry {
            task_id: "t-1".into(),
            tasklist_id: "tl-1".into(),
            agent_id: "agent-x".into(),
            status: "complete".into(),
            summary: "shipped".into(),
            details: Some("did the thing".into()),
            ts: Utc::now(),
        };
        let json = serde_json::to_string(&with).unwrap();
        let parsed: ChangelogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, with);

        let without = ChangelogEntry {
            details: None,
            ..with.clone()
        };
        let json = serde_json::to_string(&without).unwrap();
        // `details` is skipped when None so the on-the-wire JSON stays clean.
        assert!(!json.contains("\"details\""), "details should be omitted: {json}");
        let parsed: ChangelogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, without);
    }
}
