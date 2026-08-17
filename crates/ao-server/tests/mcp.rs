use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use ao_engine::AppState;
use ao_engine_tools_core::context::RunnerContext;
use ao_engine_tools_core::output::ToolOutput;
use ao_engine_tools_core::policy::LoadPolicy;
use ao_engine_tools_core::tool::IoTool;
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use ao_protocol::error::AoError;
use ao_server::routes::build_router;

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_test_profile(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("MCP Test Agent {}", id),
        description: "A test agent for MCP".to_string(),
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

#[tokio::test]
async fn mcp_unknown_agent_returns_404() {
    let (router, _state, _tmp) = setup().await;

    let req_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    }))
    .unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/mcp/no-such-agent/some-session-uuid")
                .header("content-type", "application/json")
                .body(Body::from(req_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mcp_unknown_session_returns_404() {
    let (router, state, _tmp) = setup().await;

    // Create the agent profile.
    let profile = make_test_profile("test-mcp-no-session");
    let create_body = serde_json::to_string(&profile).unwrap();
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(create_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    // Don't register any session — expect 404 from session lookup.
    let _ = state; // keep state alive
    let mcp_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    }))
    .unwrap();

    let mcp_resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/mcp/test-mcp-no-session/nonexistent-session-uuid")
                .header("content-type", "application/json")
                .body(Body::from(mcp_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(mcp_resp.status(), StatusCode::NOT_FOUND);
    let body = read_json(mcp_resp).await;
    assert!(body["error"].as_str().unwrap().contains("session not found"));
}

#[tokio::test]
async fn mcp_agent_id_mismatch_returns_400() {
    let (router, state, _tmp) = setup().await;

    // Create agent A and agent B profiles.
    for id in &["mcp-agent-a", "mcp-agent-b"] {
        let profile = make_test_profile(id);
        let create_body = serde_json::to_string(&profile).unwrap();
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/agents")
                    .header("content-type", "application/json")
                    .body(Body::from(create_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Register a session for agent-a.
    let session_id = "test-session-for-a".to_string();
    state
        .mcp_sessions
        .register_session(
            session_id.clone(),
            "mcp-agent-a".to_string(),
            PathBuf::from("/tmp"),
            None,
        )
        .expect("register session");

    // Call the route with agent-b in the URL but agent-a's session_id.
    let mcp_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    }))
    .unwrap();

    let mcp_resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/mcp-agent-b/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(mcp_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(mcp_resp.status(), StatusCode::BAD_REQUEST);
    let body = read_json(mcp_resp).await;
    assert!(body["error"].as_str().unwrap().contains("mismatch"));
}

#[tokio::test]
async fn mcp_tools_list_returns_at_least_one_tool() {
    let (router, state, _tmp) = setup().await;

    // Create the agent profile.
    let profile = make_test_profile("test-mcp-agent");
    let create_body = serde_json::to_string(&profile).unwrap();

    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(create_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    // Register a session for this agent.
    let session_id = "test-session-tools-list".to_string();
    state
        .mcp_sessions
        .register_session(
            session_id.clone(),
            "test-mcp-agent".to_string(),
            PathBuf::from("/tmp"),
            None,
        )
        .expect("register session");

    // Send a tools/list MCP request.
    let mcp_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    }))
    .unwrap();

    let mcp_resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/test-mcp-agent/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(mcp_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(mcp_resp.status(), StatusCode::OK);

    let body = read_json(mcp_resp).await;
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);

    let tools = body["result"]["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty(), "tools/list should return at least one tool");

    // Verify each tool entry has a name field.
    for tool in tools {
        assert!(tool["name"].is_string(), "each tool must have a string name");
    }
}

#[tokio::test]
async fn mcp_invalid_json_returns_400() {
    let (router, state, _tmp) = setup().await;

    // Create the agent first.
    let profile = make_test_profile("test-mcp-bad-json");
    let create_body = serde_json::to_string(&profile).unwrap();
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(create_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    // Register a session.
    let session_id = "test-session-bad-json".to_string();
    state
        .mcp_sessions
        .register_session(
            session_id.clone(),
            "test-mcp-bad-json".to_string(),
            PathBuf::from("/tmp"),
            None,
        )
        .expect("register session");

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/test-mcp-bad-json/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from("not valid json {{"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Regression for the MCP-route skill-registry wiring: a CLI-spawned agent
/// invoking `RunSkill` over `POST /mcp/:agent/:session` must resolve a skill
/// that is enabled on its profile. Before the fix the route built a
/// `RunnerContext` without `.with_skill_registry(...)`, so every CLI `RunSkill`
/// call resolved against an empty registry and returned "not found" — including
/// for skills already advertised in the agent's system prompt.
#[tokio::test]
async fn mcp_runskill_resolves_profile_enabled_skill() {
    let (router, state, _tmp) = setup().await;
    let agent_id = "mcp-runskill";

    // Create the agent.
    let profile = make_test_profile(agent_id);
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&profile).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    // Write a skill via the route — this writes SKILL.md to the user pool and
    // appends the slug ("mcp-ping") to the persisted AgentProfile.skills list.
    let write_body = json!({
        "title": "Mcp Ping",
        "description": "regression skill for the RunSkill MCP route",
        "content": "PING_BODY_42",
    });
    let write_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/agents/{agent_id}/skills"))
                .header("content-type", "application/json")
                .body(Body::from(write_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(write_resp.status(), StatusCode::CREATED, "Failed to write skill");

    // Register an MCP session for this agent.
    let session_id = "mcp-runskill-session".to_string();
    state
        .mcp_sessions
        .register_session(
            session_id.clone(),
            agent_id.to_string(),
            PathBuf::from("/tmp"),
            None,
        )
        .expect("register session");

    // Invoke RunSkill for the just-registered, profile-enabled skill.
    let call_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "RunSkill", "arguments": { "skill": "mcp-ping" } }
    }))
    .unwrap();
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(call_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = read_json(resp).await;
    let result = &body["result"];
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert_eq!(
        result["isError"].as_bool().unwrap_or(false),
        false,
        "RunSkill should resolve a profile-enabled skill, but returned an error: {text}"
    );
    // The MCP route sets inline_skill_via_tool_result=true, so RunSkill
    // returns the substituted body directly (no "Launching skill" wrapper).
    assert!(
        text.contains("PING_BODY_42"),
        "expected skill body in inline-via-tool-result path, got: {text}"
    );
    assert!(
        !text.contains("not found"),
        "skill registry was empty on the MCP route: {text}"
    );

    // Negative control: a genuinely unknown skill still 404s, proving the
    // registry is actually consulted rather than blindly succeeding.
    let bogus_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "RunSkill", "arguments": { "skill": "no-such-skill-xyz" } }
    }))
    .unwrap();
    let bogus_resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(bogus_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bogus_resp.status(), StatusCode::OK);
    let bogus = read_json(bogus_resp).await;
    let bogus_result = &bogus["result"];
    assert_eq!(
        bogus_result["isError"].as_bool().unwrap_or(false),
        true,
        "unknown skill should report an error"
    );
    assert!(
        bogus_result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("not found"),
        "unknown skill should report 'not found'"
    );
}

