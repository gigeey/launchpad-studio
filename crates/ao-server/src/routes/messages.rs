use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use ao_engine::AppState;
use ao_protocol::error::AoError;
use ao_protocol::message::{MessageAck, QueuedMessage};
use ao_protocol::thread::{Thread, ThreadKind, ThreadScope};
use ao_protocol::transcript::{
    CursorPhase, PaginatedResponse, PaginationCursor, TranscriptEntry, TranscriptRole,
};

use crate::error::AppError;
use crate::routes::form_answers;

#[derive(Debug, serde::Deserialize)]
pub struct GetMessagesQuery {
    pub last: Option<usize>,
    pub before: Option<String>,
    pub after: Option<String>,
    pub cursor_offset: Option<u64>,
    pub cursor_message_id: Option<String>,
    pub cursor_timestamp: Option<String>,
    /// Which file `cursor_offset` addresses — round-tripped verbatim from
    /// the `phase` field of the `PaginationCursor` a prior response handed
    /// back. Absent (or omitted by older clients) defaults to `Own`. See
    /// [`CursorPhase`].
    #[serde(default)]
    pub cursor_phase: Option<CursorPhase>,
    /// Optional thread filter. When unset, the route reads the agent's
    /// pre-thread (default) transcript path — byte-equivalent to the
    /// behavior before threads existed. When set to a non-default thread, the read is
    /// routed to that thread's `transcript_path`.
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// Paginated response containing messages and an optional cursor for fetching more.
#[derive(Debug, serde::Serialize)]
pub struct PaginatedMessagesResponse {
    pub messages: Vec<TranscriptEntry>,
    pub cursor: Option<PaginationCursor>,
}

#[derive(serde::Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
    #[serde(default)]
    pub attachment_ids: Option<Vec<String>>,
    #[serde(default)]
    pub focus_path: Option<String>,
    /// Optional thread to enqueue this message against. `None` (and the
    /// agent's `Default` thread) preserves the agent-keyed write path and
    /// queue routing exactly as it was before threads landed.
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// Resolve an optional thread id from a chat request into either `None`
/// (default-thread / pre-thread behavior) or a validated non-default thread
/// row scoped to the requested agent. Centralizes the 404 / scope-mismatch
/// checks both `send_message` and `get_messages` perform.
async fn resolve_non_default_thread(
    state: &Arc<AppState>,
    agent_id: &str,
    thread_id: Option<&str>,
) -> Result<Option<Thread>, AppError> {
    let Some(tid) = thread_id else {
        return Ok(None);
    };
    if tid.is_empty() {
        return Ok(None);
    }

    let thread = state
        .persistence
        .threads
        .get(tid)
        .await?
        .ok_or_else(|| AoError::ThreadNotFound(tid.to_string()))?;

    match &thread.scope {
        ThreadScope::AgentChat { agent_id: scope_agent } if scope_agent == agent_id => {}
        _ => {
            return Err(AppError(AoError::ValidationError(format!(
                "Thread {} is not scoped to agent {}",
                tid, agent_id
            ))));
        }
    }

    if thread.kind == ThreadKind::Default {
        // Default rows alias the agent-keyed transcript path, so callers
        // collapse to the pre-thread path for byte-exact back-compat.
        return Ok(None);
    }

    Ok(Some(thread))
}

/// For a branch thread, read up to `need` more entries from the SOURCE
/// thread's inheritable prefix (`ts <= history_floor_ts`), tagged with an
/// `Inherited`-phase cursor so a follow-up "load older" call keeps walking
/// backward through pre-fork history instead of dead-ending at the fork
/// point. Returns `Ok(None)` for non-branch threads (or once `need` is
/// already satisfied) — callers only invoke this when their own-file read
/// came up short, so the branch/floor/source lookups aren't paid on the
/// common (non-branch) path.
async fn merge_inherited_tail(
    state: &Arc<AppState>,
    thread: Option<&Thread>,
    need: usize,
) -> Result<Option<PaginatedResponse<TranscriptEntry>>, AppError> {
    if need == 0 {
        return Ok(None);
    }
    let Some(t) = thread else {
        return Ok(None);
    };
    let Some(bs) = t.branch_source.as_ref() else {
        return Ok(None);
    };
    let Some(floor) = t.history_floor_ts else {
        return Ok(None);
    };
    let Some(src) = state.persistence.threads.get(&bs.source_thread_id).await? else {
        return Ok(None);
    };
    let src_path = PathBuf::from(&src.transcript_path);
    let result = state
        .persistence
        .transcripts
        .read_tail_before_floor_at(&src_path, floor, need)
        .await?;
    Ok(Some(result))
}

/// POST /agents/{agent_id}/messages — enqueue a message for an agent.
pub async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<MessageAck>, AppError> {
    // Validate agent exists
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    // Create message_id
    let message_id = Uuid::new_v4().to_string();

    // Resolve attachment IDs to full Attachment objects
    let attachment_ids = req.attachment_ids.unwrap_or_default();
    let mut attachments = Vec::with_capacity(attachment_ids.len());
    for aid in &attachment_ids {
        let attachment = state
            .persistence
            .assets
            .get_attachment(&agent_id, aid)
            .await
            .map_err(|_| AoError::ValidationError(format!("Attachment not found: {}", aid)))?;
        attachments.push(attachment);
    }

    // Mark each attachment as committed to this message
    for aid in &attachment_ids {
        state
            .persistence
            .assets
            .mark_committed(&agent_id, aid, &message_id)
            .await?;
    }

    // Persist user message to transcript with message_id and attachments in metadata
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
        "message_id".to_string(),
        serde_json::Value::String(message_id.clone()),
    );

