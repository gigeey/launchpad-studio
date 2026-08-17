use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use ao_engine::AppState;
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use ao_server::routes::build_router;

/// `ChannelSecretStore::open()` reads process-wide env vars, so tests in
/// this file must not run concurrently with each other or with
/// `email_channel_routes.rs`/`telegram_routes.rs`. Run with
/// `--test-threads=1`.
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

const CHANNEL_SECRET_FALLBACK_ENV_VAR: &str = "LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK";
const TELEGRAM_FALLBACK_ENV_VAR: &str = "LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK";

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

/// Sets up a router with an isolated data root. Holds `ENV_MUTEX` for the
/// lifetime of the returned guard so `LAUNCHPAD_STUDIO_DATA_DIR` and the
/// secret-store file-fallback flags stay fixed for the whole test.
async fn setup() -> (axum::Router, tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
    let guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::tempdir().expect("Failed to create temp dir");

    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
    std::env::set_var(CHANNEL_SECRET_FALLBACK_ENV_VAR, "1");
    std::env::set_var(TELEGRAM_FALLBACK_ENV_VAR, "1");

    let mock = MockProcessSupervisor::new(vec![]);
    let state = Arc::new(AppState::new_with_mock(mock).await.expect("init state"));
    let router = build_router(state);

    (router, tmp, guard)
}

async fn read_body(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.expect("read body").to_bytes();
    if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("valid JSON body")
    }
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

fn valid_config_body(enabled: bool) -> serde_json::Value {
    serde_json::json!({
        "allowed_users": ["111222333"],
        "allowed_roles": ["444555666"],
        "allowed_channels": ["777888999"],
        "dm_role_auth_guild": "111000111",
        "require_mention": false,
        "thread_follow": "one_shot",
        "thread_idle_timeout_minutes": 30,
        "thread_message_budget": 25,
        "backfill_limit": 50,
        "enabled": enabled,
    })
}

async fn upsert_discord(router: &axum::Router, agent_id: &str, body: serde_json::Value) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{agent_id}/channels/discord"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn set_secret(router: &axum::Router, agent_id: &str, bot_token: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{agent_id}/channels/discord/secret"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "bot_token": bot_token }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn delete_discord(router: axum::Router, agent_id: &str) -> axum::response::Response {
    router
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/agents/{agent_id}/channels/discord"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn list_channels(router: &axum::Router, agent_id: &str) -> serde_json::Value {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/agents/{agent_id}/channels"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    read_body(resp).await
}

fn discord_entry(channels: &serde_json::Value) -> serde_json::Value {
    channels
        .as_array()
        .expect("channels is an array")
        .iter()
        .find(|c| c["kind"] == "discord")
        .cloned()
        .expect("discord binding present")
}

#[tokio::test]
async fn upsert_creates_config_and_get_status_reflects_it() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("discord-agent-upsert");
    create_agent(&router, &agent).await;

    let resp = upsert_discord(&router, &agent.id, valid_config_body(false)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["binding_id"], "discord");
    assert_eq!(body["kind"], "discord");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["secret_stored"], false);
    assert_eq!(body["bridge_thread_provisioned"], false);
    assert_eq!(body["kind_config"]["allowed_users"], serde_json::json!(["111222333"]));
    assert_eq!(body["kind_config"]["allowed_roles"], serde_json::json!(["444555666"]));
    assert_eq!(body["kind_config"]["allowed_channels"], serde_json::json!(["777888999"]));
    assert_eq!(body["kind_config"]["dm_role_auth_guild"], "111000111");
    assert_eq!(body["kind_config"]["require_mention"], false);
    assert_eq!(body["kind_config"]["thread_follow"], "one_shot");
    assert_eq!(body["kind_config"]["thread_idle_timeout_minutes"], 30);
    assert_eq!(body["kind_config"]["thread_message_budget"], 25);
    assert_eq!(body["kind_config"]["backfill_limit"], 50);

    let channels = list_channels(&router, &agent.id).await;
    let discord = discord_entry(&channels);
    assert_eq!(discord["enabled"], false);
    assert_eq!(discord["kind_config"]["allowed_users"], serde_json::json!(["111222333"]));
    assert_eq!(discord["kind_config"]["thread_follow"], "one_shot");
    assert_eq!(discord["kind_config"]["backfill_limit"], 50);

    // Full-profile GET never carries the bot token (it isn't part of
    // ChannelKindConfig::Discord at all).
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/agents/{}", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let profile = read_body(resp).await;
    assert!(profile.to_string().to_lowercase().find("bot_token").is_none());
}

#[tokio::test]
async fn upsert_defaults_missing_lists_to_empty_and_fails_closed() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("discord-agent-defaults");
    create_agent(&router, &agent).await;

    let resp = upsert_discord(&router, &agent.id, serde_json::json!({ "enabled": false })).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["kind_config"]["allowed_users"], serde_json::json!([]));
    assert_eq!(body["kind_config"]["allowed_roles"], serde_json::json!([]));
    assert_eq!(body["kind_config"]["allowed_channels"], serde_json::json!([]));
    assert!(body["kind_config"]["dm_role_auth_guild"].is_null());

    // A request omitting the engagement-gating fields must reproduce the
    // exact defaults `ChannelKindConfig::Discord` itself uses — mention
    // required, sticky-decay follow, 15-minute idle timeout, 10-message
    // budget, 20-message backfill — so an older client (or a hand-authored
    // request) never silently reverts to different behavior than a
    // persisted profile predating these fields would.
    assert_eq!(body["kind_config"]["require_mention"], true);
    assert_eq!(body["kind_config"]["thread_follow"], "sticky_decay");
    assert_eq!(body["kind_config"]["thread_idle_timeout_minutes"], 15);
    assert_eq!(body["kind_config"]["thread_message_budget"], 10);
    assert_eq!(body["kind_config"]["backfill_limit"], 20);
}

