//! Messaging-channel credential storage (email inbox passwords, Discord bot
//! tokens, Slack bot/app tokens, and future per-binding secrets).
//!
//! A thin facade over [`crate::secret_vault::SecretVault`] — see that
//! module's docs for how secrets actually persist (OS keychain vs. JSON
//! file, backend selection, and the one-time legacy-item migration). This
//! module only adds what's specific to channel secrets: keying by
//! `(agent_id, binding_id, secret_role)` rather than just `agent_id`, since a
//! single binding (e.g. one email account) may need to hold more than one
//! secret — for example, distinct IMAP and SMTP credentials instead of the
//! one shared password most app-password setups use today.
//!
//! # Backend resolution order
//!
//! Every [`get`](ChannelSecretStore::get) resolves a secret in this order,
//! stopping at the first hit:
//!
//! 1. **Environment variable** — a deterministic name derived from
//!    `(agent_id, binding_id, secret_role)` by [`env_var_name`]. Always
//!    checked first, regardless of which backend the shared vault is using,
//!    so an operator (or an external secret manager that injects process env
//!    vars) can override any stored secret without touching the vault at
//!    all. An env var that is set but empty counts as unset.
//! 2. **The shared [`SecretVault`]** — OS keychain when reachable, else a
//!    JSON file under the data root.
//!
//! The environment variable is read-only from the store's perspective:
//! [`set`](ChannelSecretStore::set) and [`delete`](ChannelSecretStore::delete)
//! always write to the vault, never to the process environment. If an env
//! var is set, it keeps shadowing whatever `set()` last wrote until the
//! operator unsets it — that's the point of giving it top priority: an
//! external secret manager should always win over whatever happens to be
//! stored.

use std::path::PathBuf;

use ao_protocol::error::AoError;
use thiserror::Error;

use crate::secret_vault::{SecretVault, VaultError};

/// Keychain service name this store used before it became a facade over
/// [`crate::secret_vault::SecretVault`]. Kept as the canonical identifier of
/// "the old per-store location" — referenced by the vault's one-time legacy
/// migration, not by this module anymore.
pub(crate) const KEYRING_SERVICE: &str = "launchpad_studio_channels";
/// Legacy file-fallback name, in the same role as [`KEYRING_SERVICE`].
pub(crate) const FILE_STORE_NAME: &str = "channel_secrets.json";
/// Legacy consolidated-item account name, in the same role as
/// [`KEYRING_SERVICE`].
pub(crate) const CONSOLIDATED_ACCOUNT: &str = "__all_channel_secrets_v1__";

/// Prefix for the deterministic per-secret environment variable name built by
/// [`env_var_name`].
const ENV_VAR_PREFIX: &str = "LAUNCHPAD_CHANNEL_SECRET";

/// Secret role for an email binding's single shared IMAP/SMTP password (the
/// common app-password case: one credential authenticates both protocols).
/// Distinct IMAP-only/SMTP-only credentials would need their own roles —
/// deliberately not built yet, see the module doc.
pub const EMAIL_PASSWORD_SECRET_ROLE: &str = "password";

/// Secret role for a Discord binding's bot token. The token authenticates
/// the bot connection itself (not a per-user credential), but is keyed the
/// same way as every other channel secret — by `(agent_id, binding_id,
/// secret_role)` — so a single binding could hold more than one Discord
/// secret in the future without a naming collision.
pub const DISCORD_TOKEN_SECRET_ROLE: &str = "token";

/// Secret role for a Slack binding's bot token (`xoxb-…`), the credential
/// `chat.postMessage` and the rest of the Web API calls authenticate with.
/// Kept as its own role — not reused with [`SLACK_APP_TOKEN_SECRET_ROLE`] —
/// because a single binding holds both a bot token and an app-level token at
/// once, and `(agent_id, binding_id, secret_role)` is the only axis this
/// store keys on.
pub const SLACK_BOT_TOKEN_SECRET_ROLE: &str = "slack_bot_token";

