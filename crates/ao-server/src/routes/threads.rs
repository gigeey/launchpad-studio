use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use ao_engine::AppState;
use ao_protocol::error::AoError;
use ao_protocol::thread::{BranchSource, Thread, ThreadKind, ThreadScope};

use crate::error::AppError;
use crate::routes::form_answers;

#[derive(Debug, Deserialize)]
pub struct CreateThreadRequest {
    #[serde(default)]
    pub title: Option<String>,
    /// "fresh" or "branch". `default` is rejected — default threads are
    /// materialized automatically by the persistence layer and are not
    /// operator-created.
    pub kind: ThreadKind,
    /// Required when `kind == branch`; ignored otherwise.
    #[serde(default)]
    pub branch_source: Option<BranchSource>,
}

#[derive(Debug, Deserialize)]
pub struct PatchThreadRequest {
    pub title: Option<String>,
}

/// `GET /agents/{agent_id}/threads` — list every thread for the agent.
/// Lazily ensures the default thread row exists before returning so callers
/// always see at least one row.
pub async fn list_agent_threads(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<Thread>>, AppError> {
    // Validate agent exists so a typo doesn't silently create a default
    // thread for a non-existent agent.
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let threads = state.persistence.threads.list_for_agent(&agent_id).await?;
    Ok(Json(threads))
}

/// `GET /threads` — every thread across every agent, grouped by owning
/// agent id: `{ "<agent_id>": [Thread, ...], ... }`. Metadata only (no
/// message bodies — `transcript_path` names a file, it doesn't inline its
/// contents), so this stays cheap even across the whole thread set.
///
/// Grouping happens server-side, in
/// [`ao_persistence::ThreadStore::list_all_grouped`], using the exact same
/// ownership predicate and sort as [`list_agent_threads`]/`list_for_agent`
/// — see that method's docs. This is deliberate: `Thread::scope` is a
/// tagged enum, and only `AgentChat` counts as "belongs to this agent" —
/// `TeamChat`, `Delegation`, and `Artifact` threads never appear in any
/// bucket here, matching `list_for_agent` exactly. Returning a flat list
/// and grouping in the frontend would mean reimplementing that predicate in
/// TypeScript, with no way to keep the two from drifting apart.
///
/// No pagination — the full thread set is small (metadata-only rows), and
/// pagination would need to interact with the grouping in ways that aren't
/// worth the complexity here.
pub async fn list_all_threads(
    State(state): State<Arc<AppState>>,
) -> Result<Json<HashMap<String, Vec<Thread>>>, AppError> {
    let grouped = state.persistence.threads.list_all_grouped().await?;
    Ok(Json(grouped))
}

/// `POST /agents/{agent_id}/threads` — create a fresh or branch thread.
pub async fn create_agent_thread(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<CreateThreadRequest>,
) -> Result<Json<Thread>, AppError> {
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let row = match req.kind {
        ThreadKind::Default => {
            return Err(AppError(AoError::ValidationError(
                "Default threads are materialized automatically and cannot be \
                 created via this endpoint"
                    .to_string(),
            )));
        }
        ThreadKind::Fresh => state
            .persistence
            .threads
            .build_fresh_thread(&agent_id, req.title),
        ThreadKind::Branch => {
            let source = req.branch_source.ok_or_else(|| {
                AoError::ValidationError(
                    "branch threads require a branch_source payload".to_string(),
                )
            })?;
            // Validate the named source thread exists so the branch can later
            // be grafted at compose time. Cheap upfront check beats a
            // dangling reference deep in the runtime.
            state
                .persistence
                .threads
                .get(&source.source_thread_id)
                .await?
                .ok_or_else(|| AoError::ThreadNotFound(source.source_thread_id.clone()))?;
            state
                .persistence
                .threads
                .build_branch_thread(&agent_id, req.title, source)
        }
    };

    let created = state.persistence.threads.create(row).await?;
    Ok(Json(created))
}

/// `GET /threads/{thread_id}` — fetch a single thread.
pub async fn get_thread(
    State(state): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
) -> Result<Json<Thread>, AppError> {
    let row = state
        .persistence
        .threads
        .get(&thread_id)
        .await?
        .ok_or_else(|| AoError::ThreadNotFound(thread_id.clone()))?;
    Ok(Json(row))
}

/// `PATCH /threads/{thread_id}` — rename a thread.
pub async fn patch_thread(
    State(state): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
    Json(req): Json<PatchThreadRequest>,
) -> Result<Json<Thread>, AppError> {
    let row = state
        .persistence
        .threads
        .rename(&thread_id, req.title)
        .await?;
    Ok(Json(row))
}

/// `DELETE /threads/{thread_id}` — drop the row. Refuses to delete default
/// threads (400). Never touches the on-disk transcript bytes, but does purge
/// the thread's ephemeral memory file (`memory_threads/{thread_id}.jsonl`,
/// via `MemoryStore::purge_thread`): that tier is scoped entirely to this
/// thread's lifetime, so leaving it behind after the thread row is gone
/// would leak notes with no owner left to read or clean them up.
///
/// Also vacates any ASYNC form still pending on this thread (see
/// `form_answers::vacate_stranded_async_form`) — deleting the thread strands
/// it exactly like cancelling its run does, and without this the thread's
/// `pending_forms` slot would stay occupied forever. The row is read BEFORE
/// `threads.delete()` runs: that call returns only whether a row existed,
/// not the row itself, and vacating needs both the owning agent_id (the
/// snapshot's scope key, read off `ThreadScope::AgentChat`) and this
/// thread's own transcript path, neither of which is recoverable once the
/// row is gone. Only `AgentChat`-scoped threads carry a resolvable owning
/// agent_id this way; `TeamChat`/`Delegation`/`Artifact` threads are left
/// alone, matching `ThreadStore::list_for_agent`'s own agent-only scope.
pub async fn delete_thread(
    State(state): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
) -> Result<StatusCode, AppError> {
    let thread = state.persistence.threads.get(&thread_id).await?;

    let removed = state.persistence.threads.delete(&thread_id).await?;
    if removed {
        state.persistence.memory.purge_thread(&thread_id).await?;
        if let Some(thread) = thread {
            if let ThreadScope::AgentChat { agent_id } = &thread.scope {
                form_answers::vacate_stranded_async_form(
                    &state,
                    agent_id,
                    Some(thread_id.clone()),
                    Some(PathBuf::from(&thread.transcript_path)),
                )
                .await;
            }
        }
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError(AoError::ThreadNotFound(thread_id)))
    }
}

