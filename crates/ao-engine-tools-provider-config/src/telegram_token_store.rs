//! Per-agent Telegram bot token storage.
//!
//! A thin facade over [`crate::secret_vault::SecretVault`] — see that
//! module's docs for how tokens actually persist (OS keychain vs. JSON file,
//! backend selection, and the one-time legacy-item migration).
//!
//! # Backend resolution order
//!
//! Every [`get`](TelegramTokenStore::get) resolves a token in this order,
//! stopping at the first hit:
//!
//! 1. **Environment variable** — a deterministic name derived from
//!    `agent_id` by [`env_var_name`]. Always checked first, regardless of
//!    which backend the shared vault is using, so an operator (or an
//!    external secret manager that injects process env vars) can override a
//!    stored token without touching the vault at all. An env var that is set
//!    but empty counts as unset.
//! 2. **The shared [`SecretVault`]** — OS keychain when reachable, else a
//!    JSON file under the data root.
//!
//! The environment variable is read-only from the store's perspective:
//! [`set`](TelegramTokenStore::set) and [`delete`](TelegramTokenStore::delete)
//! always write to the vault, never to the process environment.

use std::path::PathBuf;

use ao_protocol::error::AoError;
use thiserror::Error;

use crate::secret_vault::{SecretVault, VaultError};

/// Keychain service name this store used before it became a facade over
/// [`crate::secret_vault::SecretVault`]. Kept as the canonical identifier of
/// "the old per-store location" — referenced by the vault's one-time legacy
/// migration, not by this module anymore.
pub(crate) const KEYRING_SERVICE: &str = "launchpad_studio_telegram";
/// Legacy file-fallback name, in the same role as [`KEYRING_SERVICE`].
pub(crate) const FILE_STORE_NAME: &str = "telegram_tokens.json";
/// Legacy consolidated-item account name, in the same role as
/// [`KEYRING_SERVICE`].
pub(crate) const CONSOLIDATED_ACCOUNT: &str = "__all_telegram_tokens_v1__";

/// Prefix for the deterministic per-token environment variable name built by
/// [`env_var_name`].
const ENV_VAR_PREFIX: &str = "LAUNCHPAD_TELEGRAM_TOKEN";

#[derive(Debug, Error)]
pub enum TelegramTokenStoreError {
    #[error("data root resolver failed: {0}")]
    DataRoot(#[from] AoError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("keychain error: {0}")]
    Keychain(String),
}

impl From<VaultError> for TelegramTokenStoreError {
    fn from(e: VaultError) -> Self {
        match e {
            VaultError::DataRoot(e) => TelegramTokenStoreError::DataRoot(e),
            VaultError::Io(e) => TelegramTokenStoreError::Io(e),
            VaultError::Json(e) => TelegramTokenStoreError::Json(e),
            VaultError::Keychain(e) => TelegramTokenStoreError::Keychain(e),
        }
    }
}

/// Deterministic environment-variable name for the bot token belonging to
/// `agent_id`.
///
/// Format: `LAUNCHPAD_TELEGRAM_TOKEN__<AGENT_ID>`. `agent_id` is upper-cased
/// and every byte that isn't ASCII alphanumeric is replaced with `_`.
///
/// An operator wiring up a headless deployment can compute this name for a
/// known `agent_id` without reading any source code — for example, agent
/// `support-bot` becomes `LAUNCHPAD_TELEGRAM_TOKEN__SUPPORT_BOT`.
pub fn env_var_name(agent_id: &str) -> String {
    format!("{ENV_VAR_PREFIX}__{}", sanitize_env_component(agent_id))
}

fn sanitize_env_component(raw: &str) -> String {
    raw.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect()
}

/// Reads the environment-variable backend for `agent_id`. An env var that is
/// set but empty is treated as unset, so an operator can leave a placeholder
/// blank in a `.env` file without it shadowing a vault-stored token.
fn env_get(agent_id: &str) -> Option<String> {
    std::env::var(env_var_name(agent_id)).ok().filter(|v| !v.is_empty())
}

/// Secure token store for per-agent Telegram bot tokens.
///
/// Open with [`TelegramTokenStore::open`]. All operations are synchronous
/// and delegate to a shared [`SecretVault`] — see that module for backend
/// selection and legacy migration.
pub struct TelegramTokenStore {
    vault: SecretVault,
}

impl TelegramTokenStore {
    /// Open the token store using the best available backend.
    ///
    /// Set `LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK` to any value to force the
    /// file backend (useful in CI where no keychain daemon is running) —
    /// honored by [`SecretVault::open`] as an alias for its own force-file
    /// variable.
    pub fn open() -> Result<Self, TelegramTokenStoreError> {
        Ok(Self { vault: SecretVault::open()? })
    }

