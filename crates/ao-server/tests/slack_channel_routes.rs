use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use ao_engine::channels::slack::fake_seam::FakeSlackApiSeam;
use ao_engine::channels::slack::web_api_seam::{SlackApiCallError, SlackAuthTestResult};
use ao_engine::AppState;
use ao_engine_tools_provider_config::{
    ChannelSecretStore, SLACK_APP_TOKEN_SECRET_ROLE, SLACK_BOT_TOKEN_SECRET_ROLE,
};
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{AgentProfile, ChannelKind, ChannelKindConfig, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use ao_protocol::slack_manifest::SLACK_REQUIRED_BOT_SCOPES;
use ao_server::routes::build_router;
use ao_server::routes::channels::run_slack_test_connection;

/// `ChannelSecretStore::open()` reads process-wide env vars, so tests in
/// this file must not run concurrently with each other or with
/// `discord_channel_routes.rs`/`email_channel_routes.rs`/`telegram_routes.rs`.
/// Run with `--test-threads=1`.
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

const CHANNEL_SECRET_FALLBACK_ENV_VAR: &str = "LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK";
const TELEGRAM_FALLBACK_ENV_VAR: &str = "LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK";

const VALID_BOT_TOKEN: &str = "xoxb-111-222-abcdef";
const VALID_APP_TOKEN: &str = "xapp-1-A11AAAAAA-2222222222222-abcdef0123456789abcdef0123456789";

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
        "allowed_users": ["U11112222"],
        "allowed_channels": ["C33334444"],
        "conversation_mode": "per_conversation",
        "enabled": enabled,
    })
}

async fn upsert_slack(router: &axum::Router, agent_id: &str, body: serde_json::Value) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{agent_id}/channels/slack"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn set_secret(router: &axum::Router, agent_id: &str, bot_token: &str, app_token: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{agent_id}/channels/slack/secret"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "bot_token": bot_token, "app_token": app_token }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn delete_slack(router: axum::Router, agent_id: &str) -> axum::response::Response {
    router
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/agents/{agent_id}/channels/slack"))
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

fn slack_entry(channels: &serde_json::Value) -> serde_json::Value {
    channels
        .as_array()
        .expect("channels is an array")
        .iter()
        .find(|c| c["kind"] == "slack")
        .cloned()
        .expect("slack binding present")
}

#[tokio::test]
async fn upsert_creates_config_and_get_status_reflects_it() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-upsert");
    create_agent(&router, &agent).await;

    let resp = upsert_slack(&router, &agent.id, valid_config_body(false)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["binding_id"], "slack");
    assert_eq!(body["kind"], "slack");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["secret_stored"], false);
    assert_eq!(body["bridge_thread_provisioned"], false);
    assert_eq!(body["kind_config"]["allowed_users"], serde_json::json!(["U11112222"]));
    assert_eq!(body["kind_config"]["allowed_channels"], serde_json::json!(["C33334444"]));
    assert_eq!(body["kind_config"]["conversation_mode"], "per_conversation");

    let channels = list_channels(&router, &agent.id).await;
    let slack = slack_entry(&channels);
    assert_eq!(slack["enabled"], false);
    assert_eq!(slack["kind_config"]["allowed_users"], serde_json::json!(["U11112222"]));
}

#[tokio::test]
async fn upsert_defaults_missing_lists_to_empty_and_fails_closed() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-defaults");
    create_agent(&router, &agent).await;

    let resp = upsert_slack(&router, &agent.id, serde_json::json!({ "enabled": false })).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["kind_config"]["allowed_users"], serde_json::json!([]));
    assert_eq!(body["kind_config"]["allowed_channels"], serde_json::json!([]));
    assert_eq!(body["kind_config"]["conversation_mode"], "per_conversation");
    assert!(body["kind_config"]["connection_id"].is_null());
}

#[tokio::test]
async fn upsert_unknown_agent_returns_404() {
    let (router, _tmp, _guard) = setup().await;

    let resp = upsert_slack(&router, "does-not-exist", valid_config_body(true)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_secret_stores_both_tokens_under_distinct_roles_from_one_request() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-dual-secret");
    create_agent(&router, &agent).await;
    upsert_slack(&router, &agent.id, valid_config_body(false)).await;

    let resp = set_secret(&router, &agent.id, VALID_BOT_TOKEN, VALID_APP_TOKEN).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["secret_stored"], true);

    // Read the two roles back directly from the store to prove they landed
    // under distinct roles rather than one overwriting the other.
    let store = ChannelSecretStore::open().expect("open secret store");
    let stored_bot = store
        .get(&agent.id, "slack", SLACK_BOT_TOKEN_SECRET_ROLE)
        .expect("get bot token")
        .expect("bot token present");
    let stored_app = store
        .get(&agent.id, "slack", SLACK_APP_TOKEN_SECRET_ROLE)
        .expect("get app token")
        .expect("app token present");
    assert_eq!(stored_bot, VALID_BOT_TOKEN);
    assert_eq!(stored_app, VALID_APP_TOKEN);
    assert_ne!(stored_bot, stored_app);
}

