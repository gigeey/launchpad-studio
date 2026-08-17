use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::attachment::Attachment;
use crate::scheduled_task::MessageSource;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub message_id: String,
    pub content: String,
    pub queued_at: DateTime<Utc>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub source: Option<MessageSource>,
    #[serde(default)]
    pub focus_path: Option<String>,
    /// Optional thread the message targets. `None` resolves to the agent's
    /// default thread at the runner so single-thread callers stay byte-equivalent.
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAck {
    pub message_id: String,
    pub status: String,
}