#[tokio::test]
async fn mcp_last_seen_at_updated_after_dispatch() {
    let (router, state, _tmp) = setup().await;

    // Create agent and register session.
    let profile = make_test_profile("test-mcp-touch");
    let create_body = serde_json::to_string(&profile).unwrap();
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(create_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    let session_id = "test-session-touch".to_string();
    state
        .mcp_sessions
        .register_session(
            session_id.clone(),
            "test-mcp-touch".to_string(),
            PathBuf::from("/tmp"),
            None,
        )
        .expect("register session");

    let session = state.mcp_sessions.get_by_session_id(&session_id).unwrap();
    let before = *session.last_seen_at.read().await;

    // Brief sleep so Instant::now() is measurably later.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let mcp_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    }))
    .unwrap();

    let mcp_resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/test-mcp-touch/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(mcp_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mcp_resp.status(), StatusCode::OK);

    let after = *session.last_seen_at.read().await;
    assert!(after > before, "last_seen_at must be updated after dispatch");
}

/// Regression for the MCP-route read-before-write wiring: a CLI-spawned agent
/// that `Read`s a file in one JSON-RPC call must be able to `Edit` it in a
/// *separate* call within the same session. Each `POST /mcp/:agent/:session`
/// builds and drops its own `RunnerContext`; before the fix every request
/// minted a fresh empty `ReadFileState`, so the read snapshot recorded by the
/// `Read` call vanished and the `Edit` call failed the read-before-write guard
/// with "File has not been read yet". The fix binds each per-request context to
/// a session-scoped `ReadFileState` on `McpAgentSession`, mirroring how `cwd` is
/// shared. The native runner already worked because it keeps one long-lived
/// context per run; this test pins the CLI/MCP path to the same behavior.
#[tokio::test]
async fn mcp_read_then_edit_across_requests_shares_read_state() {
    let (router, state, tmp) = setup().await;
    let agent_id = "mcp-read-edit";

    // Create the agent.
    let profile = make_test_profile(agent_id);
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&profile).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    // Write a real file on disk that the agent will read and then edit. Read
    // requires an absolute path, so anchor it inside the temp dir.
    let file_path = tmp.path().join("regression.txt");
    std::fs::write(&file_path, "line one\nline two\n").expect("seed file");
    let file_path_str = file_path.to_string_lossy().into_owned();

    // Register one MCP session; both calls below reuse it (the whole point —
    // the read snapshot must survive between the two requests).
    let session_id = "mcp-read-edit-session".to_string();
    state
        .mcp_sessions
        .register_session(
            session_id.clone(),
            agent_id.to_string(),
            tmp.path().to_path_buf(),
            None,
        )
        .expect("register session");

    // Request 1: Read the file. This populates the session-scoped ReadFileState.
    let read_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "Read", "arguments": { "file_path": file_path_str } }
    }))
    .unwrap();
    let read_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(read_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(read_resp.status(), StatusCode::OK);
    let read_json_body = read_json(read_resp).await;
    assert_eq!(
        read_json_body["result"]["isError"].as_bool().unwrap_or(false),
        false,
        "Read should succeed: {:?}",
        read_json_body["result"]["content"]
    );

    // Request 2: Edit the file in a brand-new HTTP request (and therefore a
    // brand-new RunnerContext). Without session-scoped read state this fails
    // with "File has not been read yet".
    let edit_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "Edit",
            "arguments": {
                "file_path": file_path_str,
                "old_string": "line two",
                "new_string": "line two EDITED"
            }
        }
    }))
    .unwrap();
    let edit_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(edit_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(edit_resp.status(), StatusCode::OK);
    let edit_json_body = read_json(edit_resp).await;
    let edit_text = edit_json_body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        !edit_text.contains("has not been read yet"),
        "Edit must see the Read from the prior request, got: {edit_text}"
    );
    assert_eq!(
        edit_json_body["result"]["isError"].as_bool().unwrap_or(false),
        false,
        "Edit should succeed after a prior Read in the same session: {edit_text}"
    );

    // The edit must have actually landed on disk.
    let on_disk = std::fs::read_to_string(&file_path).expect("re-read file");
    assert!(
        on_disk.contains("line two EDITED"),
        "file content should reflect the edit, got: {on_disk:?}"
    );

    // Negative control: a different session that never performed a Read must
    // still be rejected — proving the guard is genuinely consulted and the fix
    // shares state per-session rather than disabling the check globally.
    let other_session = "mcp-read-edit-session-2".to_string();
    state
        .mcp_sessions
        .register_session(
            other_session.clone(),
            agent_id.to_string(),
            tmp.path().to_path_buf(),
            None,
        )
        .expect("register second session");
    let blind_edit_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "Edit",
            "arguments": {
                "file_path": file_path_str,
                "old_string": "line one",
                "new_string": "line one EDITED"
            }
        }
    }))
    .unwrap();
    let blind_resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{other_session}"))
                .header("content-type", "application/json")
                .body(Body::from(blind_edit_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(blind_resp.status(), StatusCode::OK);
    let blind_body = read_json(blind_resp).await;
    let blind_text = blind_body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        blind_text.contains("has not been read yet"),
        "an edit from a session with no prior Read must still be rejected, got: {blind_text}"
    );
}