#[tokio::test]
async fn set_secret_rejects_swapped_prefixes_with_400() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-swapped");
    create_agent(&router, &agent).await;

    // Bot/app tokens pasted into the wrong fields.
    let resp = set_secret(&router, &agent.id, VALID_APP_TOKEN, VALID_BOT_TOKEN).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Neither token should have been stored.
    let store = ChannelSecretStore::open().expect("open secret store");
    assert!(store.get(&agent.id, "slack", SLACK_BOT_TOKEN_SECRET_ROLE).unwrap().is_none());
    assert!(store.get(&agent.id, "slack", SLACK_APP_TOKEN_SECRET_ROLE).unwrap().is_none());
}

#[tokio::test]
async fn set_secret_rejects_empty_bot_token() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-empty-bot");
    create_agent(&router, &agent).await;

    let resp = set_secret(&router, &agent.id, "   ", VALID_APP_TOKEN).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn set_secret_rejects_empty_app_token() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-empty-app");
    create_agent(&router, &agent).await;

    let resp = set_secret(&router, &agent.id, VALID_BOT_TOKEN, "   ").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn secret_stored_is_false_until_both_tokens_are_present() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-partial");
    create_agent(&router, &agent).await;

    // Directly store only the bot token, bypassing the (all-or-nothing)
    // route, to exercise `secret_stored_for`'s partial-state handling.
    let store = ChannelSecretStore::open().expect("open secret store");
    upsert_slack(&router, &agent.id, valid_config_body(false)).await;
    store
        .set(&agent.id, "slack", SLACK_BOT_TOKEN_SECRET_ROLE, VALID_BOT_TOKEN)
        .expect("set bot token");

    let channels = list_channels(&router, &agent.id).await;
    let slack = slack_entry(&channels);
    assert_eq!(
        slack["secret_stored"], false,
        "one of two tokens stored must not report as fully configured"
    );

    // Now the route stores both — secret_stored flips to true.
    let resp = set_secret(&router, &agent.id, VALID_BOT_TOKEN, VALID_APP_TOKEN).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let channels = list_channels(&router, &agent.id).await;
    let slack = slack_entry(&channels);
    assert_eq!(slack["secret_stored"], true);
}

#[tokio::test]
async fn get_never_emits_token_material() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-no-leak");
    create_agent(&router, &agent).await;
    upsert_slack(&router, &agent.id, valid_config_body(true)).await;
    let resp = set_secret(&router, &agent.id, VALID_BOT_TOKEN, VALID_APP_TOKEN).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let secret_response_text = read_body(resp).await.to_string();
    assert!(!secret_response_text.contains(VALID_BOT_TOKEN));
    assert!(!secret_response_text.contains(VALID_APP_TOKEN));
    assert!(!secret_response_text.to_lowercase().contains("bot_token"));
    assert!(!secret_response_text.to_lowercase().contains("app_token"));

    // The list endpoint's raw serialized JSON body must never carry either
    // token value or field name either.
    let channels = list_channels(&router, &agent.id).await;
    let channels_text = channels.to_string();
    assert!(!channels_text.contains(VALID_BOT_TOKEN));
    assert!(!channels_text.contains(VALID_APP_TOKEN));
    assert!(!channels_text.to_lowercase().contains("bot_token"));
    assert!(!channels_text.to_lowercase().contains("app_token"));
    let slack = slack_entry(&channels);
    assert_eq!(slack["secret_stored"], true);

    // Full-profile GET must never carry either token either.
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
    let profile_text = read_body(resp).await.to_string();
    assert!(!profile_text.contains(VALID_BOT_TOKEN));
    assert!(!profile_text.contains(VALID_APP_TOKEN));
}

