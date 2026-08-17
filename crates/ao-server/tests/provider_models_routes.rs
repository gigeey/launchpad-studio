//! Integration tests for `GET /providers/{name}/models`.
//!
//! `list_provider_models` (like its `list_providers`/`set_provider`/
//! `delete_provider` siblings) takes no `State<AppState>`, so these tests
//! mount just this one route rather than paying for the full app router +
//! `AppState` setup that other route test files need.

use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use ao_engine_tools_provider_config::ProviderConfig;
use ao_protocol::data_root::DATA_DIR_ENV_VAR;
use ao_server::routes::providers::list_provider_models;

/// `LAUNCHPAD_STUDIO_DATA_DIR` and the secret-vault file-fallback flag are
/// process-wide env vars, so tests in this file must not run concurrently.
/// Run with `--test-threads=1` (the default for a single test binary run
/// serially by cargo unless the crate opts into parallel test binaries).
static ENV_MUTEX: Mutex<()> = Mutex::new(());

const FORCE_FILE_VAULT_ENV_VAR: &str = "LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK";

struct EnvGuard {
    key: &'static str,
    prior: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, val);
        Self { key, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

fn router() -> Router {
    Router::new().route("/providers/{name}/models", axum::routing::get(list_provider_models))
}

async fn get_models(router: &Router, name: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/providers/{name}/models"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn read_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.expect("read body").to_bytes();
    serde_json::from_slice(&bytes).expect("valid JSON body")
}

#[tokio::test]
async fn success_returns_bare_array_of_model_ids() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
    let _fb = EnvGuard::set(FORCE_FILE_VAULT_ENV_VAR, "1");

    let mock_server = MockServer::start().await;
    ProviderConfig::save_provider("openai", "sk-openai-test", Some(&mock_server.uri()), None, None, None, None).expect("save");

    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "gpt-4o"}, {"id": "gpt-4o-mini"}],
        })))
        .mount(&mock_server)
        .await;

    let resp = get_models(&router(), "openai").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_json(resp).await;
    assert_eq!(body, serde_json::json!(["gpt-4o", "gpt-4o-mini"]));
}

#[tokio::test]
async fn auth_failure_returns_401_with_distinguishing_code() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
    let _fb = EnvGuard::set(FORCE_FILE_VAULT_ENV_VAR, "1");

    let mock_server = MockServer::start().await;
    ProviderConfig::save_provider("openai", "sk-bad-key", Some(&mock_server.uri()), None, None, None, None).expect("save");

    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&mock_server)
        .await;

    let resp = get_models(&router(), "openai").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = read_json(resp).await;
    assert_eq!(
        body.get("code").and_then(|v| v.as_str()),
        Some("auth_failure"),
        "body must carry a distinguishing code so the frontend can soft-warn instead of blocking save: {body}"
    );
}

#[tokio::test]
async fn network_failure_returns_502_with_distinguishing_code() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
    let _fb = EnvGuard::set(FORCE_FILE_VAULT_ENV_VAR, "1");

    // Nothing listens here — the request fails at the transport layer.
    ProviderConfig::save_provider("openai", "sk-openai-test", Some("http://127.0.0.1:1"), None, None, None, None).expect("save");

    let resp = get_models(&router(), "openai").await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body = read_json(resp).await;
    assert_eq!(body.get("code").and_then(|v| v.as_str()), Some("network_failure"));
}

#[tokio::test]
async fn malformed_response_returns_502_with_distinguishing_code() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
    let _fb = EnvGuard::set(FORCE_FILE_VAULT_ENV_VAR, "1");

    let mock_server = MockServer::start().await;
    ProviderConfig::save_provider("openai", "sk-openai-test", Some(&mock_server.uri()), None, None, None, None).expect("save");

    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_string("this is not json"))
        .mount(&mock_server)
        .await;

    let resp = get_models(&router(), "openai").await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body = read_json(resp).await;
    assert_eq!(body.get("code").and_then(|v| v.as_str()), Some("malformed_response"));
}

#[tokio::test]
async fn not_configured_returns_400_without_ever_calling_out() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
    let _fb = EnvGuard::set(FORCE_FILE_VAULT_ENV_VAR, "1");

    let resp = get_models(&router(), "anthropic").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn anthropic_pagination_is_followed_to_completion_over_http() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
    let _fb = EnvGuard::set(FORCE_FILE_VAULT_ENV_VAR, "1");

    let mock_server = MockServer::start().await;
    ProviderConfig::save_provider("anthropic", "sk-ant-test", Some(&mock_server.uri()), None, None, None, None).expect("save");

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "claude-a"}],
            "has_more": true,
            "last_id": "claude-a",
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(wiremock::matchers::query_param("after_id", "claude-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "claude-b"}],
            "has_more": false,
        })))
        .mount(&mock_server)
        .await;

    let resp = get_models(&router(), "anthropic").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_json(resp).await;
    assert_eq!(body, serde_json::json!(["claude-a", "claude-b"]));
}
