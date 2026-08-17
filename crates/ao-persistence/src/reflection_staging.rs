use ao_protocol::error::AoError;
use ao_protocol::reflection_candidate::{ReflectionCandidate, ReflectionCandidateStatus};

use crate::paths::DataRoot;

/// JSONL-based persistence for [`ReflectionCandidate`]s — the durable
/// "not live yet" representation the reflection pass
/// writes to instead of a live memory/skill store.
///
/// This exists because the reflection pass runs out-of-band: unlike an
/// in-turn `MemoryWrite`/`SkillRegister` call, whose "staged, not applied"
/// tool result at least reaches the calling turn, nobody is watching a live
/// turn when this pass runs. Without a durable record, a transient "staged"
/// response would simply be discarded and the candidate lost.
///
/// Each agent gets one candidate file living next to that agent's transcript
/// (see [`DataRoot::agent_reflection_staging_path`]), one JSON record per
/// line — the same append-only shape [`crate::outcome::OutcomeStore`] and
/// [`crate::transcript::TranscriptStore`] use.
///
/// Confirming or rejecting a staged candidate (flipping `status` away from
/// `Pending`) is the human-facing review surface, which is not
/// built yet, so this store is append/read-only today — there is nothing to
/// update in place.
pub struct ReflectionStagingStore {
    data_root: DataRoot,
}

