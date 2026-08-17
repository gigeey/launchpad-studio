use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use ao_engine::mcp_session::ParentSessionInfo;
use ao_engine::AppState;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterSessionRequest {
    pub session_id: String,
    pub agent_id: String,
    pub spawn_cwd: String,
    pub parent_session_id: Option<String>,
    pub project_id: Option<String>,
}

/// POST /sessions — register a new per-invocation MCP session before spawning a CLI agent.
/// Returns 201 on success, 409 on duplicate session_id.
pub async fn register_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RegisterSessionRequest>,
) -> Response {
    let cwd = PathBuf::from(&body.spawn_cwd);

    // If parentSessionId provided, resolve it to a full ParentSessionInfo.
    let parent_info = if let Some(ref parent_sid) = body.parent_session_id {
        match state.mcp_sessions.get_by_session_id(parent_sid) {
            Some(parent) => {
                let parent_cwd = parent.cwd.read().unwrap().clone();
                Some(ParentSessionInfo {
                    session_id: parent_sid.clone(),
                    agent_id: parent.agent_id.clone(),
                    current_cwd: parent_cwd,
                })
            }
            None => {
                let resp = axum::Json(serde_json::json!({
                    "error": format!("parent session not found: {parent_sid}")
                }));
                return (StatusCode::NOT_FOUND, resp).into_response();
            }
        }
    } else {
        None
    };

    match state.mcp_sessions.register_session_with_chains(
        body.session_id.clone(),
        body.agent_id.clone(),
        cwd,
        parent_info,
        vec![],
        vec![],
        body.project_id.clone(),
        None,
    ) {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(()) => {
            let resp = axum::Json(serde_json::json!({
                "error": format!("session already exists: {}", body.session_id)
            }));
            (StatusCode::CONFLICT, resp).into_response()
        }
    }
}

/// DELETE /sessions/:session_id — deregister a session on agent exit.
/// Returns 204 on success, 404 if session_id is unknown.
pub async fn deregister_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Response {
    if state.mcp_sessions.get_by_session_id(&session_id).is_none() {
        let resp = axum::Json(serde_json::json!({
            "error": format!("session not found: {session_id}")
        }));
        return (StatusCode::NOT_FOUND, resp).into_response();
    }
    state.mcp_sessions.remove(&session_id);
    StatusCode::NO_CONTENT.into_response()
}
