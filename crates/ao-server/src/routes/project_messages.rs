use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use ao_engine::project_queue_manager::ProjectMessage;
use ao_engine::AppState;
use ao_protocol::attachment::Attachment;
use ao_protocol::error::AoError;
use ao_protocol::message::{MessageAck, QueuedMessage};
use ao_protocol::transcript::{PaginationCursor, TranscriptEntry, TranscriptRole};

use crate::error::AppError;

use super::messages::{GetMessagesQuery, SendMessageRequest};

/// Paginated project messages response — extends the base response with the
/// project's pending async form state so the frontend can rehydrate on reload.
#[derive(Debug, serde::Serialize)]
pub struct ProjectMessagesResponse {
    pub messages: Vec<TranscriptEntry>,
    pub cursor: Option<PaginationCursor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_form_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_form_spec: Option<serde_json::Value>,
}

/// Shared core: append a user transcript entry and submit the queued message to
/// the project agent. Attachment lookup and metadata enrichment stay in each
/// call-site; this function only handles the write + dispatch.
async fn append_and_enqueue(
    state: &Arc<AppState>,
    project_id: &str,
    message_id: &str,
    content: &str,
    metadata: std::collections::HashMap<String, serde_json::Value>,
    attachments: Vec<Attachment>,
    focus_path: Option<String>,
) -> Result<(), AppError> {
    let transcript_key = format!("project_{}", project_id);
    let entry = TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("user".to_string()),
        content: content.to_string(),
        event_type: "message".to_string(),
        metadata: Some(metadata),
        hidden_from_user: false,
    };
    state.persistence.transcripts.append(&transcript_key, &entry).await?;

    let queued = QueuedMessage {
        message_id: message_id.to_string(),
        content: content.to_string(),
        queued_at: Utc::now(),
        attachments,
        source: None,
        focus_path,
        thread_id: None,
    };
    state
        .project_queue_managers
        .submit_message(project_id, ProjectMessage::User(queued))
        .await?;
    Ok(())
}

/// Persist a system announcement recording that a project was created.
/// Written as the transcript's opening entry, ahead of the interview kickoff
/// turn, so the UI can render project creation as a distinct system event
/// rather than the agent's first "user" turn. Called best-effort — callers
/// must not propagate the error.
pub(crate) async fn record_project_created(
    state: &Arc<AppState>,
    project_id: &str,
    project_name: &str,
    goal: &str,
) -> Result<(), AppError> {
    let transcript_key = format!("project_{}", project_id);
    let entry = TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("system".to_string()),
        content: format!("Project \"{project_name}\" created — goal: {goal}"),
        event_type: "project_created".to_string(),
        metadata: None,
        hidden_from_user: false,
    };
    state.persistence.transcripts.append(&transcript_key, &entry).await?;
    Ok(())
}

/// Kick off the opening interview turn for a newly-created project.
/// Appends the goal as the first user message and submits it to the project
/// agent queue. Called best-effort — callers must not propagate the error.
pub(crate) async fn kickoff_project_message(
    state: &Arc<AppState>,
    project_id: &str,
    content: &str,
) -> Result<(), AppError> {
    let message_id = Uuid::new_v4().to_string();
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
        "message_id".to_string(),
        serde_json::Value::String(message_id.clone()),
    );
    append_and_enqueue(state, project_id, &message_id, content, metadata, vec![], None).await
}

