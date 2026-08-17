//! Live model discovery for a configured provider.
//!
//! Backs `GET /providers/{name}/models`: fetches the current list of model
//! IDs a provider's API reports for the stored key, using the same
//! `providers.toml` + [`crate::SecretVault`] credentials the runner's
//! provider clients use. This doubles as the API-key validity check for the
//! settings UI — there is deliberately no separate "test connection"
//! endpoint — so [`ModelDiscoveryError`] distinguishes an auth rejection
//! from a network problem from an unparseable response: the frontend treats
//! only the first as a soft "this key looks wrong" warning and must not
//! block the user from saving over the other two.

use std::time::Duration;

use serde::Deserialize;

use crate::{ProviderConfig, ProviderConfigError};

/// Bound on establishing the TCP/TLS connection to a provider's API. Kept
/// short relative to [`REQUEST_TIMEOUT`] — an endpoint that never even
/// accepts the connection (including a user-supplied custom `base_url`,
/// which this feature explicitly allows) should fail fast rather than sit
/// alongside a healthy request for the same interactive setup-flow budget.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on the whole request/response round trip, connect phase included.
/// Provider `/models` endpoints normally answer in well under a second; 15s
/// leaves headroom for a slow network without leaving this endpoint's caller
/// — the frontend's "discovering" spinner during first-run provider setup —
/// hanging indefinitely if the upstream accepts the connection and then
/// never responds. Also bounds the server-side connection this call holds
/// open, so a stalled upstream can't leak it forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Providers this module knows how to query for a model list. A subset of
/// the crate-wide known-provider set — Gemini has a `providers.toml`
/// section ([`crate::GeminiConfig`]) but its client crate is deliberately
/// unwired (see the workspace notes on `ao-engine-tools-provider-gemini`),
/// so there is nothing here that can validate a Gemini key yet.
const DISCOVERABLE_PROVIDERS: &[&str] = &["anthropic", "openai", "openrouter"];

/// Pinned Anthropic API version, matching
/// `ao_engine_tools_provider_anthropic::auth`'s completion-request headers.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Hard cap on pagination pages fetched from Anthropic's `/v1/models` before
/// giving up. Real accounts have on the order of tens of models; a response
/// that keeps reporting `has_more: true` forever (upstream bug, or a
/// misbehaving endpoint reached via a user-supplied `base_url`) must not
/// spin this endpoint indefinitely.
const MAX_ANTHROPIC_PAGES: usize = 50;

/// Structured, machine-distinguishable outcome of a model-discovery call.
///
/// [`Self::AuthFailure`], [`Self::NetworkFailure`], and
/// [`Self::MalformedResponse`] are the three classes the caller must be able
/// to tell apart — see the module doc for why. The remaining variants are
/// precondition failures that never reach the network at all.
#[derive(Debug, thiserror::Error)]
pub enum ModelDiscoveryError {
    /// `name` isn't one of the providers this module can query — either a
    /// typo, or a known-but-unwired provider (Gemini today).
    #[error("model discovery is not available for provider {0:?}: expected one of anthropic, openai, openrouter")]
    UnsupportedProvider(String),

    /// The provider has no section in `providers.toml`, or its section has
    /// no stored API key. A local precondition failure, not an upstream
    /// call outcome — no request was ever sent.
    #[error("provider {0:?} has no stored API key")]
    NotConfigured(String),