/// Secret role for a Slack binding's app-level token (`xapp-…`), the
/// credential `apps.connections.open` uses to establish the Socket Mode
/// WebSocket. Distinct from [`SLACK_BOT_TOKEN_SECRET_ROLE`] because Slack
/// issues these as two separate tokens with different scopes — one binding,
/// two secrets, same pattern the module doc describes.
pub const SLACK_APP_TOKEN_SECRET_ROLE: &str = "slack_app_token";

#[derive(Debug, Error)]
pub enum ChannelSecretStoreError {
    #[error("data root resolver failed: {0}")]
    DataRoot(#[from] AoError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("keychain error: {0}")]
    Keychain(String),
}

impl From<VaultError> for ChannelSecretStoreError {
    fn from(e: VaultError) -> Self {
        match e {
            VaultError::DataRoot(e) => ChannelSecretStoreError::DataRoot(e),
            VaultError::Io(e) => ChannelSecretStoreError::Io(e),
            VaultError::Json(e) => ChannelSecretStoreError::Json(e),
            VaultError::Keychain(e) => ChannelSecretStoreError::Keychain(e),
        }
    }
}

/// Deterministic environment-variable name for the secret keyed by
/// `(agent_id, binding_id, secret_role)`.
///
/// Format: `LAUNCHPAD_CHANNEL_SECRET__<AGENT_ID>__<BINDING_ID>__<SECRET_ROLE>`.
/// Each component is upper-cased and every byte that isn't ASCII
/// alphanumeric is replaced with `_`. The double-underscore separators make
/// the three components visually distinguishable even though a single
/// component may itself contain underscores after sanitizing.
///
/// An operator wiring up a headless deployment can compute this name for a
/// known `(agent_id, binding_id, secret_role)` without reading any source
/// code — for example, an email binding named `email` on agent `support-bot`
/// with the shared password role becomes
/// `LAUNCHPAD_CHANNEL_SECRET__SUPPORT_BOT__EMAIL__PASSWORD`.
pub fn env_var_name(agent_id: &str, binding_id: &str, secret_role: &str) -> String {
    format!(
        "{ENV_VAR_PREFIX}__{}__{}__{}",
        sanitize_env_component(agent_id),
        sanitize_env_component(binding_id),
        sanitize_env_component(secret_role)
    )
}

fn sanitize_env_component(raw: &str) -> String {
    raw.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect()
}

/// Reads the environment-variable backend for `(agent_id, binding_id,
/// secret_role)`. An env var that is set but empty is treated as unset, so an
/// operator can leave a placeholder blank in a `.env` file without it
/// shadowing a vault-stored secret.
fn env_get(agent_id: &str, binding_id: &str, secret_role: &str) -> Option<String> {
    std::env::var(env_var_name(agent_id, binding_id, secret_role)).ok().filter(|v| !v.is_empty())
}

/// Secure secret store for channel-binding credentials, keyed by
/// `(agent_id, binding_id, secret_role)`.
///
/// Open with [`ChannelSecretStore::open`]. All operations are synchronous
/// and delegate to a shared [`SecretVault`] — see that module for backend
/// selection and legacy migration.
pub struct ChannelSecretStore {
    vault: SecretVault,
}

impl ChannelSecretStore {
    /// Open the secret store using the best available backend.
    ///
    /// Set `LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK` to any value to
    /// force the file backend (useful in CI where no keychain daemon is
    /// running) — honored by [`SecretVault::open`] as an alias for its own
    /// force-file variable.
    pub fn open() -> Result<Self, ChannelSecretStoreError> {
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

    /// Return the stored secret for `(agent_id, binding_id, secret_role)`, or
    /// `None` if absent.
    ///
    /// Checks the environment-variable backend first (see the module-level
    /// resolution order), then falls through to the shared vault.
    pub fn get(
        &self,
        agent_id: &str,
        binding_id: &str,
        secret_role: &str,
    ) -> Result<Option<String>, ChannelSecretStoreError> {
        if let Some(from_env) = env_get(agent_id, binding_id, secret_role) {
            return Ok(Some(from_env));
        }
        Ok(self.vault.get_channel(agent_id, binding_id, secret_role)?)
    }

    /// Store or overwrite the secret for `(agent_id, binding_id, secret_role)`.
    ///
    /// Always writes to the vault, never to the process environment — see
    /// the module-level note on why the env var backend is read-only. If an
    /// env var is set for this key, [`get`](Self::get) will keep returning
    /// it instead of the value written here until the operator unsets it.
    pub fn set(
        &self,
        agent_id: &str,
        binding_id: &str,
        secret_role: &str,
        secret: &str,
    ) -> Result<(), ChannelSecretStoreError> {
        Ok(self.vault.set_channel(agent_id, binding_id, secret_role, secret)?)
    }

    /// Remove the secret for `(agent_id, binding_id, secret_role)`. No-op if
    /// not present.
    pub fn delete(
        &self,
        agent_id: &str,
        binding_id: &str,
        secret_role: &str,
    ) -> Result<(), ChannelSecretStoreError> {
        Ok(self.vault.delete_channel(agent_id, binding_id, secret_role)?)
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
        let _fb = EnvGuard::set("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1");

        let store = ChannelSecretStore::open().expect("open");

        assert!(store.get("agent-a", "email", "password").expect("initial get").is_none());

        store.set("agent-a", "email", "password", "hunter2").expect("set");
        let got = store.get("agent-a", "email", "password").expect("get after set");
        assert_eq!(got, Some("hunter2".to_owned()));

        store.delete("agent-a", "email", "password").expect("delete");
        assert!(store.get("agent-a", "email", "password").expect("get after delete").is_none());
    }

    #[test]
    fn file_delete_nonexistent_is_noop() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1");

        let store = ChannelSecretStore::open().expect("open");
        store
            .delete("no-such-agent", "email", "password")
            .expect("delete nonexistent must not error");
    }

