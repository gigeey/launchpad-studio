use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use ao_protocol::error::AoError;

/// Fixed, verbatim shipping copy for the 409 response a workspace-registry
/// mutation route returns when this process's active data root is pinned
/// via `LAUNCHPAD_STUDIO_DATA_DIR` — see
/// `AoError::WorkspaceMutationBlockedByPinnedDataRoot` and
/// `ao-server/src/routes/workspaces.rs`'s mutation-route guard. Pulled out
/// to a constant, mirroring that module's own `NOT_ADOPTABLE_MESSAGE`, so
/// the response body and any test asserting against it can't drift apart on
/// wording. The raw env var name/value are reported alongside this sentence
/// as separate `env_var`/`value` fields, never folded into it.
pub const WORKSPACE_MUTATION_BLOCKED_MESSAGE: &str = "Launchpad Studio is running with a pinned \
    data directory, so it can't change the shared workspace list. Nothing was modified.";

/// Wrapper around AoError that implements IntoResponse for Axum handlers.
pub struct AppError(pub AoError);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Tasklist-already-active needs a structured body so the frontend can
        // distinguish "team already has an active tasklist" from other 409s.
        if let AoError::TasklistAlreadyActive {
            team_id,
            tasklist_id,
        } = &self.0
        {
            let body = axum::Json(json!({
                "error": self.0.to_string(),
                "code": "tasklist_already_active",
                "team_id": team_id,
                "active_tasklist_id": tasklist_id,
            }));
            return (StatusCode::CONFLICT, body).into_response();
        }

        // Same shape as `TasklistAlreadyActive` above, but the `error` field
        // is the fixed `WORKSPACE_MUTATION_BLOCKED_MESSAGE` sentence rather
        // than `self.0.to_string()` — the raw env var name/value belong in
        // their own structured fields, not composed into the user-facing
        // message (see that constant's doc comment).
        if let AoError::WorkspaceMutationBlockedByPinnedDataRoot { env_var, value } = &self.0 {
            let body = axum::Json(json!({
                "error": WORKSPACE_MUTATION_BLOCKED_MESSAGE,
                "code": "workspace_mutation_blocked_by_pinned_data_root",
                "env_var": env_var,
                "value": value,
            }));
            return (StatusCode::CONFLICT, body).into_response();
        }

        // `GET /providers/{name}/models` doubles as the frontend's API-key
        // validity check, and it must be able to tell an auth rejection
        // apart from a network hiccup or an unparseable upstream response —
        // only the former is a soft "this key looks wrong" warning that must
        // not block the user from saving. A `code` field alongside `error`
        // carries that distinction explicitly rather than relying on the
        // two non-auth classes sharing an HTTP status.
        if let Some((status, code)) = provider_discovery_status_and_code(&self.0) {
            let body = axum::Json(json!({ "error": self.0.to_string(), "code": code }));
            return (status, body).into_response();
        }

        let (status, message) = match &self.0 {
            AoError::AgentNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::ThreadNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::AgentAlreadyExists(msg) => (StatusCode::CONFLICT, msg.clone()),
            AoError::AttachmentNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::ValidationError(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AoError::WorkflowNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::TaskNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::SkillNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::RuleNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::InstructionNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::TasklistNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::InvalidTasklistTransition(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AoError::ProjectNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::ProjectAlreadyExists(msg) => (StatusCode::CONFLICT, msg.clone()),
            AoError::AssignmentNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            AoError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            AoError::MemoryNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::ArtifactNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::ArtifactGroupNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::DelegationNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AoError::WorkspaceNotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            // Same status class as `create_workspace`'s path-validation
            // failures (`ValidationError` -> 400) — the client asked to
            // activate a specific workspace whose registered path turned
            // out to be unusable, which is the same kind of "fix the
            // target and retry" problem, not a transient server fault.
            AoError::WorkspaceActivationTargetUnopenable { .. } => {
                (StatusCode::BAD_REQUEST, self.0.to_string())
            }
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                self.0.to_string(),
            ),
        };

        let body = axum::Json(json!({ "error": message }));
        (status, body).into_response()
    }
}

/// Maps the three `GET /providers/{name}/models` upstream-call outcomes to
/// their HTTP status and `code` string. `None` for every other `AoError`
/// variant, which falls through to the generic `{status, message}` table.
fn provider_discovery_status_and_code(err: &AoError) -> Option<(StatusCode, &'static str)> {
    match err {
        AoError::ProviderAuthFailure(_) => Some((StatusCode::UNAUTHORIZED, "auth_failure")),
        AoError::ProviderNetworkFailure(_) => Some((StatusCode::BAD_GATEWAY, "network_failure")),
        AoError::ProviderMalformedResponse(_) => Some((StatusCode::BAD_GATEWAY, "malformed_response")),
        _ => None,
    }
}

impl From<AoError> for AppError {
    fn from(err: AoError) -> Self {
        AppError(err)
    }
}
