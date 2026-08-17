use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use ao_engine::AppState;
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
};
use ao_protocol::assignment::{Assignment, AssignmentRun, AssignmentRunStatus};
use ao_server::routes::build_router;

/// Serialize setup calls that mutate the process-wide env var.
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
    let tmp = tempfile::tempdir().expect("create temp dir");
    let state = {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let mock = MockProcessSupervisor::new(vec![]);
        Arc::new(AppState::new_with_mock(mock).await.expect("init state"))
    };
    (build_router(state), tmp)
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
    assert_eq!(resp.status(), StatusCode::OK, "create agent failed");
}

// ----- helpers -----

async fn create_cron_assignment(router: &axum::Router, agent_id: &str) -> Assignment {
    let body = serde_json::json!({
        "name": "Daily standup",
        "instruction": "Write standup notes.",
        "trigger": {
            "type": "Cron",
            "cron_expr": "0 9 * * 1-5",
            "is_recurring": true
        }
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/agents/{agent_id}/assignments"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "create cron assignment failed");
    let bytes = read_body(resp).await;
    serde_json::from_slice(&bytes).expect("parse assignment")
}

async fn create_webhook_assignment(router: &axum::Router, agent_id: &str) -> Assignment {
    let body = serde_json::json!({
        "name": "Inbound hook",
        "instruction": "Process the inbound event.",
        "trigger": {
            "type": "Webhook",
            "token": "my-secret-token"
        }
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/agents/{agent_id}/assignments"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "create webhook assignment failed");
    let bytes = read_body(resp).await;
    serde_json::from_slice(&bytes).expect("parse assignment")
}

async fn create_agent_watch_assignment(router: &axum::Router, agent_id: &str) -> axum::response::Response {
    let body = serde_json::json!({
        "name": "Finance email watcher",
        "instruction": "Summarize the new email from finance.",
        "trigger": {
            "type": "AgentWatch",
            "instruction": "Check my inbox for a new email from finance",
            "poll_interval_secs": 900
        }
    });
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/agents/{agent_id}/assignments"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

// ----- tests -----

#[tokio::test]
async fn test_list_assignments_empty_for_new_agent() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-list-empty");
    create_agent(&router, &profile).await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/agent-list-empty/assignments")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let assignments: Vec<Assignment> = serde_json::from_slice(&bytes).unwrap();
    assert!(assignments.is_empty());
}

#[tokio::test]
async fn test_create_cron_assignment_returns_correct_shape() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-cron");
    create_agent(&router, &profile).await;

    let assignment = create_cron_assignment(&router, "agent-cron").await;

    assert!(!assignment.id.is_empty());
    assert_eq!(assignment.agent_id, "agent-cron");
    assert_eq!(assignment.name, "Daily standup");
    assert_eq!(assignment.instruction, "Write standup notes.");
    assert!(assignment.enabled);
    // Cron assignments get a next_fire_at computed at creation time.
    assert!(assignment.next_fire_at.is_some(), "Cron assignment must have next_fire_at");
    assert!(assignment.last_run_at.is_none());
}

#[tokio::test]
async fn test_create_webhook_assignment_returns_correct_shape() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-webhook");
    create_agent(&router, &profile).await;

    let assignment = create_webhook_assignment(&router, "agent-webhook").await;

    assert!(!assignment.id.is_empty());
    assert_eq!(assignment.agent_id, "agent-webhook");
    assert_eq!(assignment.name, "Inbound hook");
    assert!(assignment.enabled);
    // Webhook assignments never have a schedule-based next_fire_at.
    assert!(assignment.next_fire_at.is_none(), "Webhook assignment must not have next_fire_at");

    // Trigger type tag is preserved in the response.
    let body_json = serde_json::to_value(&assignment).unwrap();
    assert_eq!(body_json["trigger"]["type"], "Webhook");
    assert_eq!(body_json["trigger"]["token"], "my-secret-token");
}

#[tokio::test]
async fn test_create_agent_watch_assignment_returns_correct_shape() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-watch-create");
    create_agent(&router, &profile).await;

    let resp = create_agent_watch_assignment(&router, "agent-watch-create").await;
    assert_eq!(resp.status(), StatusCode::OK, "a well-formed AgentWatch trigger must be accepted");

    let bytes = read_body(resp).await;
    let assignment: Assignment = serde_json::from_slice(&bytes).expect("parse assignment");

    assert!(!assignment.id.is_empty());
    assert_eq!(assignment.agent_id, "agent-watch-create");
    assert!(assignment.enabled);
    // Poll-ASAP on creation, same convention as ConnectorEvent, so the first
    // tick seeds the dedup scratchpad's baseline.
    assert!(assignment.next_fire_at.is_some(), "AgentWatch assignment must poll ASAP after creation");

    let body_json = serde_json::to_value(&assignment).unwrap();
    assert_eq!(body_json["trigger"]["type"], "AgentWatch");
    assert_eq!(body_json["trigger"]["poll_interval_secs"], 900);
}

#[tokio::test]
async fn test_create_agent_watch_assignment_with_blank_instruction_returns_400() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-watch-blank");
    create_agent(&router, &profile).await;

    let body = serde_json::json!({
        "name": "Broken watcher",
        "instruction": "Summarize whatever fired this.",
        "trigger": {
            "type": "AgentWatch",
            "instruction": "   ",
            "poll_interval_secs": 300
        }
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/agent-watch-blank/assignments")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a blank-instruction AgentWatch trigger must still be rejected"
    );
}

#[tokio::test]
async fn test_create_agent_watch_assignment_below_poll_floor_returns_400() {
    // Proves `AssignmentTrigger::validate()` is actually wired into the
    // create route (not just covered by its own unit test in ao-protocol) —
    // a poll_interval_secs below MIN_AGENT_WATCH_POLL_INTERVAL_SECS (900)
    // must be rejected here, at the HTTP layer. Explicitly clears the
    // demo-only `MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR` override
    // first so this assertion can't silently pass or fail depending on
    // whatever a developer's shell happens to have exported — it must hold
    // with the override unset.
    let override_key = ao_protocol::assignment::MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR;
    let prev_override = {
        let _guard = ENV_MUTEX.lock().unwrap();
        let prev = std::env::var(override_key).ok();
        std::env::remove_var(override_key);
        prev
    };

    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-watch-poll-floor");
    create_agent(&router, &profile).await;

    let body = serde_json::json!({
        "name": "Too-frequent watcher",
        "instruction": "Summarize whatever fired this.",
        "trigger": {
            "type": "AgentWatch",
            "instruction": "Check my inbox for a new email from finance",
            "poll_interval_secs": 60
        }
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/agent-watch-poll-floor/assignments")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "an AgentWatch trigger with poll_interval_secs below the 900s floor must be rejected at create time"
    );

    let _guard = ENV_MUTEX.lock().unwrap();
    match prev_override {
        Some(v) => std::env::set_var(override_key, v),
        None => std::env::remove_var(override_key),
    }
}

#[tokio::test]
async fn test_create_agent_watch_assignment_exceeding_cap_returns_400() {
    // MAX_ACTIVE_AGENT_WATCHES_PER_AGENT is 10 — the 11th enabled AgentWatch
    // for the same agent must be rejected at create time.
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-watch-cap");
    create_agent(&router, &profile).await;

    for _ in 0..10 {
        let resp = create_agent_watch_assignment(&router, "agent-watch-cap").await;
        assert_eq!(resp.status(), StatusCode::OK, "the first 10 AgentWatch assignments must be accepted");
    }

    let resp = create_agent_watch_assignment(&router, "agent-watch-cap").await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "the 11th active AgentWatch assignment for the same agent must be rejected"
    );
}

#[tokio::test]
async fn test_patch_enable_agent_watch_exceeding_cap_returns_400() {
    // Same cap, exercised through the enable/patch path rather than create:
    // a disabled 11th AgentWatch must still be blocked from being enabled
    // once the agent already has 10 active.
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-watch-enable-cap");
    create_agent(&router, &profile).await;

    for _ in 0..10 {
        let resp = create_agent_watch_assignment(&router, "agent-watch-enable-cap").await;
        assert_eq!(resp.status(), StatusCode::OK, "the first 10 AgentWatch assignments must be accepted");
    }

    let eleventh_body = serde_json::json!({
        "name": "Eleventh watcher (disabled)",
        "instruction": "Summarize whatever fired this.",
        "trigger": {
            "type": "AgentWatch",
            "instruction": "Check my inbox for a new email from finance",
            "poll_interval_secs": 900
        },
        "enabled": false
    });
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/agent-watch-enable-cap/assignments")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&eleventh_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK, "a disabled 11th AgentWatch must still be creatable");
    let bytes = read_body(create_resp).await;
    let eleventh: Assignment = serde_json::from_slice(&bytes).expect("parse assignment");
    assert!(!eleventh.enabled);

    let patch_body = serde_json::json!({ "enabled": true });
    let patch_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(format!("/assignments/{}", eleventh.id))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&patch_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        patch_resp.status(),
        StatusCode::BAD_REQUEST,
        "enabling the 11th AgentWatch must be rejected once the agent already has 10 active"
    );
}