    if !attachments.is_empty() {
        let attachments_json: Vec<serde_json::Value> = attachments
            .iter()
            .map(|a| {
                serde_json::json!({
                    "id": a.id,
                    "file_path": a.file_path,
                    "mime_type": a.mime_type,
                    "original_filename": a.original_filename,
                    "attachment_type": a.attachment_type,
                })
            })
            .collect();
        metadata.insert(
            "attachments".to_string(),
            serde_json::Value::Array(attachments_json),
        );
    }

    let user_entry = TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("user".to_string()),
        content: req.content.clone(),
        event_type: "message".to_string(),
        metadata: Some(metadata),
        hidden_from_user: false,
    };

    // Resolve thread routing. A non-default thread sends the user entry to
    // that thread's own transcript file; absence / default keeps the
    // pre-thread agent-keyed append untouched.
    let target_thread =
        resolve_non_default_thread(&state, &agent_id, req.thread_id.as_deref()).await?;
    match &target_thread {
        Some(thread) => {
            let path = PathBuf::from(&thread.transcript_path);
            state
                .persistence
                .transcripts
                .append_at(&path, &user_entry)
                .await?;
        }
        None => {
            state
                .persistence
                .transcripts
                .append(&agent_id, &user_entry)
                .await?;
        }
    }

    // Auto-title: the first time a non-default thread receives a user
    // message while both `title` and `auto_title` are still unset, derive a
    // short label from it so the tab strip shows something more useful than
    // the generic placeholder until a human or the `RenameThread` tool names
    // it properly. `title.is_none()` is *not* re-checked by
    // `set_auto_title_if_unset` alone being called — the store method itself
    // re-checks both fields under its write lock right before mutating, so a
    // duplicate/racing send for the same thread can't set this twice, and an
    // explicit rename that lands concurrently always wins.
    if let Some(thread) = &target_thread {
        if thread.title.is_none() && thread.auto_title.is_none() {
            if let Some(auto_title) = ao_protocol::thread::derive_auto_title(&req.content) {
                if let Ok(Some(_)) = state
                    .persistence
                    .threads
                    .set_auto_title_if_unset(&thread.id, auto_title.clone())
                    .await
                {
                    // Live-push so the tab strip picks up the label without
                    // waiting for the next full thread-list refetch.
                    state
                        .event_bus
                        .emit(
                            &message_id,
                            &agent_id,
                            Some(thread.id.clone()),
                            ao_protocol::event::AgentEventPayload::ThreadRenamed {
                                thread_id: thread.id.clone(),
                                title: None,
                                auto_title: Some(auto_title),
                            },
                        )
                        .await;
                }
            }
        }
    }

    // Propagate the originating thread (when non-default) into the queued
    // message so the runner resolves the correct `HistorySource` and
    // transcript write override at compose / persist time.
    let queue_thread_id = target_thread.as_ref().map(|t| t.id.clone());

    // Create queued message with resolved attachments
    let queued_message = QueuedMessage {
        message_id: message_id.clone(),
        content: req.content.clone(),
        queued_at: Utc::now(),
        attachments,
        source: None,
        focus_path: req.focus_path.clone(),
        thread_id: queue_thread_id.clone(),
    };

    // Submit to QueueManager
    state
        .queue_managers
        .submit_message(&agent, queued_message)
        .await?;

    // Update snapshot (message_count, last_activity_at, last_message). The
    // `queue_depth` field is no longer persisted — it is overlaid at read
    // time from `QueueManagerRegistry::queue_depth_for`, which is the only
    // writer that actually knows the live depth.
    //
    // `last_message_thread_id` is set in the same closure as `last_message`
    // so the pair always describes the same event — the sidebar's "jump to
    // the thread with the last message" click relies on that invariant.
    state
        .persistence
        .snapshots
        .update_agent_entry(&agent_id, |entry| {
            entry.message_count += 1;
            entry.last_activity_at = Some(Utc::now());
            entry.last_message = Some(req.content.clone());
            entry.last_message_thread_id = queue_thread_id;
        })
        .await?;

    Ok(Json(MessageAck {
        message_id,
        status: "queued".to_string(),
    }))
}