/// Regression for the MCP-route cwd wiring: a `Bash` `cd` lifted in one
/// JSON-RPC call must update the session's working directory so a later call in
/// the same session resolves relative paths (and further `cd`s) against the new
/// directory. Each `POST /mcp/:agent/:session` builds and drops its own
/// `RunnerContext`; before the fix the route seeded cwd by value from the
/// session entry and never bound the shared Arc, so the Bash tool's `ctx.cwd`
/// write evaporated when the per-request context dropped. The fix binds each
/// per-request context to the session-scoped cwd Arc via `with_cwd_arc`,
/// mirroring the read-state share. The native runner already worked because it
/// keeps one long-lived context per run.
///
/// Note the command form: the Bash cd pre-parser only lifts `cd <path> &&
/// <cmd>` (a leading cd followed by a real command). A bare `cd <path>` is
/// treated as subprocess-local and intentionally not persisted, so the test
/// uses `cd nested && pwd`.
#[tokio::test]
async fn mcp_bash_cd_persists_cwd_across_requests() {
    let (router, state, tmp) = setup().await;
    let agent_id = "mcp-cd-persist";

    // Create the agent.
    let profile = make_test_profile(agent_id);
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&profile).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    // A subdirectory of the session cwd that the agent will cd into. The Bash
    // tool canonicalizes the cd target, so compare against the canonical form
    // (on macOS the temp dir resolves through /private).
    let subdir = tmp.path().join("nested");
    std::fs::create_dir(&subdir).expect("create subdir");
    let subdir_canonical = std::fs::canonicalize(&subdir).expect("canonicalize subdir");

    // Register one MCP session rooted at the temp dir; both calls reuse it —
    // the cd from the first call must survive into the second.
    let session_id = "mcp-cd-persist-session".to_string();
    state
        .mcp_sessions
        .register_session(
            session_id.clone(),
            agent_id.to_string(),
            tmp.path().to_path_buf(),
            None,
        )
        .expect("register session");

    // Request 1: `cd nested && pwd`. The Bash tool lifts the leading cd and
    // writes the canonicalized path into ctx.cwd — which, post-fix, is the
    // session's shared Arc.
    let cd_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "Bash", "arguments": { "command": "cd nested && pwd" } }
    }))
    .unwrap();
    let cd_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(cd_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cd_resp.status(), StatusCode::OK);
    let cd_json_body = read_json(cd_resp).await;
    assert_eq!(
        cd_json_body["result"]["isError"].as_bool().unwrap_or(false),
        false,
        "Bash cd should succeed: {:?}",
        cd_json_body["result"]["content"]
    );

    // The session entry's cwd must now reflect the cd, even though the
    // per-request context that ran it has already been dropped.
    let session = state.mcp_sessions.get_by_session_id(&session_id).unwrap();
    let persisted = session.cwd.read().unwrap().clone();
    assert_eq!(
        persisted, subdir_canonical,
        "Bash `cd` must persist to the session cwd across MCP requests"
    );

    // End-to-end confirmation: a second request issuing a relative `cd` resolves
    // against the persisted directory. `cd .. && pwd` from `nested` must land
    // back on the temp-dir root — only possible if request 1's cwd survived.
    let tmp_canonical = std::fs::canonicalize(tmp.path()).expect("canonicalize tmp");
    let up_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "Bash", "arguments": { "command": "cd .. && pwd" } }
    }))
    .unwrap();
    let up_resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(up_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(up_resp.status(), StatusCode::OK);
    let up_json_body = read_json(up_resp).await;
    assert_eq!(
        up_json_body["result"]["isError"].as_bool().unwrap_or(false),
        false,
        "second Bash cd should succeed: {:?}",
        up_json_body["result"]["content"]
    );
    let persisted_after_up = session.cwd.read().unwrap().clone();
    assert_eq!(
        persisted_after_up, tmp_canonical,
        "a relative `cd ..` in the second request must resolve against the cwd \
         persisted by the first request"
    );
}

