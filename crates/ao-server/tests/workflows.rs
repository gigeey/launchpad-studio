use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use ao_engine::AppState;
use ao_process::mock::MockProcessSupervisor;
use ao_server::routes::build_router;

/// Global mutex to serialize setup() calls that modify the process-wide env var.
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

async fn setup_with_workflow() -> (axum::Router, tempfile::TempDir) {
    setup_with_workflow_phases(1).await
}

async fn setup_with_workflow_state(
) -> (Arc<AppState>, axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");

    let wf_dir = tmp.path().join("workflows").join("test-wf");
    tokio::fs::create_dir_all(&wf_dir).await.unwrap();

    for i in 1..=3 {
        let phase_dir = wf_dir.join(format!("phase{}", i));
        tokio::fs::create_dir_all(&phase_dir).await.unwrap();
        tokio::fs::write(phase_dir.join("prompt.md"), format!("# Phase {}", i))
            .await
            .unwrap();
    }

    let workflow_yaml = r#"id: test-wf
name: Test Workflow
version: "1.0"
description: A test workflow
phases:
  - id: phase-1
    name: Phase One
    intent: First
    path: phase1/prompt.md
    inputs: []
    outputs: []
  - id: phase-2
    name: Phase Two
    intent: Second
    path: phase2/prompt.md
    inputs: []
    outputs: []
  - id: phase-3
    name: Phase Three
    intent: Third
    path: phase3/prompt.md
    inputs: []
    outputs: []
"#;
    tokio::fs::write(wf_dir.join("workflow.yaml"), workflow_yaml)
        .await
        .unwrap();

    let state = {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let mock = MockProcessSupervisor::new(vec![]);
        Arc::new(AppState::new_with_mock(mock).await.expect("init state"))
    };

    let router = build_router(state.clone());
    (state, router, tmp)
}

async fn setup_with_workflow_phases(
    num_phases: usize,
) -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");

    // Create a workflow directory with a test workflow
    let wf_dir = tmp.path().join("workflows").join("test-wf");
    tokio::fs::create_dir_all(&wf_dir).await.unwrap();

    let mut phases_yaml = String::new();
    for i in 1..=num_phases {
        let phase_dir = wf_dir.join(format!("phase{}", i));
        tokio::fs::create_dir_all(&phase_dir).await.unwrap();
        tokio::fs::write(
            phase_dir.join("prompt.md"),
            format!("# Phase {}\nDo something.", i),
        )
        .await
        .unwrap();

        phases_yaml.push_str(&format!(
            r#"  - id: phase-{}
    name: Phase {}
    intent: Do thing {}
    path: phase{}/prompt.md
    inputs: []
    outputs:
      - id: result{}
        filename: result{}.json
        description: The result
"#,
            i, i, i, i, i, i
        ));
    }

    let workflow_yaml = format!(
        r#"id: test-wf
name: Test Workflow
version: "1.0"
description: A test workflow for integration tests
phases:
{}
"#,
        phases_yaml
    );
    tokio::fs::write(wf_dir.join("workflow.yaml"), workflow_yaml)
        .await
        .unwrap();

    let state = {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let mock = MockProcessSupervisor::new(vec![]);
        Arc::new(AppState::new_with_mock(mock).await.expect("init state"))
    };

    let router = build_router(state);
    (router, tmp)
}

async fn read_body(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes()
        .to_vec()
}

#[tokio::test]
async fn test_get_workflows_returns_empty_list() {
    let (router, _tmp) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/workflows")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    let summaries: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert!(summaries.is_empty());
}

#[tokio::test]
async fn test_get_workflows_returns_summaries() {
    let (router, _tmp) = setup_with_workflow().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/workflows")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    let summaries: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0]["id"], "test-wf");
    assert_eq!(summaries[0]["name"], "Test Workflow");
    // New metadata fields.
    assert_eq!(summaries[0]["source"], "user");
    assert!(
        summaries[0]["updated_on"].is_string(),
        "expected updated_on to be populated from file mtime, got {}",
        summaries[0]["updated_on"]
    );
    // No tasks yet → last_run should be absent (serialized as null/missing).
    assert!(
        summaries[0].get("last_run").map_or(true, |v| v.is_null()),
        "expected last_run to be null when no tasks exist, got {:?}",
        summaries[0].get("last_run")
    );
}

#[tokio::test]
async fn test_get_workflows_populates_last_run_after_task_created() {
    let (state, router, _tmp) = setup_with_workflow_state().await;

    // Create a task for the workflow — its `created` timestamp becomes last_run.
    let task_id = state
        .workflow_runner
        .create_task("test-wf", "Test Project", None, None)
        .await
        .unwrap();
    let snapshot = state.workflow_runner.get_task_state(&task_id).await.unwrap();
    let expected_created = snapshot.created;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/workflows")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    let summaries: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(summaries.len(), 1);
    let last_run_str = summaries[0]["last_run"]
        .as_str()
        .expect("last_run should be a string after a task is created");
    let last_run: chrono::DateTime<chrono::Utc> = last_run_str.parse().unwrap();
    assert_eq!(last_run, expected_created);
}

