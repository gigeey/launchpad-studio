use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use ao_engine::skills::SkillDto;
use ao_engine::AppState;
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
};
use ao_server::routes::build_router;

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_test_profile(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Test Agent {}", id),
        description: "A test agent".to_string(),
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
        enabled_plugins: HashMap::new(),
        runner_mode: Default::default(),
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

async fn setup() -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");

    let state = {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let mock = MockProcessSupervisor::new(vec![]);
        Arc::new(AppState::new_with_mock(mock).await.expect("init state"))
    };

    let router = build_router(state);
    (router, tmp)
}

async fn create_agent(router: &axum::Router, profile: &AgentProfile) {
    let body = serde_json::to_string(profile).unwrap();
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn write_skill_api(router: &axum::Router, agent_id: &str, title: &str) {
    let body = serde_json::json!({
        "title": title,
        "description": format!("Description of {}", title),
        "content": format!("# {}\n\nBody.", title),
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{agent_id}/skills"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

async fn list_skills(router: &axum::Router, agent_id: &str) -> Vec<SkillDto> {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&format!("/agents/{agent_id}/skills"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("decode SkillDto list")
}

#[tokio::test]
async fn test_list_skills_merges_usage_counts() {
    let (router, tmp) = setup().await;
    let agent_id = "skills-usage-merge";
    create_agent(&router, &make_test_profile(agent_id)).await;

    // Write skills via the API (writes to user pool, updates profile.skills)
    write_skill_api(&router, agent_id, "Alpha").await;
    write_skill_api(&router, agent_id, "Beta").await;

    // Write usage to per-agent location (agent_home/skills/.usage.json)
    let agent_home_skills = tmp
        .path()
        .join("agent_homes")
        .join(agent_id)
        .join("skills");
    tokio::fs::create_dir_all(&agent_home_skills).await.unwrap();
    let usage_json = r#"{"alpha": {"count": 7, "last_used": "2026-04-20T12:00:00Z"}}"#;
    tokio::fs::write(agent_home_skills.join(".usage.json"), usage_json)
        .await
        .unwrap();

    let skills = list_skills(&router, agent_id).await;
    let by_id: HashMap<_, _> = skills.iter().map(|s| (s.id.clone(), s.clone())).collect();

    let alpha = by_id.get("alpha").expect("alpha skill present");
    assert_eq!(alpha.usage_count, 7);
    assert!(
        alpha.last_used.is_some(),
        "alpha should carry last_used from .usage.json"
    );

    let beta = by_id.get("beta").expect("beta skill present");
    assert_eq!(beta.usage_count, 0, "skills with no usage entry default to 0");
    assert!(
        beta.last_used.is_none(),
        "skills with no usage entry have last_used = null"
    );
}

#[tokio::test]
async fn test_list_skills_with_no_usage_file_returns_zeros() {
    let (router, _tmp) = setup().await;
    let agent_id = "skills-no-usage-file";
    create_agent(&router, &make_test_profile(agent_id)).await;

    write_skill_api(&router, agent_id, "Solo").await;

    let skills = list_skills(&router, agent_id).await;
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "solo");
    assert_eq!(skills[0].usage_count, 0);
    assert!(skills[0].last_used.is_none());
}

#[tokio::test]
async fn test_list_skills_with_unreadable_usage_file_returns_zeros() {
    let (router, tmp) = setup().await;
    let agent_id = "skills-bad-usage-file";
    create_agent(&router, &make_test_profile(agent_id)).await;

    write_skill_api(&router, agent_id, "Solo").await;

    // Write invalid JSON to the usage file
    let agent_home_skills = tmp
        .path()
        .join("agent_homes")
        .join(agent_id)
        .join("skills");
    tokio::fs::create_dir_all(&agent_home_skills).await.unwrap();
    tokio::fs::write(agent_home_skills.join(".usage.json"), b"this is not valid json")
        .await
        .unwrap();

    let skills = list_skills(&router, agent_id).await;
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].usage_count, 0);
    assert!(skills[0].last_used.is_none());
}