impl ReflectionStagingStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Append one candidate to the agent's staging file, creating parent
    /// directories on first write.
    pub async fn stage(
        &self,
        agent_id: &str,
        candidate: &ReflectionCandidate,
    ) -> Result<(), AoError> {
        let path = self.data_root.agent_reflection_staging_path(agent_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let line =
            serde_json::to_string(candidate).map_err(|e| AoError::Json(e.to_string()))?;

        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(format!("{line}\n").as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Read every persisted candidate for an agent, oldest first. Returns an
    /// empty vec when the file does not exist yet.
    pub async fn read_all(&self, agent_id: &str) -> Result<Vec<ReflectionCandidate>, AoError> {
        let path = self.data_root.agent_reflection_staging_path(agent_id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let contents = tokio::fs::read_to_string(&path).await?;
        let mut candidates = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            candidates.push(serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?);
        }
        Ok(candidates)
    }

    /// Candidates still awaiting review — the list a future confirm/reject
    /// surface and the skill-generalization pass both read from.
    pub async fn list_pending(&self, agent_id: &str) -> Result<Vec<ReflectionCandidate>, AoError> {
        Ok(self
            .read_all(agent_id)
            .await?
            .into_iter()
            .filter(|c| c.status == ReflectionCandidateStatus::Pending)
            .collect())
    }

    /// Flip `status` on every candidate in `ids` and rewrite the file.
    ///
    /// The distillation pass (`ao_engine::skill_distillation`) is the
    /// first writer: once a group of repeated `Skill` candidates has been
    /// folded into one generalized template, their status moves to
    /// [`ReflectionCandidateStatus::Distilled`] so a later pass never
    /// re-clusters the same observations into a second template. Unlike
    /// [`Self::stage`], this rewrites the whole file rather than appending —
    /// there is exactly one line per candidate id at all times, so `read_all`
    /// never has to reason about which of several lines for the same id is
    /// authoritative.
    pub async fn update_status(
        &self,
        agent_id: &str,
        ids: &[String],
        status: ReflectionCandidateStatus,
    ) -> Result<(), AoError> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut all = self.read_all(agent_id).await?;
        if all.is_empty() {
            // Nothing staged for this agent yet (or the file doesn't exist) —
            // leave it untouched rather than creating an empty file.
            return Ok(());
        }
        let id_set: std::collections::HashSet<&String> = ids.iter().collect();
        for candidate in all.iter_mut() {
            if id_set.contains(&candidate.id) {
                candidate.status = status;
            }
        }

        let path = self.data_root.agent_reflection_staging_path(agent_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut content = String::new();
        for candidate in &all {
            let line =
                serde_json::to_string(candidate).map_err(|e| AoError::Json(e.to_string()))?;
            content.push_str(&line);
            content.push('\n');
        }
        let tmp_path = path.with_extension("jsonl.tmp");
        tokio::fs::write(&tmp_path, &content).await?;
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::outcome::ArtifactKind;
    use chrono::Utc;

    fn make_candidate(id: &str, kind: ArtifactKind) -> ReflectionCandidate {
        ReflectionCandidate {
            id: id.to_string(),
            kind,
            agent_id: "agent-1".to_string(),
            source_thread_id: "thread-1".to_string(),
            content: "some proposed content".to_string(),
            status: ReflectionCandidateStatus::Pending,
            target_scope: ao_protocol::memory::MemoryScope::Agent,
            target_scope_key: Some("agent-1".to_string()),
            contradicts: None,
            reason: "self-improvement candidate defaults to quarantine pending confirmation"
                .to_string(),
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn stage_then_read_all_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ReflectionStagingStore::new(data_root);

        store
            .stage("agent-1", &make_candidate("cand-1", ArtifactKind::Memory))
            .await
            .unwrap();
        store
            .stage("agent-1", &make_candidate("cand-2", ArtifactKind::Skill))
            .await
            .unwrap();

        let all = store.read_all("agent-1").await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "cand-1");
        assert_eq!(all[1].id, "cand-2");
    }

    #[tokio::test]
    async fn read_all_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ReflectionStagingStore::new(data_root);

        let all = store.read_all("nonexistent-agent").await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn list_pending_excludes_non_pending_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ReflectionStagingStore::new(data_root);

        let mut confirmed = make_candidate("cand-confirmed", ArtifactKind::Memory);
        confirmed.status = ReflectionCandidateStatus::Confirmed;
        store.stage("agent-1", &confirmed).await.unwrap();
        store
            .stage("agent-1", &make_candidate("cand-pending", ArtifactKind::Skill))
            .await
            .unwrap();

        let pending = store.list_pending("agent-1").await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "cand-pending");
    }

    #[tokio::test]
    async fn staging_file_sits_next_to_transcript_file() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ReflectionStagingStore::new(data_root.clone());

        store
            .stage("agent-1", &make_candidate("cand-1", ArtifactKind::Memory))
            .await
            .unwrap();

        let transcript_dir = data_root.agent_transcript_path("agent-1");
        let staging_path = data_root.agent_reflection_staging_path("agent-1");
        assert_eq!(transcript_dir.parent(), staging_path.parent());
    }

    #[tokio::test]
    async fn candidates_from_different_agents_do_not_mix() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ReflectionStagingStore::new(data_root);

        let mut for_agent_2 = make_candidate("cand-1", ArtifactKind::Memory);
        for_agent_2.agent_id = "agent-2".to_string();
        store.stage("agent-1", &make_candidate("cand-1", ArtifactKind::Memory)).await.unwrap();
        store.stage("agent-2", &for_agent_2).await.unwrap();

        assert_eq!(store.read_all("agent-1").await.unwrap().len(), 1);
        assert_eq!(store.read_all("agent-2").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_status_flips_only_the_named_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ReflectionStagingStore::new(data_root);

        store.stage("agent-1", &make_candidate("cand-1", ArtifactKind::Skill)).await.unwrap();
        store.stage("agent-1", &make_candidate("cand-2", ArtifactKind::Skill)).await.unwrap();
        store.stage("agent-1", &make_candidate("cand-3", ArtifactKind::Skill)).await.unwrap();

        store
            .update_status(
                "agent-1",
                &["cand-1".to_string(), "cand-3".to_string()],
                ReflectionCandidateStatus::Distilled,
            )
            .await
            .unwrap();

        let all = store.read_all("agent-1").await.unwrap();
        assert_eq!(all.len(), 3, "rewrite must not duplicate or drop lines");
        let by_id = |id: &str| all.iter().find(|c| c.id == id).unwrap();
        assert_eq!(by_id("cand-1").status, ReflectionCandidateStatus::Distilled);
        assert_eq!(by_id("cand-2").status, ReflectionCandidateStatus::Pending);
        assert_eq!(by_id("cand-3").status, ReflectionCandidateStatus::Distilled);

        let pending = store.list_pending("agent-1").await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "cand-2");
    }

    #[tokio::test]
    async fn update_status_on_missing_file_is_a_harmless_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ReflectionStagingStore::new(data_root);

        store
            .update_status(
                "nonexistent-agent",
                &["cand-1".to_string()],
                ReflectionCandidateStatus::Distilled,
            )
            .await
            .unwrap();
        assert!(store.read_all("nonexistent-agent").await.unwrap().is_empty());
    }
}
