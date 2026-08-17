use ao_protocol::error::AoError;
use ao_protocol::memory::{MemoryEntry, MemoryScope, MemorySource, MemoryStatus};
use ao_search_index::{ArtifactKind, IndexRecord, IndexScope, SearchFilter, SearchIndex};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::paths::DataRoot;

/// Build the search-index row for a live memory entry. Kept as a free
/// function (rather than a `MemoryEntry` method) since the mapping is a
/// storage-layer concern, not part of the wire/protocol type.
fn to_index_record(entry: &MemoryEntry) -> IndexRecord {
    let key = entry.scope_key.clone().unwrap_or_default();
    let scope = match entry.scope {
        MemoryScope::Agent => IndexScope::Agent(key),
        MemoryScope::Project => IndexScope::Project(key),
        MemoryScope::Global => IndexScope::Global,
        MemoryScope::AgentProject => IndexScope::AgentProject(key),
        MemoryScope::Thread => unreachable!(
            "thread-scope entries never reach the search index: add_thread/edit_thread/delete_thread \
             never call sync_index_upsert/sync_index_delete, and reindex_all never scans the threads dir"
        ),
    };
    IndexRecord {
        id: entry.id.clone(),
        scope,
        artifact: ArtifactKind::Memory,
        text: entry.content.clone(),
    }
}

/// Result of a memory write, edit, or delete operation.
#[derive(Debug, Clone)]
pub struct MemoryOpResult {
    pub id: String,
    pub deduplicated: bool,
}

/// JSONL-based memory persistence with three scopes: agent, global, and project.
///
/// Entries are soft-tombstoned on delete/edit (append-only). Compaction removes
/// tombstones when live entries fall below 80% of raw line count.
pub struct MemoryStore {
    data_root: DataRoot,
    locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
    /// Search index kept incrementally consistent with every write below —
    /// `None` when no caller has attached one (e.g. most existing tests),
    /// in which case indexing is silently skipped. See [`Self::with_index`].
    index: Option<SearchIndex>,
}

