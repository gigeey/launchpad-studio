use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use ao_engine::AppState;
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use ao_protocol::transcript::TranscriptRole;
use ao_server::routes::build_router;

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_agent_profile(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Agent {}", id),
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
    let (router, _state, tmp) = setup_with_state().await;
    (router, tmp)
}

async fn setup_with_state() -> (axum::Router, Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    let state = {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let mock = MockProcessSupervisor::new(vec![]);
        Arc::new(AppState::new_with_mock(mock).await.expect("init state"))
    };
    let router = build_router(Arc::clone(&state));
    (router, state, tmp)
}

async fn read_body(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes()
        .to_vec()
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

async fn create_project_request(
    router: &axum::Router,
    body: serde_json::Value,
) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/projects")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn test_create_project_returns_201() {
    let (router, _tmp) = setup().await;
    let agent = make_agent_profile("proj-agent-1");
    create_agent(&router, &agent).await;

    let resp = create_project_request(
        &router,
        serde_json::json!({
            "goal": "Build a rocket ship",
            "agent_id": "proj-agent-1"
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let bytes = read_body(resp).await;
    let project: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(project["goal"], "Build a rocket ship");
    assert_eq!(project["status"], "interviewing");
    assert_eq!(project["agent_id"], "proj-agent-1");
    // name derived from first 5 words of goal when not supplied
    assert_eq!(project["name"], "Build a rocket ship");
}

#[tokio::test]
async fn test_create_project_with_explicit_name() {
    let (router, _tmp) = setup().await;
    let agent = make_agent_profile("proj-agent-2");
    create_agent(&router, &agent).await;

    let resp = create_project_request(
        &router,
        serde_json::json!({
            "goal": "Some long description of a goal",
            "agent_id": "proj-agent-2",
            "name": "My Custom Name"
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let bytes = read_body(resp).await;
    let project: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(project["name"], "My Custom Name");
}

#[tokio::test]
async fn test_create_project_unknown_agent_returns_400() {
    let (router, _tmp) = setup().await;

    let resp = create_project_request(
        &router,
        serde_json::json!({
            "goal": "Do something",
            "agent_id": "nonexistent-agent"
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_list_projects_includes_created() {
    let (router, _tmp) = setup().await;
    let agent = make_agent_profile("list-proj-agent");
    create_agent(&router, &agent).await;

    create_project_request(
        &router,
        serde_json::json!({ "goal": "Goal one", "agent_id": "list-proj-agent" }),
    )
    .await;
    create_project_request(
        &router,
        serde_json::json!({ "goal": "Goal two", "agent_id": "list-proj-agent" }),
    )
    .await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/projects")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let projects: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(projects.len(), 2);
    // Snapshots have limited fields
    assert!(projects[0].get("id").is_some());
    assert!(projects[0].get("status").is_some());
    assert!(projects[0].get("goal").is_none(), "snapshots should not include goal");
}

#[tokio::test]
async fn test_get_project_returns_full_record() {
    let (router, _tmp) = setup().await;
    let agent = make_agent_profile("get-proj-agent");
    create_agent(&router, &agent).await;

    let resp = create_project_request(
        &router,
        serde_json::json!({
            "goal": "Full goal text",
            "agent_id": "get-proj-agent",
            "emoji": "🚀"
        }),
    )
    .await;
    let bytes = read_body(resp).await;
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = created["id"].as_str().unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&format!("/projects/{}", id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let project: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(project["goal"], "Full goal text");
    assert_eq!(project["emoji"], "🚀");
}

#[tokio::test]
async fn test_get_nonexistent_project_returns_404() {
    let (router, _tmp) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/projects/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_patch_project_updates_fields() {
    let (router, _tmp) = setup().await;
    let agent = make_agent_profile("patch-proj-agent");
    create_agent(&router, &agent).await;

    let resp = create_project_request(
        &router,
        serde_json::json!({ "goal": "Original goal", "agent_id": "patch-proj-agent" }),
    )
    .await;
    let bytes = read_body(resp).await;
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = created["id"].as_str().unwrap();

    let patch_body = serde_json::json!({
        "name": "Patched Name",
        "status": "active",
        "spec": "Detailed spec text"
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(&format!("/projects/{}", id))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&patch_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let updated: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(updated["name"], "Patched Name");
    assert_eq!(updated["status"], "active");
    assert_eq!(updated["spec"], "Detailed spec text");
    // goal unchanged
    assert_eq!(updated["goal"], "Original goal");
}

#[tokio::test]
async fn test_delete_project_returns_204() {
    let (router, _tmp) = setup().await;
    let agent = make_agent_profile("del-proj-agent");
    create_agent(&router, &agent).await;

    let resp = create_project_request(
        &router,
        serde_json::json!({ "goal": "Delete me", "agent_id": "del-proj-agent" }),
    )
    .await;
    let bytes = read_body(resp).await;
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = created["id"].as_str().unwrap();

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(&format!("/projects/{}", id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Subsequent GET should 404
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&format!("/projects/{}", id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_nonexistent_project_returns_404() {
    let (router, _tmp) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/projects/ghost-project")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_create_project_writes_system_transcript_entry() {
    let (router, state, _tmp) = setup_with_state().await;
    let agent = make_agent_profile("transcript-test-agent");
    create_agent(&router, &agent).await;

    let resp = create_project_request(
        &router,
        serde_json::json!({
            "goal": "Teach a robot to bake bread",
            "agent_id": "transcript-test-agent",
            "name": "Bread Robot"
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let bytes = read_body(resp).await;
    let project: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let project_id = project["id"].as_str().unwrap();

    let transcript_key = format!("project_{}", project_id);
    let entries = state
        .persistence
        .transcripts
        .read_all(&transcript_key)
        .await
        .expect("read project transcript");

    assert!(!entries.is_empty(), "transcript should have at least one entry");

    let first = &entries[0];
    assert!(
        matches!(&first.role, TranscriptRole::System(r) if r == "system"),
        "first entry role should serialize as system, got: {:?}",
        first.role
    );
    assert!(
        first.content.contains("Bread Robot"),
        "content should contain project name, got: {}",
        first.content
    );
    assert!(
        first.content.contains("Teach a robot to bake bread"),
        "content should contain project goal, got: {}",
        first.content
    );

    // Verify the role serializes to "system" as the frontend expects.
    let serialized = serde_json::to_value(first).unwrap();
    assert_eq!(serialized["role"], serde_json::json!("system"));
}