/// Verify that the `tools/list` response carries `annotations.readOnlyHint`
/// for the Delegate tool (our primary concurrent-batch target).
#[tokio::test]
async fn mcp_tools_list_delegate_carries_read_only_hint() {
    let (router, state, _tmp) = setup().await;
    let agent_id = "mcp-annotations-check";

    let profile = make_test_profile(agent_id);
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&profile).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    let session_id = "mcp-annotations-session".to_string();
    state
        .mcp_sessions
        .register_session(
            session_id.clone(),
            agent_id.to_string(),
            std::path::PathBuf::from("/tmp"),
            None,
        )
        .expect("register session");

    let list_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    }))
    .unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(list_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = read_json(resp).await;
    let tools = body["result"]["tools"].as_array().expect("tools array");

    let delegate = tools
        .iter()
        .find(|t| t["name"].as_str() == Some("Delegate"))
        .expect("Delegate must be present in tools/list");

    assert_eq!(
        delegate["annotations"]["readOnlyHint"], true,
        "Delegate must carry readOnlyHint=true to enable parallel dispatch; got: {:?}",
        delegate.get("annotations")
    );
    assert_eq!(
        delegate["annotations"]["openWorldHint"], true,
        "Delegate must carry openWorldHint=true; got: {:?}",
        delegate.get("annotations")
    );

    // Verify that a genuinely mutating tool (Bash) has no annotations.
    if let Some(bash) = tools.iter().find(|t| t["name"].as_str() == Some("Bash")) {
        assert!(
            bash.get("annotations").is_none() || bash["annotations"] == serde_json::Value::Null,
            "Bash must not carry annotations; got: {:?}",
            bash.get("annotations")
        );
    }
}

