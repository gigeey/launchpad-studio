use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use ao_engine::AppState;
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{
    AgentProfile, ChannelKind, CliProviderConfig, InputMode, OutputFormat, PairingCode,
    ProviderConfig, TelegramConfig, TelegramThreadMode,
};
use ao_server::routes::build_router;

/// `TelegramClient::new()` and `TelegramTokenStore::open()` both read
/// process-wide env vars, so tests in this file must not run concurrently
/// with each other. Run with `--test-threads=1`.
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

const API_BASE_ENV_VAR: &str = "LAUNCHPAD_TELEGRAM_API_BASE_URL";
const FILE_FALLBACK_ENV_VAR: &str = "LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK";

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

/// Sets up a router plus a mock Telegram API server. Holds `ENV_MUTEX` for
/// the lifetime of the returned guard so `LAUNCHPAD_STUDIO_DATA_DIR`,
/// `LAUNCHPAD_TELEGRAM_API_BASE_URL`, and the file-fallback flag stay fixed
/// for the whole test.
async fn setup() -> (axum::Router, Arc<AppState>, tempfile::TempDir, MockServer, std::sync::MutexGuard<'static, ()>) {
    let guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
    std::env::set_var(API_BASE_ENV_VAR, mock_server.uri());
    std::env::set_var(FILE_FALLBACK_ENV_VAR, "1");

    let mock = MockProcessSupervisor::new(vec![]);
    let state = Arc::new(AppState::new_with_mock(mock).await.expect("init state"));
    let router = build_router(Arc::clone(&state));

    (router, state, tmp, mock_server, guard)
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

async fn mount_get_me_success(mock_server: &MockServer, token: &str, username: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/bot{token}/getMe")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "id": 42,
                "is_bot": true,
                "username": username,
                "first_name": "Axew Research",
            }
        })))
        .mount(mock_server)
        .await;
}

async fn mount_get_me_unauthorized(mock_server: &MockServer, token: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/bot{token}/getMe")))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "ok": false,
            "description": "Unauthorized",
        })))
        .mount(mock_server)
        .await;
}

#[tokio::test]
async fn put_token_validates_stores_and_enables() {
    let (router, _state, _tmp, mock_server, _guard) = setup().await;
    let agent = make_test_profile("tg-agent");
    create_agent(&router, &agent).await;

    mount_get_me_success(&mock_server, "123:valid-token", "axew_research_bot").await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{}/telegram/token", agent.id))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "token": "123:valid-token" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["bot_username"], "axew_research_bot");

    // Status reflects the stored token, cached username, and enabled flag —
    // and never echoes the token itself.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/agents/{}/telegram/status", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let status = read_body(resp).await;
    assert_eq!(status["has_token"], true);
    assert_eq!(status["bot_username"], "axew_research_bot");
    assert_eq!(status["enabled"], true);
    assert_eq!(status["linked"], false);
    assert!(status.get("token").is_none());

    // Full-profile GET also never carries the secret.
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
    assert_eq!(profile["channels"][0]["enabled"], true);
    assert_eq!(
        profile["channels"][0]["kind_config"]["bot_username"],
        "axew_research_bot"
    );
}

#[tokio::test]
async fn put_token_rejects_invalid_token_without_storing() {
    let (router, _state, _tmp, mock_server, _guard) = setup().await;
    let agent = make_test_profile("tg-agent-invalid");
    create_agent(&router, &agent).await;

    mount_get_me_unauthorized(&mock_server, "bad-token").await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{}/telegram/token", agent.id))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "token": "bad-token" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = read_body(resp).await;
    assert_eq!(body["error"], "invalid Telegram bot token");

    // Nothing was persisted: status still shows no token configured.
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/agents/{}/telegram/status", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = read_body(resp).await;
    assert_eq!(status["has_token"], false);
    assert_eq!(status["enabled"], false);
}