    #[test]
    fn file_set_overwrites_existing() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1");

        let store = ChannelSecretStore::open().expect("open");

        store.set("agent-overwrite", "email", "password", "v1").expect("set v1");
        store.set("agent-overwrite", "email", "password", "v2").expect("set v2");

        let got = store.get("agent-overwrite", "email", "password").expect("get");
        assert_eq!(got, Some("v2".to_owned()));
    }

    /// Proves the reason this store is keyed by `(agent_id, binding_id,
    /// secret_role)` instead of just `agent_id`: a single binding must be
    /// able to hold more than one secret (e.g. distinct IMAP/SMTP passwords
    /// down the line) without one role's write clobbering another's.
    #[test]
    fn multiple_roles_on_the_same_binding_are_isolated() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1");

        let store = ChannelSecretStore::open().expect("open");

        store.set("agent-a", "email", "password", "shared-password").expect("set password");
        store.set("agent-a", "email", "imap_password", "imap-only-password").expect("set imap_password");

        assert_eq!(
            store.get("agent-a", "email", "password").expect("get password"),
            Some("shared-password".to_owned())
        );
        assert_eq!(
            store.get("agent-a", "email", "imap_password").expect("get imap_password"),
            Some("imap-only-password".to_owned())
        );

        store.delete("agent-a", "email", "password").expect("delete password");
        assert!(store.get("agent-a", "email", "password").expect("get after delete").is_none());
        assert_eq!(
            store.get("agent-a", "email", "imap_password").expect("imap_password survives sibling delete"),
            Some("imap-only-password".to_owned()),
            "deleting one role must not affect a sibling role on the same binding"
        );
    }

    #[test]
    fn multiple_bindings_and_agents_are_isolated() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1");

        let store = ChannelSecretStore::open().expect("open");

        store.set("agent-a", "email-1", "password", "a1").expect("set a1");
        store.set("agent-a", "email-2", "password", "a2").expect("set a2");
        store.set("agent-b", "email-1", "password", "b1").expect("set b1");

        assert_eq!(store.get("agent-a", "email-1", "password").unwrap(), Some("a1".to_owned()));
        assert_eq!(store.get("agent-a", "email-2", "password").unwrap(), Some("a2".to_owned()));
        assert_eq!(store.get("agent-b", "email-1", "password").unwrap(), Some("b1".to_owned()));

        store.delete("agent-a", "email-1", "password").expect("delete a1");
        assert!(store.get("agent-a", "email-1", "password").unwrap().is_none());
        assert_eq!(store.get("agent-a", "email-2", "password").unwrap(), Some("a2".to_owned()));
        assert_eq!(store.get("agent-b", "email-1", "password").unwrap(), Some("b1".to_owned()));
    }

    #[test]
    fn new_with_file_fallback_round_trips_without_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ChannelSecretStore::new_with_file_fallback(dir.path().to_path_buf());

        store.set("agent-direct", "email", "password", "direct-secret").expect("set");
        assert_eq!(store.get("agent-direct", "email", "password").expect("get"), Some("direct-secret".to_owned()));

        store.delete("agent-direct", "email", "password").expect("delete");
        assert!(store.get("agent-direct", "email", "password").expect("get after delete").is_none());
    }

    // --- Env-var backend ---

    #[test]
    fn env_var_name_is_deterministic_and_documented_shape() {
        assert_eq!(
            env_var_name("support-bot", "email", "password"),
            "LAUNCHPAD_CHANNEL_SECRET__SUPPORT_BOT__EMAIL__PASSWORD"
        );
        // Non-alphanumeric bytes (besides the `__` separators we insert)
        // sanitize to `_` rather than being dropped, so distinct inputs that
        // differ only by punctuation still can't collide into the same name.
        assert_eq!(env_var_name("agent.a", "disc:ord-1", "token"), "LAUNCHPAD_CHANNEL_SECRET__AGENT_A__DISC_ORD_1__TOKEN");
    }

    #[test]
    fn env_backend_is_read_without_any_stored_value() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1");
        let _ev = EnvGuard::set(&env_var_name("agent-env", "email", "password"), "from-env-var");

        let store = ChannelSecretStore::open().expect("open");

        assert_eq!(
            store.get("agent-env", "email", "password").expect("get"),
            Some("from-env-var".to_owned()),
            "a secret with no vault entry must still resolve from the env var"
        );
    }

    #[test]
    fn env_var_empty_string_is_treated_as_unset() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1");
        let _ev = EnvGuard::set(&env_var_name("agent-empty", "email", "password"), "");

        let store = ChannelSecretStore::open().expect("open");
        assert!(store.get("agent-empty", "email", "password").expect("get").is_none());
    }

    /// Proves requirement (c): the env var backend outranks whatever is
    /// already stored in the vault, even when the vault has a value for the
    /// same key.
    #[test]
    fn env_var_beats_vault_contents() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1");

        let store = ChannelSecretStore::open().expect("open");
        store.set("agent-order", "email", "password", "from-vault").expect("set");
        assert_eq!(store.get("agent-order", "email", "password").unwrap(), Some("from-vault".to_owned()));

        let _ev = EnvGuard::set(&env_var_name("agent-order", "email", "password"), "from-env-var");
        assert_eq!(
            store.get("agent-order", "email", "password").unwrap(),
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

        let agent_id = "ao_test_channel_env_order_zz9z";
        let store = ChannelSecretStore::open().expect("open");
        store.vault.set_channel(agent_id, "email", "password", "from-keychain").expect("seed vault");

        let _ev = EnvGuard::set(&env_var_name(agent_id, "email", "password"), "from-env-var");
        assert_eq!(
            store.get(agent_id, "email", "password").unwrap(),
            Some("from-env-var".to_owned()),
            "env var must take priority over a value already stored in the keychain-backed vault"
        );

        store.vault.delete_channel(agent_id, "email", "password").expect("cleanup");
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

        let store = ChannelSecretStore::open().expect("open");
        let _ = store.delete("ao_test_channel_probe_zz9z", "email", "password"); // clean up any leftover

        store.set("ao_test_channel_probe_zz9z", "email", "password", "secret-value").expect("set");
        let got = store.get("ao_test_channel_probe_zz9z", "email", "password").expect("get");
        assert_eq!(got, Some("secret-value".to_owned()));

        store.delete("ao_test_channel_probe_zz9z", "email", "password").expect("delete");
        assert!(store.get("ao_test_channel_probe_zz9z", "email", "password").expect("get after delete").is_none());
    }
}