#[tokio::test]
async fn test_get_assignment_by_id() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-get");
    create_agent(&router, &profile).await;
    let created = create_cron_assignment(&router, "agent-get").await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/assignments/{}", created.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let got: Assignment = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(got.id, created.id);
    assert_eq!(got.name, "Daily standup");
}

#[tokio::test]
async fn test_list_assignments_returns_created_row() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-list");
    create_agent(&router, &profile).await;
    let created = create_cron_assignment(&router, "agent-list").await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/agent-list/assignments")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let assignments: Vec<Assignment> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].id, created.id);
}

#[tokio::test]
async fn test_patch_assignment_enable_disable() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-patch-enable");
    create_agent(&router, &profile).await;
    let created = create_cron_assignment(&router, "agent-patch-enable").await;
    assert!(created.enabled);

    // Disable the assignment.
    let patch_body = serde_json::json!({ "enabled": false });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(format!("/assignments/{}", created.id))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&patch_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let patched: Assignment = serde_json::from_slice(&bytes).unwrap();
    assert!(!patched.enabled, "assignment should be disabled after patch");
    // Other fields must be unchanged.
    assert_eq!(patched.name, "Daily standup");
    assert_eq!(patched.id, created.id);
}

#[tokio::test]
async fn test_patch_assignment_rename_and_update_instruction() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-patch-rename");
    create_agent(&router, &profile).await;
    let created = create_cron_assignment(&router, "agent-patch-rename").await;

    let patch_body = serde_json::json!({
        "name": "Evening recap",
        "instruction": "Summarize the day's events."
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(format!("/assignments/{}", created.id))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&patch_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let patched: Assignment = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(patched.name, "Evening recap");
    assert_eq!(patched.instruction, "Summarize the day's events.");
    // enabled should still be the default (true).
    assert!(patched.enabled);
}

#[tokio::test]
async fn test_list_runs_empty_initially() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-runs-empty");
    create_agent(&router, &profile).await;
    let created = create_cron_assignment(&router, "agent-runs-empty").await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/assignments/{}/runs", created.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let runs: Vec<AssignmentRun> = serde_json::from_slice(&bytes).unwrap();
    assert!(runs.is_empty(), "newly created assignment must have no runs");
}

