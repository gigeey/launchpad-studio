use std::fmt;

use serde::{Deserialize, Serialize};

fn default_base_url() -> String {
    "https://generativelanguage.googleapis.com/v1beta".to_string()
}

fn default_model() -> String {
    "gemini-1.5-pro".to_string()
}

#[derive(Clone, Deserialize, Serialize)]
pub struct GeminiConfig {
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
}

/// Redacts `api_key` so this struct is safe to include in a log line or
/// panic message.
impl fmt::Debug for GeminiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiConfig")
            .field("api_key", &"REDACTED")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish()
    }
}