#[tokio::test]
async fn put_token_rejects_empty_token() {
    let (router, _state, _tmp, _mock_server, _guard) = setup().await;
    let agent = make_test_profile("tg-agent-empty");
    create_agent(&router, &agent).await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{}/telegram/token", agent.id))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "token": "  " }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_token_unknown_agent_returns_404() {
    let (router, _state, _tmp, mock_server, _guard) = setup().await;
    mount_get_me_success(&mock_server, "123:valid-token", "axew_research_bot").await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/agents/does-not-exist/telegram/token")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "token": "123:valid-token" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_token_clears_token_and_disables() {
    let (router, _state, _tmp, mock_server, _guard) = setup().await;
    let agent = make_test_profile("tg-agent-delete");
    create_agent(&router, &agent).await;

    mount_get_me_success(&mock_server, "123:valid-token", "axew_research_bot").await;
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{}/telegram/token", agent.id))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "token": "123:valid-token" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/agents/{}/telegram/token", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/agents/{}/telegram/status", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = read_body(resp).await;
    assert_eq!(status["has_token"], false);
    assert_eq!(status["bot_username"], serde_json::Value::Null);
    assert_eq!(status["enabled"], false);
}

#[tokio::test]
async fn status_for_agent_with_no_token_is_all_false() {
    let (router, _state, _tmp, _mock_server, _guard) = setup().await;
    let agent = make_test_profile("tg-agent-fresh");
    create_agent(&router, &agent).await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/agents/{}/telegram/status", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let status = read_body(resp).await;
    assert_eq!(status["has_token"], false);
    assert_eq!(status["bot_username"], serde_json::Value::Null);
    assert_eq!(status["enabled"], false);
    assert_eq!(status["linked"], false);
}

// --- Dedicated bridge thread provisioning ---

async fn put_agent(router: &axum::Router, profile: &AgentProfile) -> AgentProfile {
    let body = serde_json::to_string(profile).unwrap();
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{}", profile.id))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.expect("read body").to_bytes();
    serde_json::from_slice(&bytes).expect("valid AgentProfile body")
}

#[tokio::test]
async fn enabling_telegram_does_not_provision_a_bridge_thread() {
    // Telegram mints a fresh per-conversation bridge thread on demand for
    // every distinct `chat_id` it sees (see
    // `resolve_telegram_conversation_thread`), rather than routing every
    // conversation through one eagerly-provisioned thread — so enabling a
    // binding never provisions one anymore.
    let (router, _state, _tmp, _mock_server, _guard) = setup().await;
    let agent = make_test_profile("tg-agent-provision");
    create_agent(&router, &agent).await;

    let mut enabled = agent.clone();
    enabled.set_telegram_config(Some(TelegramConfig {
        enabled: true,
        bot_username: Some("axew_research_bot".to_string()),
        thread_mode: TelegramThreadMode::Dedicated,
        bridge_thread_id: None,
        allowed_chat_ids: vec![],
        pending_pairing_code: None,
    }));

    let saved = put_agent(&router, &enabled).await;
    let telegram = saved.telegram_config_view().expect("telegram config present");
    assert!(telegram.bridge_thread_id.is_none(), "enabling telegram must not provision a bridge_thread_id");
}

#[tokio::test]
async fn telegram_bridge_thread_id_stays_unprovisioned_across_subsequent_saves() {
    let (router, _state, _tmp, _mock_server, _guard) = setup().await;
    let agent = make_test_profile("tg-agent-preserve");
    create_agent(&router, &agent).await;

    let mut enabled = agent.clone();
    enabled.set_telegram_config(Some(TelegramConfig {
        enabled: true,
        bot_username: Some("axew_research_bot".to_string()),
        thread_mode: TelegramThreadMode::Dedicated,
        bridge_thread_id: None,
        allowed_chat_ids: vec![],
        pending_pairing_code: None,
    }));
    let first_save = put_agent(&router, &enabled).await;
    assert!(
        first_save.telegram_config_view().and_then(|t| t.bridge_thread_id).is_none(),
        "enabling telegram must not provision a bridge_thread_id"
    );

    // A subsequent PUT that tries to set bridge_thread_id must be ignored —
    // it's server-owned, and Telegram no longer provisions one at all.
    let mut clobber_attempt = first_save.clone();
    let mut clobber_telegram = clobber_attempt.telegram_config_view().unwrap();
    clobber_telegram.bridge_thread_id = Some("client-supplied-bogus-id".to_string());
    clobber_attempt.set_telegram_config(Some(clobber_telegram));
    let second_save = put_agent(&router, &clobber_attempt).await;
    assert!(
        second_save.telegram_config_view().unwrap().bridge_thread_id.is_none(),
        "server-owned bridge_thread_id must not be set from a client-supplied value"
    );
}

