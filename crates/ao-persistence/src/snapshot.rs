use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use ao_protocol::error::AoError;

use crate::paths::DataRoot;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub name: String,
    #[serde(default)]
    pub emoji: Option<String>,
    pub last_activity_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_agent_activity_at: Option<DateTime<Utc>>,
    pub last_message: Option<String>,
    pub message_count: u64,
    /// Runtime field — overlaid at read time by `list_agents`. Persisted as
    /// `false` on disk regardless of actual run state; `#[serde(default)]`
    /// keeps deserialization tolerant of legacy snapshots that still carry
    /// stale `true`s.
    #[serde(default)]
    pub has_active_run: bool,
    /// Runtime field — overlaid at read time. Which of this agent's threads
    /// currently have an active run, so the sidebar can badge the exact
    /// thread row instead of only the agent row. See `has_active_run`.
    #[serde(default)]
    pub running_thread_ids: Vec<String>,
    /// Runtime field — overlaid at read time. See `has_active_run`.
    #[serde(default)]
    pub queue_depth: u32,
    /// Which thread `last_message` landed in — `None` means the agent's
    /// default thread, `Some(id)` a concrete fresh/branch thread. Written
    /// alongside `last_message` at both its production write sites (the
    /// message-enqueue route and `TimelineAdapter::persist_pending`) so the
    /// pair can never describe different events.
    ///
    /// This repurposes the field that used to be named `thread_id` — it was
    /// never actually written past its `None` default, so nothing reads a
    /// stale value here. `#[serde(rename = "thread_id")]` keeps the wire/
    /// on-disk key exactly as it always was (both for legacy snapshots and
    /// for every frontend `AgentSnapshot` fixture that already types this
    /// key as `thread_id`) — only the meaning changes, from "always null"
    /// to "actually populated," so nothing downstream needs to migrate.
    #[serde(rename = "thread_id")]
    pub last_message_thread_id: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub file_capabilities_supported: bool,
    /// When set, this agent is an inline team coordinator owned by the given team.
    /// Filtered out of chat surfaces by default; opt in with `include_team_coordinators=true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owning_team_id: Option<String>,
    /// Computed at read time from the agent's full profile — not persisted.
    /// 0 = leaf agent (no delegation), N = deepest delegation chain of length N.
    /// `#[serde(default)]` keeps deserialization tolerant of stored snapshots that
    /// predate this field (they'll read back as 0 and get overridden on the next list).
    #[serde(default)]
    pub coordinator_level: u8,
    /// Async forms posted by the runner and not yet answered or dismissed —
    /// at most one per distinct `PendingForm::thread_id` (including at most
    /// one with `thread_id: None`, the agent's default thread). Gives the UI
    /// O(1) "pending form?" lookup per thread without paginating the
    /// transcript. Mutated only via [`SnapshotStore::set_pending_form`] (which
    /// upserts by `thread_id`) and [`SnapshotStore::clear_pending_form`]
    /// (which removes by `form_id`) — never push/retain this directly, or the
    /// at-most-one-per-thread invariant can be broken.
    ///
    /// Project-scoped snapshot entries (`project_{id}` keys) never have more
    /// than one entry here, since projects have no thread concept — the
    /// runner always records project-scoped forms with `thread_id: None`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_forms: Vec<PendingForm>,
    /// Title of the agent's currently-active tasklist, or `None` when no
    /// tasklist is in the `Active` state. Cleared on terminal transitions.
    /// Recomputed on server startup and kept in sync by the agent snapshot
    /// tasklist sync. Gives the sidebar an O(1) "running tasklist?" lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tasklist_title: Option<String>,
}

