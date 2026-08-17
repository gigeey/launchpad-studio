use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use sha2::Sha256;
use tower::ServiceExt;

use ao_engine::AppState;
use ao_engine_tools_provider_config::ChannelSecretStore;
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use ao_protocol::assignment::Assignment;
use ao_protocol::webhook_template::DEFAULT_GITHUB_PR_REVIEW_TEMPLATE;
use ao_server::routes::build_router;
use ao_server::webhook_gateway::{
    BIND_HOST_ENV_VAR, INSECURE_NO_AUTH_SENTINEL, WEBHOOK_HMAC_SECRET_ROLE, WEBHOOK_SECRET_VAULT_SCOPE,
};

type HmacSha256 = Hmac<Sha256>;

/// `ChannelSecretStore::open()` reads process-wide env vars and this file
/// also flips `AO_BIND_HOST` to exercise the loopback gate, so tests here
/// must not run concurrently with each other or with other files that touch
/// the same vars. Run with `--test-threads=1`.
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

const CHANNEL_SECRET_FALLBACK_ENV_VAR: &str = "LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK";

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

/// Sets up a router with an isolated data root and a forced file-fallback
/// secret store. Holds `ENV_MUTEX` for the lifetime of the returned guard so
/// the process-wide env vars this test file depends on stay fixed for the
/// whole test. Bind host defaults to loopback (`127.0.0.1`, matching
/// `ao_server::webhook_gateway::DEFAULT_BIND_HOST`) unless a test overrides
/// `AO_BIND_HOST` itself before issuing its request.
async fn setup() -> (axum::Router, tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
    let guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::tempdir().expect("Failed to create temp dir");

    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
    std::env::set_var(CHANNEL_SECRET_FALLBACK_ENV_VAR, "1");
    std::env::remove_var(BIND_HOST_ENV_VAR);

    let mock = MockProcessSupervisor::new(vec![]);
    let state = Arc::new(AppState::new_with_mock(mock).await.expect("init state"));
    let router = build_router(state);

    (router, tmp, guard)
}

async fn read_body(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body().collect().await.expect("read body").to_bytes().to_vec()
}

async fn read_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = read_body(resp).await;
    serde_json::from_slice(&bytes).expect("parse json body")
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

/// Creates a `Webhook`-triggered assignment on `route_name` whose
/// `secret_ref` is `secret_ref`. The literal HMAC secret itself is set
/// separately via [`set_route_secret`] — `secret_ref` is only the lookup key.
async fn create_webhook_assignment(router: &axum::Router, agent_id: &str, route_name: &str, secret_ref: &str) -> Assignment {
    create_webhook_assignment_with_trigger(
        router,
        agent_id,
        serde_json::json!({
            "type": "Webhook",
            "route_name": route_name,
            "secret_ref": secret_ref
        }),
    )
    .await
    .expect("create webhook assignment failed")
}