#[tokio::test]
async fn upsert_rejects_blank_dm_role_auth_guild() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("discord-agent-blank-guild");
    create_agent(&router, &agent).await;

    let mut bad = valid_config_body(false);
    bad["dm_role_auth_guild"] = serde_json::json!("   ");
    let resp = upsert_discord(&router, &agent.id, bad).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upsert_unknown_agent_returns_404() {
    let (router, _tmp, _guard) = setup().await;

    let resp = upsert_discord(&router, "does-not-exist", valid_config_body(true)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_secret_marks_secret_stored_and_never_echoes_it() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("discord-agent-secret");
    create_agent(&router, &agent).await;
    upsert_discord(&router, &agent.id, valid_config_body(false)).await;

    let resp = set_secret(&router, &agent.id, "super-secret-bot-token").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["secret_stored"], true);
    assert!(body.get("bot_token").is_none());
    assert!(!body.to_string().contains("super-secret-bot-token"));

    let channels = list_channels(&router, &agent.id).await;
    let discord = discord_entry(&channels);
    assert_eq!(discord["secret_stored"], true);
    assert!(!channels.to_string().contains("super-secret-bot-token"));
}

#[tokio::test]
async fn set_secret_rejects_empty_bot_token() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("discord-agent-empty-secret");
    create_agent(&router, &agent).await;

    let resp = set_secret(&router, &agent.id, "   ").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn enabling_discord_does_not_provision_a_bridge_thread() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("discord-agent-provision");
    create_agent(&router, &agent).await;

    let resp = upsert_discord(&router, &agent.id, valid_config_body(true)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["enabled"], true);
    // Discord mints a fresh per-conversation bridge thread on demand for
    // every distinct `channel_id` it sees, rather than routing every
    // conversation through one eagerly-provisioned thread — so enabling a
    // binding never provisions one anymore.
    assert_eq!(body["bridge_thread_provisioned"], false);

    let channels = list_channels(&router, &agent.id).await;
    let discord = discord_entry(&channels);
    assert_eq!(discord["bridge_thread_provisioned"], false);

    // Cross-check against the full profile: no bridge_thread_id at all.
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/agents/{}", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let profile = read_body(resp).await;
    let discord_binding = profile["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "discord")
        .unwrap();
    assert!(discord_binding["bridge_thread_id"].is_null());
}

#[tokio::test]
async fn setting_secret_after_enabling_discord_still_does_not_provision_a_bridge_thread() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("discord-agent-secret-then-provision");
    create_agent(&router, &agent).await;

    // Set the secret before any config exists — the binding is created
    // disabled, so no bridge thread yet.
    let resp = set_secret(&router, &agent.id, "bot-token-1").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["bridge_thread_provisioned"], false);

    // Enabling via upsert still never provisions one for Discord.
    let resp = upsert_discord(&router, &agent.id, valid_config_body(true)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["bridge_thread_provisioned"], false);
    assert_eq!(body["secret_stored"], true);
}

#[tokio::test]
async fn discord_bridge_thread_id_stays_unprovisioned_across_subsequent_upserts() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("discord-agent-preserve");
    create_agent(&router, &agent).await;

    let resp = upsert_discord(&router, &agent.id, valid_config_body(true)).await;
    let first = read_body(resp).await;
    assert_eq!(first["bridge_thread_provisioned"], false);

    // A second upsert with different (still valid) config must not
    // provision one either — Discord mints fresh per-conversation threads
    // on demand instead.
    let mut second_body = valid_config_body(true);
    second_body["allowed_roles"] = serde_json::json!(["a-different-role"]);
    let resp = upsert_discord(&router, &agent.id, second_body).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/agents/{}", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let profile = read_body(resp).await;
    assert!(profile["channels"][0]["bridge_thread_id"].is_null());
    assert_eq!(
        profile["channels"][0]["kind_config"]["allowed_roles"],
        serde_json::json!(["a-different-role"])
    );
}

#[tokio::test]
async fn delete_clears_config_secret_and_status() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("discord-agent-delete");
    create_agent(&router, &agent).await;

    upsert_discord(&router, &agent.id, valid_config_body(true)).await;
    set_secret(&router, &agent.id, "bot-token-delete").await;

    let channels = list_channels(&router, &agent.id).await;
    let discord = discord_entry(&channels);
    assert_eq!(discord["secret_stored"], true);
    assert_eq!(discord["enabled"], true);

    let resp = delete_discord(router.clone(), &agent.id).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let channels = list_channels(&router, &agent.id).await;
    assert!(channels.as_array().unwrap().iter().all(|c| c["kind"] != "discord"));

    // Re-adding a binding must not resurrect the deleted secret.
    upsert_discord(&router, &agent.id, valid_config_body(false)).await;
    let channels = list_channels(&router, &agent.id).await;
    let discord = discord_entry(&channels);
    assert_eq!(discord["secret_stored"], false);
}

#[tokio::test]
async fn delete_on_agent_with_no_discord_binding_is_idempotent() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("discord-agent-delete-noop");
    create_agent(&router, &agent).await;

    let resp = delete_discord(router.clone(), &agent.id).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = delete_discord(router, &agent.id).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}
