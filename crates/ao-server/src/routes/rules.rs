use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use ao_engine::rules::{
    delete_rule as engine_delete_rule, import_file_as_rule, import_folder_as_rule,
    import_link_as_rule, list_rules as engine_list_rules, patch_rule as engine_patch_rule,
    refresh_agent_rules, resolve_agent_rules_dir, RulePatch,
};
use ao_engine::AppState;
use ao_protocol::error::AoError;
use ao_protocol::rules::RuleDto;

use crate::error::AppError;

fn delete_io_error_to_app_error(e: std::io::Error) -> AppError {
    match e.kind() {
        std::io::ErrorKind::NotFound => AppError(AoError::RuleNotFound(e.to_string())),
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::PermissionDenied => {
            AppError(AoError::ValidationError(e.to_string()))
        }
        _ => AppError(AoError::Internal(format!("rule operation failed: {e}"))),
    }
}

fn import_io_error_to_app_error(e: std::io::Error) -> AppError {
    match e.kind() {
        std::io::ErrorKind::NotFound
        | std::io::ErrorKind::InvalidInput
        | std::io::ErrorKind::PermissionDenied => {
            AppError(AoError::ValidationError(e.to_string()))
        }
        _ => AppError(AoError::Internal(format!("rule import failed: {e}"))),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ImportPathRequest {
    pub src_path: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct ImportLinkRequest {
    pub url: String,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct PatchRuleRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub auto_sync: Option<bool>,
}

fn patch_io_error_to_app_error(e: std::io::Error) -> AppError {
    match e.kind() {
        std::io::ErrorKind::NotFound => AppError(AoError::RuleNotFound(e.to_string())),
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::PermissionDenied => {
            AppError(AoError::ValidationError(e.to_string()))
        }
        _ => AppError(AoError::Internal(format!("rule operation failed: {e}"))),
    }
}

/// GET /agents/{agent_id}/rules — list every rule discovered under the
/// agent's rules directory. Returns `[]` when the directory is missing.
pub async fn list_rules(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<RuleDto>>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let data_root = state.persistence.data_root.clone();
    let rules = tokio::task::spawn_blocking(move || engine_list_rules(&agent, &data_root))
        .await
        .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?
        .map_err(|e| AppError(AoError::Internal(format!("Failed to scan rules dir: {e}"))))?;

    Ok(Json(rules))
}

/// DELETE /agents/{agent_id}/rules/{*rule_id} — remove a top-level rule
/// bundle or flat top-level rule file. Returns 400 when a nested id is
/// supplied. Returns 404 when the rule does not exist.
pub async fn delete_rule(
    State(state): State<Arc<AppState>>,
    Path((agent_id, rule_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let rules_dir = ao_engine::rules::resolve_agent_rules_dir(&agent, &state.persistence.data_root);
    let rule_id_task = rule_id.clone();

    tokio::task::spawn_blocking(move || engine_delete_rule(&rules_dir, &rule_id_task))
        .await
        .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?
        .map_err(delete_io_error_to_app_error)?;

    state.context_cache.invalidate(&agent_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// PATCH /agents/{agent_id}/rules/{*rule_id} — updates `enabled` and/or
/// `auto_sync` on the rule's manifest. Unset fields are preserved. Nested ids
/// may patch `enabled` only; `auto_sync=true` on a nested id returns 400.
pub async fn patch_rule(
    State(state): State<Arc<AppState>>,
    Path((agent_id, rule_id)): Path<(String, String)>,
    Json(req): Json<PatchRuleRequest>,
) -> Result<Json<RuleDto>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
    let patch = RulePatch {
        enabled: req.enabled,
        auto_sync: req.auto_sync,
    };
    let rule_id_task = rule_id.clone();

    let dto = tokio::task::spawn_blocking(move || {
        engine_patch_rule(&rules_dir, &rule_id_task, patch)
    })
    .await
    .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?
    .map_err(patch_io_error_to_app_error)?;

    state.context_cache.invalidate(&agent_id).await;
    Ok(Json(dto))
}

/// POST /agents/{agent_id}/rules/import-file — copies a single `.md` file
/// into the agent's rules directory as a bundle with a single rule.
pub async fn import_file(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<ImportPathRequest>,
) -> Result<Json<Vec<RuleDto>>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let src_raw = req.src_path.trim().to_string();
    if src_raw.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "src_path must not be empty".to_string(),
        )));
    }

    let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
    let src = std::path::PathBuf::from(src_raw);

    let rules = tokio::task::spawn_blocking(move || import_file_as_rule(&rules_dir, &src))
        .await
        .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?
        .map_err(import_io_error_to_app_error)?;

    state.context_cache.invalidate(&agent_id).await;
    Ok(Json(rules))
}

/// POST /agents/{agent_id}/rules/import-folder — copies a local folder into
/// the agent's rules directory as a bundle.
pub async fn import_folder(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<ImportPathRequest>,
) -> Result<Json<Vec<RuleDto>>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let src_raw = req.src_path.trim().to_string();
    if src_raw.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "src_path must not be empty".to_string(),
        )));
    }

    let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
    let src = std::path::PathBuf::from(src_raw);

    let rules = tokio::task::spawn_blocking(move || import_folder_as_rule(&rules_dir, &src))
        .await
        .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?
        .map_err(import_io_error_to_app_error)?;

    state.context_cache.invalidate(&agent_id).await;
    Ok(Json(rules))
}

