mod path;

pub mod anthropic;
pub mod channel_secret_store;
pub mod gemini;
pub mod mcp_servers;
pub mod mcp_token_store;
pub mod model_discovery;
pub mod openai;
pub mod openrouter;
pub mod secret_vault;
pub mod telegram_token_store;

#[cfg(test)]
mod test_env;
#[cfg(test)]
mod tests;

pub use ao_protocol::data_root::resolve_data_root;
pub use anthropic::AnthropicConfig;
pub use channel_secret_store::{
    ChannelSecretStore, ChannelSecretStoreError, DISCORD_TOKEN_SECRET_ROLE, EMAIL_PASSWORD_SECRET_ROLE,
    SLACK_APP_TOKEN_SECRET_ROLE, SLACK_BOT_TOKEN_SECRET_ROLE,
};
pub use gemini::GeminiConfig;
pub use mcp_servers::{McpConfigError, McpLoadingPolicy, McpServerEntry, McpServersConfig};
pub use mcp_token_store::{McpTokenRecord, McpTokenStore, McpTokenStoreError, derive_server_key};
pub use model_discovery::{list_models, ModelDiscoveryError};
pub use openai::OpenAIConfig;
pub use openrouter::OpenRouterConfig;
pub use secret_vault::{
    disable_interactive_keychain_prompts, propagate_keychain_forbidden, should_suppress_keychain_prompts,
    SecretVault, VaultError,
};
pub use telegram_token_store::{TelegramTokenStore, TelegramTokenStoreError};

use std::path::{Path, PathBuf};

use ao_protocol::agent::ReasoningEffort;
use ao_protocol::error::AoError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Provider names accepted by [`ProviderConfig::save_provider`] /
/// [`ProviderConfig::delete_provider`] / [`ProviderConfig::statuses`].
///
/// Kept in one place so the "known providers" list can't drift between the
/// write path and the status/read path.
const KNOWN_PROVIDERS: &[&str] = &["anthropic", "openai", "openrouter", "gemini"];

#[derive(Debug, Error)]
pub enum ProviderConfigError {
    #[error("providers.toml not found at {path}")]
    NotFound { path: PathBuf },

    #[error("IO error reading providers.toml: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML parse error: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("Data root resolver error: {0}")]
    Resolver(#[from] AoError),

    #[error("TOML edit error: {0}")]
    TomlEdit(String),

    #[error("unknown provider {0:?}: expected one of anthropic, openai, openrouter, gemini")]
    UnknownProvider(String),

    #[error("secret vault error: {0}")]
    Vault(#[from] VaultError),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub anthropic: Option<AnthropicConfig>,
    pub openai: Option<OpenAIConfig>,
    pub openrouter: Option<OpenRouterConfig>,
    pub gemini: Option<GeminiConfig>,
}

impl ProviderConfig {
    /// Load from `<resolve_data_root()>/providers.toml`, with each
    /// configured provider's `api_key` populated from [`SecretVault`] rather
    /// than the file.
    ///
    /// Returns `ProviderConfigError::NotFound` when the file is absent — the
    /// caller decides whether that is fatal.
    ///
    /// If the file still carries a plaintext `api_key` for a provider (from
    /// before this crate moved keys into the vault, or from a headless
    /// deployment that writes the file directly as an injection channel —
    /// see [`Self::save_provider`]), that key is absorbed into the vault on
    /// this call. When the vault is backed by a real OS keychain the
    /// absorbed key is also scrubbed from the file, since the keychain fully
    /// removes the plaintext rather than just relocating it. When the vault
    /// is only file-backed (headless/CI, no keychain daemon reachable), the
    /// key is absorbed but left in place so the file keeps working as a
    /// valid way to provision a key without one — the next load simply
    /// finds the vault already populated and does nothing further.
    pub fn load() -> Result<Self, ProviderConfigError> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Err(ProviderConfigError::NotFound { path });
        }
        let raw = std::fs::read_to_string(&path)?;
        let mut cfg: ProviderConfig = toml::from_str(&raw)?;

        let vault = SecretVault::open()?;
        let mut doc = raw
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| ProviderConfigError::TomlEdit(e.to_string()))?;
        if absorb_plaintext_api_keys(&mut doc, &vault, vault.is_keychain_backed())? {
            write_document_secure(&path, &doc)?;
        }

