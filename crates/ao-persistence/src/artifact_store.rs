use std::sync::Arc;

use ao_protocol::artifact::{
    ArtifactHistoryEntry, ArtifactKind, ArtifactRecord, CapabilitySpec, IntentLedgerEntry, IntentSource,
    OriginIntent, PayloadFormat, RefreshIntent, ARTIFACT_HISTORY_MAX_LEN, INTENT_LEDGER_MAX_LEN,
};
use ao_protocol::error::AoError;
use chrono::Utc;
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::paths::DataRoot;

/// The fields a caller supplies to create a new artifact. Bundled into one
/// struct because [`ArtifactStore::create`] otherwise needs a long positional
/// argument list for what is really one semantic request.
pub struct NewArtifact {
    pub title: String,
    pub kind: ArtifactKind,
    pub format: PayloadFormat,
    pub payload: Vec<u8>,
    pub refresh_intent: RefreshIntent,
    pub origin_intent: Option<OriginIntent>,
    pub capabilities: Vec<CapabilitySpec>,
    pub source_message_id: Option<String>,
    /// Point-in-time summary of what this artifact is for, seeded straight
    /// into `intent_ledger`'s creation entry. `None` if the caller supplied
    /// none — see [`IntentLedgerEntry::intent_note`].
    pub intent_note: Option<String>,
}

/// Push one entry onto an artifact's intent ledger, trimming back down to
/// [`INTENT_LEDGER_MAX_LEN`] when it overflows. The creation entry at index 0
/// is never evicted — only one entry is ever pushed per call, so removing the
/// entry at index 1 (the oldest non-creation entry) is always enough to
/// restore the bound.
fn push_intent_ledger_entry(ledger: &mut Vec<IntentLedgerEntry>, entry: IntentLedgerEntry) {
    ledger.push(entry);
    if ledger.len() > INTENT_LEDGER_MAX_LEN {
        ledger.remove(1);
    }
}

/// File extension a blob is stored under for a given payload format. Shared
/// by [`ArtifactStore::create`]'s main blob and
/// [`ArtifactStore::push_history_snapshot`]'s undo-history blobs, so the two
/// can never drift out of sync on how a format maps to a filename.
fn extension_for_format(format: PayloadFormat) -> &'static str {
    match format {
        PayloadFormat::Json => "json",
        PayloadFormat::Html => "html",
    }
}

/// Bump an artifact record's refresh bookkeeping (`updated_at`,
/// `last_refreshed_at`, `refresh_count`) and append one `intent_ledger`
/// entry. Shared by [`ArtifactStore::refresh`]'s normal overwrite and
/// [`ArtifactStore::undo`]'s restore overwrite — the two differ only in
/// whether a history snapshot is pushed beforehand (refresh does, undo must
/// not), so this shared step deliberately never touches `history` itself;
/// that stays each caller's own responsibility.
fn apply_refresh_bookkeeping(
    record: &mut ArtifactRecord,
    payload: &[u8],
    source: IntentSource,
    intent_note: Option<String>,
    source_message_id: Option<String>,
) {
    let now = Utc::now();
    record.size_bytes = payload.len() as u64;
    record.checksum_sha256 = hex_sha256(payload);
    record.updated_at = now;
    record.last_refreshed_at = Some(now);
    record.refresh_count += 1;
    push_intent_ledger_entry(
        &mut record.intent_ledger,
        IntentLedgerEntry {
            timestamp: now,
            source,
            intent_note,
            source_message_id,
        },
    );
}

/// Registry-JSON + file-blob persistence for artifacts, scoped per agent:
/// `{root}/artifacts/{agent_id}/registry.json` plus payload blobs under
/// `{root}/artifacts/{agent_id}/blobs/`. Read-modify-write sequences on the
/// registry are serialized per agent via an in-memory mutex map, and writes
/// land atomically through a temp-file-then-rename swap.
pub struct ArtifactStore {
    data_root: DataRoot,
    locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
}