/// `POST /threads/{thread_id}/archive` — hide a thread from the tab strip,
/// overflow panel, Threads panel's main list, and Home's thread list without
/// deleting anything. Refuses to archive a default thread (400).
pub async fn archive_thread(
    State(state): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
) -> Result<Json<Thread>, AppError> {
    let row = state.persistence.threads.archive(&thread_id).await?;
    Ok(Json(row))
}

/// `POST /threads/{thread_id}/unarchive` — reverse of [`archive_thread`],
/// restoring the thread to every surface it was hidden from.
pub async fn unarchive_thread(
    State(state): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
) -> Result<Json<Thread>, AppError> {
    let row = state.persistence.threads.unarchive(&thread_id).await?;
    Ok(Json(row))
}

#[cfg(test)]
mod delete_thread_purge_tests {
    use super::*;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::memory::MemorySource;

    async fn setup_state() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("Failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = PersistenceLayer::init();
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock).await.expect("AppState init")
        };
        (Arc::new(state), tmp)
    }

    /// `AppError` doesn't implement `Debug` (see `crate::error::AppError`),
    /// so `Result::expect`/`unwrap` can't be used directly on route-handler
    /// results here. Mirrors the `unwrap_ok` helper in `routes::memories`'s
    /// test modules.
    fn unwrap_ok<T>(r: Result<T, AppError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {:?}", e.0),
        }
    }

    /// Deleting a thread that has thread-scoped memory must purge that
    /// memory's JSONL file, not just drop the thread row — otherwise the
    /// file leaks forever with no thread left to own or clean it up.
    #[tokio::test]
    async fn delete_thread_purges_its_memory_file() {
        let (state, _tmp) = setup_state().await;
        let row = state.persistence.threads.build_fresh_thread("agent-1", None);
        let created = state.persistence.threads.create(row).await.unwrap();

        state
            .persistence
            .memory
            .add_thread(&created.id, "scratch note", MemorySource::Manual)
            .await
            .unwrap();
        assert_eq!(
            state.persistence.memory.list_thread(&created.id).await.unwrap().len(),
            1
        );

        let status = unwrap_ok(delete_thread(State(Arc::clone(&state)), Path(created.id.clone())).await);
        assert_eq!(status, StatusCode::NO_CONTENT);

        assert!(
            state.persistence.memory.list_thread(&created.id).await.unwrap().is_empty(),
            "memory must read back empty after the thread is deleted"
        );
        assert!(
            state.persistence.threads.get(&created.id).await.unwrap().is_none(),
            "thread row must be gone too"
        );
    }

    /// A thread that never wrote a memory has no file to purge — deleting it
    /// must still succeed (purge_thread is a no-op on a missing file).
    #[tokio::test]
    async fn delete_thread_with_no_memory_still_succeeds() {
        let (state, _tmp) = setup_state().await;
        let row = state.persistence.threads.build_fresh_thread("agent-1", None);
        let created = state.persistence.threads.create(row).await.unwrap();

        let status = unwrap_ok(delete_thread(State(Arc::clone(&state)), Path(created.id.clone())).await);
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    /// Deleting an unknown thread id must 404 and must not attempt a purge
    /// call at all (nothing to purge, and no thread_id to trust).
    #[tokio::test]
    async fn delete_unknown_thread_returns_404() {
        let (state, _tmp) = setup_state().await;
        let err = delete_thread(State(Arc::clone(&state)), Path("ghost-thread".to_string()))
            .await
            .expect_err("unknown thread should fail");
        assert!(matches!(err.0, AoError::ThreadNotFound(_)));
    }
}