        if let Some(anthropic) = cfg.anthropic.as_mut() {
            anthropic.api_key = vault.get_provider("anthropic")?.unwrap_or_default();
        }
        if let Some(openai) = cfg.openai.as_mut() {
            openai.api_key = vault.get_provider("openai")?.unwrap_or_default();
        }
        if let Some(openrouter) = cfg.openrouter.as_mut() {
            openrouter.api_key = vault.get_provider("openrouter")?.unwrap_or_default();
        }
        if let Some(gemini) = cfg.gemini.as_mut() {
            gemini.api_key = vault.get_provider("gemini")?.unwrap_or_default();
        }

        Ok(cfg)
    }

    /// Returns the path that `load()` will read.
    ///
    /// Useful for error messages and for tests that write fixture files.
    pub fn config_path() -> Result<PathBuf, ProviderConfigError> {
        path::config_path()
    }

    /// Masked status for every known provider — never includes the stored
    /// API key, only whether one is configured. Safe to send to the frontend.
    ///
    /// A missing `providers.toml` is treated as "no provider configured yet"
    /// rather than an error, since that's the normal state before a user has
    /// set up any API-mode agent. `has_api_key` is sourced from the vault
    /// directly rather than from the loaded config's `api_key` field, so it
    /// stays correct even in the edge case of a vault entry with no
    /// corresponding `providers.toml` section.
    pub fn statuses() -> Result<Vec<ProviderStatus>, ProviderConfigError> {
        let cfg = match Self::load() {
            Ok(cfg) => cfg,
            Err(ProviderConfigError::NotFound { .. }) => ProviderConfig {
                anthropic: None,
                openai: None,
                openrouter: None,
                gemini: None,
            },
            Err(e) => return Err(e),
        };
        let vault = SecretVault::open()?;

        let anthropic_key = vault.get_provider("anthropic")?;
        let openai_key = vault.get_provider("openai")?;
        let openrouter_key = vault.get_provider("openrouter")?;
        let gemini_key = vault.get_provider("gemini")?;

        Ok(vec![
            ProviderStatus::from_parts(
                "anthropic",
                anthropic_key.is_some(),
                anthropic_key.as_deref().and_then(api_key_fingerprint),
                cfg.anthropic.as_ref().map(|c| TuningParts {
                    base_url: c.base_url.clone(),
                    model: c.model.clone(),
                    max_output_tokens: c.max_output_tokens,
                    max_context_tokens: c.max_context_tokens,
                    reasoning_effort: c.reasoning_effort,
                }),
            ),
            ProviderStatus::from_parts(
                "openai",
                openai_key.is_some(),
                openai_key.as_deref().and_then(api_key_fingerprint),
                cfg.openai.as_ref().map(|c| TuningParts {
                    base_url: c.base_url.clone(),
                    model: c.model.clone(),
                    max_output_tokens: c.max_output_tokens,
                    max_context_tokens: c.max_context_tokens,
                    reasoning_effort: c.reasoning_effort,
                }),
            ),
            ProviderStatus::from_parts(
                "openrouter",
                openrouter_key.is_some(),
                openrouter_key.as_deref().and_then(api_key_fingerprint),
                cfg.openrouter.as_ref().map(|c| TuningParts {
                    base_url: c.base_url.clone(),
                    model: c.model.clone(),
                    max_output_tokens: c.max_output_tokens,
                    max_context_tokens: c.max_context_tokens,
                    reasoning_effort: c.reasoning_effort,
                }),
            ),
            ProviderStatus::from_parts(
                "gemini",
                gemini_key.is_some(),
                gemini_key.as_deref().and_then(api_key_fingerprint),
                cfg.gemini.as_ref().map(|c| TuningParts {
                    base_url: c.base_url.clone(),
                    model: c.model.clone(),
                    max_output_tokens: None,
                    max_context_tokens: None,
                    reasoning_effort: None,
                }),
            ),
        ])
    }

    /// Write (or overwrite) one provider's `api_key` straight into
    /// [`SecretVault`] — never into `providers.toml` — and optionally update
    /// its non-secret `base_url`/`model` plus the three tuning knobs
    /// (`max_output_tokens`, `max_context_tokens`, `reasoning_effort`) in
    /// `providers.toml`.
    ///
    /// The key never touches the file, not even transiently: this is the
    /// only path through which the UI writes a key, and it must not leave a
    /// plaintext copy behind for [`Self::load`]'s migration to have to clean
    /// up later. Any stale plaintext `api_key` already in the provider's
    /// section (e.g. left over before this crate moved keys into the vault)
    /// is removed as part of this write, in both keychain- and file-backed
    /// vault modes.
    ///
    /// Every field after `api_key` follows the same "omitted means leave
    /// whatever's already stored untouched" merge semantics established by
    /// `base_url`/`model`: `None` skips that TOML key entirely rather than
    /// clearing it. There is currently no way to *revert* a persisted knob
    /// back to "unset" through this call — the same limitation `base_url`/
    /// `model` already have.
    ///
    /// Uses [`toml_edit`] to surgically merge into the existing document
    /// rather than reserializing the whole struct, so a user's hand-authored
    /// comments, field ordering, and *other* providers' sections survive a
    /// save made from the UI. This keeps the file- and UI-edit paths
    /// compatible with each other.
    #[allow(clippy::too_many_arguments)]
    pub fn save_provider(
        name: &str,
        api_key: &str,
        base_url: Option<&str>,
        model: Option<&str>,
        max_output_tokens: Option<u32>,
        max_context_tokens: Option<u32>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<(), ProviderConfigError> {
        require_known_provider(name)?;
        let vault = SecretVault::open()?;
        vault.set_provider(name, api_key)?;

        let path = Self::config_path()?;
        let mut doc = read_document(&path)?;

        if doc.get(name).and_then(|item| item.as_table()).is_none() {
            doc[name] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let table = doc[name]
            .as_table_mut()
            .expect("table entry was just ensured to exist above");
        table.remove("api_key");
        if let Some(b) = base_url {
            table["base_url"] = toml_edit::value(b);
        }
        if let Some(m) = model {
            table["model"] = toml_edit::value(m);
        }
        if let Some(t) = max_output_tokens {
            table["max_output_tokens"] = toml_edit::value(i64::from(t));
        }
        if let Some(t) = max_context_tokens {
            table["max_context_tokens"] = toml_edit::value(i64::from(t));
        }
        if let Some(e) = reasoning_effort {
            table["reasoning_effort"] = toml_edit::value(e.as_str());
        }

        write_document_secure(&path, &doc)
    }

    /// Remove one provider's stored key from [`SecretVault`] and its section
    /// from `providers.toml`. No-op if either is already absent.
    pub fn delete_provider(name: &str) -> Result<(), ProviderConfigError> {
        require_known_provider(name)?;
        let vault = SecretVault::open()?;
        vault.delete_provider(name)?;

        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(());
        }
        let mut doc = read_document(&path)?;
        if doc.remove(name).is_none() {
            return Ok(());
        }
        write_document_secure(&path, &doc)
    }
}