impl ArtifactStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self {
            data_root,
            locks: Arc::new(DashMap::new()),
        }
    }

    fn lock_for(&self, agent_id: &str) -> Arc<Mutex<()>> {
        self.locks
            .entry(agent_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Write the payload blob to disk and append a new record to the agent's
    /// registry.
    pub async fn create(&self, agent_id: &str, new_artifact: NewArtifact) -> Result<ArtifactRecord, AoError> {
        let lock = self.lock_for(agent_id);
        let _guard = lock.lock().await;

        let id = uuid::Uuid::new_v4().to_string();
        let stored_filename = format!("{id}.{}", extension_for_format(new_artifact.format));

        let blobs_dir = self.data_root.artifact_blobs_dir(agent_id);
        tokio::fs::create_dir_all(&blobs_dir).await?;
        tokio::fs::write(blobs_dir.join(&stored_filename), &new_artifact.payload).await?;

        let now = Utc::now();
        let record = ArtifactRecord {
            id,
            title: new_artifact.title,
            kind: new_artifact.kind,
            format: new_artifact.format,
            size_bytes: new_artifact.payload.len() as u64,
            checksum_sha256: hex_sha256(&new_artifact.payload),
            stored_filename,
            refresh_intent: new_artifact.refresh_intent,
            origin_intent: new_artifact.origin_intent,
            capabilities: new_artifact.capabilities,
            source_message_id: new_artifact.source_message_id.clone(),
            created_at: now,
            updated_at: now,
            last_refreshed_at: None,
            refresh_count: 0,
            pinned: false,
            pinned_at: None,
            group_id: None,
            intent_ledger: vec![IntentLedgerEntry {
                timestamp: now,
                source: IntentSource::Create,
                intent_note: new_artifact.intent_note,
                source_message_id: new_artifact.source_message_id,
            }],
            // CREATE has no prior body, so there is nothing to snapshot yet.
            history: Vec::new(),
            next_history_seq: 0,
        };

        let mut registry = self.read_registry(agent_id).await?;
        registry.push(record.clone());
        self.write_registry(agent_id, &registry).await?;

        Ok(record)
    }

    /// List every artifact record for an agent (metadata only — no payload
    /// bytes).
    pub async fn list_by_agent(&self, agent_id: &str) -> Result<Vec<ArtifactRecord>, AoError> {
        self.read_registry(agent_id).await
    }

    /// Fetch a single artifact's record.
    pub async fn get(&self, agent_id: &str, artifact_id: &str) -> Result<ArtifactRecord, AoError> {
        let registry = self.read_registry(agent_id).await?;
        registry
            .into_iter()
            .find(|r| r.id == artifact_id)
            .ok_or_else(|| AoError::ArtifactNotFound(artifact_id.to_string()))
    }

    /// Fetch an artifact's record together with its current payload bytes.
    pub async fn get_payload(&self, agent_id: &str, artifact_id: &str) -> Result<(ArtifactRecord, Vec<u8>), AoError> {
        let record = self.get(agent_id, artifact_id).await?;
        let path = self.data_root.artifact_blobs_dir(agent_id).join(&record.stored_filename);
        let bytes = tokio::fs::read(&path).await?;
        Ok((record, bytes))
    }

    /// Replace an artifact's payload blob in place, bump its refresh
    /// bookkeeping (`updated_at`, `last_refreshed_at`, `refresh_count`),
    /// append one entry to `intent_ledger`, and — before any of that —
    /// snapshot the body being replaced onto the bounded undo history. This
    /// is the single choke point every edit-in-place surface (the
    /// `ArtifactWrite` tool, the `PUT .../refresh` route) flows through, so
    /// none of them need their own ledger or undo-history bookkeeping, and
    /// `undo` transparently covers every one of them uniformly.
    pub async fn refresh(
        &self,
        agent_id: &str,
        artifact_id: &str,
        payload: &[u8],
        source: IntentSource,
        intent_note: Option<String>,
        source_message_id: Option<String>,
    ) -> Result<ArtifactRecord, AoError> {
        let lock = self.lock_for(agent_id);
        let _guard = lock.lock().await;

        let mut registry = self.read_registry(agent_id).await?;
        let record = registry
            .iter_mut()
            .find(|r| r.id == artifact_id)
            .ok_or_else(|| AoError::ArtifactNotFound(artifact_id.to_string()))?;

        let path = self.data_root.artifact_blobs_dir(agent_id).join(&record.stored_filename);

        // Snapshot the body about to be overwritten BEFORE it's gone — see
        // `push_history_snapshot` for why this lives here, at the shared
        // choke point, rather than in each individual caller.
        let prior_body = tokio::fs::read(&path).await?;
        self.push_history_snapshot(agent_id, record, &prior_body).await?;

        tokio::fs::write(&path, payload).await?;
        apply_refresh_bookkeeping(record, payload, source, intent_note, source_message_id);
        let updated = record.clone();

        self.write_registry(agent_id, &registry).await?;
        Ok(updated)
    }

    /// Push `prior_body` onto `record`'s bounded undo-history stack as a new
    /// on-disk snapshot, evicting (and deleting the blob of) the oldest
    /// snapshot once the stack exceeds [`ARTIFACT_HISTORY_MAX_LEN`]. Called
    /// once per [`Self::refresh`] call, immediately before that call's own
    /// overwrite — never called from [`Self::undo`], since a restore write
    /// must not archive the very body it's discarding (that would push the
    /// body an undo just replaced right back onto the stack, so the next
    /// undo would silently no-op instead of going further back).
    ///
    /// The snapshot's recorded `source` is the provenance of the body being
    /// archived — i.e. the artifact's current last `intent_ledger` entry at
    /// the moment of the call, read before this call's caller appends its
    /// own new entry — not the edit that is about to replace it.
    async fn push_history_snapshot(
        &self,
        agent_id: &str,
        record: &mut ArtifactRecord,
        prior_body: &[u8],
    ) -> Result<(), AoError> {
        let source = record
            .intent_ledger
            .last()
            .map(|entry| entry.source)
            .unwrap_or(IntentSource::Create);

        let seq = record.next_history_seq;
        record.next_history_seq += 1;

        let history_dir = self.data_root.artifact_history_dir(agent_id);
        tokio::fs::create_dir_all(&history_dir).await?;
        let stored_filename = format!("{}-{seq}.{}", record.id, extension_for_format(record.format));
        tokio::fs::write(history_dir.join(&stored_filename), prior_body).await?;

        record.history.push(ArtifactHistoryEntry {
            seq,
            checksum_sha256: hex_sha256(prior_body),
            size_bytes: prior_body.len() as u64,
            timestamp: Utc::now(),
            source,
            stored_filename,
        });

        if record.history.len() > ARTIFACT_HISTORY_MAX_LEN {
            let evicted = record.history.remove(0);
            let evicted_path = history_dir.join(&evicted.stored_filename);
            if tokio::fs::try_exists(&evicted_path).await.unwrap_or(false) {
                tokio::fs::remove_file(&evicted_path).await?;
            }
        }

        Ok(())
    }

    /// Restore-mode counterpart to [`Self::refresh`]: pops the most recent
    /// entry off `history` and restores its body as the artifact's current
    /// body, going through the same bookkeeping bump (`updated_at`,
    /// `last_refreshed_at`, `refresh_count`, a new `intent_ledger` entry
    /// tagged [`IntentSource::Undo`]) as a normal edit. Deliberately does NOT
    /// call [`Self::push_history_snapshot`] — see that method's doc comment
    /// for why a restore write must never archive the body it replaces.
    ///
    /// Errors with [`AoError::Conflict`] when `history` is already empty
    /// (nothing left to undo); the caller maps that to an HTTP 409.
    pub async fn undo(
        &self,
        agent_id: &str,
        artifact_id: &str,
        source_message_id: Option<String>,
    ) -> Result<ArtifactRecord, AoError> {
        let lock = self.lock_for(agent_id);
        let _guard = lock.lock().await;

        let mut registry = self.read_registry(agent_id).await?;
        let record = registry
            .iter_mut()
            .find(|r| r.id == artifact_id)
            .ok_or_else(|| AoError::ArtifactNotFound(artifact_id.to_string()))?;

        // Pop-and-restore: the popped entry is removed from `history` right
        // here, so a corrupt double-undo of the same snapshot is structurally
        // impossible regardless of what happens below.
        let snapshot = record
            .history
            .pop()
            .ok_or_else(|| AoError::Conflict(format!("artifact '{artifact_id}' has no prior edit to undo")))?;

        let history_dir = self.data_root.artifact_history_dir(agent_id);
        let snapshot_path = history_dir.join(&snapshot.stored_filename);
        let snapshot_body = tokio::fs::read(&snapshot_path).await?;

        let path = self.data_root.artifact_blobs_dir(agent_id).join(&record.stored_filename);
        tokio::fs::write(&path, &snapshot_body).await?;
        apply_refresh_bookkeeping(
            record,
            &snapshot_body,
            IntentSource::Undo,
            Some("Reverted to the previous version.".to_string()),
            source_message_id,
        );
        let updated = record.clone();

        self.write_registry(agent_id, &registry).await?;

        // The restored snapshot is fully consumed — there is no redo, so
        // nothing can ever reference it again. Clean up its blob rather than
        // leaking it; best-effort, since the restore itself already
        // succeeded and is what the caller actually asked for.
        let _ = tokio::fs::remove_file(&snapshot_path).await;

        Ok(updated)
    }

    /// Flip an artifact's pinned flag. Metadata-only — unlike [`Self::refresh`]
    /// this never touches the payload blob or the refresh bookkeeping fields,
    /// since pinning doesn't change what the artifact contains. Also stamps
    /// (or clears) `pinned_at`, which drives the Assets sidebar's
    /// newest-pinned-first ordering — see [`ArtifactRecord::pinned_at`].
    pub async fn set_pinned(&self, agent_id: &str, artifact_id: &str, pinned: bool) -> Result<ArtifactRecord, AoError> {
        let lock = self.lock_for(agent_id);
        let _guard = lock.lock().await;

        let mut registry = self.read_registry(agent_id).await?;
        let record = registry
            .iter_mut()
            .find(|r| r.id == artifact_id)
            .ok_or_else(|| AoError::ArtifactNotFound(artifact_id.to_string()))?;
        record.pinned = pinned;
        record.pinned_at = if pinned { Some(Utc::now()) } else { None };
        let updated = record.clone();

        self.write_registry(agent_id, &registry).await?;
        Ok(updated)
    }

    /// Assign (or clear, via `group_id: None`) an artifact's group. Metadata-
    /// only, same shape as [`Self::set_pinned`].
    pub async fn set_group(
        &self,
        agent_id: &str,
        artifact_id: &str,
        group_id: Option<String>,
    ) -> Result<ArtifactRecord, AoError> {
        let lock = self.lock_for(agent_id);
        let _guard = lock.lock().await;

        let mut registry = self.read_registry(agent_id).await?;
        let record = registry
            .iter_mut()
            .find(|r| r.id == artifact_id)
            .ok_or_else(|| AoError::ArtifactNotFound(artifact_id.to_string()))?;
        record.group_id = group_id;
        let updated = record.clone();

        self.write_registry(agent_id, &registry).await?;
        Ok(updated)
    }

    /// Unset `group_id` on every artifact (across every agent) that
    /// references the given group — called when a group is deleted, so no
    /// artifact is left pointing at a group that no longer exists.
    pub async fn clear_group_across_agents(&self, group_id: &str) -> Result<(), AoError> {
        let agents_dir = self.data_root.artifacts_root_dir();
        if !tokio::fs::try_exists(&agents_dir).await.unwrap_or(false) {
            return Ok(());
        }
        let mut agent_entries = tokio::fs::read_dir(&agents_dir).await?;
        let mut agent_ids = Vec::new();
        while let Some(agent_entry) = agent_entries.next_entry().await? {
            if !agent_entry.file_type().await?.is_dir() {
                continue;
            }
            if let Some(agent_id) = agent_entry.file_name().to_str() {
                agent_ids.push(agent_id.to_string());
            }
        }

        for agent_id in agent_ids {
            let lock = self.lock_for(&agent_id);
            let _guard = lock.lock().await;

            let mut registry = self.read_registry(&agent_id).await?;
            let mut changed = false;
            for record in registry.iter_mut() {
                if record.group_id.as_deref() == Some(group_id) {
                    record.group_id = None;
                    changed = true;
                }
            }
            if changed {
                self.write_registry(&agent_id, &registry).await?;
            }
        }

        Ok(())
    }

    /// Walk every agent's artifact directory and collect pinned records,
    /// paired with the owning agent id — the Assets page's cross-agent view
    /// has no separate pinned index, since this store is small enough that a
    /// full per-agent scan on each load is fine (mirrors
    /// `TasklistStore::list_active_across_agents`'s walk-and-filter shape).
    pub async fn list_pinned_across_agents(&self) -> Result<Vec<(String, ArtifactRecord)>, AoError> {
        let agents_dir = self.data_root.artifacts_root_dir();
        if !tokio::fs::try_exists(&agents_dir).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let mut agent_entries = tokio::fs::read_dir(&agents_dir).await?;
        let mut out = Vec::new();
        while let Some(agent_entry) = agent_entries.next_entry().await? {
            if !agent_entry.file_type().await?.is_dir() {
                continue;
            }
            let Some(agent_id) = agent_entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            for record in self.list_by_agent(&agent_id).await? {
                if record.pinned {
                    out.push((agent_id.clone(), record));
                }
            }
        }
        Ok(out)
    }

    /// Delete an artifact's blob, any undo-history snapshot blobs it still
    /// carries, and remove it from the registry.
    pub async fn delete(&self, agent_id: &str, artifact_id: &str) -> Result<(), AoError> {
        let lock = self.lock_for(agent_id);
        let _guard = lock.lock().await;

        let mut registry = self.read_registry(agent_id).await?;
        let idx = registry
            .iter()
            .position(|r| r.id == artifact_id)
            .ok_or_else(|| AoError::ArtifactNotFound(artifact_id.to_string()))?;
        let record = registry.remove(idx);

        let path = self.data_root.artifact_blobs_dir(agent_id).join(&record.stored_filename);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            tokio::fs::remove_file(&path).await?;
        }

        let history_dir = self.data_root.artifact_history_dir(agent_id);
        for entry in &record.history {
            let snapshot_path = history_dir.join(&entry.stored_filename);
            if tokio::fs::try_exists(&snapshot_path).await.unwrap_or(false) {
                tokio::fs::remove_file(&snapshot_path).await?;
            }
        }

        self.write_registry(agent_id, &registry).await?;
        Ok(())
    }

    async fn read_registry(&self, agent_id: &str) -> Result<Vec<ArtifactRecord>, AoError> {
        let path = self.data_root.artifact_registry_path(agent_id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let contents = tokio::fs::read_to_string(&path).await?;
        serde_json::from_str(&contents).map_err(|e| AoError::Json(e.to_string()))
    }

    async fn write_registry(&self, agent_id: &str, registry: &[ArtifactRecord]) -> Result<(), AoError> {
        let path = self.data_root.artifact_registry_path(agent_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(registry).map_err(|e| AoError::Json(e.to_string()))?;
        let tmp_path = path.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, json).await?;
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, DataRoot) {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        (tmp, data_root)
    }

    fn sample_new_artifact() -> NewArtifact {
        NewArtifact {
            title: "Inbox highlights".to_string(),
            kind: ArtifactKind::Cards,
            format: PayloadFormat::Json,
            payload: br#"{"cards":[]}"#.to_vec(),
            refresh_intent: RefreshIntent::WholeArtifact,
            origin_intent: Some(OriginIntent {
                refresh_prompt: "Summarize today's unread emails as cards.".to_string(),
            }),
            capabilities: vec![],
            source_message_id: Some("msg-1".to_string()),
            intent_note: Some("Summarize today's unread emails as cards.".to_string()),
        }
    }

    #[tokio::test]
    async fn create_then_list_then_get() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        assert_eq!(created.title, "Inbox highlights");
        assert_eq!(created.refresh_count, 0);
        assert!(created.last_refreshed_at.is_none());

        let listed = store.list_by_agent("agent-1").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);

        let fetched = store.get("agent-1", &created.id).await.unwrap();
        assert_eq!(fetched, created);

        let (record, bytes) = store.get_payload("agent-1", &created.id).await.unwrap();
        assert_eq!(record.id, created.id);
        assert_eq!(bytes, br#"{"cards":[]}"#);
    }

    #[tokio::test]
    async fn refresh_replaces_payload_and_bumps_bookkeeping() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        let original_updated_at = created.updated_at;
        assert_eq!(created.intent_ledger.len(), 1);

        let refreshed = store
            .refresh(
                "agent-1",
                &created.id,
                br#"{"cards":[{"title":"New"}]}"#,
                IntentSource::Chat,
                Some("Add a New card".to_string()),
                Some("msg-2".to_string()),
            )
            .await
            .unwrap();

        assert_eq!(refreshed.id, created.id);
        assert_eq!(refreshed.refresh_count, 1);
        assert!(refreshed.last_refreshed_at.is_some());
        assert!(refreshed.updated_at >= original_updated_at);
        assert_ne!(refreshed.checksum_sha256, created.checksum_sha256);
        assert_eq!(refreshed.size_bytes, br#"{"cards":[{"title":"New"}]}"#.len() as u64);
        // The creation entry survives, and the refresh appended a second one.
        assert_eq!(refreshed.intent_ledger.len(), 2);
        assert_eq!(refreshed.intent_ledger[0].source, IntentSource::Create);
        assert_eq!(refreshed.intent_ledger[1].source, IntentSource::Chat);
        assert_eq!(refreshed.intent_ledger[1].intent_note.as_deref(), Some("Add a New card"));
        assert_eq!(refreshed.intent_ledger[1].source_message_id.as_deref(), Some("msg-2"));

        let (_record, bytes) = store.get_payload("agent-1", &created.id).await.unwrap();
        assert_eq!(bytes, br#"{"cards":[{"title":"New"}]}"#);

        let refreshed_again = store
            .refresh("agent-1", &created.id, b"{}", IntentSource::MainThreadEdit, None, None)
            .await
            .unwrap();
        assert_eq!(refreshed_again.refresh_count, 2);
        assert_eq!(refreshed_again.intent_ledger.len(), 3);
    }

    #[tokio::test]
    async fn delete_removes_record_and_blob() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root.clone());

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        let blob_path = data_root.artifact_blobs_dir("agent-1").join(&created.stored_filename);
        assert!(blob_path.exists());

        store.delete("agent-1", &created.id).await.unwrap();

        assert!(!blob_path.exists());
        let listed = store.list_by_agent("agent-1").await.unwrap();
        assert!(listed.is_empty());
        assert!(matches!(
            store.get("agent-1", &created.id).await,
            Err(AoError::ArtifactNotFound(_))
        ));
    }

    #[tokio::test]
    async fn intent_ledger_is_bounded_and_never_evicts_the_creation_entry() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        let creation_timestamp = created.intent_ledger[0].timestamp;

        let mut latest = created;
        for i in 0..(INTENT_LEDGER_MAX_LEN + 5) {
            latest = store
                .refresh(
                    "agent-1",
                    &latest.id,
                    format!("{{\"rev\":{i}}}").as_bytes(),
                    IntentSource::Chat,
                    Some(format!("edit {i}")),
                    None,
                )
                .await
                .unwrap();
        }

        assert_eq!(latest.intent_ledger.len(), INTENT_LEDGER_MAX_LEN);
        assert_eq!(latest.intent_ledger[0].source, IntentSource::Create);
        assert_eq!(latest.intent_ledger[0].timestamp, creation_timestamp);
        // The newest entries survive; older non-creation entries were evicted.
        assert_eq!(
            latest.intent_ledger.last().unwrap().intent_note.as_deref(),
            Some("edit 24")
        );
    }

    #[tokio::test]
    async fn get_missing_artifact_returns_not_found() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let err = store.get("agent-1", "ghost").await.unwrap_err();
        assert!(matches!(err, AoError::ArtifactNotFound(_)));
    }

    #[tokio::test]
    async fn refresh_missing_artifact_returns_not_found() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let err = store
            .refresh("agent-1", "ghost", b"{}", IntentSource::Chat, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AoError::ArtifactNotFound(_)));
    }

    #[tokio::test]
    async fn agents_are_isolated() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let a = store.create("agent-a", sample_new_artifact()).await.unwrap();
        let b = store.create("agent-b", sample_new_artifact()).await.unwrap();

        let a_list = store.list_by_agent("agent-a").await.unwrap();
        let b_list = store.list_by_agent("agent-b").await.unwrap();
        assert_eq!(a_list.len(), 1);
        assert_eq!(b_list.len(), 1);
        assert_ne!(a.id, b.id);

        // Cross-agent lookups must not resolve.
        assert!(matches!(
            store.get("agent-a", &b.id).await,
            Err(AoError::ArtifactNotFound(_))
        ));
        assert!(matches!(
            store.get("agent-b", &a.id).await,
            Err(AoError::ArtifactNotFound(_))
        ));
    }

    #[tokio::test]
    async fn list_empty_for_unknown_agent() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let listed = store.list_by_agent("no-such-agent").await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn set_pinned_toggles_and_persists() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        assert!(!created.pinned);

        let pinned = store.set_pinned("agent-1", &created.id, true).await.unwrap();
        assert!(pinned.pinned);
        // Persisted, not just returned.
        let fetched = store.get("agent-1", &created.id).await.unwrap();
        assert!(fetched.pinned);

        let unpinned = store.set_pinned("agent-1", &created.id, false).await.unwrap();
        assert!(!unpinned.pinned);
    }

    #[tokio::test]
    async fn set_pinned_missing_artifact_returns_not_found() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let err = store.set_pinned("agent-1", "ghost", true).await.unwrap_err();
        assert!(matches!(err, AoError::ArtifactNotFound(_)));
    }

    #[tokio::test]
    async fn set_pinned_stamps_and_clears_pinned_at() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        assert!(created.pinned_at.is_none());

        let pinned = store.set_pinned("agent-1", &created.id, true).await.unwrap();
        assert!(pinned.pinned_at.is_some());

        let unpinned = store.set_pinned("agent-1", &created.id, false).await.unwrap();
        assert!(unpinned.pinned_at.is_none());
    }

    #[tokio::test]
    async fn set_group_assigns_and_clears() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        assert!(created.group_id.is_none());

        let grouped = store
            .set_group("agent-1", &created.id, Some("group-1".to_string()))
            .await
            .unwrap();
        assert_eq!(grouped.group_id.as_deref(), Some("group-1"));
        // Persisted, not just returned.
        let fetched = store.get("agent-1", &created.id).await.unwrap();
        assert_eq!(fetched.group_id.as_deref(), Some("group-1"));

        let ungrouped = store.set_group("agent-1", &created.id, None).await.unwrap();
        assert!(ungrouped.group_id.is_none());
    }

    #[tokio::test]
    async fn set_group_missing_artifact_returns_not_found() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let err = store
            .set_group("agent-1", "ghost", Some("group-1".to_string()))
            .await
            .unwrap_err();
        assert!(matches!(err, AoError::ArtifactNotFound(_)));
    }

    #[tokio::test]
    async fn clear_group_across_agents_unsets_matching_artifacts_only() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let a = store.create("agent-a", sample_new_artifact()).await.unwrap();
        let b = store.create("agent-b", sample_new_artifact()).await.unwrap();
        let c = store.create("agent-a", sample_new_artifact()).await.unwrap();

        store.set_group("agent-a", &a.id, Some("group-1".to_string())).await.unwrap();
        store.set_group("agent-b", &b.id, Some("group-1".to_string())).await.unwrap();
        store.set_group("agent-a", &c.id, Some("group-2".to_string())).await.unwrap();

        store.clear_group_across_agents("group-1").await.unwrap();

        assert!(store.get("agent-a", &a.id).await.unwrap().group_id.is_none());
        assert!(store.get("agent-b", &b.id).await.unwrap().group_id.is_none());
        // A different group is left untouched.
        assert_eq!(
            store.get("agent-a", &c.id).await.unwrap().group_id.as_deref(),
            Some("group-2")
        );
    }

    #[tokio::test]
    async fn clear_group_across_agents_empty_when_no_artifacts_dir() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        // Must not error when nothing has been created yet.
        store.clear_group_across_agents("ghost").await.unwrap();
    }

    #[tokio::test]
    async fn list_pinned_across_agents_aggregates_and_filters() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let a1 = store.create("agent-a", sample_new_artifact()).await.unwrap();
        let a2 = store.create("agent-a", sample_new_artifact()).await.unwrap();
        let b1 = store.create("agent-b", sample_new_artifact()).await.unwrap();

        store.set_pinned("agent-a", &a1.id, true).await.unwrap();
        store.set_pinned("agent-b", &b1.id, true).await.unwrap();
        // a2 stays unpinned.

        let pinned = store.list_pinned_across_agents().await.unwrap();
        assert_eq!(pinned.len(), 2);
        let ids: Vec<&str> = pinned.iter().map(|(_, r)| r.id.as_str()).collect();
        assert!(ids.contains(&a1.id.as_str()));
        assert!(ids.contains(&b1.id.as_str()));
        assert!(!ids.contains(&a2.id.as_str()));

        let agent_ids: Vec<&str> = pinned.iter().map(|(agent_id, _)| agent_id.as_str()).collect();
        assert!(agent_ids.contains(&"agent-a"));
        assert!(agent_ids.contains(&"agent-b"));
    }

    #[tokio::test]
    async fn list_pinned_across_agents_empty_when_no_artifacts_dir() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let pinned = store.list_pinned_across_agents().await.unwrap();
        assert!(pinned.is_empty());
    }

    // --- undo history: snapshot-on-edit + restore -----------------------

    #[tokio::test]
    async fn create_pushes_no_history_since_there_is_no_prior_body() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        assert!(created.history.is_empty());
        assert_eq!(created.next_history_seq, 0);
    }

    #[tokio::test]
    async fn refresh_pushes_prior_body_onto_history() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root.clone());

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        let original_payload = br#"{"cards":[]}"#;
        let original_checksum = hex_sha256(original_payload);

        let refreshed = store
            .refresh(
                "agent-1",
                &created.id,
                br#"{"cards":[{"title":"New"}]}"#,
                IntentSource::Chat,
                Some("Add a New card".to_string()),
                Some("msg-2".to_string()),
            )
            .await
            .unwrap();

        assert_eq!(refreshed.history.len(), 1);
        let snapshot = &refreshed.history[0];
        assert_eq!(snapshot.seq, 0);
        assert_eq!(snapshot.checksum_sha256, original_checksum);
        assert_eq!(snapshot.size_bytes, original_payload.len() as u64);
        // The snapshot's source is the provenance of the ARCHIVED body
        // (this artifact's creation), not the edit that just superseded it.
        assert_eq!(snapshot.source, IntentSource::Create);
        assert_eq!(refreshed.next_history_seq, 1);

        // The snapshot is a real file on disk, holding the pre-edit body.
        let snapshot_path = data_root.artifact_history_dir("agent-1").join(&snapshot.stored_filename);
        let snapshot_bytes = tokio::fs::read(&snapshot_path).await.unwrap();
        assert_eq!(snapshot_bytes, original_payload);
    }

    #[tokio::test]
    async fn history_is_bounded_and_evicts_oldest_blob_too() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root.clone());

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        let mut latest = created;
        for i in 0..(ao_protocol::artifact::ARTIFACT_HISTORY_MAX_LEN + 5) {
            latest = store
                .refresh(
                    "agent-1",
                    &latest.id,
                    format!("{{\"rev\":{i}}}").as_bytes(),
                    IntentSource::Chat,
                    Some(format!("edit {i}")),
                    None,
                )
                .await
                .unwrap();
        }

        assert_eq!(latest.history.len(), ao_protocol::artifact::ARTIFACT_HISTORY_MAX_LEN);
        // The oldest surviving snapshot's seq reflects exactly the entries
        // evicted so far: 15 pushes total (0..15), bounded to the newest 10.
        assert_eq!(latest.history.first().unwrap().seq, 5);
        assert_eq!(latest.history.last().unwrap().seq, 14);

        // The evicted snapshots' blobs are gone from disk, not just dropped
        // from the registry — otherwise the bound is cosmetic and history/
        // grows unbounded anyway.
        let history_dir = data_root.artifact_history_dir("agent-1");
        let mut remaining = tokio::fs::read_dir(&history_dir).await.unwrap();
        let mut count = 0;
        while remaining.next_entry().await.unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, ao_protocol::artifact::ARTIFACT_HISTORY_MAX_LEN);
    }

    #[tokio::test]
    async fn undo_restores_prior_body_and_appends_undo_ledger_entry() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        let original_payload = br#"{"cards":[]}"#.to_vec();

        let refreshed = store
            .refresh(
                "agent-1",
                &created.id,
                br#"{"cards":[{"title":"New"}]}"#,
                IntentSource::Chat,
                Some("Add a New card".to_string()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(refreshed.history.len(), 1);

        let undone = store.undo("agent-1", &created.id, Some("msg-undo".to_string())).await.unwrap();

        // Body is back to what it was before the edit.
        let (_record, bytes) = store.get_payload("agent-1", &created.id).await.unwrap();
        assert_eq!(bytes, original_payload);
        assert_eq!(undone.checksum_sha256, hex_sha256(&original_payload));
        assert_eq!(undone.size_bytes, original_payload.len() as u64);

        // The undone snapshot was popped, not left behind — nothing left to
        // undo further.
        assert!(undone.history.is_empty());

        // The undo appended a ledger entry with source `Undo`, but crucially
        // did NOT push a new history snapshot (which would corrupt the
        // stack — see `ArtifactStore::undo`'s doc comment).
        let last_entry = undone.intent_ledger.last().unwrap();
        assert_eq!(last_entry.source, IntentSource::Undo);
        assert_eq!(last_entry.source_message_id.as_deref(), Some("msg-undo"));
        assert!(undone.history.is_empty());
    }

    #[tokio::test]
    async fn undo_pop_and_restore_removes_snapshot_blob_from_disk() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root.clone());

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        let refreshed = store
            .refresh("agent-1", &created.id, b"{}", IntentSource::Chat, None, None)
            .await
            .unwrap();
        let snapshot_filename = refreshed.history[0].stored_filename.clone();
        let snapshot_path = data_root.artifact_history_dir("agent-1").join(&snapshot_filename);
        assert!(snapshot_path.exists());

        store.undo("agent-1", &created.id, None).await.unwrap();

        assert!(!snapshot_path.exists());
    }

    #[tokio::test]
    async fn undo_down_to_empty_then_conflict() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();

        // No edits yet — nothing to undo.
        let err = store.undo("agent-1", &created.id, None).await.unwrap_err();
        assert!(matches!(err, AoError::Conflict(_)));

        // Two edits pushed two snapshots; undo twice drains them, and a
        // third undo hits the same empty-history conflict.
        let after_edit_1 = store
            .refresh("agent-1", &created.id, b"{\"rev\":1}", IntentSource::Chat, None, None)
            .await
            .unwrap();
        assert_eq!(after_edit_1.history.len(), 1);
        let after_edit_2 = store
            .refresh("agent-1", &created.id, b"{\"rev\":2}", IntentSource::Chat, None, None)
            .await
            .unwrap();
        assert_eq!(after_edit_2.history.len(), 2);

        let after_undo_1 = store.undo("agent-1", &created.id, None).await.unwrap();
        assert_eq!(after_undo_1.history.len(), 1);
        let (_record, bytes) = store.get_payload("agent-1", &created.id).await.unwrap();
        assert_eq!(bytes, b"{\"rev\":1}");

        let after_undo_2 = store.undo("agent-1", &created.id, None).await.unwrap();
        assert!(after_undo_2.history.is_empty());
        let (_record, bytes) = store.get_payload("agent-1", &created.id).await.unwrap();
        assert_eq!(bytes, br#"{"cards":[]}"#);

        let err = store.undo("agent-1", &created.id, None).await.unwrap_err();
        assert!(matches!(err, AoError::Conflict(_)));
    }

    #[tokio::test]
    async fn undo_missing_artifact_returns_not_found() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root);

        let err = store.undo("agent-1", "ghost", None).await.unwrap_err();
        assert!(matches!(err, AoError::ArtifactNotFound(_)));
    }

    #[tokio::test]
    async fn delete_also_removes_surviving_history_snapshot_blobs() {
        let (_tmp, data_root) = setup();
        let store = ArtifactStore::new(data_root.clone());

        let created = store.create("agent-1", sample_new_artifact()).await.unwrap();
        let refreshed = store
            .refresh("agent-1", &created.id, b"{}", IntentSource::Chat, None, None)
            .await
            .unwrap();
        let snapshot_path = data_root
            .artifact_history_dir("agent-1")
            .join(&refreshed.history[0].stored_filename);
        assert!(snapshot_path.exists());

        store.delete("agent-1", &created.id).await.unwrap();

        assert!(!snapshot_path.exists());
    }
}