/// One agent-visible async form waiting on an answer, scoped to the thread
/// it was posted on. See [`AgentSnapshot::pending_forms`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingForm {
    /// `None` = the agent's default thread, `Some(id)` = a concrete thread —
    /// same convention as `AgentSnapshot::last_message_thread_id` and
    /// `RunnerContext::thread_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub form_id: String,
    /// Full `AsyncFormRequestMeta` JSON (mirrors the old `pending_form_spec`
    /// shape) so the UI can render the form even when the originating
    /// `form_request` transcript entry is outside the loaded message window.
    pub spec: serde_json::Value,
    /// Runtime field — overlaid at response-build time by `ao-server`'s
    /// `GET /agents` route (`list_agents`), the same way `AgentSnapshot`'s
    /// own `has_active_run`/`queue_depth` are. `true` iff this form's own
    /// `form_request` transcript entry is still the last non-hidden entry in
    /// its thread — i.e. nothing (a skipped-past message, an agent reply, a
    /// stopped run) has happened in the thread since it was posted. The value
    /// written to disk is never meaningful on its own since every `GET
    /// /agents` recomputes it fresh from the thread's transcript tail;
    /// `set_pending_form` always writes `true` as a sane baseline.
    /// `#[serde(default = "default_true")]` resolves a missing field (an
    /// on-disk snapshot from before this field existed, or a frontend talking
    /// to a backend that predates it) to `true` — never silently hiding a
    /// live badge. `skip_serializing_if` omits the field entirely when
    /// `true`, so the common case (still latest) costs nothing on the wire;
    /// the frontend applies the identical missing-means-true default.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub is_latest_in_thread: bool,
    /// `true` once startup hydration has determined that this form's owning
    /// run/session did not survive a process restart — set only for
    /// `spec["mode"] == "sync"` entries, by `ao-engine`'s startup sync-form
    /// reaper (see `sync_form_reaper::reap_orphaned_sync_forms`). A
    /// deliberately separate field rather than an overload of
    /// `is_latest_in_thread`: that field answers "did something else happen
    /// in this thread since," recomputed fresh on every `GET /agents`; this
    /// one answers "will this process EVER deliver an answer for this form,"
    /// a durable fact stamped once and never recomputed, since a sync form's
    /// suspended `tokio::sync::oneshot` + parked task cannot be resurrected
    /// by any later event. Async forms never get this field set — they are
    /// restart-durable by construction (they never suspend a task at all),
    /// so the reaper skips every entry whose mode isn't `"sync"`.
    /// `#[serde(default)]` reads a pre-existing on-disk snapshot (from
    /// before this field existed) as not-orphaned, which is simply true for
    /// every one of those entries.
    #[serde(default, skip_serializing_if = "is_false")]
    pub orphaned: bool,
}

fn default_true() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub agents: HashMap<String, AgentSnapshot>,
    pub updated_at: DateTime<Utc>,
}

impl Snapshot {
    fn empty() -> Self {
        Self {
            agents: HashMap::new(),
            updated_at: Utc::now(),
        }
    }
}

/// In-memory snapshot backed by atomic disk persistence.
pub struct SnapshotStore {
    data_root: DataRoot,
    snapshot: Arc<RwLock<Snapshot>>,
}

