//! Startup sweep that reaps persisted `mode: "sync"` pending forms whose
//! owning run/session did not survive a process restart.
//!
//! # Why sync forms can't simply resume
//!
//! A synchronous `AskUserQuestionWithForm` call parks the calling tool's
//! future on a `tokio::sync::oneshot::Receiver` (see
//! `ao_engine_tools_runner::prompt_bridge::LiveFormBridge::ask_form`). It
//! persists a pointer to that pending form into the snapshot's
//! `pending_forms` (tagged `spec.mode == "sync"`) purely so the frontend can
//! reconstruct the answerable UI after a page reload — the persisted copy
//! carries no continuation. When the server process restarts, the
//! `oneshot::Sender` half and the parked tokio task both vanish with it;
//! there is no serialized future to resume and no code path that could
//! deliver an answer into a form like this ever again. Async forms don't
//! have this problem — they never suspend a task in the first place, so a
//! restart genuinely loses nothing for them.
//!
//! This module does not — and must not — try to make sync forms
//! restart-durable, or quietly convert one into an async form. Either would
//! change the tool call's control-flow semantics behind the agent's back.
//! The only honest thing this sweep can do is mark the form dead so the
//! frontend stops presenting it as answerable.
//!
//! [`reap_orphaned_sync_forms`] runs once at boot (see call sites in
//! `crate::state::AppState::new`/`new_with_mocks`), reads every persisted
//! `pending_forms` entry, and — for the ones tagged `mode: "sync"` whose
//! scope has no live run in this process — flips `PendingForm::orphaned` to
//! `true` via [`SnapshotStore::mark_pending_form_orphaned`]. Async entries
//! and already-orphaned entries are left completely untouched.

use std::sync::Arc;

use ao_persistence::PersistenceLayer;
use tracing::error;

use crate::instance_registry::InstanceRegistry;

