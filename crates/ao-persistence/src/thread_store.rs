use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use uuid::Uuid;

use ao_protocol::error::AoError;
use ao_protocol::reflection_trigger::{
    NoopReflectionSubscriber, ReflectionTrigger, ReflectionTriggerReason,
    ReflectionTriggerSubscriber,
};
use ao_protocol::thread::{
    artifact_thread_id, default_thread_id, AssignmentBridgeOrigin, BranchSource,
    ChannelBridgeOrigin, Thread, ThreadId, ThreadKind, ThreadScope,
};

use crate::paths::DataRoot;

/// In-memory thread metadata store, backed by an atomic JSON file at
/// [`DataRoot::threads_path`].
///
/// One row per thread. Default rows are materialized lazily by
/// [`Self::ensure_default_thread`] and alias the agent's pre-existing
/// transcript file at [`DataRoot::agent_transcript_path`]. Fresh and branch
/// rows own a distinct transcript path under
/// [`DataRoot::threads_data_dir`].
///
/// There is no eager pass that pre-creates default rows at startup. Every
/// read path either calls [`Self::ensure_default_thread`] itself
/// ([`Self::list_for_agent`], [`Self::find_by_transcript_path`],
/// [`Self::resolve_or_default`]) or falls back to the agent-keyed transcript
/// path, which is the same file a default row aliases — so an agent whose row
/// does not exist yet is indistinguishable from one whose row does.
pub struct ThreadStore {
    data_root: DataRoot,
    threads: Arc<RwLock<Vec<Thread>>>,
    /// Reflection-trigger subscriber — fired on
    /// explicit archive. Defaults to a no-op; late-bound to a real subscriber
    /// via [`Self::set_reflection_subscriber`] once the reflection pass
    /// exists. `std::sync::RwLock` (not tokio's) since reads/writes here are
    /// never held across an `.await`.
    reflection_subscriber: std::sync::RwLock<Arc<dyn ReflectionTriggerSubscriber>>,
}

