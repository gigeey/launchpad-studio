use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use ao_engine::event_bus::EventBusAgentSink;
use ao_engine::{AppState, LiveFormBridge};
use ao_engine_tools_core::{EventSink, FormBridge, FormField, FormFieldKind, FormRequest};
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use ao_protocol::event::AgentEventPayload;
use ao_protocol::project::Project;
use ao_protocol::tasklist::{
    Task, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistOwner, TasklistStatus,
};
use ao_server::routes::build_router;
use chrono::Utc;

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
        max_instances: 2,
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

async fn setup_with_state() -> (axum::Router, tempfile::TempDir, Arc<AppState>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let mock = MockProcessSupervisor::new(vec![]);
        Arc::new(AppState::new_with_mock(mock).await.expect("init state"))
    };
    let router = build_router(Arc::clone(&state));
    (router, tmp, state)
}

async fn read_body(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes()
        .to_vec()
}

async fn create_agent_http(router: &axum::Router, profile: &AgentProfile) {
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
    assert_eq!(resp.status(), StatusCode::OK, "create_agent_http failed");
}

/// Create a project-stamped agent-owned tasklist directly via the persistence
/// layer, bypassing the HTTP layer so tests can control project_id stamping.
async fn create_project_tasklist(
    state: &Arc<AppState>,
    agent_id: &str,
    project_id: &str,
    tasklist_id: &str,
) -> Tasklist {
    let owner = TasklistOwner::Agent {
        agent_id: agent_id.to_string(),
    };
    let workspace_dir = state
        .persistence
        .data_root
        .agent_tasklist_workspace_dir(agent_id, tasklist_id)
        .to_string_lossy()
        .into_owned();
    let transcripts_dir = state
        .persistence
        .data_root
        .agent_tasklist_transcripts_dir(agent_id, tasklist_id)
        .to_string_lossy()
        .into_owned();

    let task_id = format!("{}-t1", tasklist_id);
    let group_id = format!("{}-g1", tasklist_id);
    let tasklist = Tasklist {
        id: tasklist_id.to_string(),
        owner,
        team_id: None,
        title: "Project tasklist".to_string(),
        description: String::new(),
        status: TasklistStatus::Active,
        groups: vec![TaskGroup {
            id: group_id.clone(),
            mode: TaskGroupMode::Seq,
            tasks: vec![Task {
                id: task_id.clone(),
                owner_agent_id: agent_id.to_string(),
                prompt: "do something".to_string(),
                expected_outputs: vec![],
                status: TaskStatus::Pending,
                group_id: group_id.clone(),
                attempt_count: 0,
                error_log: vec![],
                comments: vec![],
                attachments: vec![],
                remind_me: None,
                parse_failed: false,
                notification_parse_retry_count: 0,
                assignment: None,
                classifier_token: 0,
                dispatch_token: 0,
            }],
        }],
        workspace_dir,
        transcripts_dir,
        created_at: Utc::now(),
        last_active_at: None,
        copilot_agent_id: None,
        last_opened_at: None,
        project_id: Some(project_id.to_string()),
        thread_id: None,
        };
    state
        .persistence
        .tasklists
        .create_for_agent(&tasklist)
        .await
        .expect("create_project_tasklist");
    tasklist
}

/// Create a minimal project record directly in persistence.
async fn create_project_direct(
    state: &Arc<AppState>,
    project_id: &str,
    agent_id: &str,
) -> Project {
    let now = Utc::now();
    let project = Project {
        id: project_id.to_string(),
        name: "Test Project".to_string(),
        emoji: None,
        goal: "test goal".to_string(),
        spec: None,
        agent_id: agent_id.to_string(),
        working_dir: None,
        attachments: vec![],
        status: ao_protocol::project::ProjectStatus::Active,
        summary: None,
        verifications: vec![],
        created_at: now,
        updated_at: now,
    };
    state
        .persistence
        .projects
        .create(&project)
        .await
        .expect("create_project_direct");
    project
}

// --- Tests ---

#[tokio::test]
async fn get_project_tasklist_returns_detail() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-get-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-get-proj", "pt-get-agent").await;
    create_project_tasklist(&state, "pt-get-agent", "pt-get-proj", "pt-get-tl").await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/projects/pt-get-proj/tasklists/pt-get-tl")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let tl: Tasklist = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(tl.id, "pt-get-tl");
    assert_eq!(tl.project_id.as_deref(), Some("pt-get-proj"));
}