    /// `providers.toml` itself could not be read or parsed.
    #[error("failed to read provider configuration: {0}")]
    Config(#[from] ProviderConfigError),

    /// Upstream rejected the stored credential (HTTP 401/403).
    #[error("provider {provider:?} rejected the stored API key (HTTP {status})")]
    AuthFailure { provider: String, status: u16 },

    /// Any other transport-level or non-2xx failure: DNS/connect/timeout, or
    /// a non-auth error status (429, 5xx, ...).
    #[error("network error contacting provider {provider:?}: {message}")]
    NetworkFailure { provider: String, message: String },

    /// A 2xx response whose body didn't parse as the shape this module
    /// expects.
    #[error("malformed response from provider {provider:?}: {message}")]
    MalformedResponse { provider: String, message: String },
}

/// Fetch the list of model IDs `provider` currently reports, using its
/// stored `providers.toml` config (base URL) and [`crate::SecretVault`]
/// credential (API key).
///
/// `provider` must be one of `"anthropic"`, `"openai"`, or `"openrouter"` —
/// see [`DISCOVERABLE_PROVIDERS`]. Anthropic's `/v1/models` is paginated and
/// is followed to completion; the two OpenAI-compatible providers return
/// their full list in one response.
pub async fn list_models(provider: &str) -> Result<Vec<String>, ModelDiscoveryError> {
    list_models_with_timeouts(provider, CONNECT_TIMEOUT, REQUEST_TIMEOUT).await
}

/// [`list_models`], parameterized on its HTTP client's timeouts so tests can
/// exercise the timeout-to-[`ModelDiscoveryError::NetworkFailure`] mapping
/// in milliseconds instead of waiting out the real [`REQUEST_TIMEOUT`].
async fn list_models_with_timeouts(
    provider: &str,
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<Vec<String>, ModelDiscoveryError> {
    if !DISCOVERABLE_PROVIDERS.contains(&provider) {
        return Err(ModelDiscoveryError::UnsupportedProvider(provider.to_string()));
    }

    let cfg = match ProviderConfig::load() {
        Ok(cfg) => cfg,
        Err(ProviderConfigError::NotFound { .. }) => {
            return Err(ModelDiscoveryError::NotConfigured(provider.to_string()));
        }
        Err(e) => return Err(e.into()),
    };

    let http = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .build()
        // The builder only errors on a broken TLS backend or resolver setup,
        // never on plain config values like fixed timeouts — the same
        // infallible-in-practice construction `reqwest::Client::new()`
        // (which this replaces) already assumed.
        .expect("model-discovery HTTP client with fixed timeouts must always build");

    match provider {
        "anthropic" => {
            let ant = cfg
                .anthropic
                .filter(|c| !c.api_key.is_empty())
                .ok_or_else(|| ModelDiscoveryError::NotConfigured(provider.to_string()))?;
            fetch_anthropic_models(&http, &ant.base_url, &ant.api_key).await
        }
        "openai" => {
            let oai = cfg
                .openai
                .filter(|c| !c.api_key.is_empty())
                .ok_or_else(|| ModelDiscoveryError::NotConfigured(provider.to_string()))?;
            fetch_openai_compatible_models(&http, "openai", &oai.base_url, &oai.api_key).await
        }
        "openrouter" => {
            let router = cfg
                .openrouter
                .filter(|c| !c.api_key.is_empty())
                .ok_or_else(|| ModelDiscoveryError::NotConfigured(provider.to_string()))?;
            fetch_openai_compatible_models(&http, "openrouter", &router.base_url, &router.api_key).await
        }
        _ => unreachable!("checked against DISCOVERABLE_PROVIDERS above"),
    }
}

/// Turns a `reqwest::Error` from `.send()` into the message carried by
/// [`ModelDiscoveryError::NetworkFailure`]. Singles out the timeout and
/// connect-failure cases with a plain-English reason instead of reqwest's
/// technical `Display` output, since a stalled upstream (including a
/// user-supplied custom `base_url`) hitting [`REQUEST_TIMEOUT`]/
/// [`CONNECT_TIMEOUT`] is the failure this endpoint is most likely to hit.
fn describe_send_error(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out waiting for a response from the provider".to_string()
    } else if e.is_connect() {
        format!("could not connect to the provider: {e}")
    } else {
        e.to_string()
    }
}

#[derive(Deserialize)]
struct AnthropicModelsPage {
    data: Vec<AnthropicModel>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicModel {
    id: String,
}

/// Follows Anthropic's `after_id` cursor pagination on `GET /v1/models`
/// until `has_more` is false, up to [`MAX_ANTHROPIC_PAGES`].
async fn fetch_anthropic_models(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, ModelDiscoveryError> {
    let mut ids = Vec::new();
    let mut after_id: Option<String> = None;

    for _ in 0..MAX_ANTHROPIC_PAGES {
        let mut request = http
            .get(format!("{base_url}/v1/models"))
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION);
        if let Some(cursor) = after_id.as_deref() {
            request = request.query(&[("after_id", cursor)]);
        }

        let response = request.send().await.map_err(|e| ModelDiscoveryError::NetworkFailure {
            provider: "anthropic".to_string(),
            message: describe_send_error(e),
        })?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ModelDiscoveryError::AuthFailure {
                provider: "anthropic".to_string(),
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ModelDiscoveryError::NetworkFailure {
                provider: "anthropic".to_string(),
                message: format!("HTTP {}: {body}", status.as_u16()),
            });
        }

        let page: AnthropicModelsPage = response.json().await.map_err(|e| ModelDiscoveryError::MalformedResponse {
            provider: "anthropic".to_string(),
            message: e.to_string(),
        })?;

        ids.extend(page.data.into_iter().map(|m| m.id));

        if !page.has_more {
            break;
        }
        match page.last_id {
            Some(next) => after_id = Some(next),
            // has_more=true with no cursor to continue from is a shape we
            // can't act on further — stop rather than loop forever.
            None => break,
        }
    }