/// Absorbs any plaintext `api_key` still present in `doc` into `vault`,
/// establishing the vault as the source of truth for that provider going
/// forward. An existing vault entry is never overwritten by a value found in
/// `doc` — if the vault already has one, it was set by a more recent
/// [`ProviderConfig::save_provider`] call than whatever stale plaintext this
/// file might still carry.
///
/// When `scrub` is true, an absorbed key is also removed from `doc` — see
/// [`SecretVault::is_keychain_backed`] for why that's only appropriate on a
/// real keychain backend. Returns whether `doc` was mutated, so the caller
/// knows whether the file needs rewriting.
fn absorb_plaintext_api_keys(
    doc: &mut toml_edit::DocumentMut,
    vault: &SecretVault,
    scrub: bool,
) -> Result<bool, ProviderConfigError> {
    let mut changed = false;
    for name in KNOWN_PROVIDERS {
        let Some(table) = doc.get_mut(name).and_then(|item| item.as_table_mut()) else {
            continue;
        };
        let key = match table.get("api_key").and_then(|v| v.as_str()) {
            Some(k) if !k.is_empty() => k.to_owned(),
            _ => continue,
        };
        if vault.get_provider(name)?.is_none() {
            vault.set_provider(name, &key)?;
        }
        if scrub {
            table.remove("api_key");
            changed = true;
        }
    }
    Ok(changed)
}

