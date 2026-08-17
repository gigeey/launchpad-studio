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

/// `ChannelSecretStore::open()` and `TelegramTokenStore::open()` both read
/// process-wide env vars, so tests in this file must not run concurrently
/// with each other or with `telegram_routes.rs`. Run with `--test-threads=1`.
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
        "address": "agent@example.com",
        "imap_host": "imap.example.com",
        "imap_port": 993,
        "smtp_host": "smtp.example.com",
        "smtp_port": 587,
        "poll_secs": 60,
        "require_auth_results": true,
        "allowed_senders": ["boss@example.com"],
        "enabled": enabled,
    })
}

async fn upsert_email(router: &axum::Router, agent_id: &str, body: serde_json::Value) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{agent_id}/channels/email"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn set_secret(router: &axum::Router, agent_id: &str, password: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{agent_id}/channels/email/secret"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "password": password }).to_string()))
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

fn email_entry(channels: &serde_json::Value) -> serde_json::Value {
    channels
        .as_array()
        .expect("channels is an array")
        .iter()
        .find(|c| c["kind"] == "email")
        .cloned()
        .expect("email binding present")
}

#[tokio::test]
async fn upsert_creates_config_and_get_status_reflects_it() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("email-agent-upsert");
    create_agent(&router, &agent).await;

    let resp = upsert_email(&router, &agent.id, valid_config_body(false)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["binding_id"], "email");
    assert_eq!(body["kind"], "email");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["secret_stored"], false);
    assert_eq!(body["bridge_thread_provisioned"], false);
    assert_eq!(body["allowed_senders"], serde_json::json!(["boss@example.com"]));
    assert_eq!(body["kind_config"]["address"], "agent@example.com");
    assert_eq!(body["kind_config"]["imap_host"], "imap.example.com");
    assert_eq!(body["kind_config"]["imap_port"], 993);
    assert_eq!(body["kind_config"]["smtp_host"], "smtp.example.com");
    assert_eq!(body["kind_config"]["smtp_port"], 587);
    assert_eq!(body["kind_config"]["poll_secs"], 60);
    assert_eq!(body["kind_config"]["require_auth_results"], true);

    let channels = list_channels(&router, &agent.id).await;
    let email = email_entry(&channels);
    assert_eq!(email["enabled"], false);
    assert_eq!(email["kind_config"]["address"], "agent@example.com");

    // Full-profile GET never carries the password (it isn't part of
    // ChannelKindConfig::Email at all).
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
    assert!(profile.to_string().to_lowercase().find("password").is_none());
}

#[tokio::test]
async fn set_secret_marks_secret_stored_and_never_echoes_it() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("email-agent-secret");
    create_agent(&router, &agent).await;
    upsert_email(&router, &agent.id, valid_config_body(false)).await;

    let resp = set_secret(&router, &agent.id, "hunter2-app-password").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["secret_stored"], true);
    assert!(body.get("password").is_none());
    assert!(!body.to_string().contains("hunter2"));

    let channels = list_channels(&router, &agent.id).await;
    let email = email_entry(&channels);
    assert_eq!(email["secret_stored"], true);
    assert!(!channels.to_string().contains("hunter2"));
}

#[tokio::test]
async fn set_secret_rejects_empty_password() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("email-agent-empty-secret");
    create_agent(&router, &agent).await;

    let resp = set_secret(&router, &agent.id, "   ").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upsert_rejects_invalid_config() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("email-agent-invalid");
    create_agent(&router, &agent).await;

    let mut bad = valid_config_body(false);
    bad["address"] = serde_json::json!("not-an-email");
    let resp = upsert_email(&router, &agent.id, bad).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let mut bad = valid_config_body(false);
    bad["imap_port"] = serde_json::json!(0);
    let resp = upsert_email(&router, &agent.id, bad).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let mut bad = valid_config_body(false);
    bad["poll_secs"] = serde_json::json!(0);
    let resp = upsert_email(&router, &agent.id, bad).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upsert_unknown_agent_returns_404() {
    let (router, _tmp, _guard) = setup().await;

    let resp = upsert_email(&router, "does-not-exist", valid_config_body(true)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn enabling_email_does_not_provision_a_bridge_thread() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("email-agent-provision");
    create_agent(&router, &agent).await;

    let resp = upsert_email(&router, &agent.id, valid_config_body(true)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["enabled"], true);
    // Email mints a fresh per-conversation bridge thread on demand for
    // every distinct sender+subject it sees, rather than routing every
    // conversation through one eagerly-provisioned thread — so enabling a
    // binding never provisions one anymore.
    assert_eq!(body["bridge_thread_provisioned"], false);

    let channels = list_channels(&router, &agent.id).await;
    let email = email_entry(&channels);
    assert_eq!(email["bridge_thread_provisioned"], false);

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
    let email_binding = profile["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "email")
        .unwrap();
    assert!(email_binding["bridge_thread_id"].is_null());
}

#[tokio::test]
async fn setting_secret_after_enabling_email_still_does_not_provision_a_bridge_thread() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("email-agent-secret-then-provision");
    create_agent(&router, &agent).await;

    // Set the secret before any config exists — the binding is created
    // disabled, so no bridge thread yet.
    let resp = set_secret(&router, &agent.id, "app-password").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["bridge_thread_provisioned"], false);

    // Enabling via upsert still never provisions one for Email.
    let resp = upsert_email(&router, &agent.id, valid_config_body(true)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["bridge_thread_provisioned"], false);
    assert_eq!(body["secret_stored"], true);
}