    Ok(ids)
}

#[derive(Deserialize)]
struct OpenAiCompatibleModelsResponse {
    data: Vec<OpenAiCompatibleModel>,
}

#[derive(Deserialize)]
struct OpenAiCompatibleModel {
    id: String,
}

/// Fetches `GET {base_url}/models` for an OpenAI-compatible provider
/// (OpenAI itself, or OpenRouter). Neither paginates this endpoint — the
/// full model list comes back in one response.
async fn fetch_openai_compatible_models(
    http: &reqwest::Client,
    provider: &str,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, ModelDiscoveryError> {
    let response = http
        .get(format!("{base_url}/models"))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| ModelDiscoveryError::NetworkFailure {
            provider: provider.to_string(),
            message: describe_send_error(e),
        })?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(ModelDiscoveryError::AuthFailure {
            provider: provider.to_string(),
            status: status.as_u16(),
        });
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(ModelDiscoveryError::NetworkFailure {
            provider: provider.to_string(),
            message: format!("HTTP {}: {body}", status.as_u16()),
        });
    }

    let body: OpenAiCompatibleModelsResponse =
        response.json().await.map_err(|e| ModelDiscoveryError::MalformedResponse {
            provider: provider.to_string(),
            message: e.to_string(),
        })?;

    Ok(body.data.into_iter().map(|m| m.id).collect())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ao_protocol::data_root::DATA_DIR_ENV_VAR;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::test_env::{lock_env, EnvGuard};

    const FORCE_FILE_VAULT_ENV_VAR: &str = "LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK";

    /// Points the data root at a fresh tempdir, forces the file-backed
    /// vault (never the real OS keychain in tests), and saves `provider`
    /// with `api_key` pointed at `base_url`. Returns the guards, which must
    /// stay alive for the duration of the test.
    fn configure_provider(provider: &str, api_key: &str, base_url: &str) -> (tempfile::TempDir, EnvGuard, EnvGuard) {
        let dir = tempfile::tempdir().expect("tempdir");
        let dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let fb = EnvGuard::set(FORCE_FILE_VAULT_ENV_VAR, "1");
        ProviderConfig::save_provider(provider, api_key, Some(base_url), None, None, None, None).expect("save provider");
        (dir, dd, fb)
    }

    // --- Precondition failures (no network call) ---