/// Like [`create_webhook_assignment`] but accepts the full `trigger` JSON
/// object (`events`, `filters`, `prompt_template`, `deliver`, …) and returns
/// the raw response outcome instead of asserting success, so callers can
/// also exercise the registration-time validation error path.
async fn create_webhook_assignment_with_trigger(
    router: &axum::Router,
    agent_id: &str,
    trigger_json: serde_json::Value,
) -> Result<Assignment, StatusCode> {
    let body = serde_json::json!({
        "name": "Inbound hook",
        "instruction": "Process the inbound event.",
        "trigger": trigger_json
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
    if resp.status() != StatusCode::OK {
        return Err(resp.status());
    }
    let bytes = read_body(resp).await;
    Ok(serde_json::from_slice(&bytes).expect("parse assignment"))
}

/// Stores the literal HMAC secret a route's `secret_ref` resolves to,
/// through the same `ChannelSecretStore` scope/role the gateway reads.
fn set_route_secret(secret_ref: &str, secret: &str) {
    let store = ChannelSecretStore::open().expect("open channel secret store");
    store.set(WEBHOOK_SECRET_VAULT_SCOPE, secret_ref, WEBHOOK_HMAC_SECRET_ROLE, secret).expect("set webhook secret");
}

fn github_signature(secret: &str, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={:x}", mac.finalize().into_bytes())
}

fn generic_signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mut signed = Vec::new();
    signed.extend_from_slice(timestamp.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(body);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(&signed);
    format!("sha256={:x}", mac.finalize().into_bytes())
}

fn webhook_post(route_name: &str) -> axum::http::request::Builder {
    Request::builder().method(Method::POST).uri(format!("/webhooks/{route_name}")).header("content-type", "application/json")
}

/// A realistic (trimmed) GitHub `pull_request` webhook payload — the fixture
/// for the day-one copy-paste flow: `repository.full_name` +
/// `pull_request.number` are what
/// `github_comment` delivery resolves its target from, and `pull_request.title`
/// is what the default review template renders into the agent's instruction.
fn realistic_pull_request_payload() -> String {
    serde_json::json!({
        "action": "opened",
        "number": 42,
        "pull_request": {
            "number": 42,
            "title": "Fix the flaky retry loop",
            "body": "Patches the retry loop's off-by-one.",
            "html_url": "https://github.com/acme/widgets/pull/42",
            "user": { "login": "octocat" }
        },
        "repository": {
            "full_name": "acme/widgets",
            "name": "widgets",
            "owner": { "login": "acme" }
        },
        "sender": { "login": "octocat" }
    })
    .to_string()
}

// ----- tests -----

#[tokio::test]
async fn valid_github_signature_dispatches_and_returns_200() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-gh-valid")).await;
    create_webhook_assignment(&router, "agent-gh-valid", "github-prs", "gh-prs-secret").await;
    set_route_secret("gh-prs-secret", "top-secret");

    let body: &'static str = r#"{"action":"opened"}"#;
    let sig = github_signature("top-secret", body.as_bytes());

    let resp = router
        .oneshot(
            webhook_post("github-prs")
                .header("X-Hub-Signature-256", sig)
                .header("X-GitHub-Delivery", "delivery-1")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["fired"], 1);
    assert_eq!(json["deduped"], 0);
}

#[tokio::test]
async fn tampered_body_fails_closed() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-gh-tamper")).await;
    create_webhook_assignment(&router, "agent-gh-tamper", "route-tamper", "tamper-secret").await;
    set_route_secret("tamper-secret", "top-secret");

    // Signature computed over a different body than what's actually sent.
    let sig = github_signature("top-secret", b"{\"action\":\"opened\"}");
    let sent_body: &'static str = r#"{"action":"closed"}"#;

    let resp = router
        .oneshot(
            webhook_post("route-tamper")
                .header("X-Hub-Signature-256", sig)
                .header("X-GitHub-Delivery", "delivery-1")
                .body(Body::from(sent_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_signature_fails_closed() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-gh-wrong")).await;
    create_webhook_assignment(&router, "agent-gh-wrong", "route-wrong", "wrong-secret").await;
    set_route_secret("wrong-secret", "top-secret");

    let body: &'static str = r#"{"action":"opened"}"#;
    let sig = github_signature("not-the-right-secret", body.as_bytes());

    let resp = router
        .oneshot(
            webhook_post("route-wrong")
                .header("X-Hub-Signature-256", sig)
                .header("X-GitHub-Delivery", "delivery-1")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn missing_signature_fails_closed() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-gh-missing")).await;
    create_webhook_assignment(&router, "agent-gh-missing", "route-missing", "missing-secret").await;
    set_route_secret("missing-secret", "top-secret");

    let body: &'static str = r#"{"action":"opened"}"#;

    let resp = router.oneshot(webhook_post("route-missing").body(Body::from(body)).unwrap()).await.unwrap();

    let status = resp.status();
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "expected 401 or 403, got {status}"
    );
}

#[tokio::test]
async fn route_with_no_secret_configured_rejects_everything() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-no-secret")).await;
    // No `secret_ref` at all on this route — never call `set_route_secret`.
    let body_json = serde_json::json!({
        "name": "Inbound hook",
        "instruction": "Process the inbound event.",
        "trigger": { "type": "Webhook", "route_name": "route-no-secret" }
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/agent-no-secret/assignments")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body_json).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: &'static str = r#"{"action":"opened"}"#;
    let resp = router
        .oneshot(
            webhook_post("route-no-secret")
                .header("X-Hub-Signature-256", "sha256=deadbeef")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "expected 401 or 403, got {status}"
    );
}

#[tokio::test]
async fn duplicate_github_delivery_id_is_200_noop() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-dedup")).await;
    create_webhook_assignment(&router, "agent-dedup", "route-dedup", "dedup-secret").await;
    set_route_secret("dedup-secret", "top-secret");

    let body: &'static str = r#"{"action":"opened"}"#;
    let sig = github_signature("top-secret", body.as_bytes());

    let first = router
        .clone()
        .oneshot(
            webhook_post("route-dedup")
                .header("X-Hub-Signature-256", sig.clone())
                .header("X-GitHub-Delivery", "delivery-dup")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_json = read_json(first).await;
    assert_eq!(first_json["fired"], 1);

    let second = router
        .oneshot(
            webhook_post("route-dedup")
                .header("X-Hub-Signature-256", sig)
                .header("X-GitHub-Delivery", "delivery-dup")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_json = read_json(second).await;
    assert_eq!(second_json["fired"], 0);
    assert_eq!(second_json["deduped"], 1);
}

#[tokio::test]
async fn generic_timestamped_scheme_valid_passes() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-generic")).await;
    create_webhook_assignment(&router, "agent-generic", "route-generic", "generic-secret").await;
    set_route_secret("generic-secret", "top-secret");

    let body: &'static str = r#"{"hello":"world"}"#;
    let now = chrono::Utc::now().timestamp().to_string();
    let sig = generic_signature("top-secret", &now, body.as_bytes());

    let resp = router
        .oneshot(
            webhook_post("route-generic")
                .header("X-Webhook-Signature", sig)
                .header("X-Webhook-Timestamp", now)
                .header("X-Request-ID", "req-1")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["fired"], 1);
}

#[tokio::test]
async fn insecure_no_auth_allowed_on_loopback_bind() {
    let (router, _tmp, _guard) = setup().await;
    // `setup()` already clears AO_BIND_HOST, which defaults to loopback.
    create_agent(&router, &make_test_profile("agent-insecure-loopback")).await;
    create_webhook_assignment(&router, "agent-insecure-loopback", "route-insecure-lb", "insecure-secret-lb").await;
    set_route_secret("insecure-secret-lb", INSECURE_NO_AUTH_SENTINEL);

    let body: &'static str = r#"{"anything":true}"#;
    let resp = router.oneshot(webhook_post("route-insecure-lb").body(Body::from(body)).unwrap()).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["fired"], 1);
}

#[tokio::test]
async fn insecure_no_auth_refused_on_non_loopback_bind() {
    let (router, _tmp, _guard) = setup().await;
    std::env::set_var(BIND_HOST_ENV_VAR, "0.0.0.0");
    create_agent(&router, &make_test_profile("agent-insecure-public")).await;
    create_webhook_assignment(&router, "agent-insecure-public", "route-insecure-pub", "insecure-secret-pub").await;
    set_route_secret("insecure-secret-pub", INSECURE_NO_AUTH_SENTINEL);

    let body: &'static str = r#"{"anything":true}"#;
    let resp = router.oneshot(webhook_post("route-insecure-pub").body(Body::from(body)).unwrap()).await.unwrap();

    std::env::remove_var(BIND_HOST_ENV_VAR);

    let status = resp.status();
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "expected 401 or 403, got {status}"
    );
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let (router, _tmp, _guard) = setup().await;
    let body: &'static str = r#"{}"#;
    let resp = router.oneshot(webhook_post("no-such-route").body(Body::from(body)).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// The legacy per-assignment trigger endpoint stays reachable alongside the
/// named-route gateway — this is a smoke check, not a full re-test of
/// `assignments_api.rs`'s trigger coverage.
#[tokio::test]
async fn legacy_per_assignment_trigger_route_still_works() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-legacy")).await;
    let body_json = serde_json::json!({
        "name": "Legacy hook",
        "instruction": "Process the inbound event.",
        "trigger": { "type": "Webhook", "token": "legacy-token" }
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/agent-legacy/assignments")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body_json).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let assignment: Assignment = serde_json::from_slice(&read_body(resp).await).unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/assignments/{}/trigger", assignment.id))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "token": "legacy-token" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

// ----- pre-agent relevance gating (events allowlist + declarative filters) -----

#[tokio::test]
async fn event_not_in_allowlist_is_filtered_with_no_agent_run() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-events-filter")).await;
    create_webhook_assignment_with_trigger(
        &router,
        "agent-events-filter",
        serde_json::json!({
            "type": "Webhook",
            "route_name": "route-events-filter",
            "secret_ref": "events-filter-secret",
            "events": ["pull_request"]
        }),
    )
    .await
    .expect("create assignment");
    set_route_secret("events-filter-secret", "top-secret");

    let body: &'static str = r#"{"action":"opened"}"#;
    let sig = github_signature("top-secret", body.as_bytes());

    // X-GitHub-Event says "push", but the route only allows "pull_request".
    let resp = router
        .oneshot(
            webhook_post("route-events-filter")
                .header("X-Hub-Signature-256", sig)
                .header("X-GitHub-Event", "push")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["fired"], 0);
    assert_eq!(json["filtered"], 1);
}

#[tokio::test]
async fn declarative_filter_mismatch_is_filtered_with_no_agent_run() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-field-filter")).await;
    create_webhook_assignment_with_trigger(
        &router,
        "agent-field-filter",
        serde_json::json!({
            "type": "Webhook",
            "route_name": "route-field-filter",
            "secret_ref": "field-filter-secret",
            "filters": { "field": "action", "op": "equals", "value": "opened" }
        }),
    )
    .await
    .expect("create assignment");
    set_route_secret("field-filter-secret", "top-secret");

    let body: &'static str = r#"{"action":"closed"}"#;
    let sig = github_signature("top-secret", body.as_bytes());

    let resp = router
        .oneshot(
            webhook_post("route-field-filter")
                .header("X-Hub-Signature-256", sig)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["fired"], 0);
    assert_eq!(json["filtered"], 1);
}

#[tokio::test]
async fn matching_event_type_with_prompt_template_fires_agent() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-template-fire")).await;
    create_webhook_assignment_with_trigger(
        &router,
        "agent-template-fire",
        serde_json::json!({
            "type": "Webhook",
            "route_name": "route-template-fire",
            "secret_ref": "template-fire-secret",
            "events": ["pull_request"],
            "prompt_template": "Review PR #{pull_request.number}: {pull_request.title}"
        }),
    )
    .await
    .expect("create assignment");
    set_route_secret("template-fire-secret", "top-secret");

    let body: &'static str = r#"{"pull_request":{"number":42,"title":"Fix the flaky retry loop"}}"#;
    let sig = github_signature("top-secret", body.as_bytes());

    let resp = router
        .oneshot(
            webhook_post("route-template-fire")
                .header("X-Hub-Signature-256", sig)
                .header("X-GitHub-Event", "pull_request")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["fired"], 1);
    assert_eq!(json["filtered"], 0);
}

// ----- deliver_only: zero-token notification path -----

#[tokio::test]
async fn deliver_only_route_delivers_without_firing_agent() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-deliver-only")).await;
    create_webhook_assignment_with_trigger(
        &router,
        "agent-deliver-only",
        serde_json::json!({
            "type": "Webhook",
            "route_name": "route-deliver-only",
            "secret_ref": "deliver-only-secret",
            "deliver": { "type": "deliver_only" }
        }),
    )
    .await
    .expect("create assignment");
    set_route_secret("deliver-only-secret", "top-secret");

    let body: &'static str = r#"{"action":"opened"}"#;
    let sig = github_signature("top-secret", body.as_bytes());

    let resp = router
        .oneshot(
            webhook_post("route-deliver-only")
                .header("X-Hub-Signature-256", sig)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["fired"], 0, "deliver_only must never start an agent run");
    assert_eq!(json["delivered"], 1);
}