    /// Create a file-backed store rooted at `dir`, never touching the OS
    /// keychain.
    ///
    /// Useful in tests that need an isolated store without touching the OS
    /// keychain or the user's real data root.
    pub fn new_with_file_fallback(dir: PathBuf) -> Self {
        Self { vault: SecretVault::new_with_file_fallback(dir) }
    }

    /// Return the stored bot token for `agent_id`, or `None` if absent.
    ///
    /// Checks the environment-variable backend first (see the module-level
    /// resolution order), then falls through to the shared vault.
    pub fn get(&self, agent_id: &str) -> Result<Option<String>, TelegramTokenStoreError> {
        if let Some(from_env) = env_get(agent_id) {
            return Ok(Some(from_env));
        }
        Ok(self.vault.get_telegram(agent_id)?)
    }

    /// Store or overwrite the bot token for `agent_id`.
    ///
    /// Always writes to the vault, never to the process environment — see
    /// the module-level note on why the env var backend is read-only. If an
    /// env var is set for this agent, [`get`](Self::get) will keep returning
    /// it instead of the value written here until the operator unsets it.
    pub fn set(&self, agent_id: &str, token: &str) -> Result<(), TelegramTokenStoreError> {
        Ok(self.vault.set_telegram(agent_id, token)?)
    }

    /// Remove the bot token for `agent_id`. No-op if not present.
    pub fn delete(&self, agent_id: &str) -> Result<(), TelegramTokenStoreError> {
        Ok(self.vault.delete_telegram(agent_id)?)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::{lock_env, EnvGuard};
    use ao_protocol::data_root::DATA_DIR_ENV_VAR;

    // --- Facade parity: file-backed round trip through the shared vault ---

    #[test]
    fn file_round_trip() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1");

        let store = TelegramTokenStore::open().expect("open");
        let agent_id = "agent-a";

        assert!(store.get(agent_id).expect("initial get").is_none());

        store.set(agent_id, "bot-token-value").expect("set");
        let got = store.get(agent_id).expect("get after set");
        assert_eq!(got, Some("bot-token-value".to_owned()));

        store.delete(agent_id).expect("delete");
        assert!(store.get(agent_id).expect("get after delete").is_none());
    }

    #[test]
    fn file_delete_nonexistent_is_noop() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1");

        let store = TelegramTokenStore::open().expect("open");
        store.delete("no-such-agent").expect("delete nonexistent must not error");
    }

    #[test]
    fn file_set_overwrites_existing() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1");

        let store = TelegramTokenStore::open().expect("open");
        let agent_id = "agent-overwrite";

        store.set(agent_id, "token-v1").expect("set v1");
        store.set(agent_id, "token-v2").expect("set v2");

