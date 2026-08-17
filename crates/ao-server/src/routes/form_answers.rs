use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use ao_engine::project_queue_manager::ProjectMessage;
use ao_engine::AppState;
use ao_engine_tools_core::{
    form_answer_content, form_answer_spec_snapshot, form_withdrawn_entry, FormAction, FormAnswer,
    FormAnswerMeta, FormDismissedMeta, FormResponse, FORM_ANSWER, FORM_DISMISSED,
};
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::message::QueuedMessage;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

use crate::error::AppError;

/// The three JSON shapes for a single field's answer in the submitted form.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum AnswerPayload {
    /// Answer for `text` and `textarea` fields.
    Text { value: String },
    /// Answer for `checkbox` and `radio` fields — selected option ids.
    Selections { values: Vec<String> },
    /// Answer for `file` fields — server-assigned attachment ids.
    Files { attachment_ids: Vec<String> },
}

#[derive(Debug, Deserialize)]
pub struct SubmitFormAnswerRequest {
    pub form_id: String,
    #[serde(default)]
    pub(crate) answers: HashMap<String, AnswerPayload>,
    /// Set when the operator clicked an action button (Cancel / Regenerate /
    /// Something else) instead of submitting. When present, `answers` is
    /// ignored (expected empty) and this is what reaches the waiting tool.
    #[serde(default)]
    pub action: Option<FormAction>,
    /// Optional free-text note accompanying `action`. Currently never sent by
    /// the UI — reserved for a future inline note field.
    #[serde(default)]
    pub note: Option<String>,
}

/// POST /agents/{agent_id}/form-answer — deliver a completed form (or an
/// action-button click) to the waiting tool.
///
/// Returns 200 on success, 404 if the form_id is not found (session ended,
/// already answered, or never registered). The frontend should handle 404
/// gracefully and not block the UI on a stale form submission.
pub async fn submit_form_answer(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<SubmitFormAnswerRequest>,
) -> Result<StatusCode, AppError> {
    let answers: HashMap<String, FormAnswer> = req
        .answers
        .into_iter()
        .map(|(field_id, payload)| {
            let answer = match payload {
                AnswerPayload::Text { value } => FormAnswer::Text(value),
                AnswerPayload::Selections { values } => FormAnswer::Selections(values),
                AnswerPayload::Files { attachment_ids } => FormAnswer::Files(attachment_ids),
            };
            (field_id, answer)
        })
        .collect();

    let response = FormResponse {
        form_id: req.form_id.clone(),
        answers,
        action: req.action,
        note: req.note,
    };

    match state.form_bridge_registry.deliver(&agent_id, &req.form_id, response) {
        Ok(()) => Ok(StatusCode::OK),
        Err(_) => Ok(StatusCode::NOT_FOUND),
    }
}

// --- Async form routes ---

#[derive(Debug, Deserialize)]
pub struct AsyncFormAnswerRequest {
    pub values: HashMap<String, Value>,
}

#[derive(Debug, Serialize)]
pub struct AsyncFormAnswerAck {
    pub message_id: String,
    pub status: String,
}

/// POST /agents/{agent_id}/async-forms/{form_id}/answer
///
/// Validates form_id matches the agent's pending form, appends a
/// self-rendering form_answer transcript entry, clears the pending pointer,
/// emits a `FormResolved` event, then queues a new agent turn whose content
/// carries the structured values. The transcript entry, the event, and the
/// queued turn are all routed to the thread the form was originally posted
/// on (see [`pending_form_record`]), not the agent's default thread.
pub async fn async_form_answer(
    State(state): State<Arc<AppState>>,
    Path((agent_id, form_id)): Path<(String, String)>,
    Json(req): Json<AsyncFormAnswerRequest>,
) -> Result<Json<AsyncFormAnswerAck>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    // Capture the pending form's thread_id AND spec up front — the pointer is
    // cleared a few lines down, and that clear is the only place either value
    // lives. Reading them late (after the clear) would silently lose both:
    // thread_id would leave the writes below back on the agent's default
    // thread, and spec is what lets the transcript entry below render itself
    // without depending on the (by-then-gone) pending record.
    let pending = pending_form_record(&state, &agent_id, &form_id).await?;
    let thread_id = pending.thread_id.clone();

    let entry = build_form_answer_entry(&agent_id, &form_id, &pending.spec, req.values.clone());
    match state
        .persistence
        .threads
        .resolve_transcript_path_override(thread_id.as_deref())
        .await
    {
        Some(path) => state.persistence.transcripts.append_at(&path, &entry).await?,
        None => state.persistence.transcripts.append(&agent_id, &entry).await?,
    }
    state.persistence.snapshots.clear_pending_form(&agent_id, &form_id).await?;

    // Live-push so the UI can clear its pending-form indicator without
    // polling or refetching agent state — same EventBus/AgentEventPayload/SSE
    // transport the CREATE-path `FormPosted` event uses
    // (`ao_engine_tools_core::form_events::wire_posted_async_form`), and the
    // same run_id==agent_id convention that event's `EventBusAgentSink` emits
    // under (see `ao-engine`'s `event_bus.rs`).
    state
        .event_bus
        .emit(
            &agent_id,
            &agent_id,
            thread_id.clone(),
            AgentEventPayload::FormResolved { form_id: form_id.clone() },
        )
        .await;

    let message_id = Uuid::new_v4().to_string();
    let content = serde_json::json!({ "form_id": &form_id, "values": req.values }).to_string();
    let queued = QueuedMessage {
        message_id: message_id.clone(),
        content,
        queued_at: Utc::now(),
        attachments: vec![],
        source: None,
        focus_path: None,
        thread_id,
    };
    state.queue_managers.submit_message(&agent, queued).await?;

    Ok(Json(AsyncFormAnswerAck {
        message_id,
        status: "queued".to_string(),
    }))
}