#[derive(Debug, serde::Deserialize)]
pub struct CancelAgentRunQuery {
    /// Which thread's run to cancel. Omitted (or absent) means the
    /// default/no-thread conversation — NOT "every thread for this agent".
    /// Mirrors `GetMessagesQuery::thread_id`.
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// POST /agents/{agent_id}/cancel — cancel the active run for the given
/// agent on the given thread (or the default thread, if `thread_id` is
/// omitted). Scoped to a single thread on purpose: an agent can have
/// multiple concurrent runs across different threads, and stopping one must
/// not tear down the others.
pub async fn cancel_agent_run(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Query(query): Query<CancelAgentRunQuery>,
) -> Result<StatusCode, AppError> {
    // Cancel via the unified RunningAgents map. This fires the per-runner
    // CancellationToken regardless of whether the active run is on the CLI or
    // native path — no new trait method needed.
    state
        .running_agents
        .cancel(&agent_id, query.thread_id.as_deref());

    // Cancelling the run leaves any ASYNC form it posted on this thread
    // stranded: nothing will ever answer or dismiss it, and an occupied
    // slot rejects every future form on this thread too. Vacate it so the
    // thread doesn't lock out of posting another one.
    // A SYNC form on this same thread is untouched here — it's cleared by
    // `PendingFormClearGuard`'s `Drop` impl, which fires from this same
    // cancellation token independently of this call.
    let transcript_override = state
        .persistence
        .threads
        .resolve_transcript_path_override(query.thread_id.as_deref())
        .await;
    form_answers::vacate_stranded_async_form(
        &state,
        &agent_id,
        query.thread_id.clone(),
        transcript_override,
    )
    .await;

    Ok(StatusCode::OK)
}

/// GET /agents/{agent_id}/messages — read transcript for an agent with optional filters.
pub async fn get_messages(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Query(query): Query<GetMessagesQuery>,
) -> Result<Json<PaginatedMessagesResponse>, AppError> {
    // Validate agent exists
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    // Resolve thread routing. Non-default threads redirect reads to their
    // own transcript path; default / missing falls back to the agent-keyed
    // helpers so existing clients see no behavior change. `thread` is kept
    // (not just its transcript path) so the tail/cursor paths below can
    // check `branch_source` / `history_floor_ts` and merge in pre-fork
    // history from the SOURCE thread when this is a branch.
    let thread = resolve_non_default_thread(&state, &agent_id, query.thread_id.as_deref()).await?;
    let thread_path: Option<PathBuf> = thread.as_ref().map(|t| PathBuf::from(&t.transcript_path));

    // When after/before timestamp filters are used, fall back to read_all + filter
    if query.after.is_some() || query.before.is_some() {
        let mut entries = match &thread_path {
            Some(p) => state.persistence.transcripts.read_all_at(p).await?,
            None => state.persistence.transcripts.read_all(&agent_id).await?,
        };

        if let Some(ref after_str) = query.after {
            if let Ok(after_ts) = after_str.parse::<DateTime<Utc>>() {
                entries.retain(|e| e.ts > after_ts);
            }
        }

        if let Some(ref before_str) = query.before {
            if let Ok(before_ts) = before_str.parse::<DateTime<Utc>>() {
                entries.retain(|e| e.ts < before_ts);
            }
        }

        if let Some(last) = query.last {
            let len = entries.len();
            if last < len {
                entries = entries.split_off(len - last);
            }
        }

        return Ok(Json(PaginatedMessagesResponse {
            messages: entries,
            cursor: None,
        }));
    }

    // Cursor-based pagination: all three cursor params must be present
    if let (Some(offset), Some(ref message_id), Some(ref timestamp_str)) = (
        query.cursor_offset,
        &query.cursor_message_id,
        &query.cursor_timestamp,
    ) {
        let timestamp = timestamp_str
            .parse::<DateTime<Utc>>()
            .map_err(|e| AoError::Json(format!("Invalid cursor_timestamp: {}", e)))?;

        let phase = query.cursor_phase.unwrap_or_default();
        let cursor = PaginationCursor {
            byte_offset: offset,
            last_message_id: message_id.clone(),
            timestamp,
            phase,
        };

        let n = query.last.unwrap_or(50);
        let result = if phase == CursorPhase::Inherited {
            // Already paginating inherited (pre-fork) history: byte_offset
            // addresses the branch's SOURCE thread file, not its own.
            // Walking further "before" an anchor already inside the
            // inheritable prefix only moves strictly further backward in
            // time, so it can never cross back past the fork — no floor
            // re-check needed here (unlike the initial hand-off below).
            let t = thread.as_ref().ok_or_else(|| {
                AppError(AoError::ValidationError(
                    "cursor_phase=inherited requires a branch thread_id".to_string(),
                ))
            })?;
            let bs = t.branch_source.as_ref().ok_or_else(|| {
                AppError(AoError::ValidationError(
                    "cursor_phase=inherited requires a branch thread_id".to_string(),
                ))
            })?;
            let src = state
                .persistence
                .threads
                .get(&bs.source_thread_id)
                .await?
                .ok_or_else(|| AoError::ThreadNotFound(bs.source_thread_id.clone()))?;
            let src_path = PathBuf::from(&src.transcript_path);
            let mut r = state
                .persistence
                .transcripts
                .read_before_cursor_at(&src_path, &cursor, n)
                .await?;
            if let Some(ref mut c) = r.cursor {
                c.phase = CursorPhase::Inherited;
            }
            r
        } else {
            match &thread_path {
                Some(p) => {
                    let mut r = state
                        .persistence
                        .transcripts
                        .read_before_cursor_at(p, &cursor, n)
                        .await?;
                    if r.cursor.is_none() {
                        // Own file's start reached — but that only means
                        // nothing more precedes it in the OWN file, not that
                        // there's no more history: a branch's own file can
                        // legitimately return exactly `n` entries with
                        // nothing further behind them while still having a
                        // full inheritable prefix in the source thread. Peek
                        // for at least one inherited entry (`.max(1)`) so
                        // that case still yields a continuation cursor
                        // instead of a false "no more history" None.
                        if let Some(merged) = merge_inherited_tail(
                            &state,
                            thread.as_ref(),
                            n.saturating_sub(r.entries.len()).max(1),
                        )
                        .await?
                        {
                            let mut combined = merged.entries;
                            combined.extend(r.entries);
                            r.entries = combined;
                            r.cursor = merged.cursor;
                        }
                    }
                    r
                }
                None => state
                    .persistence
                    .transcripts
                    .read_before_cursor(&agent_id, &cursor, n)
                    .await?,
            }
        };

        return Ok(Json(PaginatedMessagesResponse {
            messages: result.entries,
            cursor: result.cursor,
        }));
    }

    // Default: read tail (last N messages)
    let n = query.last.unwrap_or(50);
    let mut result = match &thread_path {
        Some(p) => state.persistence.transcripts.read_tail_at(p, n).await?,
        None => state.persistence.transcripts.read_tail(&agent_id, n).await?,
    };

    // Branch threads start with an empty own file — top up from the
    // source's inheritable prefix so opening a freshly forked thread shows
    // context instead of nothing. Trigger on `cursor.is_none()` rather than
    // `entries.len() < n`: a branch's own file can also return exactly `n`
    // entries with nothing further behind them (own file exhausted right at
    // the page boundary) while a full inheritable prefix still sits behind
    // it in the source thread — `.max(1)` peeks for at least one inherited
    // entry so that boundary case still yields a continuation cursor
    // instead of a false "no more history" None.
    if result.cursor.is_none() {
        if let Some(merged) = merge_inherited_tail(
            &state,
            thread.as_ref(),
            n.saturating_sub(result.entries.len()).max(1),
        )
        .await?
        {
            let mut combined = merged.entries;
            combined.extend(result.entries);
            result.entries = combined;
            result.cursor = merged.cursor;
        }
    }

    Ok(Json(PaginatedMessagesResponse {
        messages: result.entries,
        cursor: result.cursor,
    }))
}

#[cfg(test)]
mod cancel_agent_run_form_vacate_tests {
    use super::*;
    use ao_engine_tools_core::FORM_WITHDRAWN;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::event::AgentEventPayload;

