use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use ao_engine::AppState;
use ao_protocol::error::AoError;
use ao_protocol::preferences::UserPreferences;

use crate::error::AppError;

/// GET /preferences — return current user preferences (or defaults if not set).
pub async fn get_preferences(
    State(state): State<Arc<AppState>>,
) -> Result<Json<UserPreferences>, AppError> {
    let prefs = state
        .persistence
        .preferences
        .get()
        .await?
        .unwrap_or_default();
    Ok(Json(prefs))
}

/// PUT /preferences — save user preferences.
pub async fn put_preferences(
    State(state): State<Arc<AppState>>,
    Json(prefs): Json<UserPreferences>,
) -> Result<Json<UserPreferences>, AppError> {
    state.persistence.preferences.save(&prefs).await?;
    Ok(Json(prefs))
}

#[derive(Serialize)]
pub struct PreferencesStatus {
    pub configured: bool,
}

/// GET /preferences/status — check if preferences are configured.
pub async fn get_preferences_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<PreferencesStatus>, AppError> {
    let prefs = state.persistence.preferences.get().await?;
    let configured = prefs
        .as_ref()
        .and_then(|p| p.full_name.as_deref())
        .map(|name| !name.is_empty())
        .unwrap_or(false);
    Ok(Json(PreferencesStatus { configured }))
}