// ----- route registration validation -----

#[tokio::test]
async fn create_assignment_rejects_deliver_only_without_route_name() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-invalid-deliver")).await;

    let status = create_webhook_assignment_with_trigger(
        &router,
        "agent-invalid-deliver",
        serde_json::json!({
            "type": "Webhook",
            "deliver": { "type": "deliver_only" }
        }),
    )
    .await
    .expect_err("a DeliverOnly route with no route_name must be rejected at registration");

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patch_assignment_rejects_deliver_only_without_route_name() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-patch-invalid-deliver")).await;
    let assignment = create_webhook_assignment(&router, "agent-patch-invalid-deliver", "route-patch-valid", "patch-valid-secret").await;

    let patch_body = serde_json::json!({
        "trigger": {
            "type": "Webhook",
            "deliver": { "type": "deliver_only" }
        }
    });
    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(format!("/assignments/{}", assignment.id))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&patch_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ----- GitHub day-one copy-paste flow -----

/// The flagship demo end-to-end: a signed `pull_request` webhook POST for a
/// route configured exactly the way the copy-paste setup instructions
/// produce (`events: ["pull_request"]`, the default review template) must
/// clear HMAC verification, pass the events allowlist, render the template,
/// and fire the assignment's agent — the full pipeline a real GitHub webhook
/// delivery would drive.
#[tokio::test]
async fn github_pr_review_day_one_flow_fires_agent_with_default_template() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-gh-pr-review")).await;
    create_webhook_assignment_with_trigger(
        &router,
        "agent-gh-pr-review",
        serde_json::json!({
            "type": "Webhook",
            "route_name": "route-gh-pr-review",
            "secret_ref": "gh-pr-review-secret",
            "events": ["pull_request"],
            "prompt_template": DEFAULT_GITHUB_PR_REVIEW_TEMPLATE
        }),
    )
    .await
    .expect("create assignment");
    set_route_secret("gh-pr-review-secret", "top-secret");

    let body = realistic_pull_request_payload();
    let sig = github_signature("top-secret", body.as_bytes());

    let resp = router
        .oneshot(
            webhook_post("route-gh-pr-review")
                .header("X-Hub-Signature-256", sig)
                .header("X-GitHub-Event", "pull_request")
                .header("X-GitHub-Delivery", "delivery-pr-42")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(
        json["fired"], 1,
        "a signed pull_request payload on an events:[pull_request] route with the default review template must fire the agent"
    );
    assert_eq!(json["filtered"], 0);
    assert_eq!(json["deduped"], 0);
}

