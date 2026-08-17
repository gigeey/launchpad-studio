//! Global provider-credential routes.
//!
//! `providers.toml` (one file per data root, not per-agent) holds API keys
//! for API-mode agents. These routes let the Agent Creation modal write a
//! key without the user hand-editing the file — but the plaintext key is
//! never read back over HTTP. `GET /providers` returns only a masked view
//! (`has_api_key`, plus the non-secret `base_url`/`model`); writes go
//! through a narrow per-provider `PUT` so the frontend never needs to hold —
//! and can't accidentally echo back — the full config to avoid clobbering
//! fields it doesn't render.
//!
//! `GET /providers/{name}/models` is the live model list for a provider's
//! stored key, and doubles as this app's only API-key validity check —
//! there is deliberately no separate "test connection" route. A 401/403 from
//! upstream maps to `AoError::ProviderAuthFailure` (HTTP 401, `code:
//! "auth_failure"`), which the frontend treats as a soft warning rather than
//! blocking the `PUT` that already persisted the key.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use ao_engine_tools_provider_config::{list_models, ModelDiscoveryError, ProviderConfig, ProviderConfigError, ProviderStatus};
use ao_protocol::agent::ReasoningEffort;
use ao_protocol::error::AoError;

use crate::error::AppError;

fn map_err(e: ProviderConfigError) -> AppError {
    match e {
        ProviderConfigError::UnknownProvider(name) => AppError(AoError::ValidationError(format!(
            "unknown provider {name:?}: expected one of anthropic, openai, openrouter, gemini"
        ))),
        e => AppError(AoError::Internal(e.to_string())),
    }
}

/// Maps [`ModelDiscoveryError`] onto the three structured upstream-outcome
/// `AoError` variants for the classes the frontend must distinguish
/// (auth/network/malformed), and onto plain 400/500s for the precondition
/// failures that never reach the network.
fn map_model_discovery_err(e: ModelDiscoveryError) -> AppError {
    let message = e.to_string();
    match e {
        ModelDiscoveryError::UnsupportedProvider(_) | ModelDiscoveryError::NotConfigured(_) => {
            AppError(AoError::ValidationError(message))
        }
        ModelDiscoveryError::Config(_) => AppError(AoError::Internal(message)),
        ModelDiscoveryError::AuthFailure { .. } => AppError(AoError::ProviderAuthFailure(message)),
        ModelDiscoveryError::NetworkFailure { .. } => AppError(AoError::ProviderNetworkFailure(message)),
        ModelDiscoveryError::MalformedResponse { .. } => AppError(AoError::ProviderMalformedResponse(message)),
    }
}

/// `GET /providers` — masked status for every known provider.
pub async fn list_providers() -> Result<Json<Vec<ProviderStatus>>, AppError> {
    let statuses = ProviderConfig::statuses().map_err(map_err)?;
    Ok(Json(statuses))
}

#[derive(Debug, Deserialize)]
pub struct SetProviderKeyRequest {
    pub api_key: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Same "omitted means leave whatever's already stored untouched" merge
    /// semantics as `base_url`/`model` — see [`ProviderConfig::save_provider`].
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub max_context_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// `PUT /providers/{name}` — write (or overwrite) one provider's API key and
/// (optionally) its `base_url`/`model`/tuning-knob defaults.
///
/// Merges into the existing `providers.toml` via [`ProviderConfig::save_provider`]
/// rather than reserializing the whole file, so a hand-edited file and the UI
/// stay compatible with each other.
pub async fn set_provider(
    Path(name): Path<String>,
    Json(body): Json<SetProviderKeyRequest>,
) -> Result<StatusCode, AppError> {
    let api_key = body.api_key.trim();
    if api_key.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "api_key must not be empty".to_string(),
        )));
    }
    ProviderConfig::save_provider(
        &name,
        api_key,
        body.base_url.as_deref(),
        body.model.as_deref(),
        body.max_output_tokens,
        body.max_context_tokens,
        body.reasoning_effort,
    )
    .map_err(map_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /providers/{name}` — clear one provider's stored key.
pub async fn delete_provider(Path(name): Path<String>) -> Result<StatusCode, AppError> {
    ProviderConfig::delete_provider(&name).map_err(map_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /providers/{name}/models` — live model IDs for a configured
/// provider, fetched straight from its API using the stored key and (when
/// set) the persisted `base_url`.
///
/// `name` must be `"anthropic"`, `"openai"`, or `"openrouter"` — Gemini has
/// a `providers.toml` section but no wired client, so discovery isn't
/// available for it yet. Response body on success is a bare JSON array of
/// model ID strings, e.g. `["gpt-4o", "gpt-4o-mini"]`.
///
/// See the module doc for how failures are distinguished; the frontend must
/// treat a `code: "auth_failure"` response as a non-blocking warning.
pub async fn list_provider_models(Path(name): Path<String>) -> Result<Json<Vec<String>>, AppError> {
    let models = list_models(&name).await.map_err(map_model_discovery_err)?;
    Ok(Json(models))
}