#[tokio::test]
async fn test_get_workflow_definition() {
    let (router, _tmp) = setup_with_workflow().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/workflows/test-wf")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    let def: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(def["id"], "test-wf");
    assert_eq!(def["phases"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_get_workflow_not_found() {
    let (router, _tmp) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/workflows/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_create_task() {
    let (router, _tmp) = setup_with_workflow().await;

    let body = serde_json::json!({
        "project_name": "My Project"
    });

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/workflows/test-wf/tasks")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(result["task_id"].as_str().unwrap().starts_with("test-wf_"));
}

#[tokio::test]
async fn test_list_tasks_empty() {
    let (router, _tmp) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/tasks")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn test_refresh_workflows() {
    let (router, _tmp) = setup_with_workflow().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/workflows/refresh")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(result["count"], 1);
}

#[tokio::test]
async fn test_list_tasks_completed_phases_count() {
    let (state, router, _tmp) = setup_with_workflow_state().await;

    // Create a task (3 phases)
    let task_id = state
        .workflow_runner
        .create_task("test-wf", "Test Project", None, None)
        .await
        .unwrap();

    // Complete phase-1
    state
        .workflow_runner
        .complete_phase(&task_id, "phase-1")
        .await
        .unwrap();

    // Fail phase-2 (this should NOT count as completed)
    state
        .workflow_runner
        .fail_phase(&task_id, "phase-2", "something went wrong")
        .await
        .unwrap();

    // List tasks and verify completed_phases = 1 (only phase-1 is completed, phase-2 is failed)
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/tasks")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["completed_phases"], 1);
    assert_eq!(tasks[0]["total_phases"], 3);
}

#[tokio::test]
async fn test_start_task_transitions_pending_to_running() {
    let (state, router, _tmp) = setup_with_workflow_state().await;

    // Create a task (status = Pending)
    let task_id = state
        .workflow_runner
        .create_task("test-wf", "Test Project", None, None)
        .await
        .unwrap();

    // Start the task via the API
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/tasks/{}/start", task_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    let snapshot: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(snapshot["status"], "running");
}

#[tokio::test]
async fn test_start_task_non_pending_returns_error() {
    let (state, router, _tmp) = setup_with_workflow_state().await;

    // Create and start a task
    let task_id = state
        .workflow_runner
        .create_task("test-wf", "Test Project", None, None)
        .await
        .unwrap();
    state.workflow_runner.start_task(&task_id).await.unwrap();

    // Try to start again — should fail
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/tasks/{}/start", task_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_list_tasks_includes_status() {
    let (state, router, _tmp) = setup_with_workflow_state().await;

    // Create a task (Pending)
    state
        .workflow_runner
        .create_task("test-wf", "Test Project", None, None)
        .await
        .unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/tasks")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["status"], "pending");
}

#[tokio::test]
async fn test_resume_task_clears_paused_and_requeues() {
    let (state, router, _tmp) = setup_with_workflow_state().await;

    // Create and start a task
    let task_id = state
        .workflow_runner
        .create_task("test-wf", "Test Project", None, None)
        .await
        .unwrap();
    state.workflow_runner.start_task(&task_id).await.unwrap();

    // Pause phase-1
    state
        .workflow_runner
        .pause_phase(&task_id, "phase-1", "Missing inputs")
        .await
        .unwrap();

    // Verify phase-1 is paused
    let snapshot = state.workflow_runner.get_task_state(&task_id).await.unwrap();
    assert!(matches!(
        snapshot.phases.get("phase-1").unwrap().status,
        ao_protocol::workflow::PhaseStatus::Paused
    ));

    // Resume via API
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/tasks/{}/resume", task_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Give the queue manager a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify the paused phase was cleared from the snapshot
    let snapshot = state.workflow_runner.get_task_state(&task_id).await.unwrap();
    // Phase-1 should either be absent (cleared) or re-evaluated
    // Since there are no actual inputs declared on these test phases,
    // the queue manager will proceed to execute the phase
    assert!(
        !snapshot.phases.contains_key("phase-1")
            || !matches!(
                snapshot.phases.get("phase-1").unwrap().status,
                ao_protocol::workflow::PhaseStatus::Paused
            )
    );
}

#[tokio::test]
async fn test_resume_task_returns_400_when_no_paused_phase() {
    let (state, router, _tmp) = setup_with_workflow_state().await;

    // Create a task (no paused phases)
    let task_id = state
        .workflow_runner
        .create_task("test-wf", "Test Project", None, None)
        .await
        .unwrap();
    state.workflow_runner.start_task(&task_id).await.unwrap();

    // Resume should return 400 since no phase is paused
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/tasks/{}/resume", task_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_resume_task_returns_404_when_not_found() {
    let (_, router, _tmp) = setup_with_workflow_state().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/tasks/nonexistent-task/resume")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