#[tokio::test]
async fn set_secret_before_config_stores_secret_without_persisting_empty_allow_list_binding() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("email-agent-secret-before-config");
    create_agent(&router, &agent).await;

    // No `PUT .../channels/email` has ever happened for this agent, so
    // there's no Email binding yet. Setting the password first must not
    // fabricate-and-persist one with an empty `allowed_senders` -- that
    // would fail-closed once the binding is later enabled, silently
    // rejecting every inbound sender (see
    // `ao_engine::channels::email::security::DenyReason::AllowListEmpty`).
    let resp = set_secret(&router, &agent.id, "app-password").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["secret_stored"], true);
    assert_eq!(body["allowed_senders"], serde_json::json!([]));

    let channels = list_channels(&router, &agent.id).await;
    assert!(
        channels.as_array().unwrap().iter().all(|c| c["kind"] != "email"),
        "no email binding should be persisted by the secret-set path alone, got: {channels}"
    );

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
    assert_eq!(profile["channels"], serde_json::json!([]));
}

#[tokio::test]
async fn set_secret_after_config_preserves_existing_allowed_senders() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("email-agent-secret-preserves-allowlist");
    create_agent(&router, &agent).await;

    // Config (with a non-empty allow-list) is saved first, as the normal
    // flow expects.
    upsert_email(&router, &agent.id, valid_config_body(false)).await;

    let resp = set_secret(&router, &agent.id, "app-password").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["secret_stored"], true);
    assert_eq!(body["allowed_senders"], serde_json::json!(["boss@example.com"]));

    let channels = list_channels(&router, &agent.id).await;
    let email = email_entry(&channels);
    assert_eq!(email["allowed_senders"], serde_json::json!(["boss@example.com"]));
    assert_eq!(email["secret_stored"], true);
}

#[tokio::test]
async fn email_bridge_thread_id_stays_unprovisioned_across_subsequent_upserts() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("email-agent-preserve");
    create_agent(&router, &agent).await;

    let resp = upsert_email(&router, &agent.id, valid_config_body(true)).await;
    let first = read_body(resp).await;
    assert_eq!(first["bridge_thread_provisioned"], false);

    // A second upsert with different (still valid) config must not
    // provision one either — Email mints fresh per-conversation threads on
    // demand instead.
    let mut second_body = valid_config_body(true);
    second_body["poll_secs"] = serde_json::json!(120);
    let resp = upsert_email(&router, &agent.id, second_body).await;
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
    assert_eq!(profile["channels"][0]["kind_config"]["poll_secs"], 120);
}

#[tokio::test]
async fn delete_clears_config_secret_and_status() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("email-agent-delete");
    create_agent(&router, &agent).await;

    upsert_email(&router, &agent.id, valid_config_body(true)).await;
    set_secret(&router, &agent.id, "app-password").await;

    let channels = list_channels(&router, &agent.id).await;
    let email = email_entry(&channels);
    assert_eq!(email["secret_stored"], true);
    assert_eq!(email["enabled"], true);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/agents/{}/channels/email", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let channels = list_channels(&router, &agent.id).await;
    assert!(channels.as_array().unwrap().iter().all(|c| c["kind"] != "email"));

    // Re-adding a binding must not resurrect the deleted secret.
    upsert_email(&router, &agent.id, valid_config_body(false)).await;
    let channels = list_channels(&router, &agent.id).await;
    let email = email_entry(&channels);
    assert_eq!(email["secret_stored"], false);
}

#[tokio::test]
async fn delete_on_agent_with_no_email_binding_is_idempotent() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("email-agent-delete-noop");
    create_agent(&router, &agent).await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/agents/{}/channels/email", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/agents/{}/channels/email", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn list_channels_for_fresh_agent_is_empty() {
    let (router, _tmp, _guard) = setup().await;
    let agent = make_test_profile("email-agent-fresh");
    create_agent(&router, &agent).await;

    let channels = list_channels(&router, &agent.id).await;
    assert_eq!(channels, serde_json::json!([]));
}

#[tokio::test]
async fn list_channels_unknown_agent_returns_404() {
    let (router, _tmp, _guard) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/does-not-exist/channels")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