/// Two concurrent `tools/call` POSTs on the same session must both complete
/// successfully. This validates that the per-request `RunnerContext`
/// construction is safe for concurrent access to the session's shared
/// Arc-backed state (cwd, read_file_state, background_agents).
#[tokio::test]
async fn mcp_concurrent_tools_call_same_session_both_succeed() {
    let (router, state, _tmp) = setup().await;
    let agent_id = "mcp-concurrent";

    let profile = make_test_profile(agent_id);
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&profile).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    let session_id = "mcp-concurrent-session".to_string();
    state
        .mcp_sessions
        .register_session(
            session_id.clone(),
            agent_id.to_string(),
            std::path::PathBuf::from("/tmp"),
            None,
        )
        .expect("register session");

    let make_body = |id: u32, cmd: &str| {
        serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": "Bash", "arguments": { "command": cmd } }
        }))
        .unwrap()
    };

    let r1 = router.clone();
    let r2 = router.clone();
    let s1 = session_id.clone();
    let s2 = session_id.clone();
    let a1 = agent_id.to_string();
    let a2 = agent_id.to_string();

    // Fire both requests concurrently.
    let (resp1, resp2) = tokio::join!(
        r1.oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{a1}/{s1}"))
                .header("content-type", "application/json")
                .body(Body::from(make_body(10, "echo concurrent-a")))
                .unwrap()
        ),
        r2.oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{a2}/{s2}"))
                .header("content-type", "application/json")
                .body(Body::from(make_body(11, "echo concurrent-b")))
                .unwrap()
        ),
    );

    let resp1 = resp1.unwrap();
    let resp2 = resp2.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK, "first concurrent request failed");
    assert_eq!(resp2.status(), StatusCode::OK, "second concurrent request failed");

    let body1 = read_json(resp1).await;
    let body2 = read_json(resp2).await;

    assert_eq!(body1["result"]["isError"].as_bool().unwrap_or(true), false,
        "first concurrent Bash call must succeed; got: {:?}", body1["result"]);
    assert_eq!(body2["result"]["isError"].as_bool().unwrap_or(true), false,
        "second concurrent Bash call must succeed; got: {:?}", body2["result"]);

    let text1 = body1["result"]["content"][0]["text"].as_str().unwrap_or("");
    let text2 = body2["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text1.contains("concurrent-a"), "first call must return its own output; got: {text1}");
    assert!(text2.contains("concurrent-b"), "second call must return its own output; got: {text2}");
}

/// A background command started in one JSON-RPC call must still be reachable
/// by `BashKill` in the next call on the same session.
///
/// The MCP route builds a fresh `RunnerContext` per request and drops it on
/// return, so anything the tools need across calls has to be bound to the
/// session explicitly. `background_commands` was the one registry that binding
/// missed: `Bash` handed the model a `process_id`, the context died with the
/// response, and the follow-up `BashKill` searched a brand-new empty registry
/// and rejected the id as unknown — while the subprocess kept running with
/// nothing able to poll or stop it.
///
/// Unit tests could not catch this. They build one `RunnerContext` and use it
/// for both calls, which is precisely the assumption the MCP path breaks. This
/// test drives the real HTTP path with two separate requests.
#[tokio::test]
async fn mcp_background_command_id_survives_to_the_next_request() {
    let (router, state, _tmp) = setup().await;
    let agent_id = "mcp-bg-cmd";

    let profile = make_test_profile(agent_id);
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&profile).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    let session_id = "mcp-bg-cmd-session".to_string();
    state
        .mcp_sessions
        .register_session(
            session_id.clone(),
            agent_id.to_string(),
            std::path::PathBuf::from("/tmp"),
            None,
        )
        .expect("register session");

    let call = |id: u32, name: &str, args: Value| {
        serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": args }
        }))
        .unwrap()
    };

    // Request 1: start a long-running command in the background.
    let spawn_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(call(
                    20,
                    "Bash",
                    json!({ "command": "sleep 300", "run_in_background": true }),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(spawn_resp.status(), StatusCode::OK);
    let spawn_body = read_json(spawn_resp).await;
    assert_eq!(
        spawn_body["result"]["isError"].as_bool().unwrap_or(true),
        false,
        "background Bash call must succeed; got: {:?}",
        spawn_body["result"]
    );

    // Pull the id out of whatever shape the result carries.
    let spawn_text = spawn_body["result"].to_string();
    let process_id = spawn_text
        .find("bash_")
        .map(|start| {
            let rest = &spawn_text[start..];
            let end = rest
                .char_indices()
                .find(|(i, c)| *i > 4 && !c.is_ascii_digit())
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            rest[..end].to_string()
        })
        .unwrap_or_else(|| panic!("no process_id in Bash result: {spawn_text}"));

    // Request 2: a *separate* request, so a fresh RunnerContext. This is the
    // call that used to fail with "unknown background command".
    let kill_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(call(
                    21,
                    "BashKill",
                    json!({ "process_id": process_id }),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(kill_resp.status(), StatusCode::OK);
    let kill_body = read_json(kill_resp).await;

    // Scan the whole envelope, not just `result`: a validation failure inside
    // the tool surfaces as a JSON-RPC `error` with `result` left null.
    let kill_envelope = kill_body.to_string();
    assert!(
        !kill_envelope.contains("unknown background command"),
        "BashKill could not find the id Bash returned one request earlier — the \
         session's background-command registry is not bound into the per-request \
         context; got: {kill_envelope}"
    );
    let kill_text = kill_body["result"].to_string();
    assert_eq!(
        kill_body["result"]["isError"].as_bool().unwrap_or(true),
        false,
        "BashKill must succeed on the id from the previous request; got: {:?}",
        kill_body["result"]
    );
    // BashKill only reports `killed` after the drain task confirms the child was
    // reaped, so this also asserts the signal actually landed on the process.
    assert!(
        kill_text.contains("killed"),
        "BashKill must report a confirmed kill; got: {kill_text}"
    );
}

// ── SSE response mode tests ───────────────────────────────────────────────────

/// A test tool that blocks on a tokio timer before returning.
///
/// Using `tokio::time::sleep` (not an OS-level sleep) means this tool
/// respects tokio's mock-time controls (`start_paused = true` /
/// `tokio::time::advance`) in tests that need to trigger keepalive ticks
/// without waiting for real wall-clock time.
struct SlowTool;

#[async_trait]
impl IoTool for SlowTool {
    fn name(&self) -> &str {
        "SlowTool"
    }

    fn description(&self) -> &str {
        "Waits for a tokio timer then returns a fixed text result."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::AlwaysLoad
    }

    async fn invoke(&self, _input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(ToolOutput::text("slow result"))
    }
}

/// Helper: collect the raw bytes of a response body as a `Vec<u8>`.
async fn collect_body(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .expect("collect body bytes")
        .to_bytes()
        .to_vec()
}

/// Build the MCP agent + session scaffolding needed by SSE tests.
async fn setup_agent_and_session(
    router: &axum::Router,
    state: &Arc<AppState>,
    agent_id: &str,
    session_id: &str,
) {
    let profile = make_test_profile(agent_id);
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&profile).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK, "agent creation failed");

    state
        .mcp_sessions
        .register_session(
            session_id.to_string(),
            agent_id.to_string(),
            PathBuf::from("/tmp"),
            None,
        )
        .expect("register session");
}

/// When a `tools/call` request carries `Accept: application/json, text/event-stream`
/// the route must return 200 with `Content-Type: text/event-stream` immediately —
/// before the tool finishes — so the client's per-POST fetch timeout (typically 60 s
/// to first response byte) cannot kill long-running synchronous tool calls.
#[tokio::test]
async fn mcp_sse_tools_call_returns_event_stream_content_type() {
    let (router, state, _tmp) = setup().await;
    let agent_id = "mcp-sse-ct";
    let session_id = "mcp-sse-ct-session";

    setup_agent_and_session(&router, &state, agent_id, session_id).await;

    let call_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "Bash", "arguments": { "command": "echo sse-test" } }
    }))
    .unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from(call_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/event-stream"),
        "SSE-mode tools/call must return Content-Type: text/event-stream; got: {ct:?}"
    );

    // Drain the body so the spawned stream task can finish cleanly.
    let _ = collect_body(resp).await;
}

