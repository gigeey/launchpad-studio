use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use ao_engine_tools_core::background_agents::sidechain_persister::{
    SidechainEventMeta, SidechainPersister,
};
use ao_engine_tools_core::background_agents::RunnerEvent;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

/// Persists sidechain child events as JSONL transcript entries under
/// `<data_root>/messages/data/<child_agent_id>.jsonl`.
///
/// Every entry carries `parent_agent_id`, `background_agent_id`,
/// `subagent_type`, and `spawned_at` in its `metadata` field so the UI loader
/// can render the sidechain card and link it back to the parent session.
pub struct FileSidechainPersister {
    data_root: PathBuf,
}

impl FileSidechainPersister {
    /// Create a persister rooted at `data_root` (used in tests with a temp dir).
    pub fn new(data_root: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            data_root: data_root.into(),
        })
    }

    /// Create a persister resolving the data root from `LAUNCHPAD_STUDIO_DATA_DIR`
    /// or `~/.launchpad_studio`.
    pub fn resolve() -> Result<Arc<Self>, ao_protocol::error::AoError> {
        let root = ao_protocol::data_root::resolve_data_root()?;
        Ok(Self::new(root))
    }

    fn transcript_path(&self, agent_id: &str) -> PathBuf {
        self.data_root
            .join("messages")
            .join("data")
            .join(format!("{}.jsonl", agent_id))
    }
}

#[async_trait]
impl SidechainPersister for FileSidechainPersister {
    async fn persist_event(&self, meta: &SidechainEventMeta, event: &RunnerEvent) {
        let (event_type, content) = event_to_parts(event);

        let mut metadata: HashMap<String, serde_json::Value> = HashMap::new();
        metadata.insert("parent_agent_id".to_string(), json!(meta.parent_agent_id));
        metadata.insert(
            "background_agent_id".to_string(),
            json!(meta.background_agent_id.as_str()),
        );
        metadata.insert("subagent_type".to_string(), json!(meta.subagent_type));
        metadata.insert("spawned_at".to_string(), json!(meta.spawned_at));

        let entry = TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::Agent {
                agent: meta.background_agent_id.to_string(),
            },
            content,
            event_type,
            metadata: Some(metadata),
            hidden_from_user: false,
        };

        let path = self.transcript_path(&meta.background_agent_id.to_string());

        if let Some(parent_dir) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent_dir).await {
                tracing::warn!(
                    "sidechain: failed to create transcript dir {:?}: {}",
                    parent_dir,
                    e
                );
                return;
            }
        }

        let line = match serde_json::to_string(&entry) {
            Ok(s) => format!("{s}\n"),
            Err(e) => {
                tracing::warn!("sidechain: failed to serialize event: {}", e);
                return;
            }
        };

        match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(line.as_bytes()).await {
                    tracing::warn!("sidechain: failed to write to {:?}: {}", path, e);
                }
            }
            Err(e) => {
                tracing::warn!("sidechain: failed to open {:?}: {}", path, e);
            }
        }
    }
}

fn event_to_parts(event: &RunnerEvent) -> (String, String) {
    match event {
        RunnerEvent::AssistantText { text, .. } => ("text_complete".to_string(), text.clone()),
        RunnerEvent::ToolUse { tool_name, .. } => ("tool_use".to_string(), tool_name.clone()),
        RunnerEvent::Completed { .. } => {
            ("session_completed".to_string(), "completed".to_string())
        }
        RunnerEvent::Cancelled { .. } => {
            ("session_cancelled".to_string(), "cancelled".to_string())
        }
        RunnerEvent::Failed { error, .. } => ("session_failed".to_string(), error.clone()),
        RunnerEvent::AsyncLaunched { subagent_type, .. } => {
            ("async_launched".to_string(), subagent_type.clone())
        }
    }
}
