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
use ao_server::routes::build_router;

/// Global mutex to serialize setup() calls that modify the process-wide env var.
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

    // Hold mutex while setting env var and creating AppState to avoid race conditions
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
async fn test_create_agent_returns_200() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("crud-create");
    let body = serde_json::to_string(&profile).unwrap();

    let resp = router
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

    let bytes = read_body(resp).await;
    let returned: AgentProfile = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(returned.id, "crud-create");
    assert_eq!(returned.name, "Test Agent crud-create");
}

#[tokio::test]
async fn test_list_agents_includes_created() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("list-agent");
    let body = serde_json::to_string(&profile).unwrap();

    // Create agent
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

    // List agents
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let agents: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["name"], "Test Agent list-agent");
}

#[tokio::test]
async fn test_get_agent_returns_full_profile() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("get-agent");
    let body = serde_json::to_string(&profile).unwrap();

    // Create
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

    // Get
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/get-agent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let returned: AgentProfile = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(returned.id, "get-agent");
    assert_eq!(returned.name, "Test Agent get-agent");
}

#[tokio::test]
async fn test_update_agent_persists_changes() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("upd-agent");
    let body = serde_json::to_string(&profile).unwrap();

    // Create
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

    // Update
    let mut updated = profile.clone();
    updated.name = "Updated Name".to_string();
    let update_body = serde_json::to_string(&updated).unwrap();

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/agents/upd-agent")
                .header("content-type", "application/json")
                .body(Body::from(update_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify via GET
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/upd-agent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let returned: AgentProfile = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(returned.name, "Updated Name");
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

async fn delete_agent(router: &axum::Router, id: &str) -> StatusCode {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(&format!("/agents/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    resp.status()
}

#[tokio::test]
async fn test_delete_agent_returns_204() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("del-agent");
    let body = serde_json::to_string(&profile).unwrap();

    // Create
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

    // Delete
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/agents/del-agent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // GET should return 404
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/del-agent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_create_duplicate_returns_409() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("dup-agent");
    let body = serde_json::to_string(&profile).unwrap();

    // Create first
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Create duplicate
    let resp = router
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
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_clone_agent_returns_new_profile() {
    let (router, _tmp) = setup().await;
    let profile = make_test_profile("clone-parent");
    let body = serde_json::to_string(&profile).unwrap();

    // Create parent
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

    // Clone
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/clone-parent/clone")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let returned: AgentProfile = serde_json::from_slice(&bytes).unwrap();
    assert_ne!(returned.id, "clone-parent");
    assert_eq!(returned.name, "Test Agent clone-parent - copy");

    // List should now contain both
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let agents: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(agents.len(), 2);
}

#[tokio::test]
async fn test_clone_agent_does_not_copy_channel_bindings() {
    use ao_protocol::agent::{ChannelBinding, ChannelKind, ChannelKindConfig, TelegramThreadMode};

    let (router, _tmp) = setup().await;
    let mut profile = make_test_profile("clone-channels-parent");
    profile.channels = vec![ChannelBinding {
        binding_id: "telegram".to_string(),
        kind: ChannelKind::Telegram,
        enabled: true,
        bridge_thread_id: Some("thread-parent".to_string()),
        allowed_senders: vec!["12345".to_string()],
        pending_pairing_code: None,
        kind_config: ChannelKindConfig::Telegram {
            bot_username: Some("@parent_bot".to_string()),
            thread_mode: TelegramThreadMode::default(),
        },
    }];
    let body = serde_json::to_string(&profile).unwrap();

    // Create parent with an enabled Telegram channel binding.
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

    // Clone it.
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/clone-channels-parent/clone")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let cloned: AgentProfile = serde_json::from_slice(&bytes).unwrap();

    // The clone must NOT inherit the parent's channel binding: two agents
    // both wired to the same Telegram bot/bridge thread would both handle
    // every inbound message, double-firing responses.
    assert!(
        cloned.channels.is_empty(),
        "clone must not inherit channel bindings, got {:?}",
        cloned.channels
    );
}

#[tokio::test]
async fn test_clone_agent_missing_parent_returns_404() {
    let (router, _tmp) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/does-not-exist/clone")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_nonexistent_returns_404() {
    let (router, _tmp) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// --- End-to-end clone integration tests ---

#[tokio::test]
async fn test_clone_agent_default_home_copies_skills_isolated() {
    let (router, tmp) = setup().await;

    // Create parent agent with a default home.
    let parent_id = "clone-default-parent";
    let profile = make_test_profile(parent_id);
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
    assert_eq!(resp.status(), StatusCode::OK);

    // Seed a skill file under the parent's default home.
    let parent_home = tmp.path().join("agent_homes").join(parent_id);
    tokio::fs::create_dir_all(parent_home.join("skills"))
        .await
        .unwrap();
    tokio::fs::write(parent_home.join("skills/review.md"), b"parent skill")
        .await
        .unwrap();

    // Clone over HTTP.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{parent_id}/clone"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let cloned: AgentProfile = serde_json::from_slice(&bytes).unwrap();

    assert_ne!(cloned.id, parent_id);
    assert_eq!(cloned.name, format!("Test Agent {parent_id} - copy"));

    // home_dir is left None so the clone continues to resolve against the
    // managed default directory (same convention as freshly-created agents).
    assert_eq!(cloned.home_dir, None);

    // Seeded skill file lives under the clone's managed default home...
    let expected_home = tmp.path().join("agent_homes").join(&cloned.id);
    let clone_skill = expected_home.join("skills/review.md");
    assert_eq!(
        tokio::fs::read_to_string(&clone_skill).await.unwrap(),
        "parent skill",
    );

    // ...and is a separate copy (edits to clone do not affect parent).
    tokio::fs::write(&clone_skill, b"edited in clone")
        .await
        .unwrap();
    assert_eq!(
        tokio::fs::read_to_string(parent_home.join("skills/review.md"))
            .await
            .unwrap(),
        "parent skill",
    );
}

#[tokio::test]
async fn test_clone_agent_custom_home_shares_parent_path() {
    let (router, tmp) = setup().await;

    // Custom home lives outside the managed agent_homes tree.
    let custom_tmp = tempfile::tempdir().unwrap();
    let custom_home = custom_tmp.path().to_path_buf();

    let parent_id = "clone-custom-parent";
    let mut profile = make_test_profile(parent_id);
    profile.home_dir = Some(custom_home.to_string_lossy().into_owned());
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
    assert_eq!(resp.status(), StatusCode::OK);

    // Clone over HTTP.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{parent_id}/clone"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let cloned: AgentProfile = serde_json::from_slice(&bytes).unwrap();

    // The clone's home_dir is the parent's custom path (shared source of truth).
    assert_eq!(
        cloned.home_dir.as_deref(),
        Some(custom_home.to_string_lossy().as_ref()),
    );

    // No managed default directory was materialized for the clone.
    let managed_clone_home = tmp.path().join("agent_homes").join(&cloned.id);
    assert!(
        !managed_clone_home.exists(),
        "custom-home clones must not materialize a managed default dir: {}",
        managed_clone_home.display(),
    );
}

#[tokio::test]
async fn test_clone_agent_rollback_leaves_no_orphans() {
    let (router, tmp) = setup().await;

    // Create parent.
    let parent_id = "clone-rollback-parent";
    let profile = make_test_profile(parent_id);
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
    assert_eq!(resp.status(), StatusCode::OK);

    // Replace the agent_homes directory with a regular file so
    // ensure_agent_home cannot create a subpath for the new agent id.
    let homes = tmp.path().join("agent_homes");
    tokio::fs::remove_dir_all(&homes).await.unwrap();
    tokio::fs::write(&homes, b"not a directory").await.unwrap();

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(&format!("/agents/{parent_id}/clone"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !resp.status().is_success(),
        "clone must fail when agent_homes is unusable (got {})",
        resp.status(),
    );

    // Restore agent_homes so we can inspect final state without leaking side-effects.
    tokio::fs::remove_file(&homes).await.unwrap();
    tokio::fs::create_dir_all(&homes).await.unwrap();

    // No partial home directory remains for any new id beyond the parent.
    let mut entries = tokio::fs::read_dir(&homes).await.unwrap();
    while let Some(entry) = entries.next_entry().await.unwrap() {
        let name = entry.file_name();
        assert_eq!(
            name.to_string_lossy(),
            parent_id,
            "unexpected orphan dir under agent_homes after rollback",
        );
    }

    // The agent list still contains only the parent.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let agents: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["name"], format!("Test Agent {parent_id}"));
}

// --- Cascade-delete integration tests ---

#[tokio::test]
async fn test_delete_agent_not_in_any_team_succeeds() {
    let (router, _tmp) = setup().await;
    let agent = make_test_profile("lone-agent");
    create_agent(&router, &agent).await;

    assert_eq!(
        delete_agent(&router, &agent.id).await,
        StatusCode::NO_CONTENT,
    );
}

// --- Inline-coordinator filtering tests ---

#[tokio::test]
async fn test_list_agents_excludes_inline_coordinators_by_default() {
    let (router, _tmp) = setup().await;

    let plain = make_test_profile("plain-agent");
    let mut coord = make_test_profile("inline-coord");
    coord.owning_team_id = Some("team-xyz".to_string());

    create_agent(&router, &plain).await;
    create_agent(&router, &coord).await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let agents: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    let ids: Vec<&str> = agents
        .iter()
        .map(|a| a["agent_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["plain-agent"]);
}

#[tokio::test]
async fn test_list_agents_include_team_coordinators_returns_all() {
    let (router, _tmp) = setup().await;

    let plain = make_test_profile("plain-2");
    let mut coord = make_test_profile("inline-coord-2");
    coord.owning_team_id = Some("team-abc".to_string());

    create_agent(&router, &plain).await;
    create_agent(&router, &coord).await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents?include_team_coordinators=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let agents: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    let mut ids: Vec<&str> = agents
        .iter()
        .map(|a| a["agent_id"].as_str().unwrap())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["inline-coord-2", "plain-2"]);
}

#[tokio::test]
async fn test_update_agent_can_set_and_clear_owning_team_id_in_snapshot() {
    let (router, _tmp) = setup().await;

    let profile = make_test_profile("toggle-coord");
    create_agent(&router, &profile).await;

    // Initially listed (owning_team_id is None).
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = read_body(resp).await;
    let agents: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(agents.len(), 1);

    // Promote to inline coordinator via PUT — should disappear from default list.
    let mut coord = profile.clone();
    coord.owning_team_id = Some("team-promote".to_string());
    let body = serde_json::to_string(&coord).unwrap();
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(&format!("/agents/{}", coord.id))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = read_body(resp).await;
    let agents: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert!(agents.is_empty(), "inline coordinator must be hidden after PUT");

    // Demote back to plain agent — should reappear.
    let mut demoted = coord.clone();
    demoted.owning_team_id = None;
    let body = serde_json::to_string(&demoted).unwrap();
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(&format!("/agents/{}", demoted.id))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = read_body(resp).await;
    let agents: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(agents.len(), 1);
}

#[tokio::test]
async fn test_delete_agent_recursively_removes_home_directory() {
    let (router, tmp) = setup().await;
    let agent = make_test_profile("home-dir-agent");
    create_agent(&router, &agent).await;

    let home = tmp.path().join("agent_homes").join(&agent.id);
    tokio::fs::create_dir_all(home.join("nested")).await.unwrap();
    tokio::fs::write(home.join("nested/notes.md"), b"hello")
        .await
        .unwrap();
    assert!(home.exists());

    assert_eq!(
        delete_agent(&router, &agent.id).await,
        StatusCode::NO_CONTENT,
    );

    assert!(
        !home.exists(),
        "agent_homes/<id>/ should be recursively removed after delete",
    );
}