/// POST /projects/{id}/messages — send a user message to the project's main agent.
pub async fn send_project_message(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<MessageAck>, AppError> {
    let project = state
        .persistence
        .projects
        .get(&project_id)
        .await?
        .ok_or_else(|| AoError::ProjectNotFound(project_id.clone()))?;

    // Verify the project's agent exists.
    state
        .persistence
        .agents
        .get(&project.agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(project.agent_id.clone()))?;

    let message_id = Uuid::new_v4().to_string();
    let attachment_ids = req.attachment_ids.unwrap_or_default();
    let mut attachments = Vec::with_capacity(attachment_ids.len());
    let asset_key = format!("project_{}", project_id);

    for aid in &attachment_ids {
        let attachment = state
            .persistence
            .assets
            .get_attachment(&asset_key, aid)
            .await
            .map_err(|_| AoError::ValidationError(format!("Attachment not found: {aid}")))?;
        attachments.push(attachment);
    }
    for aid in &attachment_ids {
        state
            .persistence
            .assets
            .mark_committed(&asset_key, aid, &message_id)
            .await?;
    }

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

    append_and_enqueue(&state, &project_id, &message_id, &req.content, metadata, attachments, req.focus_path.clone()).await?;

    Ok(Json(MessageAck {
        message_id,
        status: "queued".to_string(),
    }))
}

/// GET /projects/{id}/messages — paginated project conversation history.
pub async fn get_project_messages(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
    Query(query): Query<GetMessagesQuery>,
) -> Result<Json<ProjectMessagesResponse>, AppError> {
    state
        .persistence
        .projects
        .get(&project_id)
        .await?
        .ok_or_else(|| AoError::ProjectNotFound(project_id.clone()))?;

    let transcript_key = format!("project_{}", project_id);

    // Read pending form state from snapshot (in-memory; no disk I/O). Projects
    // have no thread concept, so the runner always records project-scoped
    // forms with `thread_id: None` — at most one entry ever lands here.
    let snapshot = state.persistence.snapshots.get().await;
    let snap_entry = snapshot.agents.get(&transcript_key);
    let pending_form = snap_entry.and_then(|e| e.pending_forms.iter().find(|f| f.thread_id.is_none()));
    let pending_form_id = pending_form.map(|f| f.form_id.clone());
    let pending_form_spec = pending_form.map(|f| f.spec.clone());

    if query.after.is_some() || query.before.is_some() {
        let mut entries = state
            .persistence
            .transcripts
            .read_all(&transcript_key)
            .await?;

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
        return Ok(Json(ProjectMessagesResponse {
            messages: entries,
            cursor: None,
            pending_form_id,
            pending_form_spec,
        }));
    }

    if let (Some(offset), Some(ref message_id), Some(ref timestamp_str)) = (
        query.cursor_offset,
        &query.cursor_message_id,
        &query.cursor_timestamp,
    ) {
        let timestamp = timestamp_str
            .parse::<DateTime<Utc>>()
            .map_err(|e| AoError::Json(format!("Invalid cursor_timestamp: {e}")))?;

        let cursor = PaginationCursor {
            byte_offset: offset,
            last_message_id: message_id.clone(),
            timestamp,
            // Projects have no branch-thread concept; cursors always address
            // the project's own transcript.
            phase: Default::default(),
        };

        let n = query.last.unwrap_or(50);
        let result = state
            .persistence
            .transcripts
            .read_before_cursor(&transcript_key, &cursor, n)
            .await?;

        return Ok(Json(ProjectMessagesResponse {
            messages: result.entries,
            cursor: result.cursor,
            pending_form_id,
            pending_form_spec,
        }));
    }

    // `read_tail` (not `read_recent`) so this default page carries a real
    // continuation cursor — mirrors the personal/team chat route
    // (`routes/messages.rs`). `read_recent` only ever returns entries with no
    // cursor, which previously made this branch report `cursor: None`
    // unconditionally regardless of how much earlier history existed,
    // permanently hiding the project's opening messages (including its
    // original goal/kickoff turn) behind a "load more" that could never
    // appear once the transcript passed one page.
    let n = query.last.unwrap_or(50);
    let result = state
        .persistence
        .transcripts
        .read_tail(&transcript_key, n)
        .await?;

    Ok(Json(ProjectMessagesResponse {
        messages: result.entries,
        cursor: result.cursor,
        pending_form_id,
        pending_form_spec,
    }))
}

/// POST /projects/{id}/cancel — cancel the current in-flight run for this project.
pub async fn cancel_project_run(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
) -> Response {
    state
        .project_queue_managers
        .cancel_project(&project_id)
        .await;
    StatusCode::NO_CONTENT.into_response()
}
