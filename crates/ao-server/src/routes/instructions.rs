use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;

use ao_engine::instructions::{
    list_instructions as engine_list_instructions, patch_instruction as engine_patch_instruction,
};
use ao_engine::AppState;
use ao_protocol::error::AoError;
use ao_protocol::instructions::InstructionDto;

use crate::error::AppError;

fn io_error_to_app_error(e: std::io::Error) -> AppError {
    match e.kind() {
        std::io::ErrorKind::NotFound => AppError(AoError::InstructionNotFound(e.to_string())),
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::PermissionDenied => {
            AppError(AoError::ValidationError(e.to_string()))
        }
        _ => AppError(AoError::Internal(format!("instruction operation failed: {e}"))),
    }
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct PatchInstructionRequest {
    pub enabled: bool,
}

/// GET /agents/{agent_id}/instructions — scans the agent home root for files
/// whose filename matches one of the user's configured
/// `UserPreferences.instruction_filenames` entries (case-insensitive). Returns
/// `[]` when no matches (including when the home directory is missing).
pub async fn list_instructions(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<InstructionDto>>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let prefs = state
        .persistence
        .preferences
        .get()
        .await?
        .unwrap_or_default();
    let patterns = prefs.instruction_filenames.clone();
    let data_root = state.persistence.data_root.clone();

    let instructions = tokio::task::spawn_blocking(move || {
        engine_list_instructions(&agent, &data_root, &patterns)
    })
    .await
    .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?
    .map_err(|e| AppError(AoError::Internal(format!("Failed to scan instructions: {e}"))))?;

    Ok(Json(instructions))
}

/// PATCH /agents/{agent_id}/instructions/{id} — toggles the per-file
/// `enabled` state. `id` is the exact on-disk filename.
pub async fn patch_instruction(
    State(state): State<Arc<AppState>>,
    Path((agent_id, id)): Path<(String, String)>,
    Json(req): Json<PatchInstructionRequest>,
) -> Result<Json<InstructionDto>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let data_root = state.persistence.data_root.clone();
    let id_for_task = id.clone();

    let dto = tokio::task::spawn_blocking(move || {
        engine_patch_instruction(&agent, &data_root, &id_for_task, req.enabled)
    })
    .await
    .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?
    .map_err(io_error_to_app_error)?;

    state.context_cache.invalidate(&agent_id).await;
    Ok(Json(dto))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_engine::instructions::resolve_agent_home_dir;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };
    use ao_protocol::preferences::UserPreferences;
    use std::collections::HashMap;

    fn make_agent(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: "Test".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
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
                no_output_timeout_ms: 30_000,
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
            runner_mode: Default::default(),
            enabled_plugins: HashMap::new(),
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
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            max_turns: None,
        }
    }

    async fn setup_state() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("Failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = PersistenceLayer::init();
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock)
                .await
                .expect("AppState init")
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
    async fn list_instructions_default_claude_md() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("inst-agent");
        state.persistence.agents.create(&agent).await.unwrap();

        let home = resolve_agent_home_dir(&agent, &state.persistence.data_root);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("CLAUDE.md"), "body").unwrap();

        let Json(got) = unwrap_ok(
            list_instructions(
                State(Arc::clone(&state)),
                Path("inst-agent".to_string()),
            )
            .await,
        );

        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "CLAUDE.md");
        assert!(got[0].enabled);
    }

    #[tokio::test]
    async fn list_instructions_uses_current_prefs() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("inst-prefs");
        state.persistence.agents.create(&agent).await.unwrap();

        // Save prefs adding Cursor.md to the pattern list.
        let prefs = UserPreferences {
            instruction_filenames: vec!["CLAUDE.md".to_string(), "Cursor.md".to_string()],
            ..Default::default()
        };
        state.persistence.preferences.save(&prefs).await.unwrap();

        let home = resolve_agent_home_dir(&agent, &state.persistence.data_root);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("CLAUDE.md"), "c").unwrap();
        std::fs::write(home.join("Cursor.md"), "u").unwrap();
        std::fs::write(home.join("README.md"), "r").unwrap();

        let Json(got) = unwrap_ok(
            list_instructions(
                State(Arc::clone(&state)),
                Path("inst-prefs".to_string()),
            )
            .await,
        );

        let ids: Vec<_> = got.iter().map(|i| i.id.clone()).collect();
        assert_eq!(ids, vec!["CLAUDE.md".to_string(), "Cursor.md".to_string()]);
    }

    #[tokio::test]
    async fn list_instructions_unknown_agent_returns_404() {
        let (state, _tmp) = setup_state().await;
        let err = list_instructions(State(Arc::clone(&state)), Path("ghost".to_string()))
            .await
            .expect_err("unknown agent should fail");
        assert!(matches!(err.0, AoError::AgentNotFound(_)));
    }

    #[tokio::test]
    async fn patch_instruction_toggles_enabled() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("inst-patch");
        state.persistence.agents.create(&agent).await.unwrap();

        let home = resolve_agent_home_dir(&agent, &state.persistence.data_root);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("CLAUDE.md"), "body").unwrap();

        let Json(dto) = unwrap_ok(
            patch_instruction(
                State(Arc::clone(&state)),
                Path(("inst-patch".to_string(), "CLAUDE.md".to_string())),
                Json(PatchInstructionRequest { enabled: false }),
            )
            .await,
        );

        assert!(!dto.enabled);
        assert_eq!(dto.id, "CLAUDE.md");
        assert!(home.join(".instructions").join("CLAUDE.md.manifest.json").exists());
    }

    #[tokio::test]
    async fn patch_instruction_unknown_returns_404() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("inst-404");
        state.persistence.agents.create(&agent).await.unwrap();

        let err = patch_instruction(
            State(Arc::clone(&state)),
            Path(("inst-404".to_string(), "CLAUDE.md".to_string())),
            Json(PatchInstructionRequest { enabled: false }),
        )
        .await
        .expect_err("unknown instruction should fail");

        assert!(matches!(err.0, AoError::InstructionNotFound(_)));
    }
}