/// Regression for the atomicity bug: `PUT /agents/{id}/telegram/token`
/// enables the binding in the same request — not leave the agent silently
/// unpolled until a later, unrelated `PUT /agents/{id}` happens to enable
/// it. Provisioning a bridge thread is no longer part of that atomicity
/// guarantee: Telegram mints its per-conversation threads lazily, on first
/// inbound message, so there is nothing to provision here at all.
#[tokio::test]
async fn set_telegram_token_alone_enables_the_binding_without_provisioning_a_bridge_thread() {
    let (router, _state, _tmp, mock_server, _guard) = setup().await;
    let agent = make_test_profile("tg-agent-token-only");
    create_agent(&router, &agent).await;

    mount_get_me_success(&mock_server, "123:token-only", "axew_research_bot").await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{}/telegram/token", agent.id))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "token": "123:token-only" }).to_string(),
                ))
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
                .uri(format!("/agents/{}", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.expect("read body").to_bytes();
    let saved: AgentProfile = serde_json::from_slice(&bytes).expect("valid AgentProfile body");
    let telegram = saved.telegram_config_view().expect("telegram config present");
    assert!(telegram.enabled);
    assert!(telegram.bridge_thread_id.is_none(), "setting the token alone must not provision a bridge_thread_id");
}

// --- Pairing codes and chat unlinking ---

async fn create_pairing_code(router: &axum::Router, agent_id: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/agents/{agent_id}/telegram/pairing-code"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn delete_chat(router: &axum::Router, agent_id: &str, chat_id: i64) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/agents/{agent_id}/telegram/chats/{chat_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn get_status(router: &axum::Router, agent_id: &str) -> serde_json::Value {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/agents/{agent_id}/telegram/status"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    read_body(resp).await
}

#[tokio::test]
async fn pairing_code_generate_returns_valid_code_and_persists() {
    let (router, _state, _tmp, _mock_server, _guard) = setup().await;
    let agent = make_test_profile("tg-agent-pairing");
    create_agent(&router, &agent).await;

    let resp = create_pairing_code(&router, &agent.id).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    let code = body["code"].as_str().expect("code is a string");
    assert_eq!(code.chars().count(), 6);
    assert!(body["expires_at_unix"].as_i64().is_some());

    // Persisted onto the agent's telegram config and visible via status.
    let status = get_status(&router, &agent.id).await;
    assert_eq!(status["pending_pairing_code"]["code"], code);
    assert_eq!(
        status["pending_pairing_code"]["expires_at_unix"],
        body["expires_at_unix"]
    );

    // Regenerating overwrites the prior pending code.
    let resp = create_pairing_code(&router, &agent.id).await;
    let second_body = read_body(resp).await;
    let status = get_status(&router, &agent.id).await;
    assert_eq!(
        status["pending_pairing_code"]["code"],
        second_body["code"]
    );
}

#[tokio::test]
async fn unlink_removes_chat_id_and_is_idempotent() {
    let (router, _state, _tmp, _mock_server, _guard) = setup().await;

    // Seed the already-linked allow-list on the creating `POST /agents` call
    // rather than a later `PUT /agents/{id}` — a whole-document PUT now
    // always forces `allowed_senders` from the server's existing copy
    // (fail-closed, see `merge_preserving_non_telegram_channels`), so it can
    // no longer be used to seed test state the way this test used to.
    let mut agent = make_test_profile("tg-agent-unlink");
    agent.set_telegram_config(Some(TelegramConfig {
        enabled: true,
        bot_username: Some("axew_research_bot".to_string()),
        thread_mode: TelegramThreadMode::default(),
        bridge_thread_id: None,
        allowed_chat_ids: vec![111, 222],
        pending_pairing_code: None,
    }));
    create_agent(&router, &agent).await;

    let resp = delete_chat(&router, &agent.id, 111).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["allowed_chat_ids"], serde_json::json!([222]));

    // Idempotent: deleting an id that's already gone leaves the list as-is.
    let resp = delete_chat(&router, &agent.id, 111).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["allowed_chat_ids"], serde_json::json!([222]));
}