/// Reap every persisted `mode: "sync"` pending form whose owning agent/
/// project scope has no live run in this process, marking it
/// [`orphaned`](ao_persistence::snapshot::PendingForm::orphaned).
///
/// `scope_key` here is exactly the key the snapshot's `agents` map is keyed
/// by — an agent id for a personal-chat form, or `project_{id}` for a
/// project-scoped one (see `SyncFormPersistence::scope_key` in
/// `ao-engine-tools-runner`). Liveness is checked via
/// [`InstanceRegistry::running_count`], the same registry `GET /agents`
/// overlays `has_active_run` from — this process constructs `instance_registry`
/// empty at the very start of [`crate::state::AppState::new`], before any run
/// is dispatched, so in practice every check made from this sweep resolves
/// to "not live." The check still runs (rather than being skipped
/// unconditionally) so a scope that somehow already has a run registered by
/// the time this sweep executes — which requirement 2 calls out as "should
/// be impossible across a restart, but handle it" — is left untouched
/// instead of having its live form yanked out from under it.
///
/// Per-form failures are logged at `error` level with both the scope key and
/// form id and do not abort the sweep — one bad snapshot write must not
/// leave every other agent's orphaned forms silently un-reaped.
pub async fn reap_orphaned_sync_forms(
    persistence: Arc<PersistenceLayer>,
    instance_registry: Arc<InstanceRegistry>,
) {
    let snapshot = persistence.snapshots.get().await;

    for (scope_key, agent) in snapshot.agents.iter() {
        for form in &agent.pending_forms {
            if form.orphaned {
                continue; // Already reaped on a previous boot.
            }

            let mode = form
                .spec
                .get("mode")
                .and_then(|m| m.as_str())
                .unwrap_or("async");
            if mode != "sync" {
                continue; // Async forms are restart-durable — never touched.
            }

            if instance_registry.running_count(scope_key).await > 0 {
                // Should be impossible across a real restart (see doc
                // comment above) — defensive, not a real path.
                continue;
            }

            if let Err(e) = persistence
                .snapshots
                .mark_pending_form_orphaned(scope_key, &form.form_id)
                .await
            {
                error!(
                    agent_id = %scope_key,
                    form_id = %form.form_id,
                    error = %e,
                    "failed to mark orphaned sync form during startup reap"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_persistence::paths::DataRoot;

    async fn make_test_persistence() -> (Arc<PersistenceLayer>, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let data_root = DataRoot::new(tmp.path());
        data_root.ensure_directories().await.expect("ensure_directories");
        let p = PersistenceLayer::init_with_root(data_root)
            .await
            .expect("init persistence");
        (Arc::new(p), tmp)
    }

    #[tokio::test]
    async fn sync_form_with_no_live_run_is_marked_orphaned() {
        let (persistence, _tmp) = make_test_persistence().await;
        persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                None,
                "form-sync".to_string(),
                serde_json::json!({"form_id": "form-sync", "spec": {}, "mode": "sync"}),
            )
            .await
            .unwrap();

        let instance_registry = Arc::new(InstanceRegistry::new());
        reap_orphaned_sync_forms(Arc::clone(&persistence), instance_registry).await;

        let snap = persistence.snapshots.get().await;
        let form = &snap.agents.get("agent-1").unwrap().pending_forms[0];
        assert!(form.orphaned, "sync form with no live run must be marked orphaned");
    }

    #[tokio::test]
    async fn async_form_is_left_completely_untouched() {
        let (persistence, _tmp) = make_test_persistence().await;
        persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                None,
                "form-async".to_string(),
                serde_json::json!({"form_id": "form-async", "spec": {}, "mode": "async"}),
            )
            .await
            .unwrap();

        let instance_registry = Arc::new(InstanceRegistry::new());
        reap_orphaned_sync_forms(Arc::clone(&persistence), instance_registry).await;

        let snap = persistence.snapshots.get().await;
        let form = &snap.agents.get("agent-1").unwrap().pending_forms[0];
        assert!(!form.orphaned, "async forms must never be marked orphaned");
    }

    #[tokio::test]
    async fn sync_form_with_a_live_run_is_left_untouched() {
        let (persistence, _tmp) = make_test_persistence().await;
        persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                None,
                "form-sync".to_string(),
                serde_json::json!({"form_id": "form-sync", "spec": {}, "mode": "sync"}),
            )
            .await
            .unwrap();

        // Simulate the "should be impossible across a restart" case: the
        // scope already has a run registered by the time the sweep runs.
        let instance_registry = Arc::new(InstanceRegistry::new());
        instance_registry
            .register_run(&"agent-1".to_string(), "run-1")
            .await;

        reap_orphaned_sync_forms(Arc::clone(&persistence), Arc::clone(&instance_registry)).await;

        let snap = persistence.snapshots.get().await;
        let form = &snap.agents.get("agent-1").unwrap().pending_forms[0];
        assert!(!form.orphaned, "a form whose scope has a live run must be left untouched");
    }

    #[tokio::test]
    async fn already_orphaned_form_is_not_touched_again() {
        let (persistence, _tmp) = make_test_persistence().await;
        persistence
            .snapshots
            .set_pending_form(
                "agent-1",
                None,
                "form-sync".to_string(),
                serde_json::json!({"form_id": "form-sync", "spec": {}, "mode": "sync"}),
            )
            .await
            .unwrap();
        persistence
            .snapshots
            .mark_pending_form_orphaned("agent-1", "form-sync")
            .await
            .unwrap();

        // A second sweep (e.g. a hypothetical re-run) must not error or
        // otherwise disturb an already-reaped entry.
        let instance_registry = Arc::new(InstanceRegistry::new());
        reap_orphaned_sync_forms(Arc::clone(&persistence), instance_registry).await;

        let snap = persistence.snapshots.get().await;
        let form = &snap.agents.get("agent-1").unwrap().pending_forms[0];
        assert!(form.orphaned);
    }
}