#[tokio::test]
async fn enabling_slack_provisions_a_bridge_thread() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-provision");
    create_agent(&router, &agent).await;

    let resp = upsert_slack(&router, &agent.id, valid_config_body(true)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["enabled"], true);
    assert_eq!(body["bridge_thread_provisioned"], true);

    let resp = router
        .clone()
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
    let slack_binding = profile["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "slack")
        .unwrap();
    let bridge_thread_id = slack_binding["bridge_thread_id"].as_str().expect("bridge_thread_id present");

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/threads/{bridge_thread_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let thread = read_body(resp).await;
    assert_eq!(thread["title"], "💬 Slack");
    // The provisioned thread must carry `channel_origin` naming this Slack
    // binding — this is what lets a client-side composer-gating hint and the
    // backend's `is_channel_bridge_thread` tool-admission gate recognize a
    // real Slack conversation thread too, since Slack never populates
    // `bridge_thread_id` at runtime (see `ChannelBridgeOrigin`'s docstring).
    assert_eq!(thread["channel_origin"]["kind"], "slack");
    assert_eq!(thread["channel_origin"]["binding_id"], slack_binding["binding_id"]);
}

#[tokio::test]
async fn delete_removes_config_and_both_secrets() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-delete");
    create_agent(&router, &agent).await;

    upsert_slack(&router, &agent.id, valid_config_body(true)).await;
    set_secret(&router, &agent.id, VALID_BOT_TOKEN, VALID_APP_TOKEN).await;

    let channels = list_channels(&router, &agent.id).await;
    let slack = slack_entry(&channels);
    assert_eq!(slack["secret_stored"], true);
    assert_eq!(slack["enabled"], true);

    let resp = delete_slack(router.clone(), &agent.id).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let channels = list_channels(&router, &agent.id).await;
    assert!(channels.as_array().unwrap().iter().all(|c| c["kind"] != "slack"));

    // Both secrets must actually be gone from the store, not just hidden by
    // the binding's removal.
    let store = ChannelSecretStore::open().expect("open secret store");
    assert!(store.get(&agent.id, "slack", SLACK_BOT_TOKEN_SECRET_ROLE).unwrap().is_none());
    assert!(store.get(&agent.id, "slack", SLACK_APP_TOKEN_SECRET_ROLE).unwrap().is_none());

    // Re-adding a binding must not resurrect the deleted secrets.
    upsert_slack(&router, &agent.id, valid_config_body(false)).await;
    let channels = list_channels(&router, &agent.id).await;
    let slack = slack_entry(&channels);
    assert_eq!(slack["secret_stored"], false);
}

#[tokio::test]
async fn delete_on_agent_with_no_slack_binding_is_idempotent() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-delete-noop");
    create_agent(&router, &agent).await;

    let resp = delete_slack(router.clone(), &agent.id).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = delete_slack(router, &agent.id).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

// --- Manifest generator ------------------------------------------------

async fn get_manifest(router: &axum::Router, agent_id: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/agents/{agent_id}/channels/slack/manifest"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn manifest_route_returns_yaml_with_socket_mode_and_every_required_scope() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-manifest");
    create_agent(&router, &agent).await;

    let resp = get_manifest(&router, &agent.id).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    let yaml = body["manifest_yaml"].as_str().expect("manifest_yaml is a string");

    assert!(yaml.contains("socket_mode_enabled: true"));
    for scope in SLACK_REQUIRED_BOT_SCOPES {
        assert!(yaml.contains(scope), "manifest missing required bot scope {scope}");
    }
    for event in ["app_mention", "message.im", "message.channels", "message.groups"] {
        assert!(yaml.contains(event), "manifest missing event subscription {event}");
    }
    // The app-level token scope has no manifest field of its own — it must
    // still show up in the leading comment instructing the manual step.
    assert!(yaml.contains("connections:write"));
}