impl SnapshotStore {
    /// Load snapshot from disk or return empty snapshot.
    pub async fn load(data_root: DataRoot) -> Result<Self, AoError> {
        let path = data_root.snapshot_path();
        let snapshot = if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            let contents = tokio::fs::read_to_string(&path).await?;
            serde_json::from_str::<Snapshot>(&contents).map_err(|e| AoError::Json(e.to_string()))?
        } else {
            Snapshot::empty()
        };
        Ok(Self {
            data_root,
            snapshot: Arc::new(RwLock::new(snapshot)),
        })
    }

    /// Save snapshot atomically (write to temp file then rename).
    pub async fn save(&self) -> Result<(), AoError> {
        let snapshot = {
            let guard = self.snapshot.read().await;
            guard.clone()
        };
        let path = self.data_root.snapshot_path();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let json =
            serde_json::to_string_pretty(&snapshot).map_err(|e| AoError::Json(e.to_string()))?;

        // Write to temp file in the same directory, then rename (atomic on most filesystems)
        let tmp_path = path.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, json).await?;
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }

    /// Get a clone of the in-memory snapshot (no disk read).
    pub async fn get(&self) -> Snapshot {
        let guard = self.snapshot.read().await;
        guard.clone()
    }

    /// Update an agent entry using a patch closure, then persist to disk.
    /// `patch` may return a value (e.g. a record it just removed from the
    /// entry) — [`Self::set_pending_form`] uses this to hand back a replaced
    /// [`PendingForm`] to its caller. Callers that don't need a return value
    /// can leave the closure body as a plain statement; `R` infers to `()`.
    pub async fn update_agent_entry<F, R>(&self, agent_id: &str, patch: F) -> Result<R, AoError>
    where
        F: FnOnce(&mut AgentSnapshot) -> R,
    {
        let result = {
            let mut guard = self.snapshot.write().await;
            guard.updated_at = Utc::now();
            if let Some(entry) = guard.agents.get_mut(agent_id) {
                patch(entry)
            } else {
                // Create a default entry and apply patch
                let mut entry = AgentSnapshot {
                    agent_id: agent_id.to_string(),
                    name: String::new(),
                    emoji: None,
                    last_activity_at: None,
                    last_agent_activity_at: None,
                    last_message: None,
                    message_count: 0,
                    has_active_run: false,
                    running_thread_ids: Vec::new(),
                    queue_depth: 0,
                    last_message_thread_id: None,
                    created_at: Utc::now(),
                    file_capabilities_supported: false,
                    owning_team_id: None,
                    coordinator_level: 0,
                    pending_forms: Vec::new(),
                    active_tasklist_title: None,
                };
                let result = patch(&mut entry);
                guard.agents.insert(agent_id.to_string(), entry);
                result
            }
        };
        self.save().await?;
        Ok(result)
    }

    /// Upsert a [`PendingForm`] onto the agent snapshot, keyed by `thread_id`,
    /// and persist to disk. Removes any existing entry for the same
    /// `thread_id` first, so a thread can never carry more than one pending
    /// form — a second form posted on a thread before the first was
    /// answered replaces it rather than appending alongside it. That
    /// replacement is still unconditional and silent as far as this store is
    /// concerned; returning the record it dropped is what lets a caller
    /// (see `ao_engine_tools_core::form_events::persist_posted_form`) leave a
    /// trace of it elsewhere without this layer reaching for a transcript
    /// writer itself.
    ///
    /// Returns the [`PendingForm`] that was replaced, if any — `None` when
    /// `thread_id` had no prior pending form. Under the at-most-one-per-
    /// thread invariant this is always at most one record; if that invariant
    /// were ever violated, every matching record is still removed (mirroring
    /// the old `retain`-only behavior) but only the first is handed back.
    ///
    /// Runs under `update_agent_entry`'s write-lock-held closure, so the
    /// remove-then-push is atomic with respect to concurrent
    /// `set_pending_form`/`clear_pending_form` calls on the same entry.
    pub async fn set_pending_form(
        &self,
        agent_id: &str,
        thread_id: Option<String>,
        form_id: String,
        spec: serde_json::Value,
    ) -> Result<Option<PendingForm>, AoError> {
        self.update_agent_entry(agent_id, |entry| {
            let replaced = entry
                .pending_forms
                .iter()
                .position(|f| f.thread_id == thread_id)
                .map(|i| entry.pending_forms.remove(i));
            entry.pending_forms.retain(|f| f.thread_id != thread_id);
            entry.pending_forms.push(PendingForm {
                thread_id,
                form_id,
                spec,
                is_latest_in_thread: true,
                orphaned: false,
            });
            replaced
        })
        .await
    }

    /// Mark the [`PendingForm`] matching `form_id` as orphaned and persist to
    /// disk. Called only by the startup sync-form reaper (`ao-engine`'s
    /// `sync_form_reaper::reap_orphaned_sync_forms`) for a `mode: "sync"`
    /// pending form whose owning run/session did not survive a process
    /// restart. Leaves every other field on the form untouched — the entry
    /// stays in `pending_forms` (still removable via [`Self::clear_pending_form`]
    /// on the off chance a stray answer/dismiss ever arrives for it) but the
    /// frontend renders it non-interactive once `orphaned` is `true`.
    ///
    /// A no-op (not an error) when `form_id` doesn't match any pending form
    /// on `agent_id` — mirrors [`Self::clear_pending_form`]'s tolerance for a
    /// form that resolved between the reaper's snapshot read and this call.
    pub async fn mark_pending_form_orphaned(
        &self,
        agent_id: &str,
        form_id: &str,
    ) -> Result<(), AoError> {
        self.update_agent_entry(agent_id, |entry| {
            for form in entry.pending_forms.iter_mut() {
                if form.form_id == form_id {
                    form.orphaned = true;
                }
            }
        })
        .await
    }

    /// Remove the [`PendingForm`] matching `form_id` from the agent snapshot
    /// and persist to disk. Looked up by `form_id` (server-generated,
    /// globally unique) rather than `thread_id` because the `form_answer` /
    /// `form_dismissed` routes that call this only ever receive `form_id` in
    /// their path params.
    ///
    /// Call this from the `form_answer` and `form_dismissed` routes after
    /// appending the corresponding transcript entry. Calling it when the
    /// form_id isn't pending (already answered/dismissed, or never existed)
    /// is harmless — `update_agent_entry` is idempotent.
    pub async fn clear_pending_form(&self, agent_id: &str, form_id: &str) -> Result<(), AoError> {
        self.update_agent_entry(agent_id, |entry| {
            entry.pending_forms.retain(|f| f.form_id != form_id);
        })
        .await
    }

    /// Remove the ASYNC [`PendingForm`] (if any) filed on `(agent_id,
    /// thread_id)` and persist to disk. Looked up by `thread_id` rather than
    /// `form_id` because the callers of this (run cancellation, thread
    /// deletion) don't know the form_id up front — they only know which
    /// thread's slot they're about to strand.
    ///
    /// A SYNC form on the same slot is deliberately left untouched: that
    /// mode is cleaned up exclusively by `PendingFormClearGuard`'s `Drop`
    /// impl (`ao-engine-tools-runner`'s `prompt_bridge` module), which fires
    /// from the very same cancellation this method's callers are responding
    /// to. Clearing it here too would race that `Drop` — both would remove
    /// the same record, and whichever caller reaches the transcript/event
    /// side second would either double-write a withdrawn trace for a form
    /// that already has one, or (for `form_id`-keyed lookups elsewhere) find
    /// nothing left to act on. Mode is read off `PendingForm.spec`'s `mode`
    /// field (`"async"` / `"sync"` — see `set_pending_form`'s callers), not
    /// a dedicated column — see the `spec.get("mode")` read a few lines below.
    ///
    /// Returns the removed record, or `None` when nothing async was pending
    /// on that thread — nothing pending at all, or the pending record there
    /// was a sync form.
    pub async fn clear_pending_async_form_for_thread(
        &self,
        agent_id: &str,
        thread_id: Option<String>,
    ) -> Result<Option<PendingForm>, AoError> {
        self.update_agent_entry(agent_id, |entry| {
            let idx = entry.pending_forms.iter().position(|f| {
                f.thread_id == thread_id
                    && f.spec.get("mode").and_then(serde_json::Value::as_str) == Some("async")
            })?;
            Some(entry.pending_forms.remove(idx))
        })
        .await
    }

    /// Remove an agent entry and persist to disk.
    pub async fn remove_agent_entry(&self, agent_id: &str) -> Result<(), AoError> {
        {
            let mut guard = self.snapshot.write().await;
            guard.updated_at = Utc::now();
            guard.agents.remove(agent_id);
        }
        self.save().await
    }

    /// Flush the in-memory snapshot to disk.
    pub async fn flush(&self) -> Result<(), AoError> {
        self.save().await
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::DataRoot;
    use tempfile::TempDir;

    async fn store(dir: &TempDir) -> SnapshotStore {
        let root = DataRoot::new(dir.path());
        root.ensure_directories().await.unwrap();
        SnapshotStore::load(root).await.unwrap()
    }

    /// A pre-existing on-disk snapshot persisted under the field's old name
    /// (`"thread_id"`, always `null` since it was never actually written)
    /// must still deserialize cleanly into the repurposed field.
    #[tokio::test]
    async fn legacy_thread_id_key_deserializes_into_last_message_thread_id() {
        let dir = TempDir::new().unwrap();
        let root = DataRoot::new(dir.path());
        root.ensure_directories().await.unwrap();

        let legacy_json = serde_json::json!({
            "agents": {
                "agent-legacy": {
                    "agent_id": "agent-legacy",
                    "name": "Legacy Agent",
                    "last_activity_at": null,
                    "last_message": "hi",
                    "message_count": 1,
                    "has_active_run": false,
                    "queue_depth": 0,
                    "thread_id": null,
                    "created_at": "2026-01-01T00:00:00Z"
                }
            },
            "teams": {},
            "updated_at": "2026-01-01T00:00:00Z"
        });
        tokio::fs::write(root.snapshot_path(), serde_json::to_vec(&legacy_json).unwrap())
            .await
            .unwrap();

        let s = SnapshotStore::load(root).await.unwrap();
        let snap = s.get().await;
        let agent = snap.agents.get("agent-legacy").unwrap();
        assert_eq!(
            agent.last_message_thread_id, None,
            "legacy `thread_id: null` must deserialize into the renamed field"
        );

        // And once re-saved, the wire/on-disk key name is unchanged — only
        // the value now reflects a real thread instead of always `null`.
        s.update_agent_entry("agent-legacy", |e| {
            e.last_message_thread_id = Some("fresh-thread-9".to_string());
        })
        .await
        .unwrap();
        let on_disk: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(s.data_root.snapshot_path()).await.unwrap()).unwrap();
        assert_eq!(
            on_disk["agents"]["agent-legacy"]["thread_id"],
            serde_json::json!("fresh-thread-9"),
            "re-saved snapshot must keep using the historical `thread_id` key"
        );
    }

    #[tokio::test]
    async fn set_pending_form_appends_and_persists() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        s.set_pending_form(
            "agent-1",
            None,
            "form-abc".to_string(),
            serde_json::json!({"title": "t"}),
        )
        .await
        .unwrap();

        let snap = s.get().await;
        let agent = snap.agents.get("agent-1").unwrap();
        assert_eq!(agent.pending_forms.len(), 1);
        assert_eq!(agent.pending_forms[0].form_id, "form-abc");
        assert_eq!(agent.pending_forms[0].thread_id, None);
    }

    #[tokio::test]
    async fn set_pending_form_upserts_by_thread_id() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        s.set_pending_form(
            "agent-1",
            Some("thread-a".to_string()),
            "form-1".to_string(),
            serde_json::json!({}),
        )
        .await
        .unwrap();
        s.set_pending_form(
            "agent-1",
            Some("thread-a".to_string()),
            "form-2".to_string(),
            serde_json::json!({}),
        )
        .await
        .unwrap();

        let snap = s.get().await;
        let agent = snap.agents.get("agent-1").unwrap();
        assert_eq!(
            agent.pending_forms.len(),
            1,
            "second post on the same thread must replace, not append"
        );
        assert_eq!(agent.pending_forms[0].form_id, "form-2");
    }

    /// The caller (`persist_posted_form`) needs the replaced record itself —
    /// not just the fact that a replacement happened — to build a
    /// self-contained withdrawn-line trace. First post on a thread must
    /// return `None`; a post that replaces one must hand back that exact
    /// record, spec included.
    #[tokio::test]
    async fn set_pending_form_returns_the_replaced_record() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        let first = s
            .set_pending_form(
                "agent-1",
                Some("thread-a".to_string()),
                "form-1".to_string(),
                serde_json::json!({"spec": {"title": "First question"}}),
            )
            .await
            .unwrap();
        assert!(first.is_none(), "nothing pending yet on this thread");

        let replaced = s
            .set_pending_form(
                "agent-1",
                Some("thread-a".to_string()),
                "form-2".to_string(),
                serde_json::json!({"spec": {"title": "Second question"}}),
            )
            .await
            .unwrap();
        let replaced = replaced.expect("form-1 was still pending on thread-a");
        assert_eq!(replaced.form_id, "form-1");
        assert_eq!(replaced.thread_id.as_deref(), Some("thread-a"));
        assert_eq!(replaced.spec["spec"]["title"], serde_json::json!("First question"));

        // The snapshot itself still only carries the new form — same
        // replace-not-append behavior as before this change.
        let snap = s.get().await;
        let agent = snap.agents.get("agent-1").unwrap();
        assert_eq!(agent.pending_forms.len(), 1);
        assert_eq!(agent.pending_forms[0].form_id, "form-2");
    }

    #[tokio::test]
    async fn set_pending_form_keeps_separate_threads_independent() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        s.set_pending_form(
            "agent-1",
            None,
            "form-default".to_string(),
            serde_json::json!({}),
        )
        .await
        .unwrap();
        s.set_pending_form(
            "agent-1",
            Some("thread-b".to_string()),
            "form-b".to_string(),
            serde_json::json!({}),
        )
        .await
        .unwrap();

        let snap = s.get().await;
        let agent = snap.agents.get("agent-1").unwrap();
        assert_eq!(agent.pending_forms.len(), 2);
        assert!(agent.pending_forms.iter().any(|f| f.form_id == "form-default" && f.thread_id.is_none()));
        assert!(agent
            .pending_forms
            .iter()
            .any(|f| f.form_id == "form-b" && f.thread_id.as_deref() == Some("thread-b")));
    }

    #[tokio::test]
    async fn clear_pending_form_removes_only_matching_form_id() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        s.set_pending_form(
            "agent-1",
            None,
            "form-xyz".to_string(),
            serde_json::json!({}),
        )
        .await
        .unwrap();
        s.set_pending_form(
            "agent-1",
            Some("thread-b".to_string()),
            "form-b".to_string(),
            serde_json::json!({}),
        )
        .await
        .unwrap();

        s.clear_pending_form("agent-1", "form-xyz").await.unwrap();

        let snap = s.get().await;
        let agent = snap.agents.get("agent-1").unwrap();
        assert_eq!(agent.pending_forms.len(), 1, "only the matching form_id is removed");
        assert_eq!(agent.pending_forms[0].form_id, "form-b");
    }

    #[tokio::test]
    async fn mark_pending_form_orphaned_sets_flag_and_persists() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        s.set_pending_form(
            "agent-1",
            None,
            "form-sync".to_string(),
            serde_json::json!({"mode": "sync"}),
        )
        .await
        .unwrap();

        s.mark_pending_form_orphaned("agent-1", "form-sync")
            .await
            .unwrap();

        let snap = s.get().await;
        let agent = snap.agents.get("agent-1").unwrap();
        assert_eq!(agent.pending_forms.len(), 1, "orphaning must not remove the entry");
        assert!(agent.pending_forms[0].orphaned, "form must be marked orphaned");

        // And the flag survives a reload from disk.
        let reloaded = SnapshotStore::load(DataRoot::new(dir.path())).await.unwrap();
        let reloaded_snap = reloaded.get().await;
        assert!(reloaded_snap.agents.get("agent-1").unwrap().pending_forms[0].orphaned);
    }

    #[tokio::test]
    async fn mark_pending_form_orphaned_leaves_other_forms_untouched() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        s.set_pending_form(
            "agent-1",
            None,
            "form-sync".to_string(),
            serde_json::json!({"mode": "sync"}),
        )
        .await
        .unwrap();
        s.set_pending_form(
            "agent-1",
            Some("thread-b".to_string()),
            "form-async".to_string(),
            serde_json::json!({"mode": "async"}),
        )
        .await
        .unwrap();

        s.mark_pending_form_orphaned("agent-1", "form-sync")
            .await
            .unwrap();

        let snap = s.get().await;
        let agent = snap.agents.get("agent-1").unwrap();
        let sync_form = agent.pending_forms.iter().find(|f| f.form_id == "form-sync").unwrap();
        let async_form = agent.pending_forms.iter().find(|f| f.form_id == "form-async").unwrap();
        assert!(sync_form.orphaned);
        assert!(!async_form.orphaned, "unrelated form must not be touched");
    }

    #[tokio::test]
    async fn mark_pending_form_orphaned_on_unknown_form_id_is_harmless() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        s.mark_pending_form_orphaned("agent-1", "never-existed")
            .await
            .unwrap();

        let snap = s.get().await;
        assert!(snap
            .agents
            .get("agent-1")
            .map(|a| a.pending_forms.is_empty())
            .unwrap_or(true));
    }

    #[tokio::test]
    async fn clear_pending_form_on_unknown_form_id_is_harmless() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        s.clear_pending_form("agent-1", "never-existed").await.unwrap();

        let snap = s.get().await;
        assert!(snap
            .agents
            .get("agent-1")
            .map(|a| a.pending_forms.is_empty())
            .unwrap_or(true));
    }

    fn async_spec(title: &str) -> serde_json::Value {
        serde_json::json!({ "form_id": "f", "spec": { "title": title }, "mode": "async" })
    }

    fn sync_spec(title: &str) -> serde_json::Value {
        serde_json::json!({ "form_id": "f", "spec": { "title": title }, "mode": "sync" })
    }

    /// The anti-lockout case: an async form stranded by a cancelled run or a
    /// deleted thread must come out cleanly, and the record handed back must
    /// be the full one (form_id + spec) — the caller needs both to write a
    /// self-contained withdrawn-line trace, same as `set_pending_form`'s
    /// replaced-record contract.
    #[tokio::test]
    async fn clear_pending_async_form_for_thread_removes_async_form_and_returns_it() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        s.set_pending_form(
            "agent-1",
            Some("thread-a".to_string()),
            "form-async".to_string(),
            async_spec("Deploy now?"),
        )
        .await
        .unwrap();

        let removed = s
            .clear_pending_async_form_for_thread("agent-1", Some("thread-a".to_string()))
            .await
            .unwrap();
        let removed = removed.expect("an async form was pending on thread-a");
        assert_eq!(removed.form_id, "form-async");
        assert_eq!(removed.spec["spec"]["title"], serde_json::json!("Deploy now?"));

        let snap = s.get().await;
        assert!(
            snap.agents["agent-1"].pending_forms.is_empty(),
            "the slot must be vacated"
        );
    }

    /// Anti-lockout, end to end: once the stranded async form is vacated, a
    /// brand new form can be posted on the very same thread — this is the
    /// whole point of the fix (a stranded async form previously locked that
    /// thread out of ever posting another one, because the occupied-slot
    /// guard treats a live `pending_forms` entry as blocking).
    #[tokio::test]
    async fn clear_pending_async_form_for_thread_then_new_form_is_accepted() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        s.set_pending_form(
            "agent-1",
            Some("thread-a".to_string()),
            "form-stranded".to_string(),
            async_spec("Are you sure?"),
        )
        .await
        .unwrap();

        s.clear_pending_async_form_for_thread("agent-1", Some("thread-a".to_string()))
            .await
            .unwrap();

        let replaced = s
            .set_pending_form(
                "agent-1",
                Some("thread-a".to_string()),
                "form-new".to_string(),
                async_spec("Second question"),
            )
            .await
            .unwrap();
        assert!(
            replaced.is_none(),
            "the slot was vacated, so this post must not read as replacing anything"
        );

        let snap = s.get().await;
        assert_eq!(snap.agents["agent-1"].pending_forms.len(), 1);
        assert_eq!(snap.agents["agent-1"].pending_forms[0].form_id, "form-new");
    }

    /// A SYNC form on the slot must be left alone — that mode is cleared
    /// exclusively by `PendingFormClearGuard`'s `Drop` impl
    /// (`ao-engine-tools-runner`), and this method double-clearing it would
    /// race that fallback.
    #[tokio::test]
    async fn clear_pending_async_form_for_thread_leaves_sync_form_untouched() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        s.set_pending_form(
            "agent-1",
            Some("thread-a".to_string()),
            "form-sync".to_string(),
            sync_spec("Confirm deploy"),
        )
        .await
        .unwrap();

        let removed = s
            .clear_pending_async_form_for_thread("agent-1", Some("thread-a".to_string()))
            .await
            .unwrap();
        assert!(removed.is_none(), "a sync form must never be reported as cleared here");

        let snap = s.get().await;
        assert_eq!(
            snap.agents["agent-1"].pending_forms.len(),
            1,
            "the sync form's own slot must still be occupied"
        );
        assert_eq!(snap.agents["agent-1"].pending_forms[0].form_id, "form-sync");
    }

    /// Cancelling/deleting thread A must never reach into thread B's slot —
    /// each thread's pending form is independent.
    #[tokio::test]
    async fn clear_pending_async_form_for_thread_does_not_affect_other_threads() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        s.set_pending_form(
            "agent-1",
            Some("thread-a".to_string()),
            "form-a".to_string(),
            async_spec("Thread A question"),
        )
        .await
        .unwrap();
        s.set_pending_form(
            "agent-1",
            Some("thread-b".to_string()),
            "form-b".to_string(),
            async_spec("Thread B question"),
        )
        .await
        .unwrap();

        let removed = s
            .clear_pending_async_form_for_thread("agent-1", Some("thread-a".to_string()))
            .await
            .unwrap();
        assert_eq!(removed.unwrap().form_id, "form-a");

        let snap = s.get().await;
        let agent = &snap.agents["agent-1"];
        assert_eq!(agent.pending_forms.len(), 1, "thread-b's form must survive untouched");
        assert_eq!(agent.pending_forms[0].form_id, "form-b");
        assert_eq!(agent.pending_forms[0].thread_id.as_deref(), Some("thread-b"));
    }

    #[tokio::test]
    async fn clear_pending_async_form_for_thread_on_unknown_thread_is_harmless() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir).await;

        let removed = s
            .clear_pending_async_form_for_thread("agent-1", Some("never-existed".to_string()))
            .await
            .unwrap();
        assert!(removed.is_none());
    }
}