/// POST /agents/{agent_id}/async-forms/{form_id}/dismiss
///
/// Validates form_id matches the agent's pending form, appends a
/// form_dismissed transcript entry, and clears the pending pointer.
/// Does NOT queue a new turn.
pub async fn async_form_dismiss(
    State(state): State<Arc<AppState>>,
    Path((agent_id, form_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    check_pending_form_id(&state, &agent_id, &form_id).await?;

    let entry = build_form_dismissed_entry(&agent_id, &form_id);
    state.persistence.transcripts.append(&agent_id, &entry).await?;
    state.persistence.snapshots.clear_pending_form(&agent_id, &form_id).await?;

    Ok(StatusCode::OK)
}

async fn check_pending_form_id(
    state: &Arc<AppState>,
    agent_id: &str,
    form_id: &str,
) -> Result<(), AppError> {
    let snapshot = state.persistence.snapshots.get().await;
    let pending = snapshot
        .agents
        .get(agent_id)
        .map(|s| s.pending_forms.as_slice())
        .unwrap_or(&[]);
    check_form_id_is_pending(pending, form_id)?;
    Ok(())
}

/// Looks up the pending-form record matching `form_id` under snapshot key
/// `key` (an agent_id or a `project_{id}` scope key — both answer routes use
/// this), validating in the same pass that `form_id` is actually the current
/// pending form (same rule as [`check_pending_form_id`]). The answer routes
/// need the full record — not just the existence check — for its
/// `thread_id` (routing) and `spec` (the self-rendering entry's content), and
/// need both read out before `clear_pending_form` removes the record a few
/// lines later.
async fn pending_form_record(
    state: &Arc<AppState>,
    key: &str,
    form_id: &str,
) -> Result<ao_persistence::snapshot::PendingForm, AppError> {
    let snapshot = state.persistence.snapshots.get().await;
    let pending = snapshot
        .agents
        .get(key)
        .map(|s| s.pending_forms.as_slice())
        .unwrap_or(&[]);
    let form = check_form_id_is_pending(pending, form_id)?;
    Ok(form.clone())
}

/// Any thread on the agent (or project) may hold the pending form being
/// answered — `form_id`s are server-generated and globally unique, so a
/// membership check against the whole `pending_forms` list (rather than a
/// single-thread lookup) is sufficient and avoids requiring the caller to
/// know which thread posted it.
///
/// Returns the matched record (not just `()`) so [`pending_form_record`]
/// can clone the whole thing off the same lookup instead of scanning twice.
fn check_form_id_is_pending<'a>(
    pending_forms: &'a [ao_persistence::snapshot::PendingForm],
    form_id: &str,
) -> Result<&'a ao_persistence::snapshot::PendingForm, AoError> {
    pending_forms.iter().find(|f| f.form_id == form_id).ok_or_else(|| {
        AoError::ValidationError(format!("form_id '{}' is not the current pending form", form_id))
    })
}

/// POST /projects/{project_id}/form-answer
///
/// Project-scoped variant of [`submit_form_answer`]. Validates the project
/// exists, then delivers the form answer to the bridge registered under the
/// project's own agent_id. Returns 404 if the project is not found or the
/// form_id is not registered (session ended, already answered, etc.).
pub async fn submit_form_answer_project(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
    Json(req): Json<SubmitFormAnswerRequest>,
) -> Result<StatusCode, AppError> {
    let project = state
        .persistence
        .projects
        .get(&project_id)
        .await?
        .ok_or_else(|| AoError::ProjectNotFound(project_id.clone()))?;

    let answers: HashMap<String, FormAnswer> = req
        .answers
        .into_iter()
        .map(|(field_id, payload)| {
            let answer = match payload {
                AnswerPayload::Text { value } => FormAnswer::Text(value),
                AnswerPayload::Selections { values } => FormAnswer::Selections(values),
                AnswerPayload::Files { attachment_ids } => FormAnswer::Files(attachment_ids),
            };
            (field_id, answer)
        })
        .collect();

    let response = FormResponse {
        form_id: req.form_id.clone(),
        answers,
        action: req.action,
        note: req.note,
    };

    match state.form_bridge_registry.deliver(&project.agent_id, &req.form_id, response) {
        Ok(()) => Ok(StatusCode::OK),
        Err(_) => Ok(StatusCode::NOT_FOUND),
    }
}