#[cfg(test)]
mod delete_thread_form_vacate_tests {
    use super::*;
    use ao_engine_tools_core::FORM_WITHDRAWN;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::event::AgentEventPayload;

    async fn setup_state() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("Failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = PersistenceLayer::init();
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock).await.expect("AppState init")
        };

        // See the matching comment in `routes::messages`'s
        // `cancel_agent_run_form_vacate_tests::setup_state` — one throwaway,
        // discarded round trip through the snapshot store settles a
        // pre-existing startup race with `AppState::new_with_mock`'s
        // detached best-effort sweeps before any real assertion runs.
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

    /// Deleting a thread must not just drop its row — it must also vacate
    /// any ASYNC form still pending on it (nothing will ever answer or
    /// dismiss it once the thread is gone) and tell the frontend via the
    /// same `FormResolved` event `async_form_answer` emits.
    #[tokio::test]
    async fn delete_thread_vacates_async_pending_form_and_emits_withdrawn_event() {
        let (state, _tmp) = setup_state().await;
        let row = state.persistence.threads.build_fresh_thread("agent-1", None);
        let created = state.persistence.threads.create(row).await.unwrap();

        state
            .persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                Some(created.id.clone()),
                "form-async".to_string(),
                async_spec("Ship it?"),
            )
            .await
            .unwrap();

        let mut events = state.event_bus.subscribe();

        let status = unwrap_ok(delete_thread(State(Arc::clone(&state)), Path(created.id.clone())).await);
        assert_eq!(status, StatusCode::NO_CONTENT);

        let snap = state.persistence.snapshots.get().await;
        assert!(
            snap.agents["agent-1"].pending_forms.is_empty(),
            "the stranded async form's slot must be vacated"
        );

        let thread_path = std::path::PathBuf::from(&created.transcript_path);
        let entries = state.persistence.transcripts.read_all_at(&thread_path).await.unwrap();
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

    /// Anti-lockout: after deletion vacates the stranded async form, a brand
    /// new form on that same thread_id string must be accepted — the whole
    /// point of the fix.
    #[tokio::test]
    async fn delete_thread_then_new_form_on_same_thread_is_accepted() {
        let (state, _tmp) = setup_state().await;
        let row = state.persistence.threads.build_fresh_thread("agent-1", None);
        let created = state.persistence.threads.create(row).await.unwrap();

        state
            .persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                Some(created.id.clone()),
                "form-stranded".to_string(),
                async_spec("Are you sure?"),
            )
            .await
            .unwrap();

        unwrap_ok(delete_thread(State(Arc::clone(&state)), Path(created.id.clone())).await);

        let replaced = state
            .persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                Some(created.id.clone()),
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

    /// A SYNC form on the deleted thread must be left alone — cleared
    /// exclusively by `PendingFormClearGuard`'s `Drop` impl, not by this
    /// route.
    #[tokio::test]
    async fn delete_thread_leaves_sync_form_untouched() {
        let (state, _tmp) = setup_state().await;
        let row = state.persistence.threads.build_fresh_thread("agent-1", None);
        let created = state.persistence.threads.create(row).await.unwrap();

        state
            .persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                Some(created.id.clone()),
                "form-sync".to_string(),
                sync_spec("Confirm deploy"),
            )
            .await
            .unwrap();

        unwrap_ok(delete_thread(State(Arc::clone(&state)), Path(created.id.clone())).await);

        let snap = state.persistence.snapshots.get().await;
        assert_eq!(
            snap.agents["agent-1"].pending_forms.len(),
            1,
            "the sync form's slot must still be occupied — only its own Drop guard clears it"
        );
        assert_eq!(snap.agents["agent-1"].pending_forms[0].form_id, "form-sync");

        let thread_path = std::path::PathBuf::from(&created.transcript_path);
        let entries = state.persistence.transcripts.read_all_at(&thread_path).await.unwrap();
        assert!(
            !entries.iter().any(|e| e.event_type == FORM_WITHDRAWN),
            "must not write a withdrawn trace for a sync form: {entries:?}"
        );
    }

    /// Deleting thread A must never reach into thread B's pending form.
    #[tokio::test]
    async fn delete_thread_does_not_vacate_other_threads_form() {
        let (state, _tmp) = setup_state().await;
        let row_a = state.persistence.threads.build_fresh_thread("agent-1", None);
        let thread_a = state.persistence.threads.create(row_a).await.unwrap();
        let row_b = state.persistence.threads.build_fresh_thread("agent-1", None);
        let thread_b = state.persistence.threads.create(row_b).await.unwrap();

        state
            .persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                Some(thread_a.id.clone()),
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
                Some(thread_b.id.clone()),
                "form-b".to_string(),
                async_spec("Thread B question"),
            )
            .await
            .unwrap();

        unwrap_ok(delete_thread(State(Arc::clone(&state)), Path(thread_a.id.clone())).await);

        let snap = state.persistence.snapshots.get().await;
        let agent = &snap.agents["agent-1"];
        assert_eq!(agent.pending_forms.len(), 1, "thread-b's form must survive untouched");
        assert_eq!(agent.pending_forms[0].form_id, "form-b");
        assert_eq!(agent.pending_forms[0].thread_id.as_deref(), Some(thread_b.id.as_str()));
    }
}

