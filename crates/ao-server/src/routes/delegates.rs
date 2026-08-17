use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;

use ao_engine::AppState;
use ao_engine_tools_core::background_agents::{cancel_delegation, BackgroundAgentId, CancelOutcome};
use ao_protocol::error::AoError;

use crate::error::AppError;

/// One live delegation as reported by `GET /agents/{agent_id}/delegates`.
#[derive(Debug, Serialize)]
pub struct DelegateEntry {
    pub id: String,
    pub subagent_name: String,
    pub spawned_at: DateTime<Utc>,
}

/// GET /agents/{agent_id}/delegates — list delegations currently in flight
/// for `agent_id`.
///
/// An agent can have more than one live MCP session at a time — concurrent
/// spawns of the same agent profile each get their own session, each with
/// its own `BackgroundAgentRegistry` — so this aggregates
/// [`BackgroundAgentRegistry::active`](ao_engine_tools_core::background_agents::BackgroundAgentRegistry::active)
/// across every session registered for `agent_id` rather than assuming a
/// single one. Mirrors the source the `/system/stream` connect-time replay
/// reads from (see `stream::build_system_replay_events`).
pub async fn list_agent_delegates(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Json<Vec<DelegateEntry>> {
    let mut entries = Vec::new();
    for session in state.mcp_sessions.list_by_agent_id(&agent_id) {
        for snapshot in session.background_agents.active().await {
            entries.push(DelegateEntry {
                id: snapshot.id.to_string(),
                subagent_name: snapshot.subagent_name,
                spawned_at: snapshot.spawned_at,
            });
        }
    }
    Json(entries)
}

/// Response body for `POST /delegates/{delegation_id}/cancel`.
#[derive(Debug, Serialize)]
pub struct CancelDelegateResponse {
    pub status: &'static str,
    pub id: String,
}

/// POST /delegates/{delegation_id}/cancel — cancel one live delegation.
///
/// Delegation ids are globally unique (a UUID minted at spawn time), so this
/// scans every registered MCP session's `BackgroundAgentRegistry` for the
/// one holding `delegation_id` and cancels it there via
/// [`cancel_delegation`] — the same primitive the model-facing `DelegateStop`
/// tool uses, so both surfaces agree on not-found / already-cancelled /
/// cancelled semantics. Idempotent: cancelling an already-cancelled
/// delegation returns `200 OK` with `status: "already_cancelled"`, not an
/// error.
///
/// Cancels exactly one delegation and nothing more. There is deliberately no
/// kill-all endpoint here — a caller wanting to cancel every delegation for
/// an agent enumerates `GET /agents/{agent_id}/delegates` and calls this
/// route once per id.
pub async fn cancel_delegate(
    State(state): State<Arc<AppState>>,
    Path(delegation_id): Path<String>,
) -> Result<(StatusCode, Json<CancelDelegateResponse>), AppError> {
    let bg_id: BackgroundAgentId = delegation_id.parse().map_err(|e| {
        AppError(AoError::ValidationError(format!(
            "invalid delegation id '{delegation_id}': {e}"
        )))
    })?;

    for session in state.mcp_sessions.all_sessions() {
        match cancel_delegation(&session.background_agents, &bg_id).await {
            CancelOutcome::NotFound => continue,
            CancelOutcome::AlreadyCancelled => {
                return Ok((
                    StatusCode::OK,
                    Json(CancelDelegateResponse {
                        status: "already_cancelled",
                        id: bg_id.to_string(),
                    }),
                ));
            }
            CancelOutcome::Cancelled => {
                return Ok((
                    StatusCode::OK,
                    Json(CancelDelegateResponse {
                        status: "cancelled",
                        id: bg_id.to_string(),
                    }),
                ));
            }
        }
    }

    Err(AppError(AoError::DelegationNotFound(delegation_id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_engine_tools_core::background_agents::handle::{BackgroundAgentHandle, TaskFinalReport};
    use ao_process::mock::MockProcessSupervisor;

    fn unwrap_ok<T>(r: Result<T, AppError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {:?}", e.0),
        }
    }

    async fn setup_state() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = ao_persistence::PersistenceLayer::init();
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock)
                .await
                .expect("AppState init")
        };
        (Arc::new(state), tmp)
    }

    fn make_test_handle(name: &str, spawned_at: DateTime<Utc>) -> BackgroundAgentHandle {
        let (_tx, rx) = tokio::sync::broadcast::channel(1);
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let join = tokio::spawn(async move {
            cancel_clone.cancelled().await;
            Ok::<TaskFinalReport, AoError>(TaskFinalReport::cancelled())
        });
        BackgroundAgentHandle {
            id: BackgroundAgentId::new(),
            subagent_name: name.to_string(),
            spawned_at,
            cancel,
            events: rx,
            join,
        }
    }

    #[tokio::test]
    async fn list_returns_live_delegate_with_real_spawned_at() {
        let (state, _tmp) = setup_state().await;

        let session = state
            .mcp_sessions
            .register_session(
                "sess-list".to_string(),
                "agent-list".to_string(),
                std::path::PathBuf::from("/tmp"),
                None,
            )
            .expect("session registration should succeed");

        let real_spawned_at = Utc::now() - chrono::Duration::minutes(3);
        let handle = make_test_handle("Researcher", real_spawned_at);
        let delegation_id = handle.id.to_string();
        session.background_agents.insert(handle).await.unwrap();

        let Json(entries) = list_agent_delegates(
            State(Arc::clone(&state)),
            Path("agent-list".to_string()),
        )
        .await;

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, delegation_id);
        assert_eq!(entries[0].subagent_name, "Researcher");
        assert_eq!(entries[0].spawned_at, real_spawned_at);
    }

    #[tokio::test]
    async fn list_only_returns_delegates_for_the_requested_agent() {
        let (state, _tmp) = setup_state().await;

        let session_a = state
            .mcp_sessions
            .register_session(
                "sess-a".to_string(),
                "agent-a".to_string(),
                std::path::PathBuf::from("/tmp"),
                None,
            )
            .unwrap();
        let session_b = state
            .mcp_sessions
            .register_session(
                "sess-b".to_string(),
                "agent-b".to_string(),
                std::path::PathBuf::from("/tmp"),
                None,
            )
            .unwrap();

        session_a
            .background_agents
            .insert(make_test_handle("mine", Utc::now()))
            .await
            .unwrap();
        session_b
            .background_agents
            .insert(make_test_handle("not-mine", Utc::now()))
            .await
            .unwrap();

        let Json(entries) =
            list_agent_delegates(State(Arc::clone(&state)), Path("agent-a".to_string())).await;

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].subagent_name, "mine");
    }

    #[tokio::test]
    async fn list_is_empty_for_agent_with_no_sessions() {
        let (state, _tmp) = setup_state().await;

        let Json(entries) =
            list_agent_delegates(State(Arc::clone(&state)), Path("ghost-agent".to_string())).await;

        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn cancel_twice_is_idempotent() {
        let (state, _tmp) = setup_state().await;

        let session = state
            .mcp_sessions
            .register_session(
                "sess-cancel".to_string(),
                "agent-cancel".to_string(),
                std::path::PathBuf::from("/tmp"),
                None,
            )
            .unwrap();

        let handle = make_test_handle("Researcher", Utc::now());
        let delegation_id = handle.id.to_string();
        session.background_agents.insert(handle).await.unwrap();

        let (status1, Json(body1)) = unwrap_ok(
            cancel_delegate(State(Arc::clone(&state)), Path(delegation_id.clone())).await,
        );
        assert_eq!(status1, StatusCode::OK);
        assert_eq!(body1.status, "cancelled");
        assert_eq!(body1.id, delegation_id);

        let (status2, Json(body2)) = unwrap_ok(
            cancel_delegate(State(Arc::clone(&state)), Path(delegation_id.clone())).await,
        );
        assert_eq!(status2, StatusCode::OK);
        assert_eq!(body2.status, "already_cancelled");
        assert_eq!(body2.id, delegation_id);
    }

    #[tokio::test]
    async fn cancel_unknown_id_is_a_clean_not_found() {
        let (state, _tmp) = setup_state().await;

        let unknown_id = BackgroundAgentId::new().to_string();
        let err = cancel_delegate(State(Arc::clone(&state)), Path(unknown_id))
            .await
            .expect_err("unknown delegation id must fail, not panic");

        assert!(matches!(err.0, AoError::DelegationNotFound(_)));
    }

    #[tokio::test]
    async fn cancel_malformed_id_is_a_validation_error_not_a_panic() {
        let (state, _tmp) = setup_state().await;

        let err = cancel_delegate(State(Arc::clone(&state)), Path("not-a-uuid".to_string()))
            .await
            .expect_err("malformed delegation id must fail, not panic");

        assert!(matches!(err.0, AoError::ValidationError(_)));
    }

    #[tokio::test]
    async fn cancel_finds_delegation_in_second_session_when_first_has_none() {
        let (state, _tmp) = setup_state().await;

        let session_empty = state
            .mcp_sessions
            .register_session(
                "sess-empty".to_string(),
                "agent-multi".to_string(),
                std::path::PathBuf::from("/tmp"),
                None,
            )
            .unwrap();
        let session_with_delegate = state
            .mcp_sessions
            .register_session(
                "sess-with-delegate".to_string(),
                "agent-multi-2".to_string(),
                std::path::PathBuf::from("/tmp"),
                None,
            )
            .unwrap();
        let _ = session_empty; // no delegates inserted here

        let handle = make_test_handle("Researcher", Utc::now());
        let delegation_id = handle.id.to_string();
        session_with_delegate
            .background_agents
            .insert(handle)
            .await
            .unwrap();

        let (status, Json(body)) = unwrap_ok(
            cancel_delegate(State(Arc::clone(&state)), Path(delegation_id.clone())).await,
        );
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.status, "cancelled");
        assert_eq!(body.id, delegation_id);
    }
}