/// The final JSON-RPC response must arrive as an SSE `data:` event with the
/// correct envelope — `jsonrpc`, `id`, and `result` fields. The body must
/// be valid UTF-8 SSE framing (comment or data lines).
#[tokio::test]
async fn mcp_sse_tools_call_delivers_final_response_as_data_event() {
    let (router, state, _tmp) = setup().await;
    let agent_id = "mcp-sse-data";
    let session_id = "mcp-sse-data-session";

    setup_agent_and_session(&router, &state, agent_id, session_id).await;

    let call_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "tools/call",
        "params": { "name": "Bash", "arguments": { "command": "echo hello-sse" } }
    }))
    .unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from(call_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body_bytes = collect_body(resp).await;
    let body_str = std::str::from_utf8(&body_bytes).expect("SSE body must be UTF-8");

    // Find all `data:` lines in the SSE stream.
    let data_events: Vec<String> = body_str
        .lines()
        .filter(|l| l.starts_with("data: "))
        .map(|l| l["data: ".len()..].to_string())
        .collect();

    assert!(
        !data_events.is_empty(),
        "SSE body must contain at least one data event; body was:\n{body_str}"
    );

    // The last data event must be the JSON-RPC response.
    let last_event = data_events.last().unwrap();
    let parsed: Value =
        serde_json::from_str(last_event).expect("SSE data event must be valid JSON");

    assert_eq!(parsed["jsonrpc"], "2.0", "response must carry jsonrpc=2.0");
    assert_eq!(parsed["id"], 42, "response id must echo the request id");
    assert!(
        parsed["result"].is_object() || parsed["error"].is_object(),
        "response must have either a result or error field"
    );
}

/// Callers that do NOT advertise `text/event-stream` in their `Accept` header
/// receive a plain buffered JSON response, not an SSE stream — backward
/// compatibility regression guard.
#[tokio::test]
async fn mcp_plain_json_tools_call_when_accept_lacks_event_stream() {
    let (router, state, _tmp) = setup().await;
    let agent_id = "mcp-plain-json";
    let session_id = "mcp-plain-json-session";

    setup_agent_and_session(&router, &state, agent_id, session_id).await;

    let call_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": { "name": "Bash", "arguments": { "command": "echo plain" } }
    }))
    .unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .header("accept", "application/json") // no text/event-stream
                .body(Body::from(call_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Content-Type must NOT be text/event-stream.
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        !ct.contains("text/event-stream"),
        "plain-JSON Accept must not produce SSE; got Content-Type: {ct:?}"
    );

    // Body must parse as a JSON-RPC response object, not SSE lines.
    let body: Value = read_json(resp).await;
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 7);
    assert!(
        body["result"].is_object() || body["error"].is_object(),
        "expected plain JSON-RPC response, got: {body}"
    );
}

