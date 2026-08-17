use std::fmt;

use ao_protocol::agent::ReasoningEffort;
use serde::{Deserialize, Serialize};

fn default_base_url() -> String {
    "https://openrouter.ai/api/v1".to_string()
}

fn default_model() -> String {
    "openrouter/auto".to_string()
}

/// OpenRouter's own section of `providers.toml`.
///
/// A distinct type from [`crate::OpenAIConfig`] — even though the two APIs
/// are wire-compatible — purely so each provider gets its own defaults when
/// a field is omitted from the file: OpenRouter's base URL and default
/// model differ from OpenAI's, and `#[serde(default = ...)]` is resolved
/// per-type, not per-instance. See `ao_engine_tools_provider_openai`'s
/// `OpenAIClient::from_loaded_config_openrouter` for how this config is
/// adapted onto the shared OpenAI-compatible transport.
#[derive(Clone, Deserialize, Serialize)]
pub struct OpenRouterConfig {
    /// Populated from [`crate::SecretVault`] by [`crate::ProviderConfig::load`],
    /// not from `providers.toml` — the on-disk file no longer carries this
    /// field once the vault has absorbed it. Defaults to empty so a section
    /// with no `api_key` key still deserializes.
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// Persisted default for the `max_completion_tokens` cap the shared
    /// OpenAI-compatible request builder sends. `None` omits the field.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Persisted default context-window budget enforced client-side. `None`
    /// means no cap.
    #[serde(default)]
    pub max_context_tokens: Option<u32>,
    /// Persisted default reasoning-effort level, mapped onto the native
    /// `reasoning_effort` wire field. `None` omits the field entirely.
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Redacts `api_key` so this struct is safe to include in a log line or
/// panic message.
impl fmt::Debug for OpenRouterConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenRouterConfig")
            .field("api_key", &"REDACTED")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("max_context_tokens", &self.max_context_tokens)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}

/// Adapts an OpenRouter config onto [`crate::OpenAIConfig`]'s shape so the
/// runner's existing OpenAI-compatible client can drive OpenRouter without a
/// dedicated provider crate. OpenRouter has no `organization`/`project`
/// header concept, so both map to `None`.
impl From<OpenRouterConfig> for crate::OpenAIConfig {
    fn from(cfg: OpenRouterConfig) -> Self {
        crate::OpenAIConfig {
            api_key: cfg.api_key,
            base_url: cfg.base_url,
            model: cfg.model,
            organization: None,
            project: None,
            max_output_tokens: cfg.max_output_tokens,
            max_context_tokens: cfg.max_context_tokens,
            reasoning_effort: cfg.reasoning_effort,
        }
    }
}