#[tokio::test]
async fn test_delete_assignment() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-delete");
    create_agent(&router, &profile).await;
    let created = create_cron_assignment(&router, "agent-delete").await;

    // Delete the assignment.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/assignments/{}", created.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // GET should now return 404.
    let resp2 = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/assignments/{}", created.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_list_assignments_for_nonexistent_agent_returns_404() {
    let (router, _tmp) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/ghost-agent/assignments")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_nonexistent_assignment_returns_404() {
    let (router, _tmp) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/assignments/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_patch_nonexistent_assignment_returns_404() {
    let (router, _tmp) = setup().await;

    let patch_body = serde_json::json!({ "enabled": false });
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri("/assignments/ghost-assignment")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&patch_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_nonexistent_assignment_returns_404() {
    let (router, _tmp) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/assignments/ghost-assignment")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_list_runs_nonexistent_assignment_returns_404() {
    let (router, _tmp) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/assignments/ghost-assignment/runs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_create_assignment_with_invalid_cron_returns_400() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-bad-cron");
    create_agent(&router, &profile).await;

    let body = serde_json::json!({
        "name": "Bad schedule",
        "instruction": "Do something.",
        "trigger": {
            "type": "Cron",
            "cron_expr": "not-a-valid-cron",
            "is_recurring": true
        }
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/agent-bad-cron/assignments")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_assignment_for_nonexistent_agent_returns_404() {
    let (router, _tmp) = setup().await;

    let body = serde_json::json!({
        "name": "Orphan",
        "instruction": "Do something.",
        "trigger": { "type": "Webhook", "token": null }
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/ghost-agent/assignments")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_assignments_are_scoped_per_agent() {
    let (router, _tmp) = setup().await;
    let profile_a = make_test_profile("agent-scope-a");
    let profile_b = make_test_profile("agent-scope-b");
    create_agent(&router, &profile_a).await;
    create_agent(&router, &profile_b).await;

    create_cron_assignment(&router, "agent-scope-a").await;
    create_webhook_assignment(&router, "agent-scope-b").await;

    let resp_a = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/agent-scope-a/assignments")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes_a = read_body(resp_a).await;
    let assignments_a: Vec<Assignment> = serde_json::from_slice(&bytes_a).unwrap();
    assert_eq!(assignments_a.len(), 1);
    assert_eq!(assignments_a[0].agent_id, "agent-scope-a");

    let resp_b = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/agent-scope-b/assignments")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes_b = read_body(resp_b).await;
    let assignments_b: Vec<Assignment> = serde_json::from_slice(&bytes_b).unwrap();
    assert_eq!(assignments_b.len(), 1);
    assert_eq!(assignments_b[0].agent_id, "agent-scope-b");
}

#[tokio::test]
async fn test_trigger_assignment_returns_202_with_queued_run() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-trigger");
    create_agent(&router, &profile).await;
    let created = create_cron_assignment(&router, "agent-trigger").await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/assignments/{}/trigger", created.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let bytes = read_body(resp).await;
    let run: AssignmentRun = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(run.assignment_id, created.id);
    assert_eq!(run.status, AssignmentRunStatus::Queued);
    assert!(run.thread_id.is_some());
}

/// A manual trigger that supplies a structured `payload` must still succeed
/// and queue a run — the legacy endpoint no longer hardcodes
/// `event_context: None` when the caller actually has event data to give it.
#[tokio::test]
async fn test_trigger_assignment_with_payload_still_returns_202_with_queued_run() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-trigger-payload");
    create_agent(&router, &profile).await;
    let created = create_webhook_assignment(&router, "agent-trigger-payload").await;

    let body = serde_json::json!({
        "token": "my-secret-token",
        "payload": { "pull_request": { "number": 7, "title": "Sample PR" } }
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/assignments/{}/trigger", created.id))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let run: AssignmentRun = serde_json::from_slice(&read_body(resp).await).unwrap();
    assert_eq!(run.assignment_id, created.id);
    assert_eq!(run.status, AssignmentRunStatus::Queued);
}

#[tokio::test]
async fn test_trigger_nonexistent_assignment_returns_404() {
    let (router, _tmp) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/assignments/ghost-assignment/trigger")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ----- thread_policy -----

#[tokio::test]
async fn test_create_cron_assignment_defaults_thread_policy_to_main() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-policy-default-cron");
    create_agent(&router, &profile).await;

    // create_cron_assignment omits thread_policy entirely — the route must
    // resolve the trigger-dependent default (Cron -> Main).
    let assignment = create_cron_assignment(&router, "agent-policy-default-cron").await;
    let body_json = serde_json::to_value(&assignment).unwrap();
    assert_eq!(body_json["thread_policy"], "main");
    assert!(
        body_json.get("dedicated_thread_id").is_none(),
        "dedicated_thread_id must be omitted when unset"
    );
}

#[tokio::test]
async fn test_create_webhook_assignment_defaults_thread_policy_to_fresh() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-policy-default-webhook");
    create_agent(&router, &profile).await;

    // create_webhook_assignment omits thread_policy entirely — the route
    // must resolve the trigger-dependent default (Webhook -> Fresh).
    let assignment = create_webhook_assignment(&router, "agent-policy-default-webhook").await;
    let body_json = serde_json::to_value(&assignment).unwrap();
    assert_eq!(body_json["thread_policy"], "fresh");
}

#[tokio::test]
async fn test_create_assignment_with_explicit_thread_policy() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-policy-explicit");
    create_agent(&router, &profile).await;

    let body = serde_json::json!({
        "name": "Coach check-in",
        "instruction": "Check in with the user.",
        "trigger": { "type": "Cron", "cron_expr": "0 9 * * *", "is_recurring": true },
        "thread_policy": "main"
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/agent-policy-explicit/assignments")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let assignment: Assignment = serde_json::from_slice(&bytes).expect("parse assignment");
    let body_json = serde_json::to_value(&assignment).unwrap();
    assert_eq!(body_json["thread_policy"], "main");
}

#[tokio::test]
async fn test_patch_assignment_updates_thread_policy() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-policy-patch");
    create_agent(&router, &profile).await;
    let created = create_cron_assignment(&router, "agent-policy-patch").await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(format!("/assignments/{}", created.id))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({ "thread_policy": "dedicated" }))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let updated: Assignment = serde_json::from_slice(&bytes).expect("parse assignment");
    let body_json = serde_json::to_value(&updated).unwrap();
    assert_eq!(body_json["thread_policy"], "dedicated");
}

#[tokio::test]
async fn test_trigger_assignment_with_main_policy_uses_agent_default_thread() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-trigger-main");
    create_agent(&router, &profile).await;

    let body = serde_json::json!({
        "name": "Coach check-in",
        "instruction": "Check in with the user.",
        "trigger": { "type": "Webhook", "token": null },
        "thread_policy": "main"
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/agent-trigger-main/assignments")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = read_body(resp).await;
    let created: Assignment = serde_json::from_slice(&bytes).expect("parse assignment");

    let trigger_resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/assignments/{}/trigger", created.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(trigger_resp.status(), StatusCode::ACCEPTED);
    let run_bytes = read_body(trigger_resp).await;
    let run: AssignmentRun = serde_json::from_slice(&run_bytes).unwrap();
    assert_eq!(
        run.thread_id.as_deref(),
        Some("default-agent-trigger-main"),
        "Main policy must record the agent's default thread id on the run"
    );
}

#[tokio::test]
async fn test_trigger_assignment_with_dedicated_policy_reuses_thread_across_triggers() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-trigger-dedicated");
    create_agent(&router, &profile).await;

    let body = serde_json::json!({
        "name": "Morning brief",
        "instruction": "Write today's brief.",
        "trigger": { "type": "Webhook", "token": null },
        "thread_policy": "dedicated"
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/agent-trigger-dedicated/assignments")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = read_body(resp).await;
    let created: Assignment = serde_json::from_slice(&bytes).expect("parse assignment");

    let trigger_once = |router: axum::Router, assignment_id: String| async move {
        let trigger_resp = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/assignments/{}/trigger", assignment_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(trigger_resp.status(), StatusCode::ACCEPTED);
        let run_bytes = read_body(trigger_resp).await;
        let run: AssignmentRun = serde_json::from_slice(&run_bytes).unwrap();
        run
    };

    let run1 = trigger_once(router.clone(), created.id.clone()).await;
    let run2 = trigger_once(router.clone(), created.id.clone()).await;

    assert!(run1.thread_id.is_some());
    assert_eq!(
        run1.thread_id, run2.thread_id,
        "Dedicated policy must reuse the same thread across triggers"
    );

    // The assignment row itself now records the claimed dedicated thread.
    let get_resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/assignments/{}", created.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let get_bytes = read_body(get_resp).await;
    let refetched: Assignment = serde_json::from_slice(&get_bytes).unwrap();
    assert_eq!(refetched.dedicated_thread_id, run1.thread_id);
}

// ----- working_directory / expires_at (model parity) -----

#[tokio::test]
async fn test_create_get_patch_round_trips_working_directory_and_expires_at() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-parity");
    create_agent(&router, &profile).await;

    let expires_at = "2030-01-01T00:00:00Z";
    let body = serde_json::json!({
        "name": "Focused reminder",
        "instruction": "Check the repo.",
        "working_directory": "/repo/project",
        "trigger": { "type": "Cron", "cron_expr": "0 9 * * *", "is_recurring": true },
        "expires_at": expires_at
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/agent-parity/assignments")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let created: Assignment = serde_json::from_slice(&bytes).expect("parse assignment");

    assert_eq!(created.working_directory.as_deref(), Some("/repo/project"));
    let created_json = serde_json::to_value(&created).unwrap();
    assert_eq!(created_json["expires_at"], expires_at);

    // GET round-trips both fields.
    let get_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/assignments/{}", created.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let get_bytes = read_body(get_resp).await;
    let fetched: Assignment = serde_json::from_slice(&get_bytes).unwrap();
    assert_eq!(fetched.working_directory.as_deref(), Some("/repo/project"));
    assert!(fetched.expires_at.is_some());

    // PATCH updates both fields.
    let new_expires_at = "2031-06-15T12:00:00Z";
    let patch_body = serde_json::json!({
        "working_directory": "/repo/other-project",
        "expires_at": new_expires_at
    });
    let patch_resp = router
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(format!("/assignments/{}", created.id))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&patch_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(patch_resp.status(), StatusCode::OK);
    let patch_bytes = read_body(patch_resp).await;
    let patched: Assignment = serde_json::from_slice(&patch_bytes).unwrap();
    assert_eq!(patched.working_directory.as_deref(), Some("/repo/other-project"));
    let patched_json = serde_json::to_value(&patched).unwrap();
    assert_eq!(patched_json["expires_at"], new_expires_at);
}

#[tokio::test]
async fn test_create_assignment_omits_working_directory_and_expires_at_when_absent() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("agent-parity-absent");
    create_agent(&router, &profile).await;

    let created = create_cron_assignment(&router, "agent-parity-absent").await;
    assert!(created.working_directory.is_none());
    assert!(created.expires_at.is_none());

    let body_json = serde_json::to_value(&created).unwrap();
    assert!(body_json.get("working_directory").is_none());
    assert!(body_json.get("expires_at").is_none());
}