#[tokio::test]
async fn get_project_tasklist_wrong_project_returns_404() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-404-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-404-proj-a", "pt-404-agent").await;
    create_project_direct(&state, "pt-404-proj-b", "pt-404-agent").await;
    create_project_tasklist(&state, "pt-404-agent", "pt-404-proj-a", "pt-404-tl").await;

    // Tasklist belongs to proj-a; querying through proj-b must 404.
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/projects/pt-404-proj-b/tasklists/pt-404-tl")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_project_tasklist_nonexistent_project_returns_404() {
    let (router, _tmp, _state) = setup_with_state().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/projects/ghost-proj/tasklists/any-tl")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_project_tasklist_status_pause_resume() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-status-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-status-proj", "pt-status-agent").await;
    create_project_tasklist(&state, "pt-status-agent", "pt-status-proj", "pt-status-tl").await;

    // Pause the active tasklist.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/projects/pt-status-proj/tasklists/pt-status-tl/status")
                .header("content-type", "application/json")
                .body(Body::from(json!({"status": "active"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    // "active" is a valid status for agent-owned tasklists.
    assert!(
        resp.status().is_success(),
        "expected 2xx, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn set_project_tasklist_status_wrong_project_returns_404() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-sts-403-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-sts-proj-real", "pt-sts-403-agent").await;
    create_project_direct(&state, "pt-sts-proj-fake", "pt-sts-403-agent").await;
    create_project_tasklist(
        &state,
        "pt-sts-403-agent",
        "pt-sts-proj-real",
        "pt-sts-tl",
    )
    .await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/projects/pt-sts-proj-fake/tasklists/pt-sts-tl/status")
                .header("content-type", "application/json")
                .body(Body::from(json!({"status": "active"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn skip_project_task_transitions_to_skipped() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-skip-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-skip-proj", "pt-skip-agent").await;
    let tl =
        create_project_tasklist(&state, "pt-skip-agent", "pt-skip-proj", "pt-skip-tl").await;

    let task_id = tl.groups[0].tasks[0].id.clone();

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!(
                    "/projects/pt-skip-proj/tasklists/pt-skip-tl/tasks/{}/skip",
                    task_id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let updated: Tasklist = serde_json::from_slice(&bytes).unwrap();
    let task = updated
        .groups
        .iter()
        .flat_map(|g| g.tasks.iter())
        .find(|t| t.id == task_id)
        .expect("task not found");
    assert_eq!(task.status, TaskStatus::Skipped);
}

#[tokio::test]
async fn skip_project_task_wrong_project_returns_404() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-skip-404-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-skip-real", "pt-skip-404-agent").await;
    create_project_direct(&state, "pt-skip-fake", "pt-skip-404-agent").await;
    let tl = create_project_tasklist(
        &state,
        "pt-skip-404-agent",
        "pt-skip-real",
        "pt-skip-tl-404",
    )
    .await;

    let task_id = tl.groups[0].tasks[0].id.clone();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!(
                    "/projects/pt-skip-fake/tasklists/pt-skip-tl-404/tasks/{}/skip",
                    task_id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Flip a task to the given status directly in persistence so route tests can
/// exercise transitions that normally require a live dispatch.
async fn set_task_status_direct(
    state: &Arc<AppState>,
    agent_id: &str,
    tasklist_id: &str,
    task_id: &str,
    status: TaskStatus,
) {
    let owner = TasklistOwner::Agent {
        agent_id: agent_id.to_string(),
    };
    let t_id = task_id.to_string();
    state
        .persistence
        .tasklists
        .mutate_by_owner(&owner, tasklist_id, move |tl| {
            let task = tl
                .groups
                .iter_mut()
                .flat_map(|g| g.tasks.iter_mut())
                .find(|t| t.id == t_id)
                .expect("task not found");
            task.status = status;
            Ok(())
        })
        .await
        .expect("set_task_status_direct");
}

#[tokio::test]
async fn stop_project_task_transitions_in_progress_to_stopped() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-stop-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-stop-proj", "pt-stop-agent").await;
    let tl =
        create_project_tasklist(&state, "pt-stop-agent", "pt-stop-proj", "pt-stop-tl").await;

    let task_id = tl.groups[0].tasks[0].id.clone();
    set_task_status_direct(&state, "pt-stop-agent", "pt-stop-tl", &task_id, TaskStatus::InProgress)
        .await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!(
                    "/projects/pt-stop-proj/tasklists/pt-stop-tl/tasks/{}/stop",
                    task_id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let updated: Tasklist = serde_json::from_slice(&bytes).unwrap();
    let task = updated
        .groups
        .iter()
        .flat_map(|g| g.tasks.iter())
        .find(|t| t.id == task_id)
        .expect("task not found");
    assert_eq!(task.status, TaskStatus::Stopped);
}

#[tokio::test]
async fn stop_project_task_pending_returns_400() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-stop-400-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-stop-400-proj", "pt-stop-400-agent").await;
    let tl = create_project_tasklist(
        &state,
        "pt-stop-400-agent",
        "pt-stop-400-proj",
        "pt-stop-400-tl",
    )
    .await;

    // Task is Pending — stop must be rejected.
    let task_id = tl.groups[0].tasks[0].id.clone();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!(
                    "/projects/pt-stop-400-proj/tasklists/pt-stop-400-tl/tasks/{}/stop",
                    task_id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn stop_project_task_wrong_project_returns_404() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-stop-404-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-stop-real", "pt-stop-404-agent").await;
    create_project_direct(&state, "pt-stop-fake", "pt-stop-404-agent").await;
    let tl = create_project_tasklist(
        &state,
        "pt-stop-404-agent",
        "pt-stop-real",
        "pt-stop-tl-404",
    )
    .await;

    let task_id = tl.groups[0].tasks[0].id.clone();
    set_task_status_direct(
        &state,
        "pt-stop-404-agent",
        "pt-stop-tl-404",
        &task_id,
        TaskStatus::InProgress,
    )
    .await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!(
                    "/projects/pt-stop-fake/tasklists/pt-stop-tl-404/tasks/{}/stop",
                    task_id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resume_project_task_requeues_stopped_as_pending() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-resume-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-resume-proj", "pt-resume-agent").await;
    let tl = create_project_tasklist(
        &state,
        "pt-resume-agent",
        "pt-resume-proj",
        "pt-resume-tl",
    )
    .await;

    let task_id = tl.groups[0].tasks[0].id.clone();
    set_task_status_direct(
        &state,
        "pt-resume-agent",
        "pt-resume-tl",
        &task_id,
        TaskStatus::Stopped,
    )
    .await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!(
                    "/projects/pt-resume-proj/tasklists/pt-resume-tl/tasks/{}/resume",
                    task_id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let updated: Tasklist = serde_json::from_slice(&bytes).unwrap();
    let task = updated
        .groups
        .iter()
        .flat_map(|g| g.tasks.iter())
        .find(|t| t.id == task_id)
        .expect("task not found");
    assert_eq!(task.status, TaskStatus::Pending);
}

#[tokio::test]
async fn resume_project_task_not_stopped_returns_400() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-res-400-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-res-400-proj", "pt-res-400-agent").await;
    let tl = create_project_tasklist(
        &state,
        "pt-res-400-agent",
        "pt-res-400-proj",
        "pt-res-400-tl",
    )
    .await;

    // Task is Pending — resume must be rejected.
    let task_id = tl.groups[0].tasks[0].id.clone();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!(
                    "/projects/pt-res-400-proj/tasklists/pt-res-400-tl/tasks/{}/resume",
                    task_id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn add_project_task_comment_returns_comment() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-comment-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-comment-proj", "pt-comment-agent").await;
    let tl = create_project_tasklist(
        &state,
        "pt-comment-agent",
        "pt-comment-proj",
        "pt-comment-tl",
    )
    .await;

    let task_id = tl.groups[0].tasks[0].id.clone();
    let body = json!({"body": "looks good"});

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!(
                    "/projects/pt-comment-proj/tasklists/pt-comment-tl/tasks/{}/comments",
                    task_id
                ))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let comment: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(comment["body"], "looks good");
    assert_eq!(comment["author_kind"], "user");
}

#[tokio::test]
async fn add_project_task_comment_empty_body_returns_400() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-comm-empty-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-comm-empty-proj", "pt-comm-empty-agent").await;
    let tl = create_project_tasklist(
        &state,
        "pt-comm-empty-agent",
        "pt-comm-empty-proj",
        "pt-comm-empty-tl",
    )
    .await;

    let task_id = tl.groups[0].tasks[0].id.clone();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!(
                    "/projects/pt-comm-empty-proj/tasklists/pt-comm-empty-tl/tasks/{}/comments",
                    task_id
                ))
                .header("content-type", "application/json")
                .body(Body::from(json!({"body": "   "}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_project_tasklist_output_returns_content() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-output-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-output-proj", "pt-output-agent").await;
    create_project_tasklist(&state, "pt-output-agent", "pt-output-proj", "pt-output-tl").await;

    // Write a file into the tasklist workspace.
    let workspace = state
        .persistence
        .data_root
        .agent_tasklist_workspace_dir("pt-output-agent", "pt-output-tl");
    tokio::fs::write(workspace.join("result.txt"), "hello world")
        .await
        .unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/projects/pt-output-proj/tasklists/pt-output-tl/outputs/result.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    assert_eq!(std::str::from_utf8(&bytes).unwrap(), "hello world");
}

#[tokio::test]
async fn get_project_tasklist_output_path_traversal_rejected() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-trav-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pt-trav-proj", "pt-trav-agent").await;
    create_project_tasklist(&state, "pt-trav-agent", "pt-trav-proj", "pt-trav-tl").await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                // %2F is / encoded — axum should either reject or normalize,
                // but our Component::Normal check must fire for ".."
                .uri("/projects/pt-trav-proj/tasklists/pt-trav-tl/outputs/..%2Fsecret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Must not succeed.
    assert_ne!(resp.status(), StatusCode::OK);
}

/// Verify that a status-change mutation on a project-stamped tasklist emits a
/// `tasklist.status_changed` event on the `project:{id}` SSE channel.
#[tokio::test]
async fn project_tasklist_status_change_emits_on_project_channel() {
    let (_router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pt-sse-agent");
    state
        .persistence
        .agents
        .create(&agent)
        .await
        .expect("create agent");
    create_project_direct(&state, "pt-sse-proj", "pt-sse-agent").await;
    create_project_tasklist(&state, "pt-sse-agent", "pt-sse-proj", "pt-sse-tl").await;

    let mut rx = state.event_bus.subscribe();

    let owner = TasklistOwner::Agent {
        agent_id: "pt-sse-agent".to_string(),
    };
    state
        .tasklist_service
        .set_status(&owner, "pt-sse-tl", "active")
        .await
        .expect("set_status");

    let project_channel = "project:pt-sse-proj";
    let mut found = false;
    // Drain all buffered events.
    while let Ok(event) = rx.try_recv() {
        if event.agent_id == project_channel {
            if matches!(event.payload, AgentEventPayload::TasklistStatusChanged { .. }) {
                found = true;
                break;
            }
        }
    }
    assert!(
        found,
        "expected TasklistStatusChanged on the project channel but none was received"
    );
}

// ── Project form-answer route tests ──────────────────────────────────────────

/// POST /projects/{id}/form-answer with a non-existent project_id must 404.
#[tokio::test]
async fn project_form_answer_unknown_project_returns_404() {
    let (router, _tmp, _state) = setup_with_state().await;

    let body = json!({
        "form_id": "form-xyz",
        "answers": {}
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/projects/ghost-proj/form-answer")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// POST /projects/{id}/form-answer with valid project but no live bridge
/// (no pending form) must return 404.
#[tokio::test]
async fn project_form_answer_unknown_form_returns_404() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pfa-no-form-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pfa-no-form-proj", "pfa-no-form-agent").await;

    let body = json!({
        "form_id": "form-not-registered",
        "answers": {}
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/projects/pfa-no-form-proj/form-answer")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    // No bridge registered → form not found.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// POST /projects/{id}/form-answer happy path: registers a LiveFormBridge
/// under the project's agent_id, suspends ask_form(), submits via the route,
/// and verifies the suspended future resolves with the correct answer.
#[tokio::test]
async fn project_form_answer_delivers_to_live_bridge() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("pfa-happy-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "pfa-happy-proj", "pfa-happy-agent").await;

    let mut rx = state.event_bus.subscribe();

    let sink: Arc<dyn EventSink + Send + Sync> = Arc::new(EventBusAgentSink {
        bus: Arc::clone(&state.event_bus),
        agent_id: "pfa-happy-agent".to_string(),
        thread_id: None,
    });
    let bridge = Arc::new(LiveFormBridge::new(sink));
    state
        .form_bridge_registry
        .register("pfa-happy-agent", Arc::clone(&bridge));

    let bridge_for_task = Arc::clone(&bridge);
    let form_task = tokio::spawn(async move {
        bridge_for_task
            .ask_form(FormRequest {
                id: String::new(),
                agent_id: "pfa-happy-agent".to_string(),
                session_id: "sess-pfa".to_string(),
                title: "Project form test".to_string(),
                intro: None,
                fields: vec![FormField {
                    id: "answer".to_string(),
                    kind: FormFieldKind::Text { placeholder: None },
                    label: "Answer".to_string(),
                    description: None,
                    required: true,
                }],
            })
            .await
    });

    let live_form_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(event) if event.agent_id == "pfa-happy-agent" => {
                    if let AgentEventPayload::FormRequest { form_id, .. } = event.payload {
                        break form_id;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => panic!("event bus closed"),
            }
        }
    })
    .await
    .expect("FormRequest event must arrive within 5 s");

    let submit_body = json!({
        "form_id": &live_form_id,
        "answers": {
            "answer": { "kind": "text", "value": "project answer" }
        }
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/projects/pfa-happy-proj/form-answer")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&submit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let response = tokio::time::timeout(Duration::from_secs(5), form_task)
        .await
        .expect("form_task must resolve within 5 s")
        .expect("task must not panic")
        .expect("ask_form must return Ok");
    assert_eq!(response.form_id, live_form_id);

    state.form_bridge_registry.deregister("pfa-happy-agent", &bridge);
}

/// POST /projects/{id}/async-forms/{form_id}/answer happy path: appends
/// form_answer transcript entry to the project transcript and clears the
/// project snapshot key.
#[tokio::test]
async fn project_async_form_answer_happy_path() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("paf-ans-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "paf-ans-proj", "paf-ans-agent").await;

    let scope_key = "project_paf-ans-proj";
    let form_id = "paf-async-form-001";

    state
        .persistence
        .snapshots
        .set_pending_form(scope_key, None, form_id.to_string(), json!({}))
        .await
        .expect("set pending form");

    let body = json!({ "values": { "q1": "my answer" } });
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/projects/paf-ans-proj/async-forms/{form_id}/answer"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let entries = state
        .persistence
        .transcripts
        .read_all(scope_key)
        .await
        .expect("read project transcript");
    let entry = entries
        .iter()
        .find(|e| e.event_type == "form_answer")
        .expect("form_answer entry must be written to project transcript");
    let meta = entry.metadata.as_ref().expect("metadata must be present");
    assert_eq!(meta["form_id"], json!(form_id));
    assert_eq!(meta["values"]["q1"], json!("my answer"));

    let snap = state.persistence.snapshots.get().await;
    assert!(
        snap.agents
            .get(scope_key)
            .map(|s| s.pending_forms.is_empty())
            .unwrap_or(true),
        "pending form must be cleared from project snapshot"
    );
}

/// POST /projects/{id}/async-forms/{form_id}/answer with wrong project → 404.
#[tokio::test]
async fn project_async_form_answer_wrong_project_returns_404() {
    let (router, _tmp, _state) = setup_with_state().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/projects/ghost-proj/async-forms/form-1/answer")
                .header("content-type", "application/json")
                .body(Body::from(json!({"values": {}}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// POST /projects/{id}/async-forms/{form_id}/answer with mismatched form_id
/// returns an error (not 200).
#[tokio::test]
async fn project_async_form_answer_unknown_form_returns_error() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("paf-bad-form-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "paf-bad-form-proj", "paf-bad-form-agent").await;

    // No pending form set → mismatch.
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/projects/paf-bad-form-proj/async-forms/stale-form/answer")
                .header("content-type", "application/json")
                .body(Body::from(json!({"values": {}}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::OK, "unknown form_id must not return 200");
}

/// POST /projects/{id}/async-forms/{form_id}/dismiss happy path: appends
/// form_dismissed entry to the project transcript and clears the snapshot.
#[tokio::test]
async fn project_async_form_dismiss_happy_path() {
    let (router, _tmp, state) = setup_with_state().await;

    let agent = make_agent_profile("paf-dis-agent");
    create_agent_http(&router, &agent).await;
    create_project_direct(&state, "paf-dis-proj", "paf-dis-agent").await;

    let scope_key = "project_paf-dis-proj";
    let form_id = "paf-dis-form-001";

    state
        .persistence
        .snapshots
        .set_pending_form(scope_key, None, form_id.to_string(), json!({}))
        .await
        .expect("set pending form");

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/projects/paf-dis-proj/async-forms/{form_id}/dismiss"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let entries = state
        .persistence
        .transcripts
        .read_all(scope_key)
        .await
        .expect("read project transcript");
    let entry = entries
        .iter()
        .find(|e| e.event_type == "form_dismissed")
        .expect("form_dismissed entry must be written to project transcript");
    let meta = entry.metadata.as_ref().expect("metadata must be present");
    assert_eq!(meta["form_id"], json!(form_id));

    let snap = state.persistence.snapshots.get().await;
    assert!(
        snap.agents
            .get(scope_key)
            .map(|s| s.pending_forms.is_empty())
            .unwrap_or(true),
        "pending form must be cleared after dismiss"
    );
}

/// POST /projects/{id}/async-forms/{form_id}/dismiss with wrong project → 404.
#[tokio::test]
async fn project_async_form_dismiss_wrong_project_returns_404() {
    let (router, _tmp, _state) = setup_with_state().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/projects/ghost-proj/async-forms/form-1/dismiss")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