impl ThreadStore {
    /// Load existing rows from disk; returns an empty store on first boot.
    pub async fn load(data_root: DataRoot) -> Result<Self, AoError> {
        let path = data_root.threads_path();
        let threads = if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            let contents = tokio::fs::read_to_string(&path).await?;
            if contents.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str::<Vec<Thread>>(&contents)
                    .map_err(|e| AoError::Json(e.to_string()))?
            }
        } else {
            Vec::new()
        };
        Ok(Self {
            data_root,
            threads: Arc::new(RwLock::new(threads)),
            reflection_subscriber: std::sync::RwLock::new(Arc::new(NoopReflectionSubscriber)),
        })
    }

    /// Late-bind the reflection-trigger subscriber (called once the
    /// reflection pass exists — see
    /// `ao_protocol::reflection_trigger::ReflectionTriggerSubscriber`).
    /// Until this is called, [`Self::archive`] fires triggers into
    /// [`NoopReflectionSubscriber`].
    pub fn set_reflection_subscriber(&self, subscriber: Arc<dyn ReflectionTriggerSubscriber>) {
        *self
            .reflection_subscriber
            .write()
            .expect("reflection subscriber lock") = subscriber;
    }

    /// Stable id of an agent's default thread. Mirrors
    /// [`ao_protocol::thread::default_thread_id`] so persistence-layer
    /// callers don't need an extra import.
    pub fn default_thread_id(agent_id: &str) -> ThreadId {
        default_thread_id(agent_id)
    }

    /// Stable id of an artifact's chat mini-thread. Mirrors
    /// [`ao_protocol::thread::artifact_thread_id`].
    pub fn artifact_thread_id(artifact_id: &str) -> ThreadId {
        artifact_thread_id(artifact_id)
    }

    async fn save(&self) -> Result<(), AoError> {
        let threads = {
            let guard = self.threads.read().await;
            guard.clone()
        };
        let path = self.data_root.threads_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(&threads)
            .map_err(|e| AoError::Json(e.to_string()))?;
        let tmp_path = path.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, json).await?;
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }

    /// All threads scoped to the given agent. Lazily ensures the default
    /// thread row exists first, so callers always see at least one row.
    pub async fn list_for_agent(&self, agent_id: &str) -> Result<Vec<Thread>, AoError> {
        self.ensure_default_thread(agent_id).await?;
        let guard = self.threads.read().await;
        let mut out: Vec<Thread> = guard
            .iter()
            .filter(|t| Self::owning_agent_id(t) == Some(agent_id))
            .cloned()
            .collect();
        Self::sort_agent_threads(&mut out);
        Ok(out)
    }

    /// Every thread across every agent, grouped by owning agent id, using the
    /// exact same ownership predicate and sort as [`Self::list_for_agent`] —
    /// see [`Self::owning_agent_id`] and [`Self::sort_agent_threads`], which
    /// this method and `list_for_agent` both call so the two can never
    /// diverge on "which threads belong to this agent". Takes a single read
    /// lock and walks the underlying `Vec` exactly once to bucket rows by
    /// agent id, then sorts each agent's bucket in place.
    ///
    /// Unlike `list_for_agent`, this does NOT call
    /// [`Self::ensure_default_thread`] first: that call is scoped to one
    /// agent id, and there is no single agent id here to scope it to.
    /// Materializing a default row for every agent the caller might
    /// eventually ask about would mean discovering the full agent id set
    /// from elsewhere and taking a write lock per agent — precisely the
    /// per-agent-lock pattern this method exists to avoid. Practical
    /// consequence: an agent with zero persisted thread rows (its default
    /// thread has never been lazily created) is simply absent from the
    /// returned map, whereas `list_for_agent(that_agent)` would return a
    /// single freshly-materialized default row. Every agent that has ever
    /// been listed via `list_for_agent`, or otherwise touched a thread, is
    /// unaffected by this difference.
    pub async fn list_all_grouped(&self) -> Result<HashMap<String, Vec<Thread>>, AoError> {
        let guard = self.threads.read().await;
        let mut grouped: HashMap<String, Vec<Thread>> = HashMap::new();
        for thread in guard.iter() {
            if let Some(agent_id) = Self::owning_agent_id(thread) {
                grouped
                    .entry(agent_id.to_string())
                    .or_default()
                    .push(thread.clone());
            }
        }
        for threads in grouped.values_mut() {
            Self::sort_agent_threads(threads);
        }
        Ok(grouped)
    }

    /// The agent id that owns `thread`, per the ownership rule
    /// [`Self::list_for_agent`] and [`Self::list_all_grouped`] both apply:
    /// only `ThreadScope::AgentChat` threads have an owning agent.
    /// `TeamChat`, `Delegation`, and `Artifact` threads are never owned by an
    /// agent this way (see [`ThreadScope`]'s docs) and always return `None`.
    fn owning_agent_id(thread: &Thread) -> Option<&str> {
        match &thread.scope {
            ThreadScope::AgentChat { agent_id } => Some(agent_id.as_str()),
            _ => None,
        }
    }

    /// Sort a slice of one agent's threads in place: default thread first,
    /// then by creation time ascending. Shared by [`Self::list_for_agent`]
    /// and [`Self::list_all_grouped`] so the two never drift onto different
    /// orderings.
    fn sort_agent_threads(threads: &mut [Thread]) {
        threads.sort_by(|a, b| match (a.kind, b.kind) {
            (ThreadKind::Default, ThreadKind::Default) => a.created_at.cmp(&b.created_at),
            (ThreadKind::Default, _) => std::cmp::Ordering::Less,
            (_, ThreadKind::Default) => std::cmp::Ordering::Greater,
            _ => a.created_at.cmp(&b.created_at),
        });
    }

    pub async fn get(&self, thread_id: &str) -> Result<Option<Thread>, AoError> {
        let guard = self.threads.read().await;
        Ok(guard.iter().find(|t| t.id == thread_id).cloned())
    }

    /// Resolves `thread_id` to its own transcript file path when it names a
    /// non-default thread. Returns `None` for the default thread (its rows
    /// alias the scope's own pre-existing file already, so an append keyed
    /// by the scope id is byte-equivalent) and `None` for a missing
    /// `thread_id` or a lookup that comes back empty/erroring — callers
    /// should fall back to their pre-existing scope-keyed transcript path in
    /// both of those cases.
    ///
    /// Single canonical implementation of this resolution — shared by
    /// `ao-engine-tools-core`'s posted-form write path
    /// (`form_events::resolve_thread_override_path`) and `ao-server`'s
    /// answered-form write path (`routes::form_answers::async_form_answer`)
    /// so the two never drift into separate rules for the same question.
    pub async fn resolve_transcript_path_override(&self, thread_id: Option<&str>) -> Option<PathBuf> {
        let thread_id = thread_id?;
        let thread = self.get(thread_id).await.ok().flatten()?;
        if thread.kind == ThreadKind::Default {
            return None;
        }
        Some(PathBuf::from(thread.transcript_path))
    }

    /// Resolve the `Thread` row backing `transcript_path` — the identity a
    /// [`ReflectionTrigger`] carries (`agent_id` + `transcript_path`; there is
    /// no `thread_id` on the event, see `ao_protocol::reflection_trigger`).
    /// Ensures `agent_id`'s default thread row exists first, so a trigger
    /// firing before any other thread-store access for this agent still
    /// resolves instead of spuriously missing.
    pub async fn find_by_transcript_path(
        &self,
        agent_id: &str,
        transcript_path: &str,
    ) -> Result<Option<Thread>, AoError> {
        self.ensure_default_thread(agent_id).await?;
        let guard = self.threads.read().await;
        Ok(guard
            .iter()
            .find(|t| t.transcript_path == transcript_path)
            .cloned())
    }

    /// Resolve an optional `thread_id` to its `Thread` row, filtering out
    /// `ThreadKind::Default` rows so callers get back only threads that own a
    /// distinct transcript file.
    ///
    /// Returns `None` (meaning: keep the legacy agent/project-keyed transcript
    /// path) for an absent `thread_id`, a lookup miss, or a `Default`-kind
    /// thread (the default thread aliases the agent's existing transcript, so
    /// treating it as "no thread" keeps single-thread agents byte-equivalent
    /// with pre-thread behavior). Returns `Some(thread)` only for a
    /// Fresh/Branch thread, whose `transcript_path` callers should write to
    /// instead of the legacy path.
    ///
    /// Mirrors the inline resolution `agent_runner::cli` and
    /// `agent_runner::native` perform at run-start for turn history/transcript
    /// routing; shared here for the completion-marker call sites in
    /// `task_feeder` and `delegate_completion`, which resolve the same way at
    /// tasklist/delegate completion time.
    pub async fn resolve_non_default(&self, thread_id: Option<&str>) -> Option<Thread> {
        match thread_id {
            Some(id) => self
                .get(id)
                .await
                .ok()
                .flatten()
                .filter(|t| t.kind != ThreadKind::Default),
            None => None,
        }
    }

    /// Insert a brand-new row. Errors with `ValidationError` on id collision.
    pub async fn create(&self, thread: Thread) -> Result<Thread, AoError> {
        {
            let mut guard = self.threads.write().await;
            if guard.iter().any(|t| t.id == thread.id) {
                return Err(AoError::ValidationError(format!(
                    "Thread already exists: {}",
                    thread.id
                )));
            }
            guard.push(thread.clone());
        }
        self.save().await?;
        Ok(thread)
    }

    /// Update the title and bump `updated_at`. Returns the post-mutation row.
    pub async fn rename(
        &self,
        thread_id: &str,
        new_title: Option<String>,
    ) -> Result<Thread, AoError> {
        let updated = {
            let mut guard = self.threads.write().await;
            let row = guard
                .iter_mut()
                .find(|t| t.id == thread_id)
                .ok_or_else(|| AoError::ThreadNotFound(thread_id.to_string()))?;
            row.title = new_title;
            row.updated_at = Utc::now();
            row.clone()
        };
        self.save().await?;
        Ok(updated)
    }

    /// Set `auto_title` the first time a thread receives content to derive
    /// one from, but only if the thread has never been named at all —
    /// neither `title` (explicit rename) nor `auto_title` (an earlier call to
    /// this same method) is set yet.
    ///
    /// Re-checks both fields under the write lock immediately before
    /// mutating, so two concurrent calls for the same thread (e.g. a racing
    /// duplicate request) can't both "win": the first to acquire the lock
    /// sets it, the second observes `auto_title.is_some()` and no-ops.
    ///
    /// Returns `Ok(Some(thread))` with the post-mutation row when this call
    /// won the race and actually set the field, `Ok(None)` when the thread
    /// was already named in either sense (a legitimate no-op, not an error),
    /// and `Err(ThreadNotFound)` for an unknown id.
    pub async fn set_auto_title_if_unset(
        &self,
        thread_id: &str,
        auto_title: String,
    ) -> Result<Option<Thread>, AoError> {
        let updated = {
            let mut guard = self.threads.write().await;
            let row = guard
                .iter_mut()
                .find(|t| t.id == thread_id)
                .ok_or_else(|| AoError::ThreadNotFound(thread_id.to_string()))?;
            if row.title.is_some() || row.auto_title.is_some() {
                return Ok(None);
            }
            row.auto_title = Some(auto_title);
            row.updated_at = Utc::now();
            row.clone()
        };
        self.save().await?;
        Ok(Some(updated))
    }

    /// Drop the row. Refuses to delete kind `Default` so the back-compat
    /// alias to the agent's transcript file is never severed by accident.
    /// Returns `Ok(false)` if no row matched.
    pub async fn delete(&self, thread_id: &str) -> Result<bool, AoError> {
        let removed = {
            let mut guard = self.threads.write().await;
            if let Some(idx) = guard.iter().position(|t| t.id == thread_id) {
                if guard[idx].kind == ThreadKind::Default {
                    return Err(AoError::ValidationError(
                        "Cannot delete an agent's default thread".to_string(),
                    ));
                }
                guard.remove(idx);
                true
            } else {
                false
            }
        };
        if removed {
            self.save().await?;
        }
        Ok(removed)
    }

    /// Return the agent's default thread row, creating it (and persisting)
    /// if it does not yet exist. The default row's `transcript_path` aliases
    /// the agent's pre-existing JSONL file so no message movement is needed.
    pub async fn ensure_default_thread(
        &self,
        agent_id: &str,
    ) -> Result<Thread, AoError> {
        let id = Self::default_thread_id(agent_id);

        {
            let guard = self.threads.read().await;
            if let Some(existing) = guard.iter().find(|t| t.id == id) {
                return Ok(existing.clone());
            }
        }

        let transcript_path = self
            .data_root
            .agent_transcript_path(agent_id)
            .to_string_lossy()
            .into_owned();
        let now = Utc::now();
        let row = Thread {
            id: id.clone(),
            title: None,
            auto_title: None,
            scope: ThreadScope::AgentChat {
                agent_id: agent_id.to_string(),
            },
            transcript_path,
            kind: ThreadKind::Default,
            history_floor_ts: None,
            distilled_through_ts: None,
            promotion_swept_at: None,
            branch_source: None,
            archived_at: None,
            channel_origin: None,
            assignment_origin: None,
            created_at: now,
            updated_at: now,
        };

        {
            let mut guard = self.threads.write().await;
            // Re-check under write lock to stay race-safe.
            if let Some(existing) = guard.iter().find(|t| t.id == id) {
                return Ok(existing.clone());
            }
            guard.push(row.clone());
        }
        self.save().await?;
        Ok(row)
    }

    /// Return an artifact's chat mini-thread row, creating it (and
    /// persisting) if it does not yet exist. The row's `transcript_path`
    /// aliases [`crate::paths::DataRoot::artifact_thread_path`] — the same
    /// file the artifact chat panel has always used — so routing chat
    /// through a `Thread` row is a pure indirection change with no message
    /// movement.
    pub async fn ensure_artifact_thread(&self, artifact_id: &str) -> Result<Thread, AoError> {
        let id = Self::artifact_thread_id(artifact_id);

        {
            let guard = self.threads.read().await;
            if let Some(existing) = guard.iter().find(|t| t.id == id) {
                return Ok(existing.clone());
            }
        }

        let transcript_path = self
            .data_root
            .artifact_thread_path(artifact_id)
            .to_string_lossy()
            .into_owned();
        let now = Utc::now();
        let row = Thread {
            id: id.clone(),
            title: None,
            auto_title: None,
            scope: ThreadScope::Artifact {
                artifact_id: artifact_id.to_string(),
            },
            transcript_path,
            kind: ThreadKind::Default,
            history_floor_ts: None,
            distilled_through_ts: None,
            promotion_swept_at: None,
            branch_source: None,
            archived_at: None,
            channel_origin: None,
            assignment_origin: None,
            created_at: now,
            updated_at: now,
        };

        {
            let mut guard = self.threads.write().await;
            // Re-check under write lock to stay race-safe.
            if let Some(existing) = guard.iter().find(|t| t.id == id) {
                return Ok(existing.clone());
            }
            guard.push(row.clone());
        }
        self.save().await?;
        Ok(row)
    }

    /// Stamps `origin` onto `thread_id`'s `channel_origin` field if — and
    /// only if — that thread exists and doesn't already carry one. No-op
    /// (not an error) when the thread is missing (already deleted) or
    /// already stamped, so this is safe to call unconditionally and
    /// repeatedly.
    ///
    /// This is the one-time backfill for bridge threads created before
    /// `channel_origin` existed — today that's exactly Slack's
    /// per-conversation threads (see `ChannelBridgeOrigin`'s docstring),
    /// whose identity otherwise lives only in `SlackConversationRegistryStore`
    /// and was invisible to both the composer-gating hint and the backend's
    /// `is_channel_bridge_thread` tool-admission gate. Called from
    /// `PersistenceLayer::init_with_root` for every row that store has ever
    /// recorded.
    pub async fn backfill_channel_origin(
        &self,
        thread_id: &str,
        origin: ChannelBridgeOrigin,
    ) -> Result<(), AoError> {
        {
            let mut guard = self.threads.write().await;
            let Some(thread) = guard.iter_mut().find(|t| t.id == thread_id) else {
                return Ok(());
            };
            if thread.channel_origin.is_some() {
                return Ok(());
            }
            thread.channel_origin = Some(origin);
        }
        self.save().await
    }

    /// Stamps `origin` onto `thread_id`'s `assignment_origin` field if — and
    /// only if — that thread exists and doesn't already carry one. No-op
    /// (not an error) when the thread is missing or already stamped, so this
    /// is safe to call unconditionally and repeatedly.
    ///
    /// This is the one-time backfill for `Fresh`/`Dedicated`-policy
    /// assignment threads created before `assignment_origin` existed. Called
    /// from `PersistenceLayer::init_with_root`, which walks every persisted
    /// `AssignmentRun` (for `Fresh`) and every `Assignment::dedicated_thread_id`
    /// (for `Dedicated`) and stamps the thread each one resolved to.
    pub async fn backfill_assignment_origin(
        &self,
        thread_id: &str,
        origin: AssignmentBridgeOrigin,
    ) -> Result<(), AoError> {
        {
            let mut guard = self.threads.write().await;
            let Some(thread) = guard.iter_mut().find(|t| t.id == thread_id) else {
                return Ok(());
            };
            if thread.assignment_origin.is_some() {
                return Ok(());
            }
            thread.assignment_origin = Some(origin);
        }
        self.save().await
    }

    /// Resolve "the agent's history" from an optional thread id. The mapping
    /// honors the thread back-compat rule:
    ///
    /// - `None` or empty string ⇒ default thread (created on the fly).
    /// - The deterministic default id ⇒ default thread (created on the fly).
    /// - Any other id ⇒ the matching row, or [`AoError::ThreadNotFound`].
    ///
    /// Used by callers that today take only an `agent_id` and now accept an
    /// optional `thread_id` query/path parameter without breaking pre-thread
    /// clients.
    pub async fn resolve_or_default(
        &self,
        agent_id: &str,
        thread_id: Option<&str>,
    ) -> Result<Thread, AoError> {
        let default_id = Self::default_thread_id(agent_id);
        match thread_id {
            None => self.ensure_default_thread(agent_id).await,
            Some(id) if id.is_empty() || id == default_id => {
                self.ensure_default_thread(agent_id).await
            }
            Some(id) => self
                .get(id)
                .await?
                .ok_or_else(|| AoError::ThreadNotFound(id.to_string())),
        }
    }

    /// Build a Thread row for an operator-created `Fresh` thread. Caller
    /// passes the row into [`Self::create`].
    pub fn build_fresh_thread(
        &self,
        agent_id: &str,
        title: Option<String>,
    ) -> Thread {
        let id = Uuid::new_v4().to_string();
        let transcript_path = self
            .data_root
            .thread_transcript_path(&id)
            .to_string_lossy()
            .into_owned();
        let now = Utc::now();
        Thread {
            id,
            title,
            auto_title: None,
            scope: ThreadScope::AgentChat {
                agent_id: agent_id.to_string(),
            },
            transcript_path,
            kind: ThreadKind::Fresh,
            history_floor_ts: None,
            distilled_through_ts: None,
            promotion_swept_at: None,
            branch_source: None,
            archived_at: None,
            channel_origin: None,
            assignment_origin: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Build a Thread row for an operator-created `Branch` thread. The
    /// `history_floor_ts` is mirrored from `branch_source.branch_at` so the
    /// runtime sees a single source of truth at compose time.
    pub fn build_branch_thread(
        &self,
        agent_id: &str,
        title: Option<String>,
        branch_source: BranchSource,
    ) -> Thread {
        let id = Uuid::new_v4().to_string();
        let transcript_path = self
            .data_root
            .thread_transcript_path(&id)
            .to_string_lossy()
            .into_owned();
        let now = Utc::now();
        let floor = branch_source.branch_at;
        Thread {
            id,
            title,
            auto_title: None,
            scope: ThreadScope::AgentChat {
                agent_id: agent_id.to_string(),
            },
            transcript_path,
            kind: ThreadKind::Branch,
            history_floor_ts: Some(floor),
            // Starts unset, NOT inherited from the source thread: a Branch
            // only ever distills its own post-fork delta (the shared prefix
            // up to `history_floor_ts` was already handled while distilling
            // the source), so its watermark has nothing to inherit.
            distilled_through_ts: None,
            promotion_swept_at: None,
            branch_source: Some(branch_source),
            archived_at: None,
            channel_origin: None,
            assignment_origin: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Archive a thread — sets `archived_at` and bumps `updated_at`, hiding it
    /// from every surface that filters on [`Thread::is_archived`] without
    /// touching the transcript or any other metadata. Refuses to archive a
    /// `Default` thread (mirrors [`Self::delete`]'s guard) since Main must
    /// always stay visible. A no-op (returns the row unchanged) if already
    /// archived.
    pub async fn archive(&self, thread_id: &str) -> Result<Thread, AoError> {
        let (updated, newly_archived) = {
            let mut guard = self.threads.write().await;
            let row = guard
                .iter_mut()
                .find(|t| t.id == thread_id)
                .ok_or_else(|| AoError::ThreadNotFound(thread_id.to_string()))?;
            if row.kind == ThreadKind::Default {
                return Err(AoError::ValidationError(
                    "Cannot archive an agent's default thread".to_string(),
                ));
            }
            let newly_archived = row.archived_at.is_none();
            if newly_archived {
                row.archived_at = Some(Utc::now());
                row.updated_at = Utc::now();
            }
            (row.clone(), newly_archived)
        };
        self.save().await?;
        // Reflection trigger (reason `Archived`) fires only on the actual
        // None -> Some transition, not on a redundant re-archive call — the
        // thread's untrimmed history hasn't changed since the last time this
        // fired.
        if newly_archived {
            self.emit_reflection_trigger(&updated, ReflectionTriggerReason::Archived);
        }
        Ok(updated)
    }

    /// Fire a reflection trigger for `thread`, if it's an agent-chat thread.
    /// Other scopes (`TeamChat`, `Delegation`) aren't backed by the
    /// per-agent-thread distillation watermark this trigger exists to
    /// drive, so they're skipped rather than emitted with a made-up agent id.
    fn emit_reflection_trigger(&self, thread: &Thread, reason: ReflectionTriggerReason) {
        let ThreadScope::AgentChat { agent_id } = &thread.scope else {
            return;
        };
        let subscriber = self
            .reflection_subscriber
            .read()
            .expect("reflection subscriber lock")
            .clone();
        subscriber.on_reflection_trigger(ReflectionTrigger {
            reason,
            agent_id: agent_id.clone(),
            transcript_path: thread.transcript_path.clone(),
            ts: thread.updated_at,
        });
    }

    /// Reverse of [`Self::archive`] — clears `archived_at` and bumps
    /// `updated_at`, restoring the thread to every surface it was hidden
    /// from. A no-op (returns the row unchanged) if not currently archived.
    pub async fn unarchive(&self, thread_id: &str) -> Result<Thread, AoError> {
        let updated = {
            let mut guard = self.threads.write().await;
            let row = guard
                .iter_mut()
                .find(|t| t.id == thread_id)
                .ok_or_else(|| AoError::ThreadNotFound(thread_id.to_string()))?;
            if row.archived_at.is_some() {
                row.archived_at = None;
                row.updated_at = Utc::now();
            }
            row.clone()
        };
        self.save().await?;
        Ok(updated)
    }

    /// Advance [`Thread::distilled_through_ts`] to `ts`.
    ///
    /// Monotonic and idempotent by construction: a `ts` at or behind the
    /// current watermark is a no-op, so a reflection pass that reprocesses
    /// the same delta (or is called with a stale end-of-delta timestamp) can
    /// never move the watermark backward and re-open already-distilled
    /// history.
    pub async fn advance_distillation_watermark(
        &self,
        thread_id: &str,
        ts: DateTime<Utc>,
    ) -> Result<Thread, AoError> {
        let updated = {
            let mut guard = self.threads.write().await;
            let row = guard
                .iter_mut()
                .find(|t| t.id == thread_id)
                .ok_or_else(|| AoError::ThreadNotFound(thread_id.to_string()))?;
            if row.distilled_through_ts.is_none_or(|w| ts > w) {
                row.distilled_through_ts = Some(ts);
                row.updated_at = Utc::now();
            }
            row.clone()
        };
        self.save().await?;
        Ok(updated)
    }

    /// Advance [`Thread::promotion_swept_at`] (the periodic in-life
    /// promotion sweep's debounce watermark) to `ts`.
    ///
    /// Monotonic and idempotent by construction, mirroring
    /// [`Self::advance_distillation_watermark`] exactly: a `ts` at or behind
    /// the current watermark is a no-op, so a sweep that somehow re-fires
    /// with a stale timestamp can never move the watermark backward and
    /// re-open a debounce window that already elapsed.
    pub async fn advance_promotion_sweep_watermark(
        &self,
        thread_id: &str,
        ts: DateTime<Utc>,
    ) -> Result<Thread, AoError> {
        let updated = {
            let mut guard = self.threads.write().await;
            let row = guard
                .iter_mut()
                .find(|t| t.id == thread_id)
                .ok_or_else(|| AoError::ThreadNotFound(thread_id.to_string()))?;
            if row.promotion_swept_at.is_none_or(|w| ts > w) {
                row.promotion_swept_at = Some(ts);
                row.updated_at = Utc::now();
            }
            row.clone()
        };
        self.save().await?;
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
    use chrono::Duration;

    fn setup() -> (tempfile::TempDir, DataRoot) {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        (tmp, data_root)
    }

    async fn ready_store() -> (tempfile::TempDir, DataRoot, ThreadStore) {
        let (tmp, data_root) = setup();
        data_root.ensure_directories().await.unwrap();
        let store = ThreadStore::load(data_root.clone()).await.unwrap();
        (tmp, data_root, store)
    }

    #[tokio::test]
    async fn load_returns_empty_store_when_no_file() {
        let (_tmp, _root, store) = ready_store().await;
        let threads = store.list_for_agent("agent-1").await.unwrap();
        // list_for_agent lazily creates the default thread, so we should see exactly one row.
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].kind, ThreadKind::Default);
    }

    #[tokio::test]
    async fn ensure_default_thread_is_idempotent_and_persists_alias() {
        let (_tmp, data_root, store) = ready_store().await;

        let first = store.ensure_default_thread("agent-1").await.unwrap();
        assert_eq!(first.kind, ThreadKind::Default);
        assert_eq!(first.id, format!("default-{}", "agent-1"));

        // transcript_path aliases the agent's existing transcript file.
        let expected_path = data_root
            .agent_transcript_path("agent-1")
            .to_string_lossy()
            .into_owned();
        assert_eq!(first.transcript_path, expected_path);

        // Calling again returns the same row (no new id, no second push).
        let second = store.ensure_default_thread("agent-1").await.unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.created_at, first.created_at);

        // Round-trip via disk: a fresh ThreadStore sees only one row.
        let reload = ThreadStore::load(data_root).await.unwrap();
        let listed = reload.list_for_agent("agent-1").await.unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[tokio::test]
    async fn ensure_default_thread_is_non_destructive_to_existing_transcripts() {
        // Seed: write a message directly to the agent's pre-existing
        // transcript path. Materializing the default row must not touch the
        // bytes — the row aliases that file rather than owning a new one.
        let (_tmp, data_root, store) = ready_store().await;
        let path = data_root.agent_transcript_path("legacy-agent");

        let entry = TranscriptEntry {
            ts: Utc::now() - Duration::seconds(60),
            role: TranscriptRole::System("user".to_string()),
            content: "message written before the default row existed".to_string(),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        };
        let line = serde_json::to_string(&entry).unwrap();
        tokio::fs::write(&path, format!("{}\n", line)).await.unwrap();
        let bytes_before = tokio::fs::read(&path).await.unwrap();

        store.ensure_default_thread("legacy-agent").await.unwrap();

        // Default row exists and aliases the same path.
        let default = store
            .get(&ThreadStore::default_thread_id("legacy-agent"))
            .await
            .unwrap()
            .expect("default row should exist once materialized");
        assert_eq!(default.kind, ThreadKind::Default);
        assert_eq!(
            default.transcript_path,
            path.to_string_lossy().into_owned()
        );

        // Bytes are byte-for-byte identical.
        let bytes_after = tokio::fs::read(&path).await.unwrap();
        assert_eq!(bytes_before, bytes_after);

        // Re-materializing is idempotent.
        store.ensure_default_thread("legacy-agent").await.unwrap();
        let listed = store.list_for_agent("legacy-agent").await.unwrap();
        assert_eq!(
            listed
                .iter()
                .filter(|t| t.kind == ThreadKind::Default)
                .count(),
            1,
            "a second ensure must not create a duplicate default row"
        );
    }

    #[tokio::test]
    async fn create_fresh_thread_round_trip() {
        let (_tmp, _data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", Some("Spike".into()));
        let created = store.create(row.clone()).await.unwrap();

        assert_eq!(created.kind, ThreadKind::Fresh);
        assert_eq!(created.title.as_deref(), Some("Spike"));
        assert!(created.history_floor_ts.is_none());
        assert!(created.branch_source.is_none());

        let listed = store.list_for_agent("agent-1").await.unwrap();
        // default + the fresh row
        assert_eq!(listed.len(), 2);
        // default thread sorts first
        assert_eq!(listed[0].kind, ThreadKind::Default);
        assert_eq!(listed[1].id, created.id);
    }

    #[tokio::test]
    async fn create_branch_thread_mirrors_floor_from_source() {
        let (_tmp, _data_root, store) = ready_store().await;

        // Seed the default thread for our agent so the branch refers to it.
        let default = store.ensure_default_thread("agent-1").await.unwrap();

        let anchor = Utc::now() - Duration::seconds(120);
        let bs = BranchSource {
            source_thread_id: default.id.clone(),
            branch_at: anchor,
            source_message_id: Some(anchor.to_rfc3339()),
        };
        let row = store.build_branch_thread(
            "agent-1",
            Some("What-if".into()),
            bs.clone(),
        );
        let created = store.create(row).await.unwrap();

        assert_eq!(created.kind, ThreadKind::Branch);
        assert_eq!(created.history_floor_ts, Some(anchor));
        let inner = created.branch_source.as_ref().expect("branch source set");
        assert_eq!(inner.source_thread_id, default.id);
        assert_eq!(inner.branch_at, anchor);
    }

    #[tokio::test]
    async fn rename_updates_title_and_timestamp() {
        let (_tmp, _data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", None);
        let created = store.create(row).await.unwrap();

        // Brief sleep so updated_at strictly advances.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        let renamed = store
            .rename(&created.id, Some("Renamed".into()))
            .await
            .unwrap();
        assert_eq!(renamed.title.as_deref(), Some("Renamed"));
        assert!(renamed.updated_at >= created.updated_at);
    }

    #[tokio::test]
    async fn archive_sets_archived_at_and_unarchive_clears_it() {
        let (_tmp, _data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", Some("Spike".into()));
        let created = store.create(row).await.unwrap();
        assert!(created.archived_at.is_none());

        let archived = store.archive(&created.id).await.unwrap();
        assert!(archived.archived_at.is_some());
        assert!(archived.updated_at >= created.updated_at);

        // Persists across reload.
        let fetched = store.get(&created.id).await.unwrap().unwrap();
        assert!(fetched.archived_at.is_some());

        let unarchived = store.unarchive(&created.id).await.unwrap();
        assert!(unarchived.archived_at.is_none());
        let fetched_again = store.get(&created.id).await.unwrap().unwrap();
        assert!(fetched_again.archived_at.is_none());
    }

    /// Recording stub for [`ReflectionTriggerSubscriber`] — captures every
    /// trigger it receives so tests can assert on reason/agent_id/count.
    struct RecordingReflectionSubscriber {
        seen: std::sync::Mutex<Vec<ReflectionTrigger>>,
    }

    impl RecordingReflectionSubscriber {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn snapshot(&self) -> Vec<ReflectionTrigger> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl ReflectionTriggerSubscriber for RecordingReflectionSubscriber {
        fn on_reflection_trigger(&self, trigger: ReflectionTrigger) {
            self.seen.lock().unwrap().push(trigger);
        }
    }

    #[tokio::test]
    async fn archive_fires_reflection_trigger_exactly_once() {
        let (_tmp, _data_root, store) = ready_store().await;
        let subscriber = RecordingReflectionSubscriber::new();
        store.set_reflection_subscriber(subscriber.clone() as Arc<dyn ReflectionTriggerSubscriber>);

        let row = store.build_fresh_thread("agent-1", Some("Spike".into()));
        let created = store.create(row).await.unwrap();
        assert!(subscriber.snapshot().is_empty(), "create must not fire a trigger");

        let archived = store.archive(&created.id).await.unwrap();
        let seen = subscriber.snapshot();
        assert_eq!(seen.len(), 1, "archiving must fire exactly one trigger");
        assert_eq!(seen[0].reason, ReflectionTriggerReason::Archived);
        assert_eq!(seen[0].agent_id, "agent-1");
        assert_eq!(seen[0].transcript_path, archived.transcript_path);

        // Re-archiving an already-archived thread is a no-op and must not
        // refire — the untrimmed history hasn't changed since the last fire.
        store.archive(&created.id).await.unwrap();
        assert_eq!(
            subscriber.snapshot().len(),
            1,
            "redundant archive call must not refire the trigger"
        );
    }

    #[tokio::test]
    async fn archive_refuses_default_thread() {
        let (_tmp, _data_root, store) = ready_store().await;
        let default = store.ensure_default_thread("agent-1").await.unwrap();

        let err = store.archive(&default.id).await.unwrap_err();
        assert!(matches!(err, AoError::ValidationError(_)));

        let fetched = store.get(&default.id).await.unwrap().unwrap();
        assert!(fetched.archived_at.is_none());
    }

    #[tokio::test]
    async fn archive_unknown_id_errors() {
        let (_tmp, _data_root, store) = ready_store().await;
        let err = store.archive("does-not-exist").await.unwrap_err();
        assert!(matches!(err, AoError::ThreadNotFound(_)));

        let err = store.unarchive("does-not-exist").await.unwrap_err();
        assert!(matches!(err, AoError::ThreadNotFound(_)));
    }

    #[tokio::test]
    async fn set_auto_title_if_unset_sets_on_first_call() {
        let (_tmp, _data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", None);
        let created = store.create(row).await.unwrap();

        let updated = store
            .set_auto_title_if_unset(&created.id, "Derived title".into())
            .await
            .unwrap()
            .expect("first call should set auto_title");
        assert_eq!(updated.auto_title.as_deref(), Some("Derived title"));
        assert!(updated.title.is_none());
    }

    #[tokio::test]
    async fn set_auto_title_if_unset_noops_when_auto_title_already_set() {
        let (_tmp, _data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", None);
        let created = store.create(row).await.unwrap();

        store
            .set_auto_title_if_unset(&created.id, "First".into())
            .await
            .unwrap();
        let second = store
            .set_auto_title_if_unset(&created.id, "Second".into())
            .await
            .unwrap();
        assert!(second.is_none(), "second call must no-op");

        let fetched = store.get(&created.id).await.unwrap().unwrap();
        assert_eq!(fetched.auto_title.as_deref(), Some("First"));
    }

    #[tokio::test]
    async fn set_auto_title_if_unset_noops_when_explicitly_titled() {
        let (_tmp, _data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", Some("Explicit".into()));
        let created = store.create(row).await.unwrap();

        let result = store
            .set_auto_title_if_unset(&created.id, "Derived".into())
            .await
            .unwrap();
        assert!(result.is_none(), "must not override an explicit title");

        let fetched = store.get(&created.id).await.unwrap().unwrap();
        assert!(fetched.auto_title.is_none());
        assert_eq!(fetched.title.as_deref(), Some("Explicit"));
    }

    #[tokio::test]
    async fn set_auto_title_if_unset_unknown_id_errors() {
        let (_tmp, _data_root, store) = ready_store().await;
        let err = store
            .set_auto_title_if_unset("does-not-exist", "X".into())
            .await
            .unwrap_err();
        assert!(matches!(err, AoError::ThreadNotFound(_)));
    }

    #[tokio::test]
    async fn delete_refuses_default_thread() {
        let (_tmp, _data_root, store) = ready_store().await;
        let default = store.ensure_default_thread("agent-1").await.unwrap();

        let err = store.delete(&default.id).await.unwrap_err();
        assert!(matches!(err, AoError::ValidationError(_)));

        // Default row is still present.
        let listed = store.list_for_agent("agent-1").await.unwrap();
        assert!(listed.iter().any(|t| t.id == default.id));
    }

    #[tokio::test]
    async fn delete_fresh_thread_is_metadata_only() {
        let (_tmp, data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", Some("Drop me".into()));
        let created = store.create(row).await.unwrap();

        // Seed a transcript at the thread path so we can prove we don't touch it.
        let path = data_root.thread_transcript_path(&created.id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&path, b"on-disk content\n").await.unwrap();

        let removed = store.delete(&created.id).await.unwrap();
        assert!(removed);

        // The transcript file still exists with its bytes intact — delete is
        // metadata-only.
        let bytes = tokio::fs::read(&path).await.unwrap();
        assert_eq!(bytes, b"on-disk content\n");

        // Deleting a missing thread reports false, no error.
        let again = store.delete(&created.id).await.unwrap();
        assert!(!again);
    }

    #[tokio::test]
    async fn back_compat_resolution_via_default_thread_id() {
        // Any caller that has only an agent_id can resolve the default thread
        // id without touching the store.
        let id = ThreadStore::default_thread_id("agent-xyz");
        assert_eq!(id, "default-agent-xyz");
    }

    #[tokio::test]
    async fn list_does_not_leak_other_agents_threads() {
        let (_tmp, _data_root, store) = ready_store().await;
        let _ = store.ensure_default_thread("agent-a").await.unwrap();
        let _ = store.ensure_default_thread("agent-b").await.unwrap();

        let row = store.build_fresh_thread("agent-a", Some("Only A".into()));
        store.create(row).await.unwrap();

        let a = store.list_for_agent("agent-a").await.unwrap();
        let b = store.list_for_agent("agent-b").await.unwrap();

        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 1);
        assert!(b.iter().all(|t| t.kind == ThreadKind::Default));
    }

    #[tokio::test]
    async fn resolve_or_default_handles_none_and_explicit_default() {
        let (_tmp, _data_root, store) = ready_store().await;
        let by_none = store.resolve_or_default("agent-a", None).await.unwrap();
        let by_default = store
            .resolve_or_default("agent-a", Some(&ThreadStore::default_thread_id("agent-a")))
            .await
            .unwrap();
        let by_empty = store
            .resolve_or_default("agent-a", Some(""))
            .await
            .unwrap();
        assert_eq!(by_none.id, by_default.id);
        assert_eq!(by_none.id, by_empty.id);
        assert_eq!(by_none.kind, ThreadKind::Default);
    }

    #[tokio::test]
    async fn resolve_or_default_returns_explicit_custom_thread() {
        let (_tmp, _data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-a", Some("Side".into()));
        let created = store.create(row).await.unwrap();
        let resolved = store
            .resolve_or_default("agent-a", Some(&created.id))
            .await
            .unwrap();
        assert_eq!(resolved.id, created.id);
        assert_eq!(resolved.kind, ThreadKind::Fresh);
    }

    #[tokio::test]
    async fn resolve_or_default_unknown_id_errors() {
        let (_tmp, _data_root, store) = ready_store().await;
        let err = store
            .resolve_or_default("agent-a", Some("does-not-exist"))
            .await
            .unwrap_err();
        match err {
            AoError::ThreadNotFound(id) => assert_eq!(id, "does-not-exist"),
            other => panic!("expected ThreadNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_rejects_duplicate_id() {
        let (_tmp, _data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", None);
        store.create(row.clone()).await.unwrap();
        let err = store.create(row).await.unwrap_err();
        assert!(matches!(err, AoError::ValidationError(_)));
    }

    #[tokio::test]
    async fn new_thread_rows_start_with_no_distillation_watermark() {
        let (_tmp, _data_root, store) = ready_store().await;

        let default = store.ensure_default_thread("agent-1").await.unwrap();
        assert!(default.distilled_through_ts.is_none());

        let fresh_row = store.build_fresh_thread("agent-1", None);
        assert!(fresh_row.distilled_through_ts.is_none());

        let bs = BranchSource {
            source_thread_id: default.id.clone(),
            branch_at: Utc::now(),
            source_message_id: None,
        };
        let branch_row = store.build_branch_thread("agent-1", None, bs);
        // A Branch's watermark starts unset even though its history_floor_ts
        // is inherited from the source's fork point — see the field doc on
        // `Thread::distilled_through_ts`: a Branch only ever distills its own
        // post-fork delta, so there is nothing to inherit here.
        assert!(branch_row.history_floor_ts.is_some());
        assert!(branch_row.distilled_through_ts.is_none());
    }

    /// A thread row persisted before `distilled_through_ts` existed at all
    /// (no `history_floor_ts`/`branch_source` either, matching the oldest
    /// pre-thread shape) must load with the new field defaulted to `None`
    /// via `#[serde(default)]`, exactly like `history_floor_ts` already does.
    #[tokio::test]
    async fn distilled_through_ts_defaults_on_legacy_row() {
        let (_tmp, data_root) = setup();
        data_root.ensure_directories().await.unwrap();

        let legacy_json = r#"[{
            "id": "legacy-thread-1",
            "title": "Legacy",
            "scope": { "type": "AgentChat", "agent_id": "agent-1" },
            "transcript_path": "/tmp/agent-1.jsonl",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }]"#;
        tokio::fs::write(data_root.threads_path(), legacy_json)
            .await
            .unwrap();

        let store = ThreadStore::load(data_root).await.unwrap();
        let loaded = store
            .get("legacy-thread-1")
            .await
            .unwrap()
            .expect("legacy row should load");

        assert_eq!(loaded.kind, ThreadKind::Default);
        assert!(loaded.history_floor_ts.is_none());
        assert!(loaded.distilled_through_ts.is_none());
    }

    #[tokio::test]
    async fn find_by_transcript_path_resolves_fresh_and_default_threads() {
        let (_tmp, _data_root, store) = ready_store().await;

        let default = store.ensure_default_thread("agent-1").await.unwrap();
        let found_default = store
            .find_by_transcript_path("agent-1", &default.transcript_path)
            .await
            .unwrap()
            .expect("default thread should resolve by its own transcript path");
        assert_eq!(found_default.id, default.id);

        let fresh_row = store.build_fresh_thread("agent-1", Some("Spike".into()));
        let created = store.create(fresh_row).await.unwrap();
        let found_fresh = store
            .find_by_transcript_path("agent-1", &created.transcript_path)
            .await
            .unwrap()
            .expect("fresh thread should resolve by its own transcript path");
        assert_eq!(found_fresh.id, created.id);
    }

    #[tokio::test]
    async fn find_by_transcript_path_resolves_even_before_any_prior_access_for_agent() {
        // No `ensure_default_thread`/`list_for_agent` call for this agent yet
        // — the lookup itself must materialize the default row rather than
        // spuriously reporting "not found" for a brand-new agent's first
        // reflection trigger.
        let (_tmp, data_root) = setup();
        data_root.ensure_directories().await.unwrap();
        let store = ThreadStore::load(data_root.clone()).await.unwrap();

        let expected_path = data_root
            .agent_transcript_path("agent-never-seen")
            .to_string_lossy()
            .into_owned();
        let found = store
            .find_by_transcript_path("agent-never-seen", &expected_path)
            .await
            .unwrap();
        assert!(found.is_some());
    }

    #[tokio::test]
    async fn find_by_transcript_path_unknown_path_returns_none() {
        let (_tmp, _data_root, store) = ready_store().await;
        store.ensure_default_thread("agent-1").await.unwrap();
        let found = store
            .find_by_transcript_path("agent-1", "/nowhere/unknown.jsonl")
            .await
            .unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn advance_distillation_watermark_sets_and_persists() {
        let (_tmp, data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", None);
        let created = store.create(row).await.unwrap();
        assert!(created.distilled_through_ts.is_none());

        let ts = Utc::now();
        let updated = store
            .advance_distillation_watermark(&created.id, ts)
            .await
            .unwrap();
        assert_eq!(updated.distilled_through_ts, Some(ts));

        // Persisted, not just in-memory.
        let reloaded = ThreadStore::load(data_root).await.unwrap();
        let reloaded_thread = reloaded.get(&created.id).await.unwrap().unwrap();
        assert_eq!(reloaded_thread.distilled_through_ts, Some(ts));
    }

    #[tokio::test]
    async fn advance_distillation_watermark_never_moves_backward() {
        let (_tmp, _data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", None);
        let created = store.create(row).await.unwrap();

        let later = Utc::now();
        let earlier = later - Duration::seconds(60);

        store
            .advance_distillation_watermark(&created.id, later)
            .await
            .unwrap();
        let after_earlier_call = store
            .advance_distillation_watermark(&created.id, earlier)
            .await
            .unwrap();
        assert_eq!(
            after_earlier_call.distilled_through_ts,
            Some(later),
            "a stale/backward timestamp must never regress the watermark"
        );
    }

    #[tokio::test]
    async fn advance_distillation_watermark_unknown_id_errors() {
        let (_tmp, _data_root, store) = ready_store().await;
        let err = store
            .advance_distillation_watermark("no-such-thread", Utc::now())
            .await
            .unwrap_err();
        assert!(matches!(err, AoError::ThreadNotFound(_)));
    }

    #[tokio::test]
    async fn advance_promotion_sweep_watermark_sets_and_persists() {
        let (_tmp, data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", None);
        let created = store.create(row).await.unwrap();
        assert!(created.promotion_swept_at.is_none());

        let ts = Utc::now();
        let updated = store
            .advance_promotion_sweep_watermark(&created.id, ts)
            .await
            .unwrap();
        assert_eq!(updated.promotion_swept_at, Some(ts));

        // Persisted, not just in-memory.
        let reloaded = ThreadStore::load(data_root).await.unwrap();
        let reloaded_thread = reloaded.get(&created.id).await.unwrap().unwrap();
        assert_eq!(reloaded_thread.promotion_swept_at, Some(ts));
    }

    #[tokio::test]
    async fn advance_promotion_sweep_watermark_never_moves_backward() {
        let (_tmp, _data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", None);
        let created = store.create(row).await.unwrap();

        let later = Utc::now();
        let earlier = later - Duration::seconds(60);

        store
            .advance_promotion_sweep_watermark(&created.id, later)
            .await
            .unwrap();
        let after_earlier_call = store
            .advance_promotion_sweep_watermark(&created.id, earlier)
            .await
            .unwrap();
        assert_eq!(
            after_earlier_call.promotion_swept_at,
            Some(later),
            "a stale/backward timestamp must never regress the watermark"
        );
    }

    #[tokio::test]
    async fn advance_promotion_sweep_watermark_unknown_id_errors() {
        let (_tmp, _data_root, store) = ready_store().await;
        let err = store
            .advance_promotion_sweep_watermark("no-such-thread", Utc::now())
            .await
            .unwrap_err();
        assert!(matches!(err, AoError::ThreadNotFound(_)));
    }

    fn slack_origin(binding_id: &str) -> ChannelBridgeOrigin {
        ChannelBridgeOrigin {
            kind: ao_protocol::agent::ChannelKind::Slack,
            binding_id: binding_id.to_string(),
        }
    }

    #[tokio::test]
    async fn backfill_channel_origin_stamps_and_persists_when_unset() {
        let (_tmp, data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", None);
        let created = store.create(row).await.unwrap();
        assert!(created.channel_origin.is_none());

        store
            .backfill_channel_origin(&created.id, slack_origin("slack-binding"))
            .await
            .unwrap();

        let updated = store.get(&created.id).await.unwrap().unwrap();
        assert_eq!(updated.channel_origin, Some(slack_origin("slack-binding")));

        // Persisted, not just in-memory.
        let reloaded = ThreadStore::load(data_root).await.unwrap();
        let reloaded_thread = reloaded.get(&created.id).await.unwrap().unwrap();
        assert_eq!(reloaded_thread.channel_origin, Some(slack_origin("slack-binding")));
    }

    #[tokio::test]
    async fn backfill_channel_origin_never_overwrites_an_existing_origin() {
        let (_tmp, _data_root, store) = ready_store().await;
        let mut row = store.build_fresh_thread("agent-1", None);
        row.channel_origin = Some(slack_origin("original-binding"));
        let created = store.create(row).await.unwrap();

        store
            .backfill_channel_origin(&created.id, slack_origin("some-other-binding"))
            .await
            .unwrap();

        let updated = store.get(&created.id).await.unwrap().unwrap();
        assert_eq!(updated.channel_origin, Some(slack_origin("original-binding")));
    }

    #[tokio::test]
    async fn backfill_channel_origin_is_a_noop_for_a_missing_thread() {
        let (_tmp, _data_root, store) = ready_store().await;
        // Must not error — a row whose thread was deleted since is simply
        // skipped, not a startup failure.
        store
            .backfill_channel_origin("no-such-thread", slack_origin("slack-binding"))
            .await
            .unwrap();
    }

    fn assignment_origin(assignment_id: &str, run_id: Option<&str>) -> AssignmentBridgeOrigin {
        AssignmentBridgeOrigin {
            assignment_id: assignment_id.to_string(),
            run_id: run_id.map(|s| s.to_string()),
        }
    }

    #[tokio::test]
    async fn backfill_assignment_origin_stamps_and_persists_when_unset() {
        let (_tmp, data_root, store) = ready_store().await;
        let row = store.build_fresh_thread("agent-1", None);
        let created = store.create(row).await.unwrap();
        assert!(created.assignment_origin.is_none());

        store
            .backfill_assignment_origin(&created.id, assignment_origin("assign-1", Some("run-1")))
            .await
            .unwrap();

        let updated = store.get(&created.id).await.unwrap().unwrap();
        assert_eq!(
            updated.assignment_origin,
            Some(assignment_origin("assign-1", Some("run-1")))
        );

        // Persisted, not just in-memory.
        let reloaded = ThreadStore::load(data_root).await.unwrap();
        let reloaded_thread = reloaded.get(&created.id).await.unwrap().unwrap();
        assert_eq!(
            reloaded_thread.assignment_origin,
            Some(assignment_origin("assign-1", Some("run-1")))
        );
    }

    #[tokio::test]
    async fn backfill_assignment_origin_never_overwrites_an_existing_origin() {
        let (_tmp, _data_root, store) = ready_store().await;
        let mut row = store.build_fresh_thread("agent-1", None);
        row.assignment_origin = Some(assignment_origin("original-assignment", None));
        let created = store.create(row).await.unwrap();

        store
            .backfill_assignment_origin(&created.id, assignment_origin("some-other-assignment", Some("run-x")))
            .await
            .unwrap();

        let updated = store.get(&created.id).await.unwrap().unwrap();
        assert_eq!(
            updated.assignment_origin,
            Some(assignment_origin("original-assignment", None))
        );
    }

    #[tokio::test]
    async fn backfill_assignment_origin_is_a_noop_for_a_missing_thread() {
        let (_tmp, _data_root, store) = ready_store().await;
        // Must not error — a row whose thread was deleted since is simply
        // skipped, not a startup failure.
        store
            .backfill_assignment_origin("no-such-thread", assignment_origin("assign-1", None))
            .await
            .unwrap();
    }

    /// Anti-drift test for `list_all_grouped`: it must return, for every
    /// agent id, exactly what `list_for_agent` would return for that same
    /// id — same rows, same order — with no reimplementation of the
    /// ownership predicate or sort to drift out of sync. Covers all four
    /// `ThreadScope` variants (`AgentChat`, `TeamChat`, `Delegation`,
    /// `Artifact`) so a predicate change that starts leaking a non-agent
    /// scope into the grouped map — or stops matching a real one — fails
    /// this test.
    #[tokio::test]
    async fn list_all_grouped_matches_list_for_agent_for_every_scope_variant() {
        let (_tmp, _data_root, store) = ready_store().await;

        // AgentChat — two agents, each with a lazily-materialized default
        // plus one operator-created fresh thread, so both the multi-key and
        // the multi-row-per-key/sort-order cases are exercised.
        store.ensure_default_thread("agent-1").await.unwrap();
        let fresh_a1 = store.build_fresh_thread("agent-1", Some("Fresh A1".to_string()));
        store.create(fresh_a1).await.unwrap();

        store.ensure_default_thread("agent-2").await.unwrap();
        let fresh_a2 = store.build_fresh_thread("agent-2", None);
        store.create(fresh_a2).await.unwrap();

        // TeamChat and Delegation — no dedicated builder exists for these
        // scopes, so start from a `Fresh` AgentChat row (for valid
        // transcript_path/id plumbing) and overwrite `scope`. Neither must
        // ever surface in any agent's bucket.
        let mut team_row = store.build_fresh_thread("agent-1", None);
        team_row.scope = ThreadScope::TeamChat {
            team_id: "team-1".to_string(),
        };
        store.create(team_row).await.unwrap();

        let mut delegation_row = store.build_fresh_thread("agent-2", None);
        delegation_row.scope = ThreadScope::Delegation {
            team_id: "team-1".to_string(),
            delegation_id: "delegation-1".to_string(),
        };
        store.create(delegation_row).await.unwrap();

        // Artifact — has its own lazy builder, mirroring
        // `ensure_default_thread`'s shape. Also must never surface.
        let artifact = store.ensure_artifact_thread("artifact-1").await.unwrap();
        assert!(matches!(artifact.scope, ThreadScope::Artifact { .. }));

        // Reference results from the existing, independently-implemented
        // path this test guards against drifting away from. Both agents'
        // default threads are already materialized above, so these calls
        // are read-only in effect (ensure_default_thread is idempotent).
        let expected_agent_1 = store.list_for_agent("agent-1").await.unwrap();
        let expected_agent_2 = store.list_for_agent("agent-2").await.unwrap();
        assert_eq!(expected_agent_1.len(), 2, "agent-1: default + fresh");
        assert_eq!(expected_agent_2.len(), 2, "agent-2: default + fresh");

        let grouped = store.list_all_grouped().await.unwrap();

        // No non-AgentChat scope, and no unexpected agent id, leaked into
        // the grouped map.
        let mut keys: Vec<&String> = grouped.keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["agent-1", "agent-2"]);

        // Byte-for-byte equivalence (via JSON serialization, since `Thread`
        // has no `PartialEq`) between each bucket and calling
        // `list_for_agent` directly — same rows, same order.
        assert_eq!(
            serde_json::to_string(&grouped["agent-1"]).unwrap(),
            serde_json::to_string(&expected_agent_1).unwrap(),
        );
        assert_eq!(
            serde_json::to_string(&grouped["agent-2"]).unwrap(),
            serde_json::to_string(&expected_agent_2).unwrap(),
        );
    }

    /// Pins the no-lazy-write contract documented on
    /// [`ThreadStore::list_all_grouped`]: unlike `list_for_agent`, it must
    /// never call `ensure_default_thread` on the caller's behalf. Asserts
    /// both halves — (a) an agent with no persisted rows is simply absent
    /// from the returned map, and (b) critically, that absence is not just a
    /// return-value artifact: the on-disk file is byte-for-byte unchanged by
    /// the call. The final block contrasts this with `list_for_agent`, which
    /// *does* grow the store for the same agent id, so the test would fail
    /// if `list_all_grouped` regressed to eagerly materializing defaults
    /// (see the neutering check in the task write-up).
    #[tokio::test]
    async fn list_all_grouped_does_not_lazily_create_default_threads() {
        let (_tmp, data_root, store) = ready_store().await;

        // Seed the store with one real row for a different agent so
        // threads.json exists on disk before we start comparing snapshots.
        let seeded = store.build_fresh_thread("agent-with-threads", None);
        store.create(seeded).await.unwrap();

        let threads_path = data_root.threads_path();
        let before_bytes = tokio::fs::read(&threads_path).await.unwrap();
        let before_len = store.threads.read().await.len();
        assert_eq!(before_len, 1);

        // The agent under test has never been touched — no default row, no
        // other row.
        let grouped = store.list_all_grouped().await.unwrap();

        // (a) Absent (or empty) for the untouched agent; unaffected for the
        // seeded one.
        assert!(
            grouped
                .get("empty-agent")
                .map(|v| v.is_empty())
                .unwrap_or(true),
            "list_all_grouped must not materialize a default row for an agent with no threads"
        );
        assert_eq!(grouped["agent-with-threads"].len(), 1);

        // (b) No write occurred: file bytes and in-memory row count are
        // unchanged after the call.
        let after_bytes = tokio::fs::read(&threads_path).await.unwrap();
        assert_eq!(
            before_bytes, after_bytes,
            "list_all_grouped must not touch threads.json"
        );
        let after_len = store.threads.read().await.len();
        assert_eq!(
            before_len, after_len,
            "list_all_grouped must not push a new row into the in-memory store"
        );

        // Contrast: list_for_agent for that same empty agent DOES grow the
        // store (this is the existing, intentional lazy-write behaviour) —
        // proving the assertions above are actually exercising something,
        // not just describing a store that never writes at all.
        let listed = store.list_for_agent("empty-agent").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, ThreadKind::Default);

        let grown_len = store.threads.read().await.len();
        assert_eq!(grown_len, before_len + 1);
        let grown_bytes = tokio::fs::read(&threads_path).await.unwrap();
        assert_ne!(
            before_bytes, grown_bytes,
            "list_for_agent should persist the newly-materialized default row"
        );
    }
}