    async fn setup_state() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = PersistenceLayer::init();
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock).await.expect("AppState init")
        };

        // `AppState::new_with_mock` fires several best-effort startup sweeps
        // as detached tasks against `state.persistence.snapshots`
        // (`hydrate_agent_snapshot_fields`, `reap_orphaned_sync_forms`, the
        // team/agent tasklist-sync loops — see `crate::state::AppState::new_with_mock`).
        // On a data root this fresh, the very first `SnapshotStore::save()`
        // call in the process can race one of those and return a transient
        // IO error even though the in-memory mutation already applied —
        // pre-existing to this harness, unrelated to the pending-form logic
        // under test. One throwaway, discarded round trip through the same
        // store settles it before any real assertion runs.
        let _ = state
            .persistence
            .snapshots
            .set_pending_form("warmup", None, "warmup".to_string(), serde_json::json!({}))
            .await;
        let _ = state.persistence.snapshots.clear_pending_form("warmup", "warmup").await;

        (Arc::new(state), tmp)
    }

    fn async_spec(title: &str) -> serde_json::Value {
        serde_json::json!({ "form_id": "f", "spec": { "title": title }, "mode": "async" })
    }

    fn sync_spec(title: &str) -> serde_json::Value {
        serde_json::json!({ "form_id": "f", "spec": { "title": title }, "mode": "sync" })
    }

    fn unwrap_ok<T>(r: Result<T, AppError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {:?}", e.0),
        }
    }

    /// Cancelling a run must not just fire the run's cancellation token — it
    /// must also vacate any ASYNC form the run left stranded on its thread
    /// (nothing else will ever answer or dismiss it) and tell the frontend
    /// via the same `FormResolved` event `async_form_answer` emits, so the
    /// pending-form indicator doesn't sit there forever with no explanation.
    #[tokio::test]
    async fn cancel_agent_run_vacates_async_pending_form_and_emits_withdrawn_event() {
        let (state, _tmp) = setup_state().await;
        let mut events = state.event_bus.subscribe();

        state
            .persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                Some("thread-a".to_string()),
                "form-async".to_string(),
                async_spec("Ship it?"),
            )
            .await
            .unwrap();

        let status = unwrap_ok(
            cancel_agent_run(
                State(Arc::clone(&state)),
                Path("agent-1".to_string()),
                Query(CancelAgentRunQuery {
                    thread_id: Some("thread-a".to_string()),
                }),
            )
            .await,
        );
        assert_eq!(status, StatusCode::OK);

        let snap = state.persistence.snapshots.get().await;
        assert!(
            snap.agents["agent-1"].pending_forms.is_empty(),
            "the stranded async form's slot must be vacated"
        );

        let entries = state.persistence.transcripts.read_all("agent-1").await.unwrap();
        assert!(
            entries.iter().any(|e| e.event_type == FORM_WITHDRAWN),
            "a form_withdrawn trace line must be written: {entries:?}"
        );

        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("must receive an event before timing out")
            .expect("event bus channel must not close");
        assert!(
            matches!(
                event.payload,
                AgentEventPayload::FormResolved { ref form_id } if form_id == "form-async"
            ),
            "must emit FormResolved for the vacated form: {:?}",
            event.payload
        );
    }

    /// Anti-lockout: after cancellation vacates the stranded async form, a
    /// brand new form must be accepted on that same thread — this is the
    /// entire point of the fix (the occupied-slot guard would otherwise
    /// reject it forever).
    #[tokio::test]
    async fn cancel_agent_run_then_new_form_on_same_thread_is_accepted() {
        let (state, _tmp) = setup_state().await;

        state
            .persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                Some("thread-a".to_string()),
                "form-stranded".to_string(),
                async_spec("Are you sure?"),
            )
            .await
            .unwrap();

        unwrap_ok(
            cancel_agent_run(
                State(Arc::clone(&state)),
                Path("agent-1".to_string()),
                Query(CancelAgentRunQuery {
                    thread_id: Some("thread-a".to_string()),
                }),
            )
            .await,
        );

        let replaced = state
            .persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                Some("thread-a".to_string()),
                "form-new".to_string(),
                async_spec("Second question"),
            )
            .await
            .unwrap();
        assert!(replaced.is_none(), "the slot was vacated, so nothing should read as replaced");

        let snap = state.persistence.snapshots.get().await;
        assert_eq!(snap.agents["agent-1"].pending_forms.len(), 1);
        assert_eq!(snap.agents["agent-1"].pending_forms[0].form_id, "form-new");
    }

    /// A SYNC form on the cancelled thread must be left alone: its slot is
    /// cleared exclusively by `PendingFormClearGuard`'s `Drop` impl, which
    /// fires from this same cancellation token independently of this route.
    /// If this route cleared it too, either the drop guard's own clear (a
    /// harmless no-op) or this route's clear would land second and — if this
    /// route ever mis-detected mode — could fire a spurious second
    /// `form_withdrawn`/`FormResolved` for a form the guard already handled.
    #[tokio::test]
    async fn cancel_agent_run_leaves_sync_form_untouched() {
        let (state, _tmp) = setup_state().await;

        state
            .persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                Some("thread-a".to_string()),
                "form-sync".to_string(),
                sync_spec("Confirm deploy"),
            )
            .await
            .unwrap();

        unwrap_ok(
            cancel_agent_run(
                State(Arc::clone(&state)),
                Path("agent-1".to_string()),
                Query(CancelAgentRunQuery {
                    thread_id: Some("thread-a".to_string()),
                }),
            )
            .await,
        );

        let snap = state.persistence.snapshots.get().await;
        assert_eq!(
            snap.agents["agent-1"].pending_forms.len(),
            1,
            "the sync form's slot must still be occupied — only its own Drop guard clears it"
        );
        assert_eq!(snap.agents["agent-1"].pending_forms[0].form_id, "form-sync");

        let entries = state.persistence.transcripts.read_all("agent-1").await.unwrap();
        assert!(
            !entries.iter().any(|e| e.event_type == FORM_WITHDRAWN),
            "must not write a withdrawn trace for a sync form: {entries:?}"
        );
    }

    /// Cancelling thread A's run must never reach into thread B's pending
    /// form — an agent can have multiple concurrent threads, and stopping
    /// one must not disturb the others.
    #[tokio::test]
    async fn cancel_agent_run_does_not_vacate_other_threads_form() {
        let (state, _tmp) = setup_state().await;

        state
            .persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                Some("thread-a".to_string()),
                "form-a".to_string(),
                async_spec("Thread A question"),
            )
            .await
            .unwrap();
        state
            .persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                Some("thread-b".to_string()),
                "form-b".to_string(),
                async_spec("Thread B question"),
            )
            .await
            .unwrap();

        unwrap_ok(
            cancel_agent_run(
                State(Arc::clone(&state)),
                Path("agent-1".to_string()),
                Query(CancelAgentRunQuery {
                    thread_id: Some("thread-a".to_string()),
                }),
            )
            .await,
        );

        let snap = state.persistence.snapshots.get().await;
        let agent = &snap.agents["agent-1"];
        assert_eq!(agent.pending_forms.len(), 1, "thread-b's form must survive untouched");
        assert_eq!(agent.pending_forms[0].form_id, "form-b");
        assert_eq!(agent.pending_forms[0].thread_id.as_deref(), Some("thread-b"));
    }
}