#[cfg(test)]
mod list_all_threads_tests {
    use super::*;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;

    async fn setup_state() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("Failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = PersistenceLayer::init();
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock).await.expect("AppState init")
        };
        (Arc::new(state), tmp)
    }

    /// `AppError` doesn't implement `Debug` — see the matching helper in
    /// `delete_thread_purge_tests` above.
    fn unwrap_ok<T>(r: Result<T, AppError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {:?}", e.0),
        }
    }

    /// `GET /threads` must return 200 (i.e. `Ok(Json(..))`) and group every
    /// thread by its owning agent id: default + fresh rows both land under
    /// `agent-1`, agent-2's lone default row lands under `agent-2`. The
    /// predicate/sort itself — including that non-`AgentChat` scopes never
    /// appear in any bucket — is covered in depth by the store-level
    /// anti-drift test (`ThreadStore::list_all_grouped` in
    /// `ao_persistence::thread_store`); this is the thin route-level smoke
    /// test for wiring, not the predicate.
    #[tokio::test]
    async fn returns_all_threads_grouped_by_agent() {
        let (state, _tmp) = setup_state().await;

        state.persistence.threads.ensure_default_thread("agent-1").await.unwrap();
        let fresh = state
            .persistence
            .threads
            .build_fresh_thread("agent-1", Some("Fresh".to_string()));
        state.persistence.threads.create(fresh).await.unwrap();

        state.persistence.threads.ensure_default_thread("agent-2").await.unwrap();

        let Json(grouped) = unwrap_ok(list_all_threads(State(Arc::clone(&state))).await);

        assert_eq!(grouped.len(), 2, "expected exactly agent-1 and agent-2 as keys");
        assert_eq!(grouped["agent-1"].len(), 2, "agent-1: default + fresh");
        assert_eq!(grouped["agent-2"].len(), 1, "agent-2: default only");
        assert!(grouped["agent-1"].iter().any(|t| t.kind == ThreadKind::Default));
        assert!(grouped["agent-1"]
            .iter()
            .any(|t| t.title.as_deref() == Some("Fresh")));
        // Default sorts first within a bucket, matching `list_for_agent`.
        assert_eq!(grouped["agent-1"][0].kind, ThreadKind::Default);
    }

    /// An empty store still returns 200 with an empty map, not an error.
    #[tokio::test]
    async fn returns_empty_map_when_no_threads_exist() {
        let (state, _tmp) = setup_state().await;
        let Json(grouped) = unwrap_ok(list_all_threads(State(Arc::clone(&state))).await);
        assert!(grouped.is_empty());
    }
}