/// POST /projects/{project_id}/async-forms/{form_id}/answer
///
/// Project-scoped variant of [`async_form_answer`]. Validates form_id against
/// the pending form stored under the project snapshot key, appends a
/// `form_answer` entry to the project transcript, clears the pending pointer,
/// then queues a new agent turn via the project queue.
pub async fn async_form_answer_project(
    State(state): State<Arc<AppState>>,
    Path((project_id, form_id)): Path<(String, String)>,
    Json(req): Json<AsyncFormAnswerRequest>,
) -> Result<Json<AsyncFormAnswerAck>, AppError> {
    state
        .persistence
        .projects
        .get(&project_id)
        .await?
        .ok_or_else(|| AoError::ProjectNotFound(project_id.clone()))?;

    let scope_key = format!("project_{}", project_id);

    // Capture the pending form's spec up front — same reasoning as
    // `async_form_answer`: `clear_pending_form` below is the only place it
    // lives, and it's read after the clear otherwise.
    let pending = pending_form_record(&state, &scope_key, &form_id).await?;

    let entry = build_form_answer_entry(&scope_key, &form_id, &pending.spec, req.values.clone());
    state.persistence.transcripts.append(&scope_key, &entry).await?;
    state.persistence.snapshots.clear_pending_form(&scope_key, &form_id).await?;

    // Live-push on the project's own SSE channel (`project:{id}` — see
    // `GET /projects/{id}/stream`'s filter), NOT the `project_{id}` scope key
    // used for persistence above; those are deliberately different
    // namespaces (colon vs. underscore) for the same project.
    let event_channel = format!("project:{}", project_id);
    state
        .event_bus
        .emit(
            &event_channel,
            &event_channel,
            None,
            AgentEventPayload::FormResolved { form_id: form_id.clone() },
        )
        .await;

    let message_id = Uuid::new_v4().to_string();
    let content = serde_json::json!({ "form_id": &form_id, "values": req.values }).to_string();
    let queued = QueuedMessage {
        message_id: message_id.clone(),
        content,
        queued_at: Utc::now(),
        attachments: vec![],
        source: None,
        focus_path: None,
        thread_id: None,
    };
    state
        .project_queue_managers
        .submit_message(&project_id, ProjectMessage::User(queued))
        .await?;

    Ok(Json(AsyncFormAnswerAck {
        message_id,
        status: "queued".to_string(),
    }))
}

/// POST /projects/{project_id}/async-forms/{form_id}/dismiss
///
/// Project-scoped variant of [`async_form_dismiss`]. Validates form_id against
/// the pending form stored under the project snapshot key, appends a
/// `form_dismissed` entry to the project transcript, and clears the pending
/// pointer. Does NOT queue a new agent turn.
pub async fn async_form_dismiss_project(
    State(state): State<Arc<AppState>>,
    Path((project_id, form_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    state
        .persistence
        .projects
        .get(&project_id)
        .await?
        .ok_or_else(|| AoError::ProjectNotFound(project_id.clone()))?;

    let scope_key = format!("project_{}", project_id);

    check_pending_form_id_for_key(&state, &scope_key, &form_id).await?;

    let entry = build_form_dismissed_entry(&scope_key, &form_id);
    state.persistence.transcripts.append(&scope_key, &entry).await?;
    state.persistence.snapshots.clear_pending_form(&scope_key, &form_id).await?;

    Ok(StatusCode::OK)
}

async fn check_pending_form_id_for_key(
    state: &Arc<AppState>,
    key: &str,
    form_id: &str,
) -> Result<(), AppError> {
    let snapshot = state.persistence.snapshots.get().await;
    let pending = snapshot
        .agents
        .get(key)
        .map(|s| s.pending_forms.as_slice())
        .unwrap_or(&[]);
    check_form_id_is_pending(pending, form_id)?;
    Ok(())
}

fn to_meta_map<T: serde::Serialize>(v: &T) -> HashMap<String, Value> {
    match serde_json::to_value(v) {
        Ok(Value::Object(m)) => m.into_iter().collect(),
        _ => Default::default(),
    }
}

/// `pending_spec` is the wrapper JSON off the answered form's `PendingForm`
/// record — captured by the caller before the record is cleared (see
/// [`pending_form_record`]) — and is what makes `content` self-rendering:
/// reading the resulting entry alone tells a transcript reader what was
/// asked and what was answered, with no join against a `form_request` entry
/// and no live pending-form record required.
fn build_form_answer_entry(
    agent_id: &str,
    form_id: &str,
    pending_spec: &Value,
    values: HashMap<String, Value>,
) -> TranscriptEntry {
    let content = form_answer_content(pending_spec, &values);
    // Snapshot the spec onto the entry itself (see `FormAnswerMeta::spec`'s
    // doc comment) — `pending_spec` is only available here, captured by the
    // caller before the pending-form record is cleared; there is no live
    // registry to re-resolve it from later, and forms get superseded and
    // withdrawn, so a later lookup could easily find nothing or the wrong
    // form.
    let spec = form_answer_spec_snapshot(pending_spec);
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Agent {
            agent: agent_id.to_string(),
        },
        content,
        event_type: FORM_ANSWER.to_string(),
        metadata: Some(to_meta_map(&FormAnswerMeta {
            form_id: form_id.to_string(),
            values,
            spec,
        })),
        hidden_from_user: false,
    }
}

fn build_form_dismissed_entry(agent_id: &str, form_id: &str) -> TranscriptEntry {
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Agent {
            agent: agent_id.to_string(),
        },
        content: String::new(),
        event_type: FORM_DISMISSED.to_string(),
        metadata: Some(to_meta_map(&FormDismissedMeta {
            form_id: form_id.to_string(),
        })),
        hidden_from_user: false,
    }
}

