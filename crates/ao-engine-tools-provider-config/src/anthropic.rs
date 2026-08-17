use std::fmt;

use ao_protocol::agent::ReasoningEffort;
use serde::{Deserialize, Serialize};

fn default_base_url() -> String {
    "https://api.anthropic.com".to_string()
}

fn default_model() -> String {
    "claude-opus-4-7".to_string()
}

#[derive(Clone, Deserialize, Serialize)]
pub struct AnthropicConfig {
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
    /// Persisted default for [`ReasoningEffort`]-driven output-token cap.
    /// `None` here means the request builder's own hardcoded fallback
    /// applies — see `ao_engine_tools_provider_anthropic::request::build`.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Persisted default context-window budget enforced client-side (see
    /// `ao_engine_tools_runner::message::truncate_to_context_budget`).
    /// `None` means no cap.
    #[serde(default)]
    pub max_context_tokens: Option<u32>,
    /// Persisted default reasoning-effort level, mapped onto a `thinking`
    /// token budget by the Anthropic request builder. `None` means no
    /// extended thinking unless an explicit per-turn `ThinkingConfig`
    /// (the older, unrelated mechanism) is set.
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Redacts `api_key` so this struct is safe to include in a log line or
/// panic message.
impl fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicConfig")
            .field("api_key", &"REDACTED")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("max_context_tokens", &self.max_context_tokens)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}