#[tokio::test]
async fn token_delete_resets_allow_list_and_pending_code() {
    let (router, state, _tmp, mock_server, _guard) = setup().await;
    let agent = make_test_profile("tg-agent-full-reset");
    create_agent(&router, &agent).await;

    mount_get_me_success(&mock_server, "123:valid-token", "axew_research_bot").await;
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{}/telegram/token", agent.id))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "token": "123:valid-token" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    create_pairing_code(&router, &agent.id).await;

    // Simulate two chats already linked. A real pairing only ever writes to
    // `LinkedSenderStore` (see `try_link_chat`), and a client PUT can no
    // longer set the deprecated inline `allowed_senders` either — so this
    // seeds both directly through the persistence layer, the same
    // privileged, non-HTTP path production's own pairing/display code uses.
    state
        .persistence
        .linked_senders
        .add_sender(&agent.id, "telegram", "111")
        .await
        .unwrap();
    state
        .persistence
        .linked_senders
        .add_sender(&agent.id, "telegram", "222")
        .await
        .unwrap();
    let mut profile = state.persistence.agents.get(&agent.id).await.unwrap().unwrap();
    if let Some(binding) = profile.channel_of_kind_mut(ChannelKind::Telegram) {
        binding.allowed_senders = vec!["111".to_string(), "222".to_string()];
    }
    state.persistence.agents.update(&profile).await.unwrap();

    let status = get_status(&router, &agent.id).await;
    assert_eq!(status["allowed_chat_ids"], serde_json::json!([111, 222]));
    assert!(status["pending_pairing_code"]["code"].is_string());

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/agents/{}/telegram/token", agent.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let status = get_status(&router, &agent.id).await;
    assert_eq!(status["allowed_chat_ids"], serde_json::json!([]));
    assert_eq!(status["pending_pairing_code"], serde_json::Value::Null);

    // The security-relevant half: deleting the token must revoke every
    // sender that was actually authorized via `LinkedSenderStore`, not just
    // reset the deprecated inline display field above.
    let remaining = state
        .persistence
        .linked_senders
        .get(&agent.id, "telegram")
        .await
        .unwrap()
        .unwrap_or_default()
        .senders;
    assert!(
        remaining.is_empty(),
        "token delete must revoke linked senders from LinkedSenderStore, not just clear the inline field"
    );
}

#[tokio::test]
async fn status_hides_expired_pending_pairing_code() {
    let (router, _state, _tmp, _mock_server, _guard) = setup().await;

    // Seeded on the creating `POST /agents` call, not a later PUT — see the
    // comment in `unlink_removes_chat_id_and_is_idempotent` above.
    let mut agent = make_test_profile("tg-agent-expired-code");
    agent.set_telegram_config(Some(TelegramConfig {
        enabled: true,
        bot_username: None,
        thread_mode: TelegramThreadMode::default(),
        bridge_thread_id: None,
        allowed_chat_ids: vec![999],
        pending_pairing_code: Some(PairingCode {
            code: "STALE1".to_string(),
            expires_at_unix: 1,
        }),
    }));
    create_agent(&router, &agent).await;

    let status = get_status(&router, &agent.id).await;
    assert_eq!(status["pending_pairing_code"], serde_json::Value::Null);
    // Allow-list is unaffected by pending-code expiry.
    assert_eq!(status["allowed_chat_ids"], serde_json::json!([999]));
}