/// Vacate the ASYNC pending form (if any) filed on `(agent_id, thread_id)` —
/// call this from any path that ends a thread's run or removes the thread
/// itself without going through [`async_form_answer`]/[`async_form_dismiss`]
/// above, so a form stranded that way doesn't permanently occupy the
/// thread's one-pending-form slot: a later post on the same thread would
/// displace it via the newest-wins supersede path regardless, but if no
/// further form is ever posted there, an uncleared stranded entry would sit
/// in `pending_forms` forever, keeping the composer blocked on a thread that
/// has no run left to answer it.
///
/// SYNC forms are left untouched: `PendingFormClearGuard`'s `Drop` impl
/// (`ao-engine-tools-runner`'s `prompt_bridge` module) is the sole owner of
/// clearing those, and it fires from the very same cancellation this
/// function's callers are responding to — see
/// [`ao_persistence::snapshot::SnapshotStore::clear_pending_async_form_for_thread`],
/// which this delegates to for the mode check.
///
/// On a hit, appends the same `form_withdrawn` transcript entry
/// [`ao_engine_tools_core::form_events::persist_posted_form`]'s supersede
/// case already writes (via [`form_withdrawn_entry`]) and pushes a live
/// `FormResolved` event — the same event [`async_form_answer`] uses to let a
/// connected client clear its pending-form indicator without waiting for a
/// refetch. `transcript_path` lets the caller pin the write to the thread's
/// own file when the thread row itself is about to be (or has just been)
/// removed and can no longer be resolved by id from `thread_id` alone;
/// `None` falls back to the agent-keyed append, the same fallback
/// `async_form_answer` uses for the default thread.
///
/// Best-effort throughout, matching every other snapshot/transcript touch on
/// this path: a write failure here is logged and never propagated, since
/// this is cleanup running alongside — not gating — the caller's own primary
/// action (cancelling a run, deleting a thread).
pub(crate) async fn vacate_stranded_async_form(
    state: &Arc<AppState>,
    agent_id: &str,
    thread_id: Option<String>,
    transcript_path: Option<PathBuf>,
) {
    let replaced = match state
        .persistence
        .snapshots
        .clear_pending_async_form_for_thread(agent_id, thread_id.clone())
        .await
    {
        Ok(Some(replaced)) => replaced,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(
                agent_id = %agent_id,
                thread_id = ?thread_id,
                error = %e,
                "failed to clear stranded async pending form from snapshot"
            );
            return;
        }
    };

    let entry = form_withdrawn_entry(agent_id, &replaced.form_id, &replaced.spec);
    let append_result = match &transcript_path {
        Some(path) => state.persistence.transcripts.append_at(path, &entry).await,
        None => state.persistence.transcripts.append(agent_id, &entry).await,
    };
    if let Err(e) = append_result {
        tracing::warn!(
            agent_id = %agent_id,
            form_id = %replaced.form_id,
            error = %e,
            "failed to persist form_withdrawn transcript entry for stranded async form"
        );
    }

    let agent_id_owned = agent_id.to_string();
    state
        .event_bus
        .emit(
            agent_id,
            &agent_id_owned,
            thread_id,
            AgentEventPayload::FormResolved { form_id: replaced.form_id },
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_persistence::{paths::DataRoot, snapshot::PendingForm, snapshot::SnapshotStore, transcript::TranscriptStore};
    use serde_json::json;
    use tempfile::TempDir;

    fn pending(form_id: &str, thread_id: Option<&str>) -> PendingForm {
        PendingForm {
            thread_id: thread_id.map(str::to_string),
            form_id: form_id.to_string(),
            spec: json!({}),
            is_latest_in_thread: true,
            orphaned: false,
        }
    }

    #[test]
    fn form_id_match_passes() {
        let forms = [pending("form-abc", None)];
        let result = check_form_id_is_pending(&forms, "form-abc");
        assert!(result.is_ok());
    }

    #[test]
    fn stale_form_id_rejected() {
        assert!(check_form_id_is_pending(&[], "form-1").is_err());
        assert!(check_form_id_is_pending(&[pending("form-other", None)], "form-1").is_err());
    }

    #[test]
    fn pending_form_on_any_thread_is_accepted() {
        // `form_id`s are globally unique, so the check must find a match
        // regardless of which thread it's scoped to.
        let forms = [pending("form-a", None), pending("form-b", Some("thread-b"))];
        assert!(check_form_id_is_pending(&forms, "form-b").is_ok());
    }

    /// The entry must be self-rendering: non-empty `content` naming both the
    /// question that was asked (from the captured spec) and the answer that
    /// was given (from `values`) — no pending-form record, no join, no
    /// refetch required to make sense of it.
    #[test]
    fn form_answer_entry_shape() {
        let spec = json!({
            "form_id": "form-abc",
            "spec": {
                "title": "Quick check",
                "intro": null,
                "fields": [{ "id": "q1", "kind": "text", "label": "All good?", "required": false }],
            },
            "mode": "async",
        });
        let values = [("q1".to_string(), json!("yes"))].into_iter().collect();
        let entry = build_form_answer_entry("agent-1", "form-abc", &spec, values);
        assert_eq!(entry.event_type, FORM_ANSWER);
        let meta = entry.metadata.unwrap();
        assert_eq!(meta["form_id"], json!("form-abc"));
        assert_eq!(meta["values"]["q1"], json!("yes"));
        // Spec must be snapshotted onto the entry itself — this is what lets
        // the UI render the answered form as a disabled form rather than a
        // plain-text summary, even after the live pending-form record is
        // long gone.
        assert_eq!(meta["spec"]["title"], json!("Quick check"));
        assert_eq!(meta["spec"]["fields"][0]["id"], json!("q1"));
        assert!(!entry.hidden_from_user);
        assert!(!entry.content.is_empty(), "content must be self-rendering, not a ghost entry");
        assert!(entry.content.contains("All good?"), "must include the question: {}", entry.content);
        assert!(entry.content.contains("yes"), "must include the answer: {}", entry.content);
    }

    #[test]
    fn form_dismissed_entry_shape() {
        let entry = build_form_dismissed_entry("agent-1", "form-xyz");
        assert_eq!(entry.event_type, FORM_DISMISSED);
        let meta = entry.metadata.unwrap();
        assert_eq!(meta["form_id"], json!("form-xyz"));
        assert!(!entry.hidden_from_user);
        assert!(entry.content.is_empty());
    }

    /// Mirrors the real handler's ordering exactly: capture the pending
    /// record's spec into a plain local BEFORE clearing it, then build the
    /// entry from that captured value — never from a fresh snapshot read —
    /// after the clear. Proves the entry doesn't (and structurally can't)
    /// depend on the pending-form record still existing: by the time
    /// `build_form_answer_entry` runs, `snapshots.get()` would already show
    /// it gone.
    #[tokio::test]
    async fn answer_appends_transcript_and_clears_snapshot() {
        let dir = TempDir::new().unwrap();
        let root = DataRoot::new(dir.path());
        let transcripts = TranscriptStore::new(root.clone());
        let snapshots = SnapshotStore::load(root).await.unwrap();

        let spec = json!({
            "form_id": "form-1",
            "spec": {
                "title": "Quick check",
                "intro": null,
                "fields": [{ "id": "q1", "kind": "text", "label": "All good?", "required": false }],
            },
            "mode": "async",
        });
        snapshots
            .set_pending_form("agent-1", None, "form-1".to_string(), spec.clone())
            .await
            .unwrap();

        // Capture up front, exactly like the route does.
        let captured_spec = spec.clone();
        let values = [("q1".to_string(), json!("yes"))].into_iter().collect();

        // Clear FIRST — the entry is then built from `captured_spec` alone,
        // never from a live lookup, proving the ordering can't matter.
        snapshots.clear_pending_form("agent-1", "form-1").await.unwrap();
        assert!(
            snapshots.get().await.agents["agent-1"].pending_forms.is_empty(),
            "pending record must already be gone at this point"
        );

        let entry = build_form_answer_entry("agent-1", "form-1", &captured_spec, values);
        transcripts.append("agent-1", &entry).await.unwrap();

        let entries = transcripts.read_all("agent-1").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, FORM_ANSWER);
        let meta = entries[0].metadata.as_ref().unwrap();
        assert_eq!(meta["form_id"], json!("form-1"));
        assert!(
            entries[0].content.contains("All good?") && entries[0].content.contains("yes"),
            "content must still be fully self-rendering: {}",
            entries[0].content
        );

        let snap = snapshots.get().await;
        assert!(snap.agents["agent-1"].pending_forms.is_empty());
    }

    #[tokio::test]
    async fn dismiss_appends_transcript_and_clears_snapshot_without_queue() {
        let dir = TempDir::new().unwrap();
        let root = DataRoot::new(dir.path());
        let transcripts = TranscriptStore::new(root.clone());
        let snapshots = SnapshotStore::load(root).await.unwrap();

        snapshots
            .set_pending_form("agent-1", None, "form-2".to_string(), json!({}))
            .await
            .unwrap();

        let entry = build_form_dismissed_entry("agent-1", "form-2");
        transcripts.append("agent-1", &entry).await.unwrap();
        snapshots.clear_pending_form("agent-1", "form-2").await.unwrap();

        let entries = transcripts.read_all("agent-1").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, FORM_DISMISSED);
        let meta = entries[0].metadata.as_ref().unwrap();
        assert_eq!(meta["form_id"], json!("form-2"));

        let snap = snapshots.get().await;
        assert!(snap.agents["agent-1"].pending_forms.is_empty());
    }

    /// Answering a form on one thread must not clear a sibling pending form on
    /// a different thread of the same agent — the regression this whole change
    /// is guarding against (async forms outlive their run, so two threads can
    /// each have one pending at once).
    #[tokio::test]
    async fn answering_one_thread_form_does_not_clear_sibling_thread_form() {
        let dir = TempDir::new().unwrap();
        let root = DataRoot::new(dir.path());
        let snapshots = SnapshotStore::load(root).await.unwrap();

        snapshots
            .set_pending_form("agent-1", None, "form-default".to_string(), json!({}))
            .await
            .unwrap();
        snapshots
            .set_pending_form(
                "agent-1",
                Some("thread-b".to_string()),
                "form-b".to_string(),
                json!({}),
            )
            .await
            .unwrap();

        snapshots.clear_pending_form("agent-1", "form-default").await.unwrap();

        let snap = snapshots.get().await;
        let remaining = &snap.agents["agent-1"].pending_forms;
        assert_eq!(remaining.len(), 1, "sibling thread's pending form must survive");
        assert_eq!(remaining[0].form_id, "form-b");
    }

    /// `check_pending_form_id_for_key` uses a project-scope key (`project_{id}`)
    /// and must find the pending form there — not in the agent-keyed slot.
    #[tokio::test]
    async fn project_scope_key_isolates_from_agent_key() {
        let dir = TempDir::new().unwrap();
        let root = DataRoot::new(dir.path());
        let snapshots = SnapshotStore::load(root.clone()).await.unwrap();

        let scope_key = "project_proj-1";

        // Set pending form under the project scope key.
        snapshots
            .set_pending_form(scope_key, None, "form-p1".to_string(), json!({}))
            .await
            .unwrap();

        // Lookup succeeds with the correct project scope key.
        let snap = snapshots.get().await;
        let found = snap
            .agents
            .get(scope_key)
            .and_then(|s| s.pending_forms.first())
            .map(|f| f.form_id.as_str());
        assert_eq!(found, Some("form-p1"));

        // Agent's personal slot is untouched.
        assert!(
            snap.agents.get("agent-1").map(|a| a.pending_forms.is_empty()).unwrap_or(true),
            "agent personal slot must not carry project pending form"
        );
    }

    /// Project-scoped answer route appends to the project transcript and clears
    /// the project snapshot key — not the agent's personal ones.
    #[tokio::test]
    async fn project_answer_appends_to_project_transcript_and_clears_project_snapshot() {
        let dir = TempDir::new().unwrap();
        let root = DataRoot::new(dir.path());
        let transcripts = TranscriptStore::new(root.clone());
        let snapshots = SnapshotStore::load(root).await.unwrap();

        let scope_key = "project_proj-99";
        let spec = json!({
            "form_id": "form-proj",
            "spec": {
                "title": "Project check",
                "intro": null,
                "fields": [{ "id": "field1", "kind": "text", "label": "Status?", "required": false }],
            },
            "mode": "async",
        });

        snapshots
            .set_pending_form(scope_key, None, "form-proj".to_string(), spec.clone())
            .await
            .unwrap();

        let values = [("field1".to_string(), json!("answer"))].into_iter().collect();
        let entry = build_form_answer_entry(scope_key, "form-proj", &spec, values);
        transcripts.append(scope_key, &entry).await.unwrap();
        snapshots.clear_pending_form(scope_key, "form-proj").await.unwrap();

        // Project transcript has the form_answer entry.
        let entries = transcripts.read_all(scope_key).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, FORM_ANSWER);
        assert_eq!(entries[0].metadata.as_ref().unwrap()["form_id"], json!("form-proj"));
        assert!(entries[0].content.contains("Status?") && entries[0].content.contains("answer"));

        // Agent's personal transcript is empty.
        let agent_entries = transcripts.read_all("agent-1").await.unwrap();
        assert!(agent_entries.is_empty());

        // Project snapshot cleared; agent slot still absent/empty.
        let snap = snapshots.get().await;
        assert!(snap.agents[scope_key].pending_forms.is_empty());
        assert!(
            snap.agents.get("agent-1").map(|a| a.pending_forms.is_empty()).unwrap_or(true)
        );
    }
}