/// The `github_comment` sibling of the flow above: same signed realistic
/// payload, but the route delivers directly instead of starting an agent —
/// exercises `github_comment`'s payload-driven repo/PR resolution through
/// the full HTTP gateway, not just the unit-level `ao-engine` tests.
#[tokio::test]
async fn github_comment_deliver_target_routes_without_firing_agent() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-gh-comment-deliver")).await;
    create_webhook_assignment_with_trigger(
        &router,
        "agent-gh-comment-deliver",
        serde_json::json!({
            "type": "Webhook",
            "route_name": "route-gh-comment-deliver",
            "secret_ref": "gh-comment-deliver-secret",
            "events": ["pull_request"],
            "prompt_template": "New PR opened: {pull_request.title}",
            "deliver": { "type": "github_comment" }
        }),
    )
    .await
    .expect("create assignment");
    set_route_secret("gh-comment-deliver-secret", "top-secret");

    let body = realistic_pull_request_payload();
    let sig = github_signature("top-secret", body.as_bytes());

    let resp = router
        .oneshot(
            webhook_post("route-gh-comment-deliver")
                .header("X-Hub-Signature-256", sig)
                .header("X-GitHub-Event", "pull_request")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["fired"], 0, "github_comment must never start an agent run");
    assert_eq!(json["delivered"], 1);
}