/// While a tools/call SSE stream is open, the server must emit `: keepalive`
/// comment lines at the configured interval so the transport connection stays
/// alive. Uses tokio's mock-time control to trigger the interval without
/// sleeping for real wall-clock seconds.
#[tokio::test(start_paused = true)]
async fn mcp_sse_tools_call_emits_keepalive_during_long_tool() {
    let (router, state, _tmp) = setup().await;
    let agent_id = "mcp-sse-keepalive";
    let session_id = "mcp-sse-keepalive-session";

    setup_agent_and_session(&router, &state, agent_id, session_id).await;

    // Register a slow tool that blocks on a tokio timer so mock-time
    // advancement (below) controls when it completes.
    state
        .tools_registry
        .register_io_dynamic(Arc::new(SlowTool));

    let call_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "tools/call",
        "params": { "name": "SlowTool", "arguments": {} }
    }))
    .unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from(call_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.contains("text/event-stream"), "must be SSE; got: {ct}");

    // Collect the body concurrently with time advancement. The body task will
    // block until the SSE stream closes (when the tool completes and tx drops).
    let body_task = tokio::spawn(async move { collect_body(resp).await });

    // Advance past the 15 s keepalive interval so at least one comment fires.
    tokio::time::advance(Duration::from_secs(16)).await;

    // Advance past the SlowTool's 30 s sleep so the tool completes and the
    // stream closes. Total simulated time: 36 s.
    tokio::time::advance(Duration::from_secs(20)).await;

    let body_bytes = body_task.await.expect("body task must not panic");
    let body_str = String::from_utf8(body_bytes).expect("SSE body must be UTF-8");

    // At least one keepalive comment must be present.
    assert!(
        body_str.contains(": keepalive"),
        "SSE body must contain at least one keepalive comment; body was:\n{body_str}"
    );

    // The final data event must carry the JSON-RPC response.
    let data_events: Vec<String> = body_str
        .lines()
        .filter(|l| l.starts_with("data: "))
        .map(|l| l["data: ".len()..].to_string())
        .collect();
    assert!(
        !data_events.is_empty(),
        "SSE body must contain a final data event; body was:\n{body_str}"
    );

    let last_event = data_events.last().unwrap();
    let parsed: Value =
        serde_json::from_str(last_event).expect("final SSE data event must be valid JSON");
    assert_eq!(parsed["jsonrpc"], "2.0");
    assert_eq!(parsed["id"], 99);
    assert!(
        parsed.get("result").is_some() || parsed.get("error").is_some(),
        "final event must be a JSON-RPC response; got: {parsed}"
    );
}

// ── Project-scoped session tests ──────────────────────────────────────────────

/// A tool call inside a project-scoped MCP session must publish its events on
/// the project channel (`project:{id}`) — the channel the frontend's project
/// stream subscribes to — while the payload keeps the real agent id so the
/// operator's answer can still be POSTed to `/agents/{id}/form-answer`.
/// Exercises the full loop: AskUserQuestionWithForm suspends the tools/call
/// request, the FormRequest event surfaces on the project channel, the answer
/// route resolves it, and the suspended call returns the submitted answers.
#[tokio::test]
async fn mcp_project_session_form_request_routes_to_project_channel() {
    let (router, state, _tmp) = setup().await;
    let agent_id = "mcp-project-form";
    let project_id = "proj-form-routing";

    let profile = make_test_profile(agent_id);
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&profile).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    // Register a session bound to a project — mirrors a project-scoped CLI spawn.
    let session_id = "mcp-project-form-session".to_string();
    state
        .mcp_sessions
        .register_session_with_chains(
            session_id.clone(),
            agent_id.to_string(),
            PathBuf::from("/tmp"),
            None,
            vec![],
            vec![],
            Some(project_id.to_string()),
            None,
        )
        .expect("register project session");

    // Subscribe BEFORE dispatching so the FormRequest broadcast can't be missed.
    let mut rx = state.event_bus.subscribe();

    // Dispatch a sync form call — the HTTP request suspends until answered.
    let call_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "AskUserQuestionWithForm",
            "arguments": {
                "title": "Project routing check",
                "questions": [
                    { "id": "confirm", "type": "text", "label": "Confirm" }
                ]
            }
        }
    }))
    .unwrap();
    let call_router = router.clone();
    let call_agent = agent_id.to_string();
    let call_session = session_id.clone();
    let call_task = tokio::spawn(async move {
        call_router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/mcp/{call_agent}/{call_session}"))
                    .header("content-type", "application/json")
                    .body(Body::from(call_body))
                    .unwrap(),
            )
            .await
            .unwrap()
    });

    // The FormRequest must arrive on the PROJECT channel, not the agent's.
    let expected_channel = format!("project:{project_id}");
    let (live_form_id, payload_agent_id) =
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if let ao_protocol::event::AgentEventPayload::FormRequest {
                            form_id,
                            agent_id: inner_agent,
                            ..
                        } = event.payload
                        {
                            assert_eq!(
                                event.agent_id, expected_channel,
                                "FormRequest from a project-scoped session must \
                                 broadcast on the project channel"
                            );
                            break (form_id, inner_agent);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => panic!("event bus closed"),
                }
            }
        })
        .await
        .expect("FormRequest event must arrive within 5 s");

    assert_eq!(
        payload_agent_id, agent_id,
        "payload must carry the real agent id for answer delivery"
    );

    // Answer through the normal per-agent route — registry is keyed by agent id.
    let submit_body = json!({
        "form_id": &live_form_id,
        "answers": { "confirm": { "kind": "text", "value": "yes" } }
    });
    let answer_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/agents/{agent_id}/form-answer"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&submit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        answer_resp.status(),
        StatusCode::OK,
        "form-answer must deliver to the bridge registered under the real agent id"
    );

    // The suspended tools/call resolves with the submitted answers.
    let resp = tokio::time::timeout(Duration::from_secs(5), call_task)
        .await
        .expect("suspended tools/call must resolve within 5 s")
        .expect("call task must not panic");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_json(resp).await;
    let text = body["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert_eq!(
        body["result"]["isError"].as_bool().unwrap_or(false),
        false,
        "form tool call must succeed: {text}"
    );
    assert!(
        text.contains("yes"),
        "tool result must contain the submitted answer, got: {text}"
    );
}