/// End-to-end coverage of `async_form_answer` itself (not just its component
/// helpers): builds a real `AppState` and calls the handler function
/// directly, asserting both the transcript write AND the queued follow-up
/// turn land on the thread the form was posted on. A `RecordingRunner`
/// substituted in place of the real CLI/native runners lets the test observe
/// the `thread_id` that actually reaches dispatch (`AgentRunRequest::thread_id`)
/// without needing a live agent process — the queue manager, instance
/// registry, and persistence layer underneath are all real.
#[cfg(test)]
mod handler_thread_routing_tests {
    use super::*;
    use ao_engine::agent_runner::{AgentRunRequest, AgentRunner, RunnerDispatcher};
    use ao_engine::queue_manager::QueueManagerRegistry;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::agent::{
        AgentProfile, AgentRunnerMode, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };
    use ao_protocol::event::RunEndReason;
    use async_trait::async_trait;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::Mutex as AsyncMutex;

    /// `AppError` doesn't implement `Debug` (it wraps `AoError`, which does),
    /// so route-handler tests unwrap through this rather than `.expect()`
    /// directly on the handler's `Result` — same idiom `agents.rs`'s route
    /// tests use.
    fn unwrap_ok<T>(r: Result<T, AppError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {:?}", e.0),
        }
    }

    fn base_profile(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.into(),
            name: id.into(),
            description: "".into(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "claude".into(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: ao_protocol::agent::AgentRunnerMode::Cli,
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
            max_turns: None,
        }
    }

    /// Records the `thread_id` of every dispatched run, in dispatch order,
    /// instead of actually running an agent — lets the test observe exactly
    /// what `QueuedMessage::thread_id` the handler produced, all the way
    /// through the real queue manager, without spawning a live process.
    struct RecordingRunner {
        dispatched: Arc<AsyncMutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl AgentRunner for RecordingRunner {
        fn mode(&self) -> AgentRunnerMode {
            AgentRunnerMode::Cli
        }

        async fn run(
            &self,
            req: AgentRunRequest,
        ) -> Result<ao_engine::agent_runner::RunComplete, AoError> {
            self.dispatched.lock().await.push(req.thread_id.clone());
            let run_id = req
                .pre_registered_run_id
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let rc = ao_engine::agent_runner::RunComplete {
                run_id,
                output_text: String::new(),
                workflow_followups: vec![],
                end_reason: RunEndReason::Completed,
            };
            let _ = req.run_complete_tx.send(rc.clone()).await;
            Ok(rc)
        }
    }

    /// Polls `dispatched` until it has at least `expected` entries or the
    /// timeout elapses. Dispatch happens on a spawned queue-actor task, so
    /// the handler call returning is not itself proof the message has been
    /// picked up yet.
    async fn wait_for_len(
        dispatched: &Arc<AsyncMutex<Vec<Option<String>>>>,
        expected: usize,
    ) -> Vec<Option<String>> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = dispatched.lock().await.clone();
                if snapshot.len() >= expected {
                    return snapshot;
                }
                drop(snapshot);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dispatched runs never reached the expected count")
    }

    /// Builds a real `AppState` (same `AppState::new_with_mock` +
    /// env-locked temp data root idiom used by other route handler tests in
    /// this crate), then swaps in a `QueueManagerRegistry` wired to a
    /// `RecordingRunner` so queued turns are observable instead of attempting
    /// a live agent process.
    async fn setup_state_with_recording_runner()
    -> (Arc<AppState>, Arc<AsyncMutex<Vec<Option<String>>>>, TempDir) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let mut state = {
            let _guard = crate::routes::env_lock::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock).await.expect("AppState init")
        };

        let dispatched = Arc::new(AsyncMutex::new(Vec::new()));
        let runner: Arc<dyn AgentRunner> = Arc::new(RecordingRunner {
            dispatched: Arc::clone(&dispatched),
        });
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(
            Arc::clone(&runner),
            Arc::clone(&runner),
        ));
        state.queue_managers = Arc::new(QueueManagerRegistry::new(
            dispatcher,
            Arc::clone(&state.instance_registry),
            Arc::clone(&state.event_bus),
            Arc::clone(&state.persistence),
        ));

        (Arc::new(state), dispatched, tmp)
    }

    /// Regression test for the bug this change fixes: a form posted with
    /// `ctx.thread_id = Some("T")` (here, a real non-default thread) must,
    /// once answered, land BOTH its `form_answer` transcript entry AND its
    /// queued follow-up turn on that same thread — not the agent's default.
    #[tokio::test]
    async fn answer_routes_transcript_and_queued_turn_to_originating_thread() {
        let (state, dispatched, _tmp) = setup_state_with_recording_runner().await;

        let agent_id = "agent-thread-route";
        state.persistence.agents.create(&base_profile(agent_id)).await.unwrap();

        let fresh = state
            .persistence
            .threads
            .build_fresh_thread(agent_id, Some("Spike".to_string()));
        let fresh = state.persistence.threads.create(fresh).await.unwrap();

        state
            .persistence
            .snapshots
            .set_pending_form(agent_id, Some(fresh.id.clone()), "form-t".to_string(), json!({}))
            .await
            .unwrap();

        let req = AsyncFormAnswerRequest {
            values: [("q1".to_string(), json!("yes"))].into_iter().collect(),
        };
        let ack = unwrap_ok(
            async_form_answer(
                State(Arc::clone(&state)),
                Path((agent_id.to_string(), "form-t".to_string())),
                Json(req),
            )
            .await,
        )
        .0;
        assert_eq!(ack.status, "queued");

        // 1. The form_answer transcript entry landed in thread T's own file —
        // not the agent's default-thread file.
        let thread_path = std::path::PathBuf::from(&fresh.transcript_path);
        let thread_entries = state.persistence.transcripts.read_all_at(&thread_path).await.unwrap();
        assert_eq!(thread_entries.len(), 1);
        assert_eq!(thread_entries[0].event_type, FORM_ANSWER);
        assert!(
            state.persistence.transcripts.read_all(agent_id).await.unwrap().is_empty(),
            "agent's default-thread file must stay empty"
        );

        // 2. The queued follow-up turn reached the real queue manager and
        // dispatcher carrying thread T's id, not `None`.
        let seen = wait_for_len(&dispatched, 1).await;
        assert_eq!(seen, vec![Some(fresh.id.clone())]);
    }

    /// A pending form with `thread_id: None` (posted from the agent's
    /// default thread) must keep answering to the default thread — no
    /// behavior change for the common case.
    #[tokio::test]
    async fn answer_on_default_thread_keeps_targeting_default_thread() {
        let (state, dispatched, _tmp) = setup_state_with_recording_runner().await;

        let agent_id = "agent-thread-default";
        state.persistence.agents.create(&base_profile(agent_id)).await.unwrap();

        state
            .persistence
            .snapshots
            .set_pending_form(agent_id, None, "form-default".to_string(), json!({}))
            .await
            .unwrap();

        let req = AsyncFormAnswerRequest { values: HashMap::new() };
        let _ = unwrap_ok(
            async_form_answer(
                State(Arc::clone(&state)),
                Path((agent_id.to_string(), "form-default".to_string())),
                Json(req),
            )
            .await,
        );

        let entries = state.persistence.transcripts.read_all(agent_id).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, FORM_ANSWER);

        let seen = wait_for_len(&dispatched, 1).await;
        assert_eq!(seen, vec![None]);
    }

    /// The route must emit a `FormResolved` event on the same EventBus/SSE
    /// transport the CREATE-path `FormPosted` event uses (see
    /// `ao_engine_tools_core::form_events::wire_posted_async_form`), scoped to
    /// the form's own thread — so the UI can clear its pending-form indicator
    /// live, without polling or refetching agent state.
    #[tokio::test]
    async fn answer_emits_form_resolved_event_on_the_originating_thread() {
        let (state, _dispatched, _tmp) = setup_state_with_recording_runner().await;

        let agent_id = "agent-resolves-event";
        state.persistence.agents.create(&base_profile(agent_id)).await.unwrap();

        let fresh = state
            .persistence
            .threads
            .build_fresh_thread(agent_id, Some("Spike".to_string()));
        let fresh = state.persistence.threads.create(fresh).await.unwrap();

        state
            .persistence
            .snapshots
            .set_pending_form(agent_id, Some(fresh.id.clone()), "form-evt".to_string(), json!({}))
            .await
            .unwrap();

        let mut rx = state.event_bus.subscribe();

        let req = AsyncFormAnswerRequest {
            values: [("q1".to_string(), json!("yes"))].into_iter().collect(),
        };
        let _ = unwrap_ok(
            async_form_answer(
                State(Arc::clone(&state)),
                Path((agent_id.to_string(), "form-evt".to_string())),
                Json(req),
            )
            .await,
        );

        let event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let evt = rx.recv().await.expect("event bus closed unexpectedly");
                if matches!(evt.payload, AgentEventPayload::FormResolved { .. }) {
                    return evt;
                }
            }
        })
        .await
        .expect("FormResolved event never arrived");

        match event.payload {
            AgentEventPayload::FormResolved { form_id } => assert_eq!(form_id, "form-evt"),
            other => panic!("expected FormResolved, got {other:?}"),
        }
        assert_eq!(
            event.thread_id.as_deref(),
            Some(fresh.id.as_str()),
            "event must be scoped to the form's own thread, not the agent's default"
        );
    }
}