/// Normalize an instruction-filename list:
/// - trim whitespace
/// - reject empty strings
/// - reject entries containing path separators (`/`, `\`) or `..`
/// - dedupe case-insensitively, keeping the first occurrence (preserves the
///   user's chosen casing for display).
fn normalize_instruction_filenames(input: Vec<String>) -> Result<Vec<String>, AppError> {
    let mut out: Vec<String> = Vec::with_capacity(input.len());
    for raw in input {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(AppError(AoError::ValidationError(
                "instruction filename must not be empty".to_string(),
            )));
        }
        if trimmed.contains('/') || trimmed.contains('\\') {
            return Err(AppError(AoError::ValidationError(format!(
                "instruction filename must not contain path separators: {trimmed}"
            ))));
        }
        if trimmed == "." || trimmed == ".." {
            return Err(AppError(AoError::ValidationError(format!(
                "instruction filename must not be a path component: {trimmed}"
            ))));
        }
        let is_dup = out
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(trimmed));
        if !is_dup {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

/// GET /preferences/instruction-filenames — return the configured list of
/// instruction filename patterns. Returns the default (`["CLAUDE.md"]`) if no
/// preferences have been saved yet.
pub async fn get_instruction_filenames(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<String>>, AppError> {
    let prefs = state
        .persistence
        .preferences
        .get()
        .await?
        .unwrap_or_default();
    Ok(Json(prefs.instruction_filenames))
}

/// PUT /preferences/instruction-filenames — replace the instruction filename
/// list. Normalizes (trims, dedupes case-insensitively) and validates each
/// entry, persists through the preferences store, and returns the normalized
/// list.
pub async fn put_instruction_filenames(
    State(state): State<Arc<AppState>>,
    Json(list): Json<Vec<String>>,
) -> Result<Json<Vec<String>>, AppError> {
    let normalized = normalize_instruction_filenames(list)?;

    let mut prefs = state
        .persistence
        .preferences
        .get()
        .await?
        .unwrap_or_default();
    prefs.instruction_filenames = normalized.clone();
    state.persistence.preferences.save(&prefs).await?;

    Ok(Json(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;

    async fn setup_state() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("Failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = PersistenceLayer::init();
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock).await.expect("AppState init")
        };
        (Arc::new(state), tmp)
    }

    fn unwrap_ok<T>(r: Result<T, AppError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {:?}", e.0),
        }
    }

    #[tokio::test]
    async fn get_instruction_filenames_returns_default_when_unset() {
        let (state, _tmp) = setup_state().await;
        let Json(list) =
            unwrap_ok(get_instruction_filenames(State(Arc::clone(&state))).await);
        assert_eq!(list, vec!["CLAUDE.md".to_string()]);
    }

    #[tokio::test]
    async fn put_instruction_filenames_replaces_list() {
        let (state, _tmp) = setup_state().await;

        let Json(list) = unwrap_ok(
            put_instruction_filenames(
                State(Arc::clone(&state)),
                Json(vec!["CLAUDE.md".to_string(), "Cursor.md".to_string()]),
            )
            .await,
        );
        assert_eq!(list, vec!["CLAUDE.md".to_string(), "Cursor.md".to_string()]);

        // Round-trip via GET.
        let Json(got) =
            unwrap_ok(get_instruction_filenames(State(Arc::clone(&state))).await);
        assert_eq!(got, vec!["CLAUDE.md".to_string(), "Cursor.md".to_string()]);
    }

    #[tokio::test]
    async fn put_instruction_filenames_dedupes_case_insensitively_keeping_first() {
        let (state, _tmp) = setup_state().await;

        let Json(list) = unwrap_ok(
            put_instruction_filenames(
                State(Arc::clone(&state)),
                Json(vec![
                    "CLAUDE.md".to_string(),
                    "claude.md".to_string(),
                    "Cursor.md".to_string(),
                    "CURSOR.MD".to_string(),
                ]),
            )
            .await,
        );
        assert_eq!(list, vec!["CLAUDE.md".to_string(), "Cursor.md".to_string()]);
    }

    #[tokio::test]
    async fn put_instruction_filenames_trims_whitespace() {
        let (state, _tmp) = setup_state().await;

        let Json(list) = unwrap_ok(
            put_instruction_filenames(
                State(Arc::clone(&state)),
                Json(vec!["  CLAUDE.md  ".to_string(), "\tCursor.md\n".to_string()]),
            )
            .await,
        );
        assert_eq!(list, vec!["CLAUDE.md".to_string(), "Cursor.md".to_string()]);
    }

    #[tokio::test]
    async fn put_instruction_filenames_rejects_path_separator() {
        let (state, _tmp) = setup_state().await;
        let err = put_instruction_filenames(
            State(Arc::clone(&state)),
            Json(vec!["sub/CLAUDE.md".to_string()]),
        )
        .await
        .expect_err("path separator should be rejected");
        assert!(matches!(err.0, AoError::ValidationError(_)));
    }

    #[tokio::test]
    async fn put_instruction_filenames_rejects_backslash() {
        let (state, _tmp) = setup_state().await;
        let err = put_instruction_filenames(
            State(Arc::clone(&state)),
            Json(vec!["sub\\CLAUDE.md".to_string()]),
        )
        .await
        .expect_err("backslash should be rejected");
        assert!(matches!(err.0, AoError::ValidationError(_)));
    }

    #[tokio::test]
    async fn put_instruction_filenames_rejects_empty_string() {
        let (state, _tmp) = setup_state().await;
        let err = put_instruction_filenames(
            State(Arc::clone(&state)),
            Json(vec!["CLAUDE.md".to_string(), "".to_string()]),
        )
        .await
        .expect_err("empty string should be rejected");
        assert!(matches!(err.0, AoError::ValidationError(_)));
    }

    #[tokio::test]
    async fn put_instruction_filenames_rejects_whitespace_only() {
        let (state, _tmp) = setup_state().await;
        let err = put_instruction_filenames(
            State(Arc::clone(&state)),
            Json(vec!["   ".to_string()]),
        )
        .await
        .expect_err("whitespace-only string should be rejected");
        assert!(matches!(err.0, AoError::ValidationError(_)));
    }

    #[tokio::test]
    async fn put_instruction_filenames_persists_across_calls() {
        let (state, _tmp) = setup_state().await;

        let _ = unwrap_ok(
            put_instruction_filenames(
                State(Arc::clone(&state)),
                Json(vec!["AlwaysOn.md".to_string()]),
            )
            .await,
        );

        // Loading the full prefs should also reflect the change.
        let prefs = state
            .persistence
            .preferences
            .get()
            .await
            .unwrap()
            .expect("prefs should be persisted");
        assert_eq!(prefs.instruction_filenames, vec!["AlwaysOn.md".to_string()]);
    }

    #[tokio::test]
    async fn put_instruction_filenames_preserves_other_prefs() {
        let (state, _tmp) = setup_state().await;

        // Save prefs with a full_name set first.
        let mut prefs = UserPreferences::default();
        prefs.full_name = Some("Ada Lovelace".to_string());
        state.persistence.preferences.save(&prefs).await.unwrap();

        // Now update the instruction filenames via the narrow endpoint.
        let _ = unwrap_ok(
            put_instruction_filenames(
                State(Arc::clone(&state)),
                Json(vec!["NEW.md".to_string()]),
            )
            .await,
        );

        // full_name should be preserved.
        let loaded = state
            .persistence
            .preferences
            .get()
            .await
            .unwrap()
            .expect("prefs should be persisted");
        assert_eq!(loaded.full_name, Some("Ada Lovelace".to_string()));
        assert_eq!(loaded.instruction_filenames, vec!["NEW.md".to_string()]);
    }
}