        let got = store.get(agent_id).expect("get");
        assert_eq!(got, Some("token-v2".to_owned()));
    }

    #[test]
    fn file_multiple_agents_isolated() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1");

        let store = TelegramTokenStore::open().expect("open");

        store.set("agent-a", "token-a").expect("set a");
        store.set("agent-b", "token-b").expect("set b");

        assert_eq!(store.get("agent-a").expect("get a"), Some("token-a".to_owned()));
        assert_eq!(store.get("agent-b").expect("get b"), Some("token-b".to_owned()));

        store.delete("agent-a").expect("delete a");
        assert!(store.get("agent-a").expect("get a after delete").is_none());
        assert_eq!(store.get("agent-b").expect("get b still present"), Some("token-b".to_owned()));
    }

    #[test]
    fn new_with_file_fallback_round_trips_without_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TelegramTokenStore::new_with_file_fallback(dir.path().to_path_buf());

        store.set("agent-direct", "direct-token").expect("set");
        assert_eq!(store.get("agent-direct").expect("get"), Some("direct-token".to_owned()));

        store.delete("agent-direct").expect("delete");
        assert!(store.get("agent-direct").expect("get after delete").is_none());
    }

    // --- Env-var backend ---

    #[test]
    fn env_var_name_is_deterministic_and_documented_shape() {
        assert_eq!(env_var_name("support-bot"), "LAUNCHPAD_TELEGRAM_TOKEN__SUPPORT_BOT");
        assert_eq!(env_var_name("agent.a:1"), "LAUNCHPAD_TELEGRAM_TOKEN__AGENT_A_1");
    }

    #[test]
    fn env_backend_is_read_without_any_stored_value() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1");
        let _ev = EnvGuard::set(&env_var_name("agent-env"), "from-env-var");

        let store = TelegramTokenStore::open().expect("open");

        assert_eq!(
            store.get("agent-env").expect("get"),
            Some("from-env-var".to_owned()),
            "a token with no vault entry must still resolve from the env var"
        );
    }

    #[test]
    fn env_var_empty_string_is_treated_as_unset() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1");
        let _ev = EnvGuard::set(&env_var_name("agent-empty"), "");

        let store = TelegramTokenStore::open().expect("open");
        assert!(store.get("agent-empty").expect("get").is_none());
    }

    /// Proves requirement (c): the env var backend outranks whatever is
    /// already stored in the vault, even when the vault has a value for the
    /// same agent.
    #[test]
    fn env_var_beats_vault_contents() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1");

        let store = TelegramTokenStore::open().expect("open");
        store.set("agent-order", "from-vault").expect("set");
        assert_eq!(store.get("agent-order").unwrap(), Some("from-vault".to_owned()));

        let _ev = EnvGuard::set(&env_var_name("agent-order"), "from-env-var");
        assert_eq!(
            store.get("agent-order").unwrap(),
            Some("from-env-var".to_owned()),
            "env var must take priority over a value already stored in the vault"
        );
    }

    #[test]
    #[ignore = "requires OS keychain; run with LAUNCHPAD_TEST_KEYCHAIN=1 cargo test -- --ignored"]
    fn env_beats_keychain_backend() {
        if std::env::var("LAUNCHPAD_TEST_KEYCHAIN").is_err() {
            return;
        }
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());

        let agent_id = "ao_test_telegram_env_order_zz9z";
        let store = TelegramTokenStore::open().expect("open");
        store.vault.set_telegram(agent_id, "from-keychain").expect("seed vault");

        let _ev = EnvGuard::set(&env_var_name(agent_id), "from-env-var");
        assert_eq!(
            store.get(agent_id).unwrap(),
            Some("from-env-var".to_owned()),
            "env var must take priority over a value already stored in the keychain-backed vault"
        );

        store.vault.delete_telegram(agent_id).expect("cleanup");
    }

    // --- Keychain-backed test (skipped unless opted in) ---

    #[test]
    #[ignore = "requires OS keychain; run with LAUNCHPAD_TEST_KEYCHAIN=1 cargo test -- --ignored"]
    fn keychain_round_trip() {
        // This test must only run when the developer explicitly opts in, since it
        // writes a real entry to the system keychain.
        if std::env::var("LAUNCHPAD_TEST_KEYCHAIN").is_err() {
            return;
        }
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());

        let agent_id = "ao_test_telegram_probe_zz9z";
        let store = TelegramTokenStore::open().expect("open");
        let _ = store.delete(agent_id); // clean up any leftover

        store.set(agent_id, "bot-token-value").expect("set");
        assert_eq!(store.get(agent_id).expect("get"), Some("bot-token-value".to_owned()));

        store.delete(agent_id).expect("delete");
        assert!(store.get(agent_id).expect("get after delete").is_none());
    }
}