// ---------------------------------------------------------------------------
// PUT /webhooks/{route_name}/secret
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_webhook_route_secret_rejects_empty() {
    let (router, _tmp, _guard) = setup().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/webhooks/some-route/secret")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "secret": "   " }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn set_webhook_route_secret_wires_end_to_end_with_the_gateway() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-secret-endpoint")).await;
    create_webhook_assignment(&router, "agent-secret-endpoint", "route-secret-endpoint", "route-secret-endpoint").await;

    // No `set_route_secret` test helper call here — the whole point of this
    // test is that the real HTTP endpoint (not the direct-to-store helper)
    // is what the frontend actually calls, and the gateway must honor it.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/webhooks/route-secret-endpoint/secret")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "secret": "set-via-endpoint" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let body = realistic_pull_request_payload();
    let sig = github_signature("set-via-endpoint", body.as_bytes());
    let resp = router
        .oneshot(
            webhook_post("route-secret-endpoint")
                .header("X-Hub-Signature-256", sig)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "the secret written via the endpoint must verify the request signature");
}

#[tokio::test]
async fn set_webhook_route_secret_falls_back_to_route_name_when_secret_ref_is_absent() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-secret-fallback")).await;
    // No `secret_ref` at all — the route-name fallback in
    // `resolve_route_secret_ref` is what must make this resolve.
    create_webhook_assignment_with_trigger(
        &router,
        "agent-secret-fallback",
        serde_json::json!({
            "type": "Webhook",
            "route_name": "route-secret-fallback",
            "events": ["pull_request"]
        }),
    )
    .await
    .expect("create assignment");

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/webhooks/route-secret-fallback/secret")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "secret": "fallback-secret" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let body = realistic_pull_request_payload();
    let sig = github_signature("fallback-secret", body.as_bytes());
    let resp = router
        .oneshot(
            webhook_post("route-secret-fallback")
                .header("X-Hub-Signature-256", sig)
                .header("X-GitHub-Event", "pull_request")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a secret stored under route_name must verify even when the assignment has no explicit secret_ref"
    );
}