#[tokio::test]
async fn manifest_route_unknown_agent_returns_404() {
    let (router, _tmp, _guard) = setup().await;
    let resp = get_manifest(&router, "does-not-exist").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// --- Test Connection -----------------------------------------------------

/// Builds a router backed by a directly-accessible `Arc<AppState>` — the
/// shared `setup()` helper above only returns the router, but
/// `run_slack_test_connection` (the fake-seam-testable logic
/// `test_slack_connection`'s route handler delegates to) takes `&AppState`
/// directly, bypassing HTTP entirely so these tests never touch the network.
async fn setup_with_state() -> (axum::Router, Arc<AppState>, tempfile::TempDir, std::sync::MutexGuard<'static, ()>)
{
    let guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::tempdir().expect("Failed to create temp dir");

    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
    std::env::set_var(CHANNEL_SECRET_FALLBACK_ENV_VAR, "1");
    std::env::set_var(TELEGRAM_FALLBACK_ENV_VAR, "1");

    let mock = MockProcessSupervisor::new(vec![]);
    let state = Arc::new(AppState::new_with_mock(mock).await.expect("init state"));
    let router = build_router(Arc::clone(&state));

    (router, state, tmp, guard)
}

fn sample_identity() -> SlackAuthTestResult {
    SlackAuthTestResult {
        team: "Acme Corp".to_string(),
        team_id: "T0123ABCD".to_string(),
        user: "launchpad-bot".to_string(),
        user_id: "U0456WXYZ".to_string(),
        granted_scopes: SLACK_REQUIRED_BOT_SCOPES.iter().map(|s| s.to_string()).collect(),
    }
}

async fn test_connection_route(router: &axum::Router, agent_id: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/agents/{agent_id}/channels/slack/test-connection"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn test_connection_route_without_stored_tokens_returns_400_and_touches_no_seam() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("slack-agent-test-connection-no-tokens");
    create_agent(&router, &agent).await;
    upsert_slack(&router, &agent.id, valid_config_body(true)).await;

    // No `set_secret` call — the route must reject before ever constructing
    // a seam (real or fake), which is exactly what makes this test safe to
    // run through the real HTTP route without a network dependency.
    let resp = test_connection_route(&router, &agent.id).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_connection_route_unknown_agent_returns_404() {
    let (router, _tmp, _guard) = setup().await;
    let resp = test_connection_route(&router, "does-not-exist").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn run_slack_test_connection_with_all_checks_passing_persists_identity_and_bot_user_id() {
    let (router, state, _tmp, _guard) = setup_with_state().await;
    let agent = make_test_profile("slack-agent-test-connection-pass");
    create_agent(&router, &agent).await;
    upsert_slack(&router, &agent.id, valid_config_body(true)).await;
    set_secret(&router, &agent.id, VALID_BOT_TOKEN, VALID_APP_TOKEN).await;

    let seam = FakeSlackApiSeam::all_checks_pass(sample_identity());
    let report = run_slack_test_connection(&state, &agent.id, &seam).await.unwrap_or_else(|e| panic!("test connection failed: {}", e.0));

    assert!(report.auth_check.passed);
    assert!(report.connections_open_check.passed);
    assert!(report.scopes.iter().all(|s| s.granted));

    // The report itself must never carry either token.
    let report_json = serde_json::to_string(&report).unwrap();
    assert!(!report_json.contains(VALID_BOT_TOKEN));
    assert!(!report_json.contains(VALID_APP_TOKEN));

    let identity = report.identity.expect("identity present");
    assert_eq!(identity.team_id, "T0123ABCD");
    assert_eq!(identity.team_name, "Acme Corp");
    assert_eq!(identity.bot_user_id, "U0456WXYZ");

    // `bot_user_id` is load-bearing for the bot-echo guard — prove it
    // actually lands in the persisted connection record, not just the
    // in-memory report.
    let profile = state.persistence.agents.get(&agent.id).await.unwrap().expect("agent exists");
    let slack_binding = profile.channels.iter().find(|c| c.kind == ChannelKind::Slack).expect("slack binding");
    let connection_id = match &slack_binding.kind_config {
        ChannelKindConfig::Slack { connection_id: Some(id), .. } => id.clone(),
        other => panic!("expected connection_id to be set after a successful test connection, got {other:?}"),
    };
    let connection = state
        .persistence
        .slack_connections
        .get(&connection_id)
        .await
        .unwrap()
        .expect("connection record persisted");
    assert_eq!(connection.team_id, "T0123ABCD");
    assert_eq!(connection.team_name, "Acme Corp");
    assert_eq!(connection.bot_user_id, "U0456WXYZ");
}

#[tokio::test]
async fn run_slack_test_connection_reuses_the_existing_connection_id_on_a_second_run() {
    let (router, state, _tmp, _guard) = setup_with_state().await;
    let agent = make_test_profile("slack-agent-test-connection-rerun");
    create_agent(&router, &agent).await;
    upsert_slack(&router, &agent.id, valid_config_body(true)).await;
    set_secret(&router, &agent.id, VALID_BOT_TOKEN, VALID_APP_TOKEN).await;

    let seam = FakeSlackApiSeam::all_checks_pass(sample_identity());
    run_slack_test_connection(&state, &agent.id, &seam).await.unwrap_or_else(|e| panic!("first run failed: {}", e.0));
    let profile = state.persistence.agents.get(&agent.id).await.unwrap().unwrap();
    let first_connection_id = match &profile.channels.iter().find(|c| c.kind == ChannelKind::Slack).unwrap().kind_config
    {
        ChannelKindConfig::Slack { connection_id: Some(id), .. } => id.clone(),
        other => panic!("expected connection_id after first run, got {other:?}"),
    };

    run_slack_test_connection(&state, &agent.id, &seam).await.unwrap_or_else(|e| panic!("second run failed: {}", e.0));
    let profile = state.persistence.agents.get(&agent.id).await.unwrap().unwrap();
    let second_connection_id = match &profile.channels.iter().find(|c| c.kind == ChannelKind::Slack).unwrap().kind_config
    {
        ChannelKindConfig::Slack { connection_id: Some(id), .. } => id.clone(),
        other => panic!("expected connection_id after second run, got {other:?}"),
    };

    assert_eq!(first_connection_id, second_connection_id, "re-running Test Connection must not orphan the prior record");
}

#[tokio::test]
async fn run_slack_test_connection_missing_one_scope_surfaces_only_that_scope_red() {
    let (router, state, _tmp, _guard) = setup_with_state().await;
    let agent = make_test_profile("slack-agent-test-connection-missing-scope");
    create_agent(&router, &agent).await;
    upsert_slack(&router, &agent.id, valid_config_body(true)).await;
    set_secret(&router, &agent.id, VALID_BOT_TOKEN, VALID_APP_TOKEN).await;

    let mut identity = sample_identity();
    identity.granted_scopes.retain(|s| s != "users:read");
    let seam = FakeSlackApiSeam::all_checks_pass(identity);

    let report = run_slack_test_connection(&state, &agent.id, &seam).await.unwrap_or_else(|e| panic!("test connection failed: {}", e.0));

    let missing = report.scopes.iter().find(|s| s.scope == "users:read").expect("users:read present");
    assert!(!missing.granted);
    for scope in report.scopes.iter().filter(|s| s.scope != "users:read") {
        assert!(scope.granted, "expected {} to still read granted", scope.scope);
    }
}

#[tokio::test]
async fn run_slack_test_connection_bad_app_token_fails_handshake_but_not_auth() {
    let (router, state, _tmp, _guard) = setup_with_state().await;
    let agent = make_test_profile("slack-agent-test-connection-bad-app-token");
    create_agent(&router, &agent).await;
    upsert_slack(&router, &agent.id, valid_config_body(true)).await;
    set_secret(&router, &agent.id, VALID_BOT_TOKEN, VALID_APP_TOKEN).await;

    let seam =
        FakeSlackApiSeam::new(Ok(sample_identity()), Err(SlackApiCallError::Auth("invalid_auth".to_string())));
    let report = run_slack_test_connection(&state, &agent.id, &seam).await.unwrap_or_else(|e| panic!("test connection failed: {}", e.0));

    assert!(report.auth_check.passed, "a bad app token must not fail the bot-token auth check");
    assert!(report.identity.is_some());
    assert!(!report.connections_open_check.passed);

    // Identity still gets persisted even when the handshake check fails —
    // auth.test succeeded on its own, and that's the check that produces
    // the identity this route persists.
    let profile = state.persistence.agents.get(&agent.id).await.unwrap().unwrap();
    let slack_binding = profile.channels.iter().find(|c| c.kind == ChannelKind::Slack).unwrap();
    assert!(matches!(
        slack_binding.kind_config,
        ChannelKindConfig::Slack { connection_id: Some(_), .. }
    ));
}

#[tokio::test]
async fn run_slack_test_connection_network_error_is_distinguishable_from_auth_error() {
    let (router, state, _tmp, _guard) = setup_with_state().await;
    let agent = make_test_profile("slack-agent-test-connection-network-error");
    create_agent(&router, &agent).await;
    upsert_slack(&router, &agent.id, valid_config_body(true)).await;
    set_secret(&router, &agent.id, VALID_BOT_TOKEN, VALID_APP_TOKEN).await;

    let seam = FakeSlackApiSeam::new(Err(SlackApiCallError::Network("connection refused".to_string())), Ok(()));
    let report = run_slack_test_connection(&state, &agent.id, &seam).await.unwrap_or_else(|e| panic!("test connection failed: {}", e.0));

    assert!(!report.auth_check.passed);
    assert!(report.identity.is_none(), "no identity to persist on a network failure");
    let failure = report.auth_check.failure.expect("failure present");
    assert_eq!(failure.message, "connection refused");

    // No connection record should have been provisioned — the check never
    // got far enough to produce an identity to persist.
    let profile = state.persistence.agents.get(&agent.id).await.unwrap().unwrap();
    let slack_binding = profile.channels.iter().find(|c| c.kind == ChannelKind::Slack).unwrap();
    assert!(matches!(slack_binding.kind_config, ChannelKindConfig::Slack { connection_id: None, .. }));
}
