use ao_protocol::bookmark::BookmarkEntry;
use ao_protocol::error::AoError;
use ao_protocol::transcript::TranscriptRole;
use chrono::Utc;

use crate::paths::DataRoot;

/// JSONL-based bookmark persistence.
/// Each agent has a `.jsonl` file with one JSON entry per line.
pub struct BookmarkStore {
    data_root: DataRoot,
}

impl BookmarkStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// List all bookmark entries for an agent. Returns empty vec if file not found.
    pub async fn list(&self, agent_id: &str) -> Result<Vec<BookmarkEntry>, AoError> {
        let path = self.data_root.agent_bookmark_path(agent_id);
        Self::read_entries(&path).await
    }

    /// Add a new bookmark entry for an agent. Creates the file if it doesn't exist.
    pub async fn add(
        &self,
        agent_id: &str,
        message_ts: &str,
        message_content: &str,
        message_role: TranscriptRole,
    ) -> Result<BookmarkEntry, AoError> {
        let path = self.data_root.agent_bookmark_path(agent_id);
        let entry = BookmarkEntry {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            message_ts: message_ts.to_string(),
            message_content: message_content.to_string(),
            message_role,
            created_at: Utc::now(),
        };
        Self::append_entry(&path, &entry).await?;
        Ok(entry)
    }

    /// Delete a bookmark entry for an agent by ID. Returns true if found and deleted.
    pub async fn delete(&self, agent_id: &str, bookmark_id: &str) -> Result<bool, AoError> {
        let path = self.data_root.agent_bookmark_path(agent_id);
        Self::remove_entry(&path, bookmark_id).await
    }

    /// Check if a bookmark already exists for a given agent_id + message_ts combination.
    pub async fn exists(&self, agent_id: &str, message_ts: &str) -> Result<bool, AoError> {
        let entries = self.list(agent_id).await?;
        Ok(entries.iter().any(|e| e.message_ts == message_ts))
    }

    // --- Private helpers ---

    async fn read_entries(path: &std::path::Path) -> Result<Vec<BookmarkEntry>, AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }

        let contents = tokio::fs::read_to_string(path).await?;
        let mut entries = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: BookmarkEntry =
                serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    async fn append_entry(
        path: &std::path::Path,
        entry: &BookmarkEntry,
    ) -> Result<(), AoError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let line = serde_json::to_string(entry).map_err(|e| AoError::Json(e.to_string()))?;
        let line_with_newline = format!("{}\n", line);

        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        file.write_all(line_with_newline.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    async fn remove_entry(path: &std::path::Path, bookmark_id: &str) -> Result<bool, AoError> {
        let entries = Self::read_entries(path).await?;
        let original_len = entries.len();
        let filtered: Vec<&BookmarkEntry> =
            entries.iter().filter(|e| e.id != bookmark_id).collect();

        if filtered.len() == original_len {
            return Ok(false);
        }

        let tmp_path = path.with_extension("jsonl.tmp");

        let mut content = String::new();
        for entry in &filtered {
            let line = serde_json::to_string(entry).map_err(|e| AoError::Json(e.to_string()))?;
            content.push_str(&line);
            content.push('\n');
        }

        tokio::fs::write(&tmp_path, &content).await?;
        tokio::fs::rename(&tmp_path, path).await?;
        Ok(true)
    }
}