impl MemoryStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self {
            data_root,
            locks: Arc::new(DashMap::new()),
            index: None,
        }
    }

    /// Attach a search index to keep in sync with every mutating call.
    pub fn with_index(mut self, index: SearchIndex) -> Self {
        self.index = Some(index);
        self
    }

    /// Best-effort upsert into the attached search index. Index failures are
    /// logged, not propagated: the JSONL log is the source of truth, and
    /// [`Self::reindex_all`] exists precisely to repair the index if it ever
    /// drifts from it, so a transient SQLite error here must not fail the
    /// caller's memory write.
    async fn sync_index_upsert(&self, entry: &MemoryEntry) {
        if let Some(index) = &self.index {
            if let Err(e) = index.upsert(to_index_record(entry)).await {
                tracing::warn!("search index upsert failed for memory entry {}: {}", entry.id, e);
            }
        }
    }

    /// Best-effort removal from the attached search index. See
    /// [`Self::sync_index_upsert`] for the failure-handling rationale.
    async fn sync_index_delete(&self, id: &str) {
        if let Some(index) = &self.index {
            if let Err(e) = index.delete(id.to_string()).await {
                tracing::warn!("search index delete failed for memory entry {}: {}", id, e);
            }
        }
    }

    /// Rebuild the search index's memory rows from the on-disk JSONL logs —
    /// the cold-start / corruption recovery path required alongside the
    /// incremental sync above. Scans every scope (global, every agent file,
    /// every project file) for live entries and replaces only the `Memory`
    /// artifact rows in the index, leaving any other artifact kind (e.g.
    /// skill rows) untouched. No-op if no index is attached.
    pub async fn reindex_all(&self) -> Result<(), AoError> {
        let Some(index) = &self.index else {
            return Ok(());
        };

        let mut records: Vec<IndexRecord> = self
            .list_global()
            .await?
            .iter()
            .map(to_index_record)
            .collect();

        let agents_dir = self.data_root.memory_agents_dir();
        if tokio::fs::try_exists(&agents_dir).await.unwrap_or(false) {
            let mut read_dir = tokio::fs::read_dir(&agents_dir).await?;
            while let Some(dir_entry) = read_dir.next_entry().await? {
                let path = dir_entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(agent_id) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                records.extend(self.list(agent_id).await?.iter().map(to_index_record));
            }
        }

        let projects_dir = self.data_root.memory_projects_dir();
        if tokio::fs::try_exists(&projects_dir).await.unwrap_or(false) {
            let mut read_dir = tokio::fs::read_dir(&projects_dir).await?;
            while let Some(dir_entry) = read_dir.next_entry().await? {
                let path = dir_entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(hash) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                records.extend(self.list_project(hash).await?.iter().map(to_index_record));
            }
        }

        index.rebuild_kind(ArtifactKind::Memory, records).await
    }

    /// Query the attached search index for `Memory`-artifact entries in the
    /// given scope that are textually related to `text`, ranked best match
    /// first by FTS5's `bm25` relevance score.
    ///
    /// Returns an empty list — rather than an error — when no index is
    /// attached (older call sites that never opted in) and when the
    /// underlying query itself fails, so a search-index outage degrades a
    /// caller's near-duplicate detection instead of failing the write it is
    /// guarding. Only entry ids are returned; callers already hold the
    /// scope's live entries (from `list`/`list_global`/`list_project`) and
    /// look the id up there for the entry's current status and content.
    pub async fn search_similar_ids(
        &self,
        scope: MemoryScope,
        scope_key: Option<&str>,
        text: &str,
        limit: usize,
    ) -> Vec<String> {
        let Some(index) = &self.index else {
            return Vec::new();
        };
        let index_scope = match scope {
            MemoryScope::Agent => IndexScope::Agent(scope_key.unwrap_or_default().to_string()),
            MemoryScope::Project => IndexScope::Project(scope_key.unwrap_or_default().to_string()),
            MemoryScope::Global => IndexScope::Global,
            MemoryScope::AgentProject => {
                IndexScope::AgentProject(scope_key.unwrap_or_default().to_string())
            }
            MemoryScope::Thread => unreachable!(
                "MemoryWrite never queries near-duplicate candidates for thread scope — thread \
                 entries skip the contradiction-check path entirely"
            ),
        };
        let filter = SearchFilter::new().with_scope(index_scope).with_artifact(ArtifactKind::Memory);
        match index.query(text.to_string(), filter, limit).await {
            Ok(hits) => hits.into_iter().map(|hit| hit.id).collect(),
            Err(e) => {
                tracing::warn!("search index query failed during near-duplicate lookup: {}", e);
                Vec::new()
            }
        }
    }

    fn lock_for(&self, key: &str) -> Arc<Mutex<()>> {
        self.locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    // --- Scope paths ---
    //
    // Exposed so callers outside this crate can derive a scope's usage
    // sidecar path (`ao_engine_tools_core::memory_usage::usage_sidecar_path`)
    // without needing their own `DataRoot` handle — the eviction scorer is
    // the first consumer.

    pub fn agent_scope_path(&self, agent_id: &str) -> PathBuf {
        self.data_root.memory_agent_path(agent_id)
    }

    pub fn global_scope_path(&self) -> PathBuf {
        self.data_root.memory_global_path()
    }

    pub fn project_scope_path(&self, project_hash: &str) -> PathBuf {
        self.data_root.memory_project_path(project_hash)
    }

    // --- Agent scope ---

    /// List all live memory entries for an agent.
    pub async fn list(&self, agent_id: &str) -> Result<Vec<MemoryEntry>, AoError> {
        let path = self.data_root.memory_agent_path(agent_id);
        let (entries, _) = Self::read_live(&path).await?;
        Ok(entries)
    }

    /// Add a new memory entry for an agent. Returns the existing entry if byte-equal
    /// content already exists (dedup); otherwise returns the new entry.
    pub async fn add(
        &self,
        agent_id: &str,
        content: &str,
        source: MemorySource,
    ) -> Result<MemoryEntry, AoError> {
        let path = self.data_root.memory_agent_path(agent_id);
        let lock = self.lock_for(&format!("agent:{}", agent_id));
        let _guard = lock.lock().await;

        let (live, raw_count) = Self::read_live(&path).await?;

        if let Some(existing) = live.iter().find(|e| e.content == content) {
            return Ok(existing.clone());
        }

        Self::maybe_compact(&path, raw_count, &live).await?;

        let now = Utc::now();
        let entry = MemoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            content: content.to_string(),
            created_at: now,
            source: Some(source),
            scope: MemoryScope::Agent,
            scope_key: Some(agent_id.to_string()),
            updated_at: now,
            deleted_at: None,
            confidence: 1.0,
            status: MemoryStatus::Active,
            superseded_by: None,
            pinned: false,
            decay_score: 1.0,
        };
        Self::append_entry(&path, &entry).await?;
        self.sync_index_upsert(&entry).await;
        Ok(entry)
    }

    /// Soft-delete a memory entry for an agent. Returns true if found, false if not.
    pub async fn delete(&self, agent_id: &str, memory_id: &str) -> Result<bool, AoError> {
        let path = self.data_root.memory_agent_path(agent_id);
        let lock = self.lock_for(&format!("agent:{}", agent_id));
        let _guard = lock.lock().await;

        let (live, raw_count) = Self::read_live(&path).await?;
        let Some(entry) = live.iter().find(|e| e.id == memory_id) else {
            return Ok(false);
        };

        let mut tombstone = entry.clone();
        tombstone.deleted_at = Some(Utc::now());
        Self::maybe_compact(&path, raw_count, &live).await?;
        Self::append_entry(&path, &tombstone).await?;
        self.sync_index_delete(memory_id).await;
        Ok(true)
    }

    /// Edit an existing agent memory entry in place (tombstone old, append updated).
    pub async fn edit(
        &self,
        agent_id: &str,
        memory_id: &str,
        new_content: &str,
    ) -> Result<MemoryOpResult, AoError> {
        let path = self.data_root.memory_agent_path(agent_id);
        let lock = self.lock_for(&format!("agent:{}", agent_id));
        let _guard = lock.lock().await;

        let (live, raw_count) = Self::read_live(&path).await?;
        let Some(original) = live.iter().find(|e| e.id == memory_id) else {
            return Err(AoError::Internal(format!(
                "Memory entry {} not found in agent scope",
                memory_id
            )));
        };

        let mut updated = original.clone();
        updated.content = new_content.to_string();
        updated.updated_at = Utc::now();
        updated.deleted_at = None;

        Self::maybe_compact(&path, raw_count, &live).await?;
        Self::append_entry(&path, &updated).await?;
        self.sync_index_upsert(&updated).await;

        Ok(MemoryOpResult { id: memory_id.to_string(), deduplicated: false })
    }

    /// Mark an agent-scoped entry as superseded by another entry, without
    /// touching its content. Used by the contradiction guard when a new
    /// agent write restates/contradicts an existing *agent-authored* entry —
    /// the old entry is kept on disk for provenance (append-only, same as
    /// `edit`) but flagged `status: Superseded` with `superseded_by` pointing
    /// at the new entry, so it stops being surfaced as live guidance. Errors
    /// if `memory_id` is not a live entry.
    pub async fn supersede(
        &self,
        agent_id: &str,
        memory_id: &str,
        superseded_by: &str,
    ) -> Result<(), AoError> {
        let path = self.data_root.memory_agent_path(agent_id);
        let lock = self.lock_for(&format!("agent:{}", agent_id));
        let _guard = lock.lock().await;
        Self::apply_supersede(&path, memory_id, superseded_by, "agent", self.index.as_ref()).await
    }

    /// Mark an agent-scoped entry as `Archived`, without touching its
    /// content. Used by the eviction path (`write.rs` in the engine
    /// crate) when a write would exceed the scope's hard cap: instead of
    /// rejecting the write, the lowest-scoring eligible entry is archived
    /// (soft-tombstoned, kept on disk for provenance) to free room for the
    /// new one. Errors if `memory_id` is not a live entry.
    pub async fn archive(&self, agent_id: &str, memory_id: &str) -> Result<(), AoError> {
        let path = self.data_root.memory_agent_path(agent_id);
        let lock = self.lock_for(&format!("agent:{}", agent_id));
        let _guard = lock.lock().await;
        Self::apply_archive(&path, memory_id, "agent", self.index.as_ref()).await
    }

    // --- Global scope ---

    /// List all live global memory entries.
    pub async fn list_global(&self) -> Result<Vec<MemoryEntry>, AoError> {
        let path = self.data_root.memory_global_path();
        let (entries, _) = Self::read_live(&path).await?;
        Ok(entries)
    }

    /// Add a new global memory entry. Deduplicates by byte-equal content.
    pub async fn add_global(
        &self,
        content: &str,
        source: MemorySource,
    ) -> Result<MemoryEntry, AoError> {
        let path = self.data_root.memory_global_path();
        let lock = self.lock_for("global");
        let _guard = lock.lock().await;

        let (live, raw_count) = Self::read_live(&path).await?;

        if let Some(existing) = live.iter().find(|e| e.content == content) {
            return Ok(existing.clone());
        }

        Self::maybe_compact(&path, raw_count, &live).await?;

        let now = Utc::now();
        let entry = MemoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            content: content.to_string(),
            created_at: now,
            source: Some(source),
            scope: MemoryScope::Global,
            scope_key: None,
            updated_at: now,
            deleted_at: None,
            confidence: 1.0,
            status: MemoryStatus::Active,
            superseded_by: None,
            pinned: false,
            decay_score: 1.0,
        };
        Self::append_entry(&path, &entry).await?;
        self.sync_index_upsert(&entry).await;
        Ok(entry)
    }

    /// Soft-delete a global memory entry. Returns true if found, false if not.
    pub async fn delete_global(&self, memory_id: &str) -> Result<bool, AoError> {
        let path = self.data_root.memory_global_path();
        let lock = self.lock_for("global");
        let _guard = lock.lock().await;

        let (live, raw_count) = Self::read_live(&path).await?;
        let Some(entry) = live.iter().find(|e| e.id == memory_id) else {
            return Ok(false);
        };

        let mut tombstone = entry.clone();
        tombstone.deleted_at = Some(Utc::now());
        Self::maybe_compact(&path, raw_count, &live).await?;
        Self::append_entry(&path, &tombstone).await?;
        self.sync_index_delete(memory_id).await;
        Ok(true)
    }

    /// Edit an existing global memory entry.
    pub async fn edit_global(
        &self,
        memory_id: &str,
        new_content: &str,
    ) -> Result<MemoryOpResult, AoError> {
        let path = self.data_root.memory_global_path();
        let lock = self.lock_for("global");
        let _guard = lock.lock().await;

        let (live, raw_count) = Self::read_live(&path).await?;
        let Some(original) = live.iter().find(|e| e.id == memory_id) else {
            return Err(AoError::Internal(format!(
                "Memory entry {} not found in global scope",
                memory_id
            )));
        };

        let mut updated = original.clone();
        updated.content = new_content.to_string();
        updated.updated_at = Utc::now();
        updated.deleted_at = None;

        Self::maybe_compact(&path, raw_count, &live).await?;
        Self::append_entry(&path, &updated).await?;
        self.sync_index_upsert(&updated).await;

        Ok(MemoryOpResult { id: memory_id.to_string(), deduplicated: false })
    }

    /// Mark a global entry as superseded by another entry. See [`Self::supersede`].
    pub async fn supersede_global(
        &self,
        memory_id: &str,
        superseded_by: &str,
    ) -> Result<(), AoError> {
        let path = self.data_root.memory_global_path();
        let lock = self.lock_for("global");
        let _guard = lock.lock().await;
        Self::apply_supersede(&path, memory_id, superseded_by, "global", self.index.as_ref()).await
    }

    /// Mark a global entry as `Archived`. See [`Self::archive`].
    pub async fn archive_global(&self, memory_id: &str) -> Result<(), AoError> {
        let path = self.data_root.memory_global_path();
        let lock = self.lock_for("global");
        let _guard = lock.lock().await;
        Self::apply_archive(&path, memory_id, "global", self.index.as_ref()).await
    }

    // --- Project scope ---

    /// List all live project-scoped memory entries for the given project hash.
    pub async fn list_project(&self, project_hash: &str) -> Result<Vec<MemoryEntry>, AoError> {
        let path = self.data_root.memory_project_path(project_hash);
        let (entries, _) = Self::read_live(&path).await?;
        Ok(entries)
    }

    /// Add a new project-scoped memory entry. Deduplicates by byte-equal content.
    ///
    /// `source` is required (unlike the historical signature, which stamped
    /// every project entry with `source: None`) so the contradiction guard
    /// can tell a human-authored project memory from an agent-authored one —
    /// project scope is the one place both origins write through the same
    /// call site (the UI's "add project memory" route and the agent's
    /// `MemoryWrite` tool), so it needs the same provenance tag the agent and
    /// global scopes already carried.
    pub async fn add_project(
        &self,
        project_hash: &str,
        content: &str,
        source: MemorySource,
    ) -> Result<MemoryOpResult, AoError> {
        let path = self.data_root.memory_project_path(project_hash);
        let lock = self.lock_for(&format!("project:{}", project_hash));
        let _guard = lock.lock().await;

        let (live, raw_count) = Self::read_live(&path).await?;

        if let Some(existing) = live.iter().find(|e| e.content == content) {
            return Ok(MemoryOpResult { id: existing.id.clone(), deduplicated: true });
        }

        Self::maybe_compact(&path, raw_count, &live).await?;

        let now = Utc::now();
        let entry = MemoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            content: content.to_string(),
            created_at: now,
            source: Some(source),
            scope: MemoryScope::Project,
            scope_key: Some(project_hash.to_string()),
            updated_at: now,
            deleted_at: None,
            confidence: 1.0,
            status: MemoryStatus::Active,
            superseded_by: None,
            pinned: false,
            decay_score: 1.0,
        };
        Self::append_entry(&path, &entry).await?;
        self.sync_index_upsert(&entry).await;
        Ok(MemoryOpResult { id: entry.id, deduplicated: false })
    }

    /// Soft-delete a project-scoped memory entry. Errors if entry not found.
    pub async fn delete_project(
        &self,
        project_hash: &str,
        memory_id: &str,
    ) -> Result<MemoryOpResult, AoError> {
        let path = self.data_root.memory_project_path(project_hash);
        let lock = self.lock_for(&format!("project:{}", project_hash));
        let _guard = lock.lock().await;

        let (live, raw_count) = Self::read_live(&path).await?;
        let Some(entry) = live.iter().find(|e| e.id == memory_id) else {
            return Err(AoError::Internal(format!(
                "Memory entry {} not found in project scope",
                memory_id
            )));
        };

        let mut tombstone = entry.clone();
        tombstone.deleted_at = Some(Utc::now());
        Self::maybe_compact(&path, raw_count, &live).await?;
        Self::append_entry(&path, &tombstone).await?;
        self.sync_index_delete(memory_id).await;
        Ok(MemoryOpResult { id: memory_id.to_string(), deduplicated: false })
    }

    /// Edit an existing project-scoped memory entry.
    pub async fn edit_project(
        &self,
        project_hash: &str,
        memory_id: &str,
        new_content: &str,
    ) -> Result<MemoryOpResult, AoError> {
        let path = self.data_root.memory_project_path(project_hash);
        let lock = self.lock_for(&format!("project:{}", project_hash));
        let _guard = lock.lock().await;

        let (live, raw_count) = Self::read_live(&path).await?;
        let Some(original) = live.iter().find(|e| e.id == memory_id) else {
            return Err(AoError::Internal(format!(
                "Memory entry {} not found in project scope",
                memory_id
            )));
        };

        let mut updated = original.clone();
        updated.content = new_content.to_string();
        updated.updated_at = Utc::now();
        updated.deleted_at = None;

        Self::maybe_compact(&path, raw_count, &live).await?;
        Self::append_entry(&path, &updated).await?;
        self.sync_index_upsert(&updated).await;

        Ok(MemoryOpResult { id: memory_id.to_string(), deduplicated: false })
    }

    /// Mark a project-scoped entry as superseded by another entry. See [`Self::supersede`].
    pub async fn supersede_project(
        &self,
        project_hash: &str,
        memory_id: &str,
        superseded_by: &str,
    ) -> Result<(), AoError> {
        let path = self.data_root.memory_project_path(project_hash);
        let lock = self.lock_for(&format!("project:{}", project_hash));
        let _guard = lock.lock().await;
        Self::apply_supersede(&path, memory_id, superseded_by, "project", self.index.as_ref()).await
    }

    /// Mark a project-scoped entry as `Archived`. See [`Self::archive`].
    pub async fn archive_project(&self, project_hash: &str, memory_id: &str) -> Result<(), AoError> {
        let path = self.data_root.memory_project_path(project_hash);
        let lock = self.lock_for(&format!("project:{}", project_hash));
        let _guard = lock.lock().await;
        Self::apply_archive(&path, memory_id, "project", self.index.as_ref()).await
    }

    // --- Thread scope ---
    //
    // The ephemeral working-memory tier: entries keyed by thread id instead
    // of agent id or project hash. Deliberately a smaller surface than the
    // three durable scopes above — no supersede/archive/restore/set_pinned,
    // since thread entries never go through the contradiction guard, the
    // trust-gate staging queue, or the durable-eviction scorer. A thread
    // hitting its cap just drops its oldest entry (handled by the engine
    // tool crate's `MemoryWrite`, not here) rather than archiving one for
    // provenance, because the whole tier disappears with the thread anyway.

    /// List all live memory entries for a thread.
    pub async fn list_thread(&self, thread_id: &str) -> Result<Vec<MemoryEntry>, AoError> {
        let path = self.data_root.memory_thread_path(thread_id);
        let (entries, _) = Self::read_live(&path).await?;
        Ok(entries)
    }

    /// Add a new thread-scoped memory entry. Deduplicates by byte-equal
    /// content, mirroring [`Self::add`].
    pub async fn add_thread(
        &self,
        thread_id: &str,
        content: &str,
        source: MemorySource,
    ) -> Result<MemoryEntry, AoError> {
        let path = self.data_root.memory_thread_path(thread_id);
        let lock = self.lock_for(&format!("thread:{}", thread_id));
        let _guard = lock.lock().await;

        let (live, raw_count) = Self::read_live(&path).await?;

        if let Some(existing) = live.iter().find(|e| e.content == content) {
            return Ok(existing.clone());
        }

        Self::maybe_compact(&path, raw_count, &live).await?;

        let now = Utc::now();
        let entry = MemoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            content: content.to_string(),
            created_at: now,
            source: Some(source),
            scope: MemoryScope::Thread,
            scope_key: Some(thread_id.to_string()),
            updated_at: now,
            deleted_at: None,
            confidence: 1.0,
            status: MemoryStatus::Active,
            superseded_by: None,
            pinned: false,
            decay_score: 1.0,
        };
        Self::append_entry(&path, &entry).await?;
        Ok(entry)
    }

    /// Soft-delete a thread-scoped memory entry. Returns true if found, false if not.
    pub async fn delete_thread(&self, thread_id: &str, memory_id: &str) -> Result<bool, AoError> {
        let path = self.data_root.memory_thread_path(thread_id);
        let lock = self.lock_for(&format!("thread:{}", thread_id));
        let _guard = lock.lock().await;

        let (live, raw_count) = Self::read_live(&path).await?;
        let Some(entry) = live.iter().find(|e| e.id == memory_id) else {
            return Ok(false);
        };

        let mut tombstone = entry.clone();
        tombstone.deleted_at = Some(Utc::now());
        Self::maybe_compact(&path, raw_count, &live).await?;
        Self::append_entry(&path, &tombstone).await?;
        Ok(true)
    }

    /// Edit an existing thread-scoped memory entry in place (tombstone old, append updated).
    pub async fn edit_thread(
        &self,
        thread_id: &str,
        memory_id: &str,
        new_content: &str,
    ) -> Result<MemoryOpResult, AoError> {
        let path = self.data_root.memory_thread_path(thread_id);
        let lock = self.lock_for(&format!("thread:{}", thread_id));
        let _guard = lock.lock().await;

        let (live, raw_count) = Self::read_live(&path).await?;
        let Some(original) = live.iter().find(|e| e.id == memory_id) else {
            return Err(AoError::Internal(format!(
                "Memory entry {} not found in thread scope",
                memory_id
            )));
        };

        let mut updated = original.clone();
        updated.content = new_content.to_string();
        updated.updated_at = Utc::now();
        updated.deleted_at = None;

        Self::maybe_compact(&path, raw_count, &live).await?;
        Self::append_entry(&path, &updated).await?;

        Ok(MemoryOpResult { id: memory_id.to_string(), deduplicated: false })
    }

    /// Hard-delete a thread's entire memory file. Called when the thread row
    /// itself is torn down (see `ThreadStore::delete` and the server-side
    /// delete-thread route that calls this right after) — unlike every other
    /// scope, thread-scoped memory has no lifecycle independent of its
    /// thread, so there is nothing left to tombstone once the thread is
    /// gone; the whole per-thread JSONL file goes with it rather than being
    /// soft-deleted line by line. A no-op (not an error) if the thread never
    /// wrote a memory, so purging a memory-less thread still succeeds.
    pub async fn purge_thread(&self, thread_id: &str) -> Result<(), AoError> {
        let path = self.data_root.memory_thread_path(thread_id);
        let lock = self.lock_for(&format!("thread:{}", thread_id));
        let _guard = lock.lock().await;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AoError::from(e)),
        }
    }

    // --- Review/undo: restore + pin ---
    //
    // These reverse the two lifecycle mutations above (`apply_supersede`,
    // `apply_archive`) so the review queue's `undo` action can put a
    // superseded entry back into live rotation, and expose the `pin`
    // action's eviction-exemption flag. Both follow the same
    // read-live/append-updated shape as `supersede`/`archive` above.

    /// Reverse [`Self::supersede`]/[`Self::archive`] on an agent-scoped
    /// entry: flip it back to `Active` and clear `superseded_by`. Used by
    /// the `undo` action to put back an entry a since-undone write had
    /// superseded. Errors if `memory_id` is not a live (non-tombstoned)
    /// entry — an already-undone or never-existing id is the caller's bug,
    /// not a silent no-op.
    pub async fn restore(&self, agent_id: &str, memory_id: &str) -> Result<(), AoError> {
        let path = self.data_root.memory_agent_path(agent_id);
        let lock = self.lock_for(&format!("agent:{}", agent_id));
        let _guard = lock.lock().await;
        Self::apply_restore(&path, memory_id, "agent", self.index.as_ref()).await
    }

    /// Reverse [`Self::supersede_global`]/[`Self::archive_global`]. See [`Self::restore`].
    pub async fn restore_global(&self, memory_id: &str) -> Result<(), AoError> {
        let path = self.data_root.memory_global_path();
        let lock = self.lock_for("global");
        let _guard = lock.lock().await;
        Self::apply_restore(&path, memory_id, "global", self.index.as_ref()).await
    }

    /// Reverse [`Self::supersede_project`]/[`Self::archive_project`]. See [`Self::restore`].
    pub async fn restore_project(&self, project_hash: &str, memory_id: &str) -> Result<(), AoError> {
        let path = self.data_root.memory_project_path(project_hash);
        let lock = self.lock_for(&format!("project:{}", project_hash));
        let _guard = lock.lock().await;
        Self::apply_restore(&path, memory_id, "project", self.index.as_ref()).await
    }

    /// Set (or clear) the `pinned` eviction-exemption flag on an
    /// agent-scoped entry, without touching its content or lifecycle
    /// status. Errors if `memory_id` is not a live entry.
    pub async fn set_pinned(&self, agent_id: &str, memory_id: &str, pinned: bool) -> Result<(), AoError> {
        let path = self.data_root.memory_agent_path(agent_id);
        let lock = self.lock_for(&format!("agent:{}", agent_id));
        let _guard = lock.lock().await;
        Self::apply_set_pinned(&path, memory_id, pinned, "agent", self.index.as_ref()).await
    }

    /// Set (or clear) `pinned` on a global entry. See [`Self::set_pinned`].
    pub async fn set_pinned_global(&self, memory_id: &str, pinned: bool) -> Result<(), AoError> {
        let path = self.data_root.memory_global_path();
        let lock = self.lock_for("global");
        let _guard = lock.lock().await;
        Self::apply_set_pinned(&path, memory_id, pinned, "global", self.index.as_ref()).await
    }

    /// Set (or clear) `pinned` on a project-scoped entry. See [`Self::set_pinned`].
    pub async fn set_pinned_project(
        &self,
        project_hash: &str,
        memory_id: &str,
        pinned: bool,
    ) -> Result<(), AoError> {
        let path = self.data_root.memory_project_path(project_hash);
        let lock = self.lock_for(&format!("project:{}", project_hash));
        let _guard = lock.lock().await;
        Self::apply_set_pinned(&path, memory_id, pinned, "project", self.index.as_ref()).await
    }

    // --- Private helpers ---

    /// Shared body for `supersede`/`supersede_global`/`supersede_project`:
    /// read the live entry, flip its lifecycle to `Superseded`, and append
    /// the updated row (append-only, mirrors how `edit*` updates content).
    /// Must be called under the scope's write lock. `index` re-upserts the
    /// entry so the search index picks up the status change even though the
    /// searchable text itself doesn't change.
    async fn apply_supersede(
        path: &std::path::Path,
        memory_id: &str,
        superseded_by: &str,
        scope_label: &str,
        index: Option<&SearchIndex>,
    ) -> Result<(), AoError> {
        let (live, raw_count) = Self::read_live(path).await?;
        let Some(original) = live.iter().find(|e| e.id == memory_id) else {
            return Err(AoError::Internal(format!(
                "Memory entry {} not found in {} scope",
                memory_id, scope_label
            )));
        };

        let mut updated = original.clone();
        updated.status = MemoryStatus::Superseded;
        updated.superseded_by = Some(superseded_by.to_string());
        updated.updated_at = Utc::now();

        Self::maybe_compact(path, raw_count, &live).await?;
        Self::append_entry(path, &updated).await?;
        if let Some(index) = index {
            if let Err(e) = index.upsert(to_index_record(&updated)).await {
                tracing::warn!("search index upsert failed for memory entry {}: {}", updated.id, e);
            }
        }
        Ok(())
    }

    /// Shared body for `archive`/`archive_global`/`archive_project`: read the
    /// live entry, flip its lifecycle to `Archived`, and append the updated
    /// row (append-only, mirrors [`Self::apply_supersede`]). Unlike
    /// supersession this never sets `superseded_by` — an archived entry was
    /// evicted to make room, not replaced by a specific successor. Must be
    /// called under the scope's write lock.
    async fn apply_archive(
        path: &std::path::Path,
        memory_id: &str,
        scope_label: &str,
        index: Option<&SearchIndex>,
    ) -> Result<(), AoError> {
        let (live, raw_count) = Self::read_live(path).await?;
        let Some(original) = live.iter().find(|e| e.id == memory_id) else {
            return Err(AoError::Internal(format!(
                "Memory entry {} not found in {} scope",
                memory_id, scope_label
            )));
        };

        let mut updated = original.clone();
        updated.status = MemoryStatus::Archived;
        updated.updated_at = Utc::now();

        Self::maybe_compact(path, raw_count, &live).await?;
        Self::append_entry(path, &updated).await?;
        if let Some(index) = index {
            if let Err(e) = index.upsert(to_index_record(&updated)).await {
                tracing::warn!("search index upsert failed for memory entry {}: {}", updated.id, e);
            }
        }
        Ok(())
    }

    /// Shared body for `restore`/`restore_global`/`restore_project`
    /// (`undo`): read the live entry, flip its lifecycle back to `Active`,
    /// and clear `superseded_by`. The exact inverse of
    /// [`Self::apply_supersede`] — deliberately does not touch `Archived`
    /// entries any differently, since `undo` only ever targets an entry
    /// this specific write superseded (never one evicted separately).
    /// Must be called under the scope's write lock.
    async fn apply_restore(
        path: &std::path::Path,
        memory_id: &str,
        scope_label: &str,
        index: Option<&SearchIndex>,
    ) -> Result<(), AoError> {
        let (live, raw_count) = Self::read_live(path).await?;
        let Some(original) = live.iter().find(|e| e.id == memory_id) else {
            return Err(AoError::Internal(format!(
                "Memory entry {} not found in {} scope",
                memory_id, scope_label
            )));
        };

        let mut updated = original.clone();
        updated.status = MemoryStatus::Active;
        updated.superseded_by = None;
        updated.updated_at = Utc::now();

        Self::maybe_compact(path, raw_count, &live).await?;
        Self::append_entry(path, &updated).await?;
        if let Some(index) = index {
            if let Err(e) = index.upsert(to_index_record(&updated)).await {
                tracing::warn!("search index upsert failed for memory entry {}: {}", updated.id, e);
            }
        }
        Ok(())
    }

    /// Shared body for `set_pinned`/`set_pinned_global`/`set_pinned_project`
    /// (`pin`): read the live entry, flip its `pinned` flag, and append
    /// the updated row. Does not touch lifecycle `status` — pinning is
    /// orthogonal to `Active`/`Superseded`/`Archived`. Must be called under
    /// the scope's write lock.
    async fn apply_set_pinned(
        path: &std::path::Path,
        memory_id: &str,
        pinned: bool,
        scope_label: &str,
        index: Option<&SearchIndex>,
    ) -> Result<(), AoError> {
        let (live, raw_count) = Self::read_live(path).await?;
        let Some(original) = live.iter().find(|e| e.id == memory_id) else {
            return Err(AoError::Internal(format!(
                "Memory entry {} not found in {} scope",
                memory_id, scope_label
            )));
        };

        let mut updated = original.clone();
        updated.pinned = pinned;
        updated.updated_at = Utc::now();

        Self::maybe_compact(path, raw_count, &live).await?;
        Self::append_entry(path, &updated).await?;
        if let Some(index) = index {
            if let Err(e) = index.upsert(to_index_record(&updated)).await {
                tracing::warn!("search index upsert failed for memory entry {}: {}", updated.id, e);
            }
        }
        Ok(())
    }

    /// Read all raw entries from a JSONL file, deduplicate by ID (last occurrence wins),
    /// and filter to live (non-tombstoned) entries. Returns (live_entries, raw_line_count).
    async fn read_live(
        path: &std::path::Path,
    ) -> Result<(Vec<MemoryEntry>, usize), AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok((Vec::new(), 0));
        }

        let contents = tokio::fs::read_to_string(path).await?;
        let raw_lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        let raw_count = raw_lines.len();

        // Last occurrence of each ID wins (handles tombstones and edits)
        let mut last_seen: HashMap<String, MemoryEntry> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for line in raw_lines {
            let mut entry: MemoryEntry =
                serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?;
            // Legacy entries written before the `updated_at` field existed
            // deserialize with the Unix epoch sentinel from `default_updated_at`.
            // Treat that as "never edited" and fall back to `created_at` so
            // display, recency sort, and budget-truncation order all behave.
            // Every modern write path stamps `Utc::now()`, so the epoch cannot
            // legitimately appear on a real entry.
            if entry.updated_at == DateTime::<Utc>::UNIX_EPOCH {
                entry.updated_at = entry.created_at;
            }
            if !last_seen.contains_key(&entry.id) {
                order.push(entry.id.clone());
            }
            last_seen.insert(entry.id.clone(), entry);
        }

        // Filter live entries in insertion order
        let live: Vec<MemoryEntry> = order
            .into_iter()
            .filter_map(|id| {
                let entry = last_seen.remove(&id)?;
                if entry.deleted_at.is_none() { Some(entry) } else { None }
            })
            .collect();

        Ok((live, raw_count))
    }

    /// Rewrite the file with only live entries if they are below 80% of raw line count.
    /// Must be called under the scope write lock.
    async fn maybe_compact(
        path: &std::path::Path,
        raw_count: usize,
        live_entries: &[MemoryEntry],
    ) -> Result<(), AoError> {
        if raw_count == 0 || live_entries.len() * 100 >= raw_count * 80 {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp_path = path.with_extension("jsonl.tmp");
        let mut content = String::new();
        for entry in live_entries {
            let line =
                serde_json::to_string(entry).map_err(|e| AoError::Json(e.to_string()))?;
            content.push_str(&line);
            content.push('\n');
        }
        tokio::fs::write(&tmp_path, &content).await?;
        tokio::fs::rename(&tmp_path, path).await?;
        Ok(())
    }

    async fn append_entry(
        path: &std::path::Path,
        entry: &MemoryEntry,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::DataRoot;
    use ao_protocol::memory::MemorySource;

    fn make_store(tmp: &tempfile::TempDir) -> MemoryStore {
        MemoryStore::new(DataRoot::new(tmp.path()))
    }

    #[tokio::test]
    async fn test_agent_scope_write_list_restart_delete() {
        let tmp = tempfile::tempdir().unwrap();

        let entry = make_store(&tmp)
            .add("agent-1", "remember this", MemorySource::Agent)
            .await
            .unwrap();

        // Simulate restart: re-instantiate MemoryStore pointing at same path
        let store2 = make_store(&tmp);
        let entries = store2.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, entry.id);
        assert_eq!(entries[0].content, "remember this");

        let deleted = store2.delete("agent-1", &entry.id).await.unwrap();
        assert!(deleted);

        assert!(store2.list("agent-1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_thread_scope_write_list_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);

        let entry = store
            .add_thread("thread-1", "remember this turn", MemorySource::Manual)
            .await
            .unwrap();

        // Visible from a second MemoryStore instance with the same thread id.
        let store2 = make_store(&tmp);
        let entries = store2.list_thread("thread-1").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, entry.id);
        assert_eq!(entries[0].content, "remember this turn");
        assert_eq!(entries[0].scope, MemoryScope::Thread);
        assert_eq!(entries[0].scope_key, Some("thread-1".to_string()));

        // Not visible under a different thread id.
        assert!(store2.list_thread("thread-2").await.unwrap().is_empty());

        let deleted = store2.delete_thread("thread-1", &entry.id).await.unwrap();
        assert!(deleted);
        assert!(store2.list_thread("thread-1").await.unwrap().is_empty());

        // Deleting a missing entry reports false, no error.
        assert!(!store2.delete_thread("thread-1", &entry.id).await.unwrap());
    }

    #[tokio::test]
    async fn test_purge_thread_removes_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = MemoryStore::new(data_root.clone());

        store
            .add_thread("thread-1", "ephemeral note", MemorySource::Manual)
            .await
            .unwrap();
        let path = data_root.memory_thread_path("thread-1");
        assert!(tokio::fs::try_exists(&path).await.unwrap(), "file must exist after a write");

        store.purge_thread("thread-1").await.unwrap();
        assert!(
            !tokio::fs::try_exists(&path).await.unwrap(),
            "purge_thread must hard-delete the thread's memory file"
        );
        assert!(
            store.list_thread("thread-1").await.unwrap().is_empty(),
            "a purged thread must read back as having no entries"
        );
    }

    #[tokio::test]
    async fn test_purge_thread_on_missing_file_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        // Never wrote anything for this thread — must not error.
        assert!(store.purge_thread("never-written").await.is_ok());
    }

    #[tokio::test]
    async fn test_global_scope_visible_from_different_agent_instance() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = make_store(&tmp)
            .add_global("global knowledge", MemorySource::Agent)
            .await
            .unwrap();

        let store2 = make_store(&tmp);
        let globals = store2.list_global().await.unwrap();
        assert_eq!(globals.len(), 1);
        assert_eq!(globals[0].id, entry.id);
        assert_eq!(globals[0].content, "global knowledge");
    }

    #[tokio::test]
    async fn test_project_scope_isolation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);

        let hash_a = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4";
        let hash_b = "b9e8d7c6f5a4b9e8d7c6f5a4b9e8d7c6";

        let op = store.add_project(hash_a, "project A secret", MemorySource::Agent).await.unwrap();
        assert!(!op.deduplicated);

        // Not visible under project B
        assert!(
            store.list_project(hash_b).await.unwrap().is_empty(),
            "project B must not see project A's memories"
        );

        // Visible from a second MemoryStore instance with the same hash
        let a_entries = make_store(&tmp).list_project(hash_a).await.unwrap();
        assert_eq!(a_entries.len(), 1);
        assert_eq!(a_entries[0].content, "project A secret");
    }

    #[tokio::test]
    async fn test_memory_edit_preserves_created_at_updates_content() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);

        let entry = store
            .add("agent-1", "original content", MemorySource::Agent)
            .await
            .unwrap();
        let original_created_at = entry.created_at;

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        store.edit("agent-1", &entry.id, "updated content").await.unwrap();

        let entries = store.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), 1, "old content must no longer appear in list");
        assert_eq!(entries[0].content, "updated content");
        assert_eq!(entries[0].id, entry.id);
        assert_eq!(entries[0].created_at, original_created_at, "created_at must be preserved");
        assert!(entries[0].updated_at >= original_created_at);
    }

    #[tokio::test]
    async fn test_restore_reverses_supersede() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);

        let old = store.add("agent-1", "old fact", MemorySource::Agent).await.unwrap();
        let new = store.add("agent-1", "new fact", MemorySource::Agent).await.unwrap();
        store.supersede("agent-1", &old.id, &new.id).await.unwrap();

        let entries = store.list("agent-1").await.unwrap();
        let superseded = entries.iter().find(|e| e.id == old.id).unwrap();
        assert_eq!(superseded.status, MemoryStatus::Superseded);
        assert_eq!(superseded.superseded_by, Some(new.id.clone()));

        store.restore("agent-1", &old.id).await.unwrap();

        let entries = store.list("agent-1").await.unwrap();
        let restored = entries.iter().find(|e| e.id == old.id).unwrap();
        assert_eq!(restored.status, MemoryStatus::Active, "restore must flip status back to Active");
        assert_eq!(restored.superseded_by, None, "restore must clear superseded_by");
    }

    #[tokio::test]
    async fn test_restore_missing_entry_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let result = store.restore("agent-1", "nonexistent-id").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_set_pinned_flips_flag_and_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);

        let entry = store.add("agent-1", "pin me", MemorySource::Agent).await.unwrap();
        assert!(!entry.pinned, "entries are unpinned by default");

        store.set_pinned("agent-1", &entry.id, true).await.unwrap();

        // Simulate restart: pinned must be durable across a fresh MemoryStore.
        let store2 = make_store(&tmp);
        let entries = store2.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), 1, "flipping pinned must not duplicate the entry");
        assert!(entries[0].pinned);

        store2.set_pinned("agent-1", &entry.id, false).await.unwrap();
        let entries = store2.list("agent-1").await.unwrap();
        assert!(!entries[0].pinned, "set_pinned(false) must unpin");
    }

    #[tokio::test]
    async fn test_dedup_agent_scope_byte_equal_content() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);

        let e1 = store.add("agent-1", "same content", MemorySource::Agent).await.unwrap();
        let e2 = store.add("agent-1", "same content", MemorySource::Agent).await.unwrap();

        assert_eq!(e1.id, e2.id, "dedup must return the same entry ID");
        assert_eq!(store.list("agent-1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_dedup_project_scope_returns_deduplicated_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let hash = "c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6";

        let r1 = store.add_project(hash, "same content", MemorySource::Agent).await.unwrap();
        assert!(!r1.deduplicated);

        let r2 = store.add_project(hash, "same content", MemorySource::Agent).await.unwrap();
        assert!(r2.deduplicated, "second add with byte-equal content must be deduplicated");
        assert_eq!(r1.id, r2.id);
        assert_eq!(store.list_project(hash).await.unwrap().len(), 1);
    }

    /// Regression: entries written before `updated_at` existed on the struct
    /// deserialize with the Unix epoch via serde default. `read_live` must
    /// rewrite that sentinel to `created_at` so display, sort, and
    /// budget-truncation order all behave for legacy data.
    #[tokio::test]
    async fn test_legacy_entry_without_updated_at_falls_back_to_created_at() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let path = data_root.memory_agent_path("legacy-agent");

        // Hand-write a JSONL line with no `updated_at` key — matches the
        // on-disk format from before the field was added.
        let legacy_created_at = "2024-06-15T12:34:56Z";
        let legacy_line = format!(
            r#"{{"id":"legacy-001","content":"pre-updated_at entry","created_at":"{}"}}"#,
            legacy_created_at
        );
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&path, format!("{}\n", legacy_line))
            .await
            .unwrap();

        let store = MemoryStore::new(data_root);
        let entries = store.list("legacy-agent").await.unwrap();
        assert_eq!(entries.len(), 1);

        let expected: DateTime<Utc> = legacy_created_at.parse().unwrap();
        assert_eq!(entries[0].created_at, expected);
        assert_eq!(
            entries[0].updated_at, expected,
            "legacy epoch sentinel must be rewritten to created_at"
        );
        assert_ne!(
            entries[0].updated_at,
            DateTime::<Utc>::UNIX_EPOCH,
            "loaded entry must not surface the epoch sentinel"
        );
    }

    /// Regression: entries written before `confidence` / `status` / `superseded_by`
    /// existed on the struct must still deserialize, picking up the defaults
    /// (full confidence, Active status, no supersession).
    #[tokio::test]
    async fn test_legacy_entry_without_a1_fields_deserializes_with_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let path = data_root.memory_agent_path("legacy-a1-agent");

        // Hand-write a JSONL line with none of the keys — matches the
        // on-disk format from before confidence/status/superseded_by existed.
        let legacy_line = r#"{"id":"legacy-a1-001","content":"pre-A1 entry","created_at":"2024-06-15T12:34:56Z","updated_at":"2024-06-15T12:34:56Z"}"#;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&path, format!("{}\n", legacy_line))
            .await
            .unwrap();

        let store = MemoryStore::new(data_root);
        let entries = store.list("legacy-a1-agent").await.unwrap();
        assert_eq!(entries.len(), 1);

        assert_eq!(entries[0].confidence, 1.0, "legacy row must default confidence to 1.0");
        assert_eq!(
            entries[0].status,
            MemoryStatus::Active,
            "legacy row without a status key must read as Active"
        );
        assert_eq!(
            entries[0].superseded_by, None,
            "legacy row must default superseded_by to None"
        );
    }

    // --- Search index integration ---

    fn make_indexed_store(tmp: &tempfile::TempDir, index: SearchIndex) -> MemoryStore {
        MemoryStore::new(DataRoot::new(tmp.path())).with_index(index)
    }

    #[tokio::test]
    async fn add_upserts_into_attached_search_index() {
        let tmp = tempfile::tempdir().unwrap();
        let index = SearchIndex::open_in_memory().unwrap();
        let store = make_indexed_store(&tmp, index.clone());

        store.add("agent-1", "the deploy runbook lives in ops/", MemorySource::Agent).await.unwrap();

        let hits = index
            .query("deploy runbook".into(), ao_search_index::SearchFilter::new(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn delete_removes_entry_from_attached_search_index() {
        let tmp = tempfile::tempdir().unwrap();
        let index = SearchIndex::open_in_memory().unwrap();
        let store = make_indexed_store(&tmp, index.clone());

        let entry = store.add("agent-1", "ephemeral fact", MemorySource::Agent).await.unwrap();
        store.delete("agent-1", &entry.id).await.unwrap();

        let hits = index
            .query("ephemeral".into(), ao_search_index::SearchFilter::new(), 10)
            .await
            .unwrap();
        assert!(hits.is_empty(), "tombstoned entry must not remain searchable");
    }

    #[tokio::test]
    async fn edit_updates_search_index_text() {
        let tmp = tempfile::tempdir().unwrap();
        let index = SearchIndex::open_in_memory().unwrap();
        let store = make_indexed_store(&tmp, index.clone());

        let entry = store.add("agent-1", "original phrasing", MemorySource::Agent).await.unwrap();
        store.edit("agent-1", &entry.id, "revised phrasing").await.unwrap();

        assert!(index
            .query("original".into(), ao_search_index::SearchFilter::new(), 10)
            .await
            .unwrap()
            .is_empty());
        let hits = index
            .query("revised".into(), ao_search_index::SearchFilter::new(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, entry.id);
    }

    #[tokio::test]
    async fn global_and_project_writes_index_under_the_matching_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let index = SearchIndex::open_in_memory().unwrap();
        let store = make_indexed_store(&tmp, index.clone());

        store.add_global("shared convention", MemorySource::Manual).await.unwrap();
        store
            .add_project("project-hash-a", "shared convention", MemorySource::Manual)
            .await
            .unwrap();

        let global_hits = index
            .query(
                "shared convention".into(),
                ao_search_index::SearchFilter::new().with_scope(ao_search_index::IndexScope::Global),
                10,
            )
            .await
            .unwrap();
        assert_eq!(global_hits.len(), 1);

        let project_hits = index
            .query(
                "shared convention".into(),
                ao_search_index::SearchFilter::new()
                    .with_scope(ao_search_index::IndexScope::Project("project-hash-a".to_string())),
                10,
            )
            .await
            .unwrap();
        assert_eq!(project_hits.len(), 1);

        // Unfiltered query sees both.
        let all_hits = index
            .query("shared convention".into(), ao_search_index::SearchFilter::new(), 10)
            .await
            .unwrap();
        assert_eq!(all_hits.len(), 2);
    }

    #[tokio::test]
    async fn supersede_and_archive_keep_entry_searchable() {
        let tmp = tempfile::tempdir().unwrap();
        let index = SearchIndex::open_in_memory().unwrap();
        let store = make_indexed_store(&tmp, index.clone());

        let original = store.add("agent-1", "obsolete calibration steps", MemorySource::Agent).await.unwrap();
        let replacement = store.add("agent-1", "current calibration steps", MemorySource::Agent).await.unwrap();
        store.supersede("agent-1", &original.id, &replacement.id).await.unwrap();

        // Superseded entries are tombstoned in *lifecycle* status but not
        // `deleted_at`, so they remain indexed (a future retrieval consumer
        // decides whether to surface them, same as `MemoryStore::list`).
        // Query terms are OR'd (see `build_match_expression`), so "obsolete"
        // alone — a term unique to the original entry — proves it's still
        // indexed without also matching the replacement's shared vocabulary.
        let hits = index
            .query("obsolete".into(), ao_search_index::SearchFilter::new(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, original.id);

        store.archive("agent-1", &replacement.id).await.unwrap();
        let archived_hits = index
            .query("current".into(), ao_search_index::SearchFilter::new(), 10)
            .await
            .unwrap();
        assert_eq!(archived_hits.len(), 1);
    }

    #[tokio::test]
    async fn writes_without_an_attached_index_do_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        // No `.with_index(..)` attached — every sync_index_* call must be a
        // silent no-op rather than a panic or error.
        let entry = store.add("agent-1", "unindexed fact", MemorySource::Agent).await.unwrap();
        store.edit("agent-1", &entry.id, "still unindexed").await.unwrap();
        store.delete("agent-1", &entry.id).await.unwrap();
        assert!(store.reindex_all().await.is_ok());
    }

    #[tokio::test]
    async fn reindex_all_rebuilds_from_the_jsonl_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let index = SearchIndex::open_in_memory().unwrap();

        // Write entries with no index attached, simulating data that
        // predates the index (or a corrupted/deleted index file). Each
        // entry uses disjoint vocabulary so a query for one can't also
        // match another via the OR-of-terms match semantics (see
        // `build_match_expression`).
        let store = make_store(&tmp);
        store.add("agent-1", "widget calibration procedure", MemorySource::Agent).await.unwrap();
        store.add_global("sunset thermal protocol", MemorySource::Manual).await.unwrap();
        store.add_project("hash-a", "harbor lighthouse checklist", MemorySource::Manual).await.unwrap();

        assert!(index
            .query("widget".into(), ao_search_index::SearchFilter::new(), 10)
            .await
            .unwrap()
            .is_empty());

        // Attach the index after the fact and rebuild.
        let indexed_store = make_indexed_store(&tmp, index.clone());
        indexed_store.reindex_all().await.unwrap();

        for query in ["widget", "sunset", "harbor"] {
            let hits = index
                .query(query.into(), ao_search_index::SearchFilter::new(), 10)
                .await
                .unwrap();
            assert_eq!(hits.len(), 1, "expected exactly one hit for {query:?}");
        }
    }

    #[tokio::test]
    async fn reindex_all_only_touches_memory_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let index = SearchIndex::open_in_memory().unwrap();
        index
            .upsert(ao_search_index::IndexRecord {
                id: "skill-1".to_string(),
                scope: ao_search_index::IndexScope::Global,
                artifact: ArtifactKind::Skill,
                text: "unrelated skill row".to_string(),
            })
            .await
            .unwrap();

        let store = make_indexed_store(&tmp, index.clone());
        store.add_global("some fact", MemorySource::Manual).await.unwrap();
        store.reindex_all().await.unwrap();

        let hits = index
            .query("unrelated skill row".into(), ao_search_index::SearchFilter::new(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "reindex_all must not clobber non-memory rows");
    }
}
