use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::transcript::TranscriptRole;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookmarkEntry {
    pub id: String,
    pub agent_id: String,
    pub message_ts: String,
    pub message_content: String,
    pub message_role: TranscriptRole,
    pub created_at: DateTime<Utc>,
}
