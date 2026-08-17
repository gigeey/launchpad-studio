use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use ao_engine::AppState;
use ao_protocol::bookmark::BookmarkEntry;
use ao_protocol::error::AoError;
use ao_protocol::transcript::TranscriptRole;

use crate::error::AppError;

#[derive(serde::Deserialize)]
pub struct AddBookmarkRequest {
    pub message_ts: String,
    pub message_content: String,
    pub message_role: TranscriptRole,
}

/// GET /agents/{agent_id}/bookmarks — list all bookmarks for an agent.
pub async fn list_agent_bookmarks(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<BookmarkEntry>>, AppError> {
    // Validate agent exists
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let entries = state.persistence.bookmarks.list(&agent_id).await?;
    Ok(Json(entries))
}

/// POST /agents/{agent_id}/bookmarks — add a new bookmark for an agent.
pub async fn add_agent_bookmark(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<AddBookmarkRequest>,
) -> Result<Json<BookmarkEntry>, AppError> {
    // Validate agent exists
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    // Check if bookmark already exists for this message_ts
    if state
        .persistence
        .bookmarks
        .exists(&agent_id, &req.message_ts)
        .await?
    {
        // Return the existing bookmark instead of creating a duplicate
        let entries = state.persistence.bookmarks.list(&agent_id).await?;
        let existing = entries
            .into_iter()
            .find(|e| e.message_ts == req.message_ts)
            .expect("exists() returned true but entry not found");
        return Ok(Json(existing));
    }

    let entry = state
        .persistence
        .bookmarks
        .add(&agent_id, &req.message_ts, &req.message_content, req.message_role)
        .await?;
    Ok(Json(entry))
}

/// DELETE /agents/{agent_id}/bookmarks/{bookmark_id} — delete a specific agent bookmark.
pub async fn delete_agent_bookmark(
    State(state): State<Arc<AppState>>,
    Path((agent_id, bookmark_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    // Validate agent exists
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let deleted = state
        .persistence
        .bookmarks
        .delete(&agent_id, &bookmark_id)
        .await?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError(AoError::ThreadNotFound(format!(
            "Bookmark {} not found",
            bookmark_id
        ))))
    }
}
