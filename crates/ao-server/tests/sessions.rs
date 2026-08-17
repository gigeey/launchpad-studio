use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use ao_engine::AppState;
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use ao_server::routes::build_router;

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_test_profile(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Sessions Test Agent {}", id),
        description: "A test agent for sessions endpoints".to_string(),
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

async fn setup() -> (axum::Router, Arc<AppState>, tempfile::TempDir) {
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

async fn read_json(resp: axum::response::Response) -> Value {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("parse JSON body")
}

async fn create_agent(router: &axum::Router, id: &str) {
    let profile = make_test_profile(id);
    let body = serde_json::to_string(&profile).unwrap();
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
    assert_eq!(resp.status(), StatusCode::OK, "create agent {id} failed");
}

// POST /sessions → 201, then DELETE → 204, then MCP call → 404
#[tokio::test]
async fn register_deregister_lifecycle() {
    let (router, _state, _tmp) = setup().await;
    create_agent(&router, "lifecycle-agent").await;

    let session_id = "lifecycle-session-001";

    // Register the session.
    let reg_body = json!({
        "sessionId": session_id,
        "agentId": "lifecycle-agent",
        "spawnCwd": "/tmp/lifecycle"
    });
    let reg_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sessions")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&reg_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reg_resp.status(), StatusCode::CREATED);

    // MCP call succeeds while session is live.
    let mcp_body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let mcp_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/lifecycle-agent/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&mcp_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mcp_resp.status(), StatusCode::OK);

    // Deregister the session.
    let del_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/sessions/{session_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_resp.status(), StatusCode::NO_CONTENT);

    // MCP call after deregistration returns 404.
    let mcp_resp2 = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/lifecycle-agent/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&mcp_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mcp_resp2.status(), StatusCode::NOT_FOUND);
    let body = read_json(mcp_resp2).await;
    assert!(body["error"].as_str().unwrap().contains("session not found"));
}

#[tokio::test]
async fn register_duplicate_session_id_returns_409() {
    let (router, _state, _tmp) = setup().await;
    create_agent(&router, "dup-agent").await;

    let reg_body = json!({
        "sessionId": "dup-session-001",
        "agentId": "dup-agent",
        "spawnCwd": "/tmp/dup"
    });

    // First registration succeeds.
    let resp1 = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sessions")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&reg_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::CREATED);

    // Second registration with same session_id returns 409.
    let resp2 = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sessions")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&reg_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::CONFLICT);
    let body = read_json(resp2).await;
    assert!(body["error"].as_str().unwrap().contains("already exists"));
}

#[tokio::test]
async fn deregister_unknown_session_returns_404() {
    let (router, _state, _tmp) = setup().await;

    let del_resp = router
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/sessions/no-such-session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_resp.status(), StatusCode::NOT_FOUND);
    let body = read_json(del_resp).await;
    assert!(body["error"].as_str().unwrap().contains("session not found"));
}

#[tokio::test]
async fn register_with_parent_session_id_sets_parent_info() {
    let (router, state, _tmp) = setup().await;
    create_agent(&router, "parent-agent").await;
    create_agent(&router, "child-agent").await;

    // Register parent directly via store.
    state
        .mcp_sessions
        .register_session(
            "parent-sid".to_string(),
            "parent-agent".to_string(),
            PathBuf::from("/tmp/parent"),
            None,
        )
        .expect("register parent");

    // Register child via HTTP with parentSessionId.
    let reg_body = json!({
        "sessionId": "child-sid",
        "agentId": "child-agent",
        "spawnCwd": "/tmp/child",
        "parentSessionId": "parent-sid"
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sessions")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&reg_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let child = state.mcp_sessions.get_by_session_id("child-sid").unwrap();
    assert_eq!(child.parent_session_id.as_deref(), Some("parent-sid"));
    assert_eq!(child.parent_agent_id.as_deref(), Some("parent-agent"));
    assert_eq!(
        child.parent_current_cwd.as_deref(),
        Some(std::path::Path::new("/tmp/parent"))
    );
}

#[tokio::test]
async fn register_with_unknown_parent_session_returns_404() {
    let (router, _state, _tmp) = setup().await;
    create_agent(&router, "orphan-child").await;

    let reg_body = json!({
        "sessionId": "orphan-child-sid",
        "agentId": "orphan-child",
        "spawnCwd": "/tmp/child",
        "parentSessionId": "nonexistent-parent"
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sessions")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&reg_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = read_json(resp).await;
    assert!(body["error"].as_str().unwrap().contains("parent session not found"));
}