#[tokio::test]
async fn get_webhook_route_secret_status_reflects_whether_a_secret_resolves() {
    let (router, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-secret-status")).await;
    create_webhook_assignment(&router, "agent-secret-status", "route-secret-status", "route-secret-status").await;

    let resp = router
        .clone()
        .oneshot(Request::builder().method(Method::GET).uri("/webhooks/route-secret-status/secret").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["configured"], false, "no secret has been set yet");

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/webhooks/route-secret-status/secret")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "secret": "now-set" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = router
        .oneshot(Request::builder().method(Method::GET).uri("/webhooks/route-secret-status/secret").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["configured"], true, "a secret was just set for this route");
}

// ---------------------------------------------------------------------------
// POST /webhook-test (dry-run "Send test webhook")
// ---------------------------------------------------------------------------

fn webhook_test_request() -> axum::http::request::Builder {
    Request::builder().method(Method::POST).uri("/webhook-test").header("content-type", "application/json")
}

#[tokio::test]
async fn dry_run_test_endpoint_matches_and_renders_template_without_side_effects() {
    let (router, _tmp, _guard) = setup().await;

    let body = serde_json::json!({
        "events": ["pull_request"],
        "prompt_template": "Review PR #{pull_request.number}: {pull_request.title}",
        "deliver": { "type": "agent" },
        "event_type": "pull_request",
        "payload": { "pull_request": { "number": 42, "title": "Fix the flaky retry loop" } },
    });

    let resp = router
        .oneshot(webhook_test_request().body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["matched"], true);
    assert_eq!(json["would_start_agent"], true);
    assert_eq!(json["rendered_instruction"], "Review PR #42: Fix the flaky retry loop");
}

#[tokio::test]
async fn dry_run_test_endpoint_reports_filtered_event_with_no_rendered_instruction() {
    let (router, _tmp, _guard) = setup().await;

    let body = serde_json::json!({
        "events": ["push"],
        "prompt_template": "Should never render: {pull_request.title}",
        "deliver": { "type": "agent" },
        "event_type": "pull_request",
        "payload": { "pull_request": { "number": 42, "title": "Fix the flaky retry loop" } },
    });

    let resp = router
        .oneshot(webhook_test_request().body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["matched"], false);
    assert_eq!(json["would_start_agent"], false);
    assert!(json["rendered_instruction"].is_null());
}

#[tokio::test]
async fn dry_run_test_endpoint_deliver_only_never_would_start_agent() {
    let (router, _tmp, _guard) = setup().await;

    let body = serde_json::json!({
        "events": [],
        "prompt_template": "New PR: {pull_request.title}",
        "deliver": { "type": "deliver_only" },
        "payload": { "pull_request": { "title": "Fix the flaky retry loop" } },
    });

    let resp = router
        .oneshot(webhook_test_request().body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["matched"], true, "empty events allowlist matches anything");
    assert_eq!(json["would_start_agent"], false, "deliver_only must never report it would start an agent");
    assert_eq!(json["rendered_instruction"], "New PR: Fix the flaky retry loop");
}

#[tokio::test]
async fn dry_run_test_endpoint_applies_declarative_filters() {
    let (router, _tmp, _guard) = setup().await;

    let body = serde_json::json!({
        "events": [],
        "filters": { "field": "action", "op": "equals", "value": "closed" },
        "deliver": { "type": "agent" },
        "payload": { "action": "opened" },
    });

    let resp = router
        .oneshot(webhook_test_request().body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["matched"], false, "declarative filter mismatch must fail the dry run too");
}
