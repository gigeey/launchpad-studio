use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use ao_engine::AppState;
use ao_engine_tools_core::TasklistServiceHandle;
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
};
use ao_protocol::tasklist::{
    Task, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistStatus,
};
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

async fn setup() -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let mock = MockProcessSupervisor::new(vec![]);
        Arc::new(AppState::new_with_mock(mock).await.expect("init state"))
    };
    let router = build_router(state);
    (router, tmp)
}

/// Variant of [`setup`] that also hands back the `AppState`, for tests that
/// need to drive the tasklist service directly instead of over HTTP.
async fn setup_with_state() -> (axum::Router, tempfile::TempDir, Arc<AppState>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = {
        let _guard = ENV_MUTEX.lock().unwrap();
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

fn sample_tasklist_body(member_id: &str) -> serde_json::Value {
    json!({
        "title": "Agent investigation",
        "description": "Test agent tasklist",
        "groups": [
            {
                "mode": "SEQ",
                "tasks": [
                    {
                        "owner_agent_id": member_id,
                        "prompt": "Investigate the logs",
                        "expected_outputs": []
                    },
                    {
                        "owner_agent_id": member_id,
                        "prompt": "Write a summary",
                        "expected_outputs": []
                    }
                ]
            }
        ]
    })
}

async fn list_agent_tasklists(router: &axum::Router, agent_id: &str) -> serde_json::Value {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&format!("/agents/{}/tasklists", agent_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    serde_json::from_slice(&read_body(resp).await).unwrap()
}

/// Collect every tasklist id present in a `ListTasklistsResponse` (active slot
/// plus the recent list), so assertions don't hinge on which bucket a tasklist
/// lands in after the dispatcher may have advanced it.
fn listed_tasklist_ids(list: &serde_json::Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = list["active"]["id"].as_str() {
        ids.push(id.to_string());
    }
    for tl in list["recent"].as_array().into_iter().flatten() {
        if let Some(id) = tl["id"].as_str() {
            ids.push(id.to_string());
        }
    }
    ids
}

/// One-task SEQ group with a pinned owner — enough to satisfy the non-empty
/// groups constraint on the service create path.
fn single_task_group(agent_id: &str, prompt: &str) -> TaskGroup {
    let group_id = uuid::Uuid::new_v4().to_string();
    let task = Task {
        id: uuid::Uuid::new_v4().to_string(),
        owner_agent_id: agent_id.to_string(),
        prompt: prompt.to_string(),
        expected_outputs: Vec::new(),
        status: TaskStatus::Pending,
        group_id: group_id.clone(),
        attempt_count: 0,
        error_log: Vec::new(),
        comments: Vec::new(),
        attachments: Vec::new(),
        remind_me: None,
        parse_failed: false,
        notification_parse_retry_count: 0,
        assignment: None,
        classifier_token: 0,
        dispatch_token: 0,
    };
    TaskGroup {
        id: group_id,
        mode: TaskGroupMode::Seq,
        tasks: vec![task],
    }
}

#[tokio::test]
async fn create_agent_tasklist_returns_tasklist_id_and_full_state() {
    let (router, _tmp) = setup().await;
    let agent = make_agent_profile("agent-tl-create");
    create_agent(&router, &agent).await;

    let body = sample_tasklist_body(&agent.id);
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{}/tasklists", agent.id))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let returned: Tasklist = serde_json::from_slice(&bytes).unwrap();
    assert!(!returned.id.is_empty(), "tasklist_id should be non-empty");
    assert_eq!(returned.title, "Agent investigation");
    assert_eq!(returned.status, TasklistStatus::Active);
    assert_eq!(returned.groups.len(), 1);
    assert_eq!(returned.groups[0].tasks.len(), 2);
    assert!(!returned.workspace_dir.is_empty());
    assert!(!returned.transcripts_dir.is_empty());
}

#[tokio::test]
async fn create_agent_tasklist_for_unknown_agent_returns_404() {
    let (router, _tmp) = setup().await;

    let body = sample_tasklist_body("nobody");
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/no-such-agent/tasklists")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_agent_tasklists_returns_active_tasklist() {
    let (router, _tmp) = setup().await;
    let agent = make_agent_profile("agent-tl-list");
    create_agent(&router, &agent).await;

    let body = sample_tasklist_body(&agent.id);
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{}/tasklists", agent.id))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let created: Tasklist = serde_json::from_slice(&read_body(resp).await).unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&format!("/agents/{}/tasklists", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let parsed: serde_json::Value =
        serde_json::from_slice(&read_body(resp).await).unwrap();
    assert_eq!(parsed["active"]["id"], created.id);
    assert!(parsed["recent"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_agent_tasklist_returns_full_state() {
    let (router, _tmp) = setup().await;
    let agent = make_agent_profile("agent-tl-get");
    create_agent(&router, &agent).await;

    let body = sample_tasklist_body(&agent.id);
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{}/tasklists", agent.id))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let created: Tasklist = serde_json::from_slice(&read_body(resp).await).unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&format!("/agents/{}/tasklists/{}", agent.id, created.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let fetched: Tasklist = serde_json::from_slice(&read_body(resp).await).unwrap();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.groups.len(), 1);
    assert_eq!(fetched.groups[0].tasks.len(), 2);
}

#[tokio::test]
async fn get_unknown_agent_tasklist_returns_404() {
    let (router, _tmp) = setup().await;
    let agent = make_agent_profile("agent-tl-get-404");
    create_agent(&router, &agent).await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&format!("/agents/{}/tasklists/does-not-exist", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn skip_pending_task_marks_it_skipped() {
    let (router, _tmp) = setup().await;
    let agent = make_agent_profile("agent-tl-skip");
    create_agent(&router, &agent).await;

    // Create an empty shell (Paused) so the feeder doesn't auto-dispatch tasks.
    let shell_body = json!({
        "title": "Skip test shell",
        "description": "",
        "groups": [],
        "allow_empty_groups": true,
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{}/tasklists", agent.id))
                .header("content-type", "application/json")
                .body(Body::from(shell_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let shell: Tasklist = serde_json::from_slice(&read_body(resp).await).unwrap();
    assert_eq!(shell.status, TasklistStatus::Paused);

    // Append a task — for agent scope the list stays Paused and task stays Pending.
    let append_body = json!({
        "prompt": "Investigate the logs",
        "owner_agent_id": agent.id,
        "mode": "SEQ",
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{}/tasklists/{}/tasks", agent.id, shell.id))
                .header("content-type", "application/json")
                .body(Body::from(append_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let with_task: Tasklist = serde_json::from_slice(&read_body(resp).await).unwrap();
    assert_eq!(with_task.status, TasklistStatus::Paused);
    assert_eq!(with_task.groups[0].tasks[0].status, TaskStatus::Pending);
    let task_id = with_task.groups[0].tasks[0].id.clone();

    // Skip the Pending task.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!(
                    "/agents/{}/tasklists/{}/tasks/{}/skip",
                    agent.id, shell.id, task_id
                ))
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let after_skip: Tasklist = serde_json::from_slice(&read_body(resp).await).unwrap();
    let skipped_task = after_skip.groups[0]
        .tasks
        .iter()
        .find(|t| t.id == task_id)
        .expect("task present after skip");
    assert_eq!(skipped_task.status, TaskStatus::Skipped);
}

#[tokio::test]
async fn set_status_stopped_cancels_tasklist() {
    let (router, _tmp) = setup().await;
    let agent = make_agent_profile("agent-tl-stop");
    create_agent(&router, &agent).await;

    let body = sample_tasklist_body(&agent.id);
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{}/tasklists", agent.id))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let created: Tasklist = serde_json::from_slice(&read_body(resp).await).unwrap();
    assert_eq!(created.status, TasklistStatus::Active);

    let stop_body = json!({ "status": "stopped" });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!(
                    "/agents/{}/tasklists/{}/status",
                    agent.id, created.id
                ))
                .header("content-type", "application/json")
                .body(Body::from(stop_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let after: Tasklist = serde_json::from_slice(&read_body(resp).await).unwrap();
    assert_eq!(after.status, TasklistStatus::Cancelled);
    for group in &after.groups {
        for task in &group.tasks {
            assert!(
                matches!(
                    task.status,
                    TaskStatus::Skipped | TaskStatus::InProgress | TaskStatus::Completed
                ),
                "task {} should be Skipped/InProgress/Completed after stop, got {:?}",
                task.id,
                task.status
            );
        }
    }
}

/// A project-scoped tasklist is owned by the agent but belongs to the project
/// surface; it must NOT surface in the agent's personal tasklist listing, or it
/// renders in two places at once (agent chat AND project view). Driven through
/// the production service path so the project_id stamp is applied atomically at
/// creation, exactly as TodoCreate does for a project-scoped agent.
#[tokio::test]
async fn agent_tasklists_excludes_project_scoped_lists() {
    let (router, _tmp, state) = setup_with_state().await;

    let personal_agent = make_agent_profile("agent-personal-tl");
    create_agent(&router, &personal_agent).await;
    let project_agent = make_agent_profile("agent-project-tl");
    create_agent(&router, &project_agent).await;

    // Plain personal tasklist (no project) — should be listed.
    let personal = state
        .tasklist_service
        .create_for_agent(
            &personal_agent.id,
            "Personal work".to_string(),
            vec![single_task_group(&personal_agent.id, "Personal task")],
        )
        .await
        .expect("create personal tasklist");

    // Project-scoped tasklist (atomically stamped) — should be hidden.
    let project_tl = state
        .tasklist_service
        .create_for_agent_with_project(
            &project_agent.id,
            "Project work".to_string(),
            vec![single_task_group(&project_agent.id, "Project task")],
            Some("project-xyz".to_string()),
            None,
        )
        .await
        .expect("create project tasklist");
    assert_eq!(project_tl.project_id.as_deref(), Some("project-xyz"));

    // Personal agent's listing surfaces its plain tasklist.
    let personal_list = list_agent_tasklists(&router, &personal_agent.id).await;
    assert!(
        listed_tasklist_ids(&personal_list).contains(&personal.id),
        "personal tasklist should appear in the agent's listing: {personal_list:?}"
    );

    // Project agent owns only the project-scoped tasklist, which is hidden, so
    // its personal listing is empty.
    let project_list = list_agent_tasklists(&router, &project_agent.id).await;
    assert!(
        !listed_tasklist_ids(&project_list).contains(&project_tl.id),
        "project-scoped tasklist must not appear in the agent's listing: {project_list:?}"
    );
    assert!(
        project_list["active"].is_null(),
        "project-scoped tasklist must not occupy the agent's active slot"
    );
    assert!(
        project_list["recent"].as_array().unwrap().is_empty(),
        "project-scoped tasklist must not appear in the agent's recent list"
    );
}
