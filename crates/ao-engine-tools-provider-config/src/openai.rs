use std::fmt;

use ao_protocol::agent::ReasoningEffort;
use serde::{Deserialize, Serialize};

fn default_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_model() -> String {
    "gpt-4o".to_string()
}

#[derive(Clone, Deserialize, Serialize)]
pub struct OpenAIConfig {
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
    pub organization: Option<String>,
    pub project: Option<String>,
    /// Persisted default for the `max_completion_tokens` cap the OpenAI
    /// request builder sends. `None` omits the field from the wire body,
    /// leaving the model's own default in effect.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Persisted default context-window budget enforced client-side (see
    /// `ao_engine_tools_runner::message::truncate_to_context_budget`).
    /// `None` means no cap.
    #[serde(default)]
    pub max_context_tokens: Option<u32>,
    /// Persisted default reasoning-effort level, mapped onto the native
    /// `reasoning_effort` wire field. `None` omits the field entirely.
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Redacts `api_key` so this struct is safe to include in a log line or
/// panic message.
impl fmt::Debug for OpenAIConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAIConfig")
            .field("api_key", &"REDACTED")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("organization", &self.organization)
            .field("project", &self.project)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("max_context_tokens", &self.max_context_tokens)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}