    #[tokio::test]
    async fn unsupported_provider_is_rejected_before_any_lookup() {
        let _lock = lock_env();
        let err = list_models("gemini").await.expect_err("gemini discovery is unwired");
        assert!(matches!(err, ModelDiscoveryError::UnsupportedProvider(name) if name == "gemini"));
    }

    #[tokio::test]
    async fn unknown_name_is_rejected_the_same_as_unwired_provider() {
        let _lock = lock_env();
        let err = list_models("not-a-provider").await.expect_err("should reject");
        assert!(matches!(err, ModelDiscoveryError::UnsupportedProvider(_)));
    }

    #[tokio::test]
    async fn missing_providers_toml_is_not_configured() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set(FORCE_FILE_VAULT_ENV_VAR, "1");

        let err = list_models("openai").await.expect_err("no providers.toml at all");
        assert!(matches!(err, ModelDiscoveryError::NotConfigured(name) if name == "openai"));
    }

    #[tokio::test]
    async fn section_present_without_key_is_not_configured() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("providers.toml"), "[openai]\nbase_url = \"https://api.openai.com/v1\"\n")
            .expect("write fixture");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set(FORCE_FILE_VAULT_ENV_VAR, "1");

        let err = list_models("openai").await.expect_err("section has no api_key");
        assert!(matches!(err, ModelDiscoveryError::NotConfigured(name) if name == "openai"));
    }

    // --- Anthropic pagination ---

    #[tokio::test]
    async fn anthropic_follows_pagination_to_completion() {
        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        let (_dir, _dd, _fb) = configure_provider("anthropic", "sk-ant-test", &mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "sk-ant-test"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "claude-page1-a"}, {"id": "claude-page1-b"}],
                "has_more": true,
                "last_id": "claude-page1-b",
            })))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(query_param("after_id", "claude-page1-b"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "claude-page2-a"}],
                "has_more": false,
                "last_id": "claude-page2-a",
            })))
            .mount(&mock_server)
            .await;

        let models = list_models("anthropic").await.expect("discovery succeeds");
        assert_eq!(
            models,
            vec!["claude-page1-a", "claude-page1-b", "claude-page2-a"],
            "must return every page's models, not just the first"
        );
    }

    #[tokio::test]
    async fn anthropic_single_page_stops_without_after_id() {
        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        let (_dir, _dd, _fb) = configure_provider("anthropic", "sk-ant-test", &mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "claude-only"}],
                "has_more": false,
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let models = list_models("anthropic").await.expect("discovery succeeds");
        assert_eq!(models, vec!["claude-only"]);
    }

    // --- OpenAI-compatible parsing (OpenAI + OpenRouter) ---

    #[tokio::test]
    async fn openai_parses_model_list() {
        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        let (_dir, _dd, _fb) = configure_provider("openai", "sk-openai-test", &mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer sk-openai-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    {"id": "gpt-4o", "object": "model"},
                    {"id": "gpt-4o-mini", "object": "model"},
                ],
            })))
            .mount(&mock_server)
            .await;

        let models = list_models("openai").await.expect("discovery succeeds");
        assert_eq!(models, vec!["gpt-4o", "gpt-4o-mini"]);
    }

    #[tokio::test]
    async fn openrouter_parses_model_list_via_the_same_openai_compatible_path() {
        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        let (_dir, _dd, _fb) = configure_provider("openrouter", "sk-or-test", &mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer sk-or-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"id": "anthropic/claude-opus-4.7", "name": "Claude Opus 4.7"},
                    {"id": "openai/gpt-4o", "name": "GPT-4o"},
                ],
            })))
            .mount(&mock_server)
            .await;

        let models = list_models("openrouter").await.expect("discovery succeeds");
        assert_eq!(models, vec!["anthropic/claude-opus-4.7", "openai/gpt-4o"]);
    }

    // --- The three distinguishable upstream error classes ---

    #[tokio::test]
    async fn http_401_is_reported_as_auth_failure() {
        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        let (_dir, _dd, _fb) = configure_provider("openai", "sk-bad", &mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": {"message": "Invalid API key"}
            })))
            .mount(&mock_server)
            .await;

        let err = list_models("openai").await.expect_err("401 should fail");
        match err {
            ModelDiscoveryError::AuthFailure { provider, status } => {
                assert_eq!(provider, "openai");
                assert_eq!(status, 401);
            }
            other => panic!("expected AuthFailure, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_403_is_also_reported_as_auth_failure() {
        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        let (_dir, _dd, _fb) = configure_provider("anthropic", "sk-forbidden", &mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&mock_server)
            .await;

        let err = list_models("anthropic").await.expect_err("403 should fail");
        assert!(matches!(err, ModelDiscoveryError::AuthFailure { status: 403, .. }));
    }

    #[tokio::test]
    async fn http_500_is_reported_as_network_failure_not_auth() {
        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        let (_dir, _dd, _fb) = configure_provider("openai", "sk-openai-test", &mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&mock_server)
            .await;

        let err = list_models("openai").await.expect_err("500 should fail");
        match err {
            ModelDiscoveryError::NetworkFailure { provider, message } => {
                assert_eq!(provider, "openai");
                assert!(message.contains("500"));
            }
            other => panic!("expected NetworkFailure, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn connection_refused_is_reported_as_network_failure() {
        let _lock = lock_env();
        // Nothing is listening on this port — the connection itself fails
        // before any HTTP status is ever produced.
        let (_dir, _dd, _fb) = configure_provider("openai", "sk-openai-test", "http://127.0.0.1:1");

        let err = list_models("openai").await.expect_err("unreachable host should fail");
        assert!(matches!(err, ModelDiscoveryError::NetworkFailure { .. }));
    }

    /// Proves the request-timeout wiring actually fires and lands on the same
    /// [`ModelDiscoveryError::NetworkFailure`] class as any other transport
    /// failure, per the module doc's "auth vs network vs malformed"
    /// contract. The mock server accepts the connection immediately and
    /// then withholds the response past the request timeout — the exact
    /// "upstream accepted the TCP connection and never responds" scenario a
    /// stalled or malicious custom `base_url` can trigger. Goes through
    /// [`list_models_with_timeouts`] with millisecond-scale timeouts so the
    /// test doesn't have to wait out the real 15s [`REQUEST_TIMEOUT`].
    #[tokio::test]
    async fn request_timeout_is_reported_as_network_failure() {
        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        let (_dir, _dd, _fb) = configure_provider("openai", "sk-openai-test", &mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(300)))
            .mount(&mock_server)
            .await;

        let err = list_models_with_timeouts("openai", Duration::from_millis(50), Duration::from_millis(100))
            .await
            .expect_err("upstream that never responds within the timeout should fail");
        match err {
            ModelDiscoveryError::NetworkFailure { provider, message } => {
                assert_eq!(provider, "openai");
                assert!(message.contains("timed out"), "message was: {message:?}");
            }
            other => panic!("expected NetworkFailure from a timeout, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_json_body_is_reported_as_malformed_response() {
        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        let (_dir, _dd, _fb) = configure_provider("openai", "sk-openai-test", &mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json at all"))
            .mount(&mock_server)
            .await;

        let err = list_models("openai").await.expect_err("unparseable body should fail");
        assert!(matches!(err, ModelDiscoveryError::MalformedResponse { provider, .. } if provider == "openai"));
    }

    #[tokio::test]
    async fn well_formed_json_missing_data_field_is_malformed_response() {
        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        let (_dir, _dd, _fb) = configure_provider("anthropic", "sk-ant-test", &mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"unexpected": "shape"})))
            .mount(&mock_server)
            .await;

        let err = list_models("anthropic").await.expect_err("missing data field should fail");
        assert!(matches!(err, ModelDiscoveryError::MalformedResponse { provider, .. } if provider == "anthropic"));
    }
}