// ── Session resurrection tests ─────────────────────────────────────────────────

/// A server restart wipes the in-memory session store while CLI subprocesses
/// (and their session ids) survive. When the per-spawn config file still exists
/// on disk — the spawn guard deletes it on subprocess exit — the route must
/// rebuild the session entry from the metadata sidecar instead of 404ing, and
/// the resurrected session must keep its project scoping.
#[tokio::test]
async fn mcp_session_resurrected_from_spawn_files_after_registry_loss() {
    let (router, state, tmp) = setup().await;
    let agent_id = "mcp-resurrect";

    let profile = make_test_profile(agent_id);
    let create_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&profile).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::OK);

    // Simulate a pre-restart spawn: config + sidecar on disk, NO registry entry.
    let session_id = uuid::Uuid::new_v4().to_string();
    let agent_dir = state.persistence.data_root.agents_dir().join(agent_id);
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    std::fs::write(
        agent_dir.join(format!("mcp-{session_id}.json")),
        r#"{"mcpServers":{}}"#,
    )
    .expect("write config file");
    let meta = json!({
        "agentId": agent_id,
        "cwd": tmp.path().to_string_lossy(),
        "delegateChain": ["parent-agent"],
        "spawnChain": [],
        "projectId": "proj-resurrected",
        "floorTs": "2026-06-01T00:00:00Z",
    });
    std::fs::write(
        agent_dir.join(format!("mcp-{session_id}.meta.json")),
        serde_json::to_string(&meta).unwrap(),
    )
    .expect("write sidecar");

    assert!(
        state.mcp_sessions.get_by_session_id(&session_id).is_none(),
        "precondition: session must not be registered"
    );

    // The request must succeed via resurrection rather than 404.
    let list_body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    }))
    .unwrap();
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{session_id}"))
                .header("content-type", "application/json")
                .body(Body::from(list_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "session with surviving spawn files must be resurrected, not 404ed"
    );

    // The rebuilt entry must restore the sidecar state.
    let session = state
        .mcp_sessions
        .get_by_session_id(&session_id)
        .expect("session must be registered after resurrection");
    assert_eq!(session.project_id.as_deref(), Some("proj-resurrected"));
    assert_eq!(session.delegate_chain, vec!["parent-agent".to_string()]);
    assert_eq!(*session.cwd.read().unwrap(), tmp.path().to_path_buf());
    assert!(
        session.window_floor_ts.read().await.is_some(),
        "window floor must be restored from the sidecar"
    );

    // Negative control 1: a UUID session id with no spawn files still 404s —
    // resurrection must not become silent auto-registration.
    let phantom = uuid::Uuid::new_v4().to_string();
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/{phantom}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&json!({
                        "jsonrpc": "2.0", "id": 2, "method": "tools/list"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Negative control 2: a non-UUID session id is rejected before any file
    // probe (path-traversal guard).
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mcp/{agent_id}/..%2F..%2Fetc"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&json!({
                        "jsonrpc": "2.0", "id": 3, "method": "tools/list"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