fn require_known_provider(name: &str) -> Result<(), ProviderConfigError> {
    if KNOWN_PROVIDERS.contains(&name) {
        Ok(())
    } else {
        Err(ProviderConfigError::UnknownProvider(name.to_string()))
    }
}

fn read_document(path: &Path) -> Result<toml_edit::DocumentMut, ProviderConfigError> {
    let raw = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    raw.parse::<toml_edit::DocumentMut>()
        .map_err(|e| ProviderConfigError::TomlEdit(e.to_string()))
}

fn write_document_secure(path: &Path, doc: &toml_edit::DocumentMut) -> Result<(), ProviderConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, doc.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Masked view of one provider's configuration — safe to serialize and send
/// to the frontend. Never carries the API key itself, only whether one is
/// present.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderStatus {
    pub provider: String,
    pub has_api_key: bool,
    pub api_key_fingerprint: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub max_context_tokens: Option<u32>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Builds a display-safe fingerprint of a stored API key: enough for a user
/// to recognize *which* key is configured without ever reconstructing it.
///
/// Keeps the leading 12 characters because that's exactly enough to tell
/// apart the two credential shapes Anthropic issues under the same
/// `sk-ant-…` umbrella — `sk-ant-api03` (a real API key, sent as
/// `x-api-key`) versus `sk-ant-oat01` (a subscription OAuth token, which
/// needs an `Authorization: Bearer` header instead and otherwise 401s
/// identically). Mixing those two up is a real support-cost failure mode.
/// Keeps the trailing 4 characters because that's the convention every
/// provider console uses when it shows a key back to a user, so the
/// fingerprint can be eyeball-matched against the console it was copied
/// from.
///
/// Returns `None` for anything shorter than 28 characters — a deliberate
/// leak guard: a short or unknown-shaped secret gets no fingerprint at all
/// rather than exposing most of itself through the prefix+suffix window.
/// Operates on `char`s rather than byte slices so a key containing
/// multi-byte UTF-8 never panics on a byte-boundary split.
fn api_key_fingerprint(key: &str) -> Option<String> {
    let chars: Vec<char> = key.trim().chars().collect();
    if chars.len() < 28 {
        return None;
    }
    let first_12: String = chars[..12].iter().collect();
    let last_4: String = chars[chars.len() - 4..].iter().collect();
    Some(format!("{first_12}…{last_4}"))
}

/// Non-secret parts of one provider's `providers.toml` section, gathered
/// from whichever concrete config type (`AnthropicConfig`, `OpenAIConfig`,
/// `OpenRouterConfig`, `GeminiConfig`) `ProviderConfig::statuses` is reading
/// from. Exists purely to keep [`ProviderStatus::from_parts`] from taking a
/// six-argument-wide `Option<(...)>` tuple.
struct TuningParts {
    base_url: String,
    model: String,
    max_output_tokens: Option<u32>,
    max_context_tokens: Option<u32>,
    reasoning_effort: Option<ReasoningEffort>,
}

impl ProviderStatus {
    fn from_parts(
        provider: &str,
        has_api_key: bool,
        api_key_fingerprint: Option<String>,
        parts: Option<TuningParts>,
    ) -> Self {
        match parts {
            Some(p) => ProviderStatus {
                provider: provider.to_string(),
                has_api_key,
                api_key_fingerprint,
                base_url: Some(p.base_url),
                model: Some(p.model),
                max_output_tokens: p.max_output_tokens,
                max_context_tokens: p.max_context_tokens,
                reasoning_effort: p.reasoning_effort,
            },
            None => ProviderStatus {
                provider: provider.to_string(),
                has_api_key,
                api_key_fingerprint,
                base_url: None,
                model: None,
                max_output_tokens: None,
                max_context_tokens: None,
                reasoning_effort: None,
            },
        }
    }
}
