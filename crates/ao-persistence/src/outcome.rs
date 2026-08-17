use std::path::Path;

use ao_protocol::error::AoError;
use ao_protocol::outcome::OutcomeRecord;

use crate::paths::DataRoot;

/// JSONL-based persistence for per-turn [`OutcomeRecord`]s.
///
/// Each agent gets one outcome file living next to that agent's transcript
/// (see [`DataRoot::agent_outcome_path`]), with one JSON record per line —
/// the same append-only shape [`crate::transcript::TranscriptStore`] uses.
pub struct OutcomeStore {
    data_root: DataRoot,
}

impl OutcomeStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Append one outcome record to the agent's outcome file, creating
    /// parent directories on first write.
    pub async fn append(&self, agent_id: &str, record: &OutcomeRecord) -> Result<(), AoError> {
        let path = self.data_root.agent_outcome_path(agent_id);
        self.append_at(&path, record).await
    }

    /// Path-addressed equivalent of [`Self::append`].
    pub async fn append_at(&self, path: &Path, record: &OutcomeRecord) -> Result<(), AoError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let line = serde_json::to_string(record).map_err(|e| AoError::Json(e.to_string()))?;

        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        file.write_all(format!("{line}\n").as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Read every persisted record for an agent, oldest first. Returns an
    /// empty vec when the file does not exist yet.
    pub async fn read_all(&self, agent_id: &str) -> Result<Vec<OutcomeRecord>, AoError> {
        let path = self.data_root.agent_outcome_path(agent_id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let contents = tokio::fs::read_to_string(&path).await?;
        let mut records = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            records.push(serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?);
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::outcome::{ArtifactRef, OutcomeSignal};
    use chrono::Utc;

    fn make_record(turn_id: &str) -> OutcomeRecord {
        OutcomeRecord {
            turn_id: turn_id.to_string(),
            session_id: "session-1".to_string(),
            artifacts_used: vec![
                ArtifactRef::memory("mem-1"),
                ArtifactRef::skill("review-pr"),
            ],
            signal: OutcomeSignal::Implicit,
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn append_then_read_all_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = OutcomeStore::new(data_root);

        store
            .append("agent-1", &make_record("turn-1"))
            .await
            .unwrap();
        store
            .append("agent-1", &make_record("turn-2"))
            .await
            .unwrap();

        let records = store.read_all("agent-1").await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].turn_id, "turn-1");
        assert_eq!(records[1].turn_id, "turn-2");
    }

    #[tokio::test]
    async fn read_all_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = OutcomeStore::new(data_root);

        let records = store.read_all("nonexistent-agent").await.unwrap();
        assert!(records.is_empty());
    }

    #[tokio::test]
    async fn outcome_file_sits_next_to_transcript_file() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = OutcomeStore::new(data_root.clone());

        store
            .append("agent-1", &make_record("turn-1"))
            .await
            .unwrap();

        let outcome_path = data_root.agent_outcome_path("agent-1");
        let transcript_path = data_root.agent_transcript_path("agent-1");
        assert_eq!(outcome_path.parent(), transcript_path.parent());
        assert!(tokio::fs::try_exists(&outcome_path).await.unwrap());
    }

    #[tokio::test]
    async fn distinct_agents_do_not_share_outcome_files() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = OutcomeStore::new(data_root);

        store
            .append("agent-a", &make_record("turn-1"))
            .await
            .unwrap();
        store
            .append("agent-b", &make_record("turn-2"))
            .await
            .unwrap();

        let a = store.read_all("agent-a").await.unwrap();
        let b = store.read_all("agent-b").await.unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert_eq!(a[0].turn_id, "turn-1");
        assert_eq!(b[0].turn_id, "turn-2");
    }
}