/// POST /agents/{agent_id}/rules/import-link — downloads a single `.md` via
/// HTTP GET and writes it into the agent's rules directory as a bundle with
/// `added_by=link`.
pub async fn import_link(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<ImportLinkRequest>,
) -> Result<Json<Vec<RuleDto>>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let url = req.url.trim().to_string();
    if url.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "url must not be empty".to_string(),
        )));
    }

    let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
    let url_for_task = url.clone();

    let rules =
        tokio::task::spawn_blocking(move || import_link_as_rule(&rules_dir, &url_for_task))
            .await
            .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?
            .map_err(import_io_error_to_app_error)?;

    state.context_cache.invalidate(&agent_id).await;
    Ok(Json(rules))
}

/// POST /agents/{agent_id}/rules/refresh — re-runs `git pull` on every
/// auto-sync github bundle in the agent's rules directory and returns the
/// freshly scanned list.
pub async fn refresh_rules(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<RuleDto>>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
    let agent_id_task = agent_id.clone();

    let rules = tokio::task::spawn_blocking(move || {
        refresh_agent_rules(&rules_dir, &agent_id_task)
    })
    .await
    .map_err(|e| AppError(AoError::Internal(format!("Failed to refresh rules: {e}"))))?;

    state.context_cache.invalidate(&agent_id).await;
    Ok(Json(rules))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_engine::rules::{resolve_agent_rules_dir, write_bundle_manifest};
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };
    use ao_protocol::rules::{AddedBy, RuleManifest};
    use axum::http::StatusCode;
    use chrono::{DateTime, Utc};
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

    fn sample_manifest(added_by: AddedBy) -> RuleManifest {
        RuleManifest {
            added_by,
            enabled: true,
            auto_sync: false,
            source_url: None,
            imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
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
    async fn list_rules_empty_returns_vec() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-empty");
        state.persistence.agents.create(&agent).await.unwrap();

        let Json(rules) = unwrap_ok(
            list_rules(
                State(Arc::clone(&state)),
                Path("agent-empty".to_string()),
            )
            .await,
        );

        assert!(rules.is_empty());
    }

    #[tokio::test]
    async fn list_rules_with_nested_bundle_returns_all_entries() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-nested");
        state.persistence.agents.create(&agent).await.unwrap();

        let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
        std::fs::create_dir_all(rules_dir.join("bundle").join("inner")).unwrap();
        std::fs::write(rules_dir.join("bundle").join("root.md"), "root").unwrap();
        std::fs::write(
            rules_dir.join("bundle").join("inner").join("a.md"),
            "nested a",
        )
        .unwrap();
        std::fs::write(
            rules_dir.join("bundle").join("inner").join("b.md"),
            "nested b",
        )
        .unwrap();
        write_bundle_manifest(&rules_dir.join("bundle"), &sample_manifest(AddedBy::User)).unwrap();

        let Json(rules) = unwrap_ok(
            list_rules(
                State(Arc::clone(&state)),
                Path("agent-nested".to_string()),
            )
            .await,
        );

        assert_eq!(rules.len(), 3);
        let mut ids: Vec<_> = rules.iter().map(|r| r.id.clone()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "bundle/inner/a.md".to_string(),
                "bundle/inner/b.md".to_string(),
                "bundle/root.md".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn list_rules_unknown_agent_returns_404() {
        let (state, _tmp) = setup_state().await;
        let err = list_rules(State(Arc::clone(&state)), Path("ghost".to_string()))
            .await
            .expect_err("unknown agent should fail");
        assert!(matches!(err.0, AoError::AgentNotFound(_)));
    }

    #[tokio::test]
    async fn delete_rule_top_level_bundle_removes_files() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-del");
        state.persistence.agents.create(&agent).await.unwrap();

        let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
        std::fs::create_dir_all(rules_dir.join("bundle").join("inner")).unwrap();
        std::fs::write(rules_dir.join("bundle").join("root.md"), "r").unwrap();
        std::fs::write(rules_dir.join("bundle").join("inner").join("x.md"), "x").unwrap();
        write_bundle_manifest(&rules_dir.join("bundle"), &sample_manifest(AddedBy::User)).unwrap();

        let status = unwrap_ok(
            delete_rule(
                State(Arc::clone(&state)),
                Path(("agent-del".to_string(), "bundle".to_string())),
            )
            .await,
        );

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!rules_dir.join("bundle").exists());
    }

    #[tokio::test]
    async fn delete_rule_nested_id_returns_validation_error() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-nest-del");
        state.persistence.agents.create(&agent).await.unwrap();

        let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
        std::fs::create_dir_all(rules_dir.join("bundle").join("inner")).unwrap();
        std::fs::write(rules_dir.join("bundle").join("inner").join("x.md"), "x").unwrap();

        let err = delete_rule(
            State(Arc::clone(&state)),
            Path((
                "agent-nest-del".to_string(),
                "bundle/inner/x.md".to_string(),
            )),
        )
        .await
        .expect_err("nested delete should fail");

        match err.0 {
            AoError::ValidationError(msg) => {
                assert!(msg.contains("nested rules cannot be deleted directly"));
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }
        assert!(rules_dir.join("bundle").join("inner").join("x.md").exists());
    }

    #[tokio::test]
    async fn delete_rule_unknown_id_returns_not_found() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-missing");
        state.persistence.agents.create(&agent).await.unwrap();
        let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
        std::fs::create_dir_all(&rules_dir).unwrap();

        let err = delete_rule(
            State(Arc::clone(&state)),
            Path(("agent-missing".to_string(), "nope".to_string())),
        )
        .await
        .expect_err("unknown id should fail");

        assert!(matches!(err.0, AoError::RuleNotFound(_)));
    }

    #[tokio::test]
    async fn import_file_happy_path_writes_bundle() {
        let (state, tmp) = setup_state().await;
        let agent = make_agent("agent-import-file");
        state.persistence.agents.create(&agent).await.unwrap();

        let src = tmp.path().join("tip.md");
        std::fs::write(&src, "---\ntitle: \"Tip\"\n---\nbody").unwrap();

        let Json(rules) = unwrap_ok(
            import_file(
                State(Arc::clone(&state)),
                Path("agent-import-file".to_string()),
                Json(ImportPathRequest {
                    src_path: src.to_string_lossy().into_owned(),
                }),
            )
            .await,
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "tip/tip.md");
        assert_eq!(rules[0].added_by, AddedBy::User);
        let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
        assert!(rules_dir.join("tip").join("tip.md").is_file());
    }

    #[tokio::test]
    async fn import_file_rejects_non_md() {
        let (state, tmp) = setup_state().await;
        let agent = make_agent("agent-imp-bad");
        state.persistence.agents.create(&agent).await.unwrap();

        let src = tmp.path().join("notes.txt");
        std::fs::write(&src, "nope").unwrap();

        let err = import_file(
            State(Arc::clone(&state)),
            Path("agent-imp-bad".to_string()),
            Json(ImportPathRequest {
                src_path: src.to_string_lossy().into_owned(),
            }),
        )
        .await
        .expect_err("non-md should fail");

        assert!(matches!(err.0, AoError::ValidationError(_)));
    }

    #[tokio::test]
    async fn import_folder_recursive_returns_all_rules() {
        let (state, tmp) = setup_state().await;
        let agent = make_agent("agent-imp-folder");
        state.persistence.agents.create(&agent).await.unwrap();

        let src = tmp.path().join("pack");
        std::fs::create_dir_all(src.join("inner")).unwrap();
        std::fs::write(src.join("root.md"), "root").unwrap();
        std::fs::write(src.join("inner").join("a.md"), "a").unwrap();

        let Json(rules) = unwrap_ok(
            import_folder(
                State(Arc::clone(&state)),
                Path("agent-imp-folder".to_string()),
                Json(ImportPathRequest {
                    src_path: src.to_string_lossy().into_owned(),
                }),
            )
            .await,
        );

        assert_eq!(rules.len(), 2);
        let ids: Vec<_> = rules.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids, vec!["pack/inner/a.md".to_string(), "pack/root.md".to_string()]);
    }

    #[tokio::test]
    async fn import_link_rejects_non_md() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-imp-link");
        state.persistence.agents.create(&agent).await.unwrap();

        let err = import_link(
            State(Arc::clone(&state)),
            Path("agent-imp-link".to_string()),
            Json(ImportLinkRequest {
                url: "https://example.com/notes.txt".to_string(),
            }),
        )
        .await
        .expect_err("non-md url should fail");

        assert!(matches!(err.0, AoError::ValidationError(_)));
    }

    #[tokio::test]
    async fn refresh_rules_returns_non_github_bundles_unchanged() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-refresh");
        state.persistence.agents.create(&agent).await.unwrap();

        let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
        std::fs::create_dir_all(rules_dir.join("user-pack")).unwrap();
        std::fs::write(rules_dir.join("user-pack").join("a.md"), "a").unwrap();
        write_bundle_manifest(
            &rules_dir.join("user-pack"),
            &sample_manifest(AddedBy::User),
        )
        .unwrap();

        let Json(rules) = unwrap_ok(
            refresh_rules(
                State(Arc::clone(&state)),
                Path("agent-refresh".to_string()),
            )
            .await,
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "user-pack/a.md");
        assert_eq!(rules[0].added_by, AddedBy::User);
    }

    #[tokio::test]
    async fn patch_rule_top_level_bundle_toggles_enabled() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-patch-top");
        state.persistence.agents.create(&agent).await.unwrap();

        let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
        std::fs::create_dir_all(rules_dir.join("bundle").join("inner")).unwrap();
        std::fs::write(rules_dir.join("bundle").join("inner").join("a.md"), "a").unwrap();
        write_bundle_manifest(&rules_dir.join("bundle"), &sample_manifest(AddedBy::User)).unwrap();

        let Json(dto) = unwrap_ok(
            patch_rule(
                State(Arc::clone(&state)),
                Path(("agent-patch-top".to_string(), "bundle".to_string())),
                Json(PatchRuleRequest {
                    enabled: Some(false),
                    auto_sync: None,
                }),
            )
            .await,
        );

        assert_eq!(dto.id, "bundle/inner/a.md");
        assert!(!dto.enabled);
    }

    #[tokio::test]
    async fn patch_rule_nested_id_writes_per_file_sidecar() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-patch-nested");
        state.persistence.agents.create(&agent).await.unwrap();

        let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
        std::fs::create_dir_all(rules_dir.join("bundle").join("inner")).unwrap();
        std::fs::write(
            rules_dir.join("bundle").join("inner").join("rule.md"),
            "body",
        )
        .unwrap();
        write_bundle_manifest(&rules_dir.join("bundle"), &sample_manifest(AddedBy::User)).unwrap();

        let Json(dto) = unwrap_ok(
            patch_rule(
                State(Arc::clone(&state)),
                Path((
                    "agent-patch-nested".to_string(),
                    "bundle/inner/rule.md".to_string(),
                )),
                Json(PatchRuleRequest {
                    enabled: Some(false),
                    auto_sync: None,
                }),
            )
            .await,
        );

        assert_eq!(dto.id, "bundle/inner/rule.md");
        assert!(!dto.enabled);
        assert!(rules_dir
            .join("bundle")
            .join("inner")
            .join("rule.md.manifest.json")
            .exists());
    }

    #[tokio::test]
    async fn patch_rule_nested_auto_sync_returns_validation_error() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-patch-auto");
        state.persistence.agents.create(&agent).await.unwrap();

        let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
        std::fs::create_dir_all(rules_dir.join("bundle").join("inner")).unwrap();
        std::fs::write(
            rules_dir.join("bundle").join("inner").join("rule.md"),
            "body",
        )
        .unwrap();

        let err = patch_rule(
            State(Arc::clone(&state)),
            Path((
                "agent-patch-auto".to_string(),
                "bundle/inner/rule.md".to_string(),
            )),
            Json(PatchRuleRequest {
                enabled: None,
                auto_sync: Some(true),
            }),
        )
        .await
        .expect_err("nested auto_sync should fail");

        match err.0 {
            AoError::ValidationError(msg) => {
                assert!(msg.contains("auto_sync can only be toggled on the top-level bundle"));
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn patch_rule_top_level_auto_sync_on_github_bundle() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-patch-gh");
        state.persistence.agents.create(&agent).await.unwrap();

        let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
        let bundle = rules_dir.join("gh-bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("a.md"), "a").unwrap();
        write_bundle_manifest(
            &bundle,
            &RuleManifest {
                added_by: AddedBy::Github,
                enabled: true,
                auto_sync: false,
                source_url: Some("https://github.com/owner/repo".to_string()),
                imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            },
        )
        .unwrap();

        let Json(dto) = unwrap_ok(
            patch_rule(
                State(Arc::clone(&state)),
                Path(("agent-patch-gh".to_string(), "gh-bundle".to_string())),
                Json(PatchRuleRequest {
                    enabled: None,
                    auto_sync: Some(true),
                }),
            )
            .await,
        );

        assert!(dto.auto_sync);
    }

    #[tokio::test]
    async fn patch_rule_unknown_id_returns_not_found() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-patch-missing");
        state.persistence.agents.create(&agent).await.unwrap();
        let rules_dir = resolve_agent_rules_dir(&agent, &state.persistence.data_root);
        std::fs::create_dir_all(&rules_dir).unwrap();

        let err = patch_rule(
            State(Arc::clone(&state)),
            Path(("agent-patch-missing".to_string(), "ghost".to_string())),
            Json(PatchRuleRequest {
                enabled: Some(true),
                auto_sync: None,
            }),
        )
        .await
        .expect_err("unknown id should fail");

        assert!(matches!(err.0, AoError::RuleNotFound(_)));
    }

    #[tokio::test]
    async fn import_file_unknown_agent_returns_404() {
        let (state, tmp) = setup_state().await;
        let src = tmp.path().join("tip.md");
        std::fs::write(&src, "body").unwrap();

        let err = import_file(
            State(Arc::clone(&state)),
            Path("ghost".to_string()),
            Json(ImportPathRequest {
                src_path: src.to_string_lossy().into_owned(),
            }),
        )
        .await
        .expect_err("unknown agent should fail");

        assert!(matches!(err.0, AoError::AgentNotFound(_)));
    }
}
