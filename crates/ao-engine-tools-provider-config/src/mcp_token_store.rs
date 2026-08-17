//! MCP OAuth credential storage.
//!
//! A thin facade over [`crate::secret_vault::SecretVault`] — see that
//! module's docs for how tokens actually persist (OS keychain vs. JSON file,
//! backend selection, and the one-time legacy-item migration). This module
//! only adds what's specific to MCP: the [`McpTokenRecord`] shape and
//! [`derive_server_key`].

use std::fmt;
use std::path::PathBuf;

use ao_protocol::error::AoError;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::secret_vault::{SecretVault, VaultError};

/// Keychain service name this store used before it became a facade over
/// [`crate::secret_vault::SecretVault`]. Kept as the canonical identifier of
/// "the old per-store location" — referenced by the vault's one-time legacy
/// migration, not by this module anymore.
pub(crate) const KEYRING_SERVICE: &str = "launchpad_studio_mcp";
/// Legacy file-fallback name, in the same role as [`KEYRING_SERVICE`].
pub(crate) const FILE_STORE_NAME: &str = "mcp_tokens.json";
/// Legacy consolidated-item account name, in the same role as
/// [`KEYRING_SERVICE`].
pub(crate) const CONSOLIDATED_ACCOUNT: &str = "__all_mcp_tokens_v1__";

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum McpTokenStoreError {
    #[error("data root resolver failed: {0}")]
    DataRoot(#[from] AoError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("keychain error: {0}")]
    Keychain(String),
}

impl From<VaultError> for McpTokenStoreError {
    fn from(e: VaultError) -> Self {
        match e {
            VaultError::DataRoot(e) => McpTokenStoreError::DataRoot(e),
            VaultError::Io(e) => McpTokenStoreError::Io(e),
            VaultError::Json(e) => McpTokenStoreError::Json(e),
            VaultError::Keychain(e) => McpTokenStoreError::Keychain(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Token record
// ---------------------------------------------------------------------------

/// An OAuth credential set for one MCP server.
///
/// Serialized as JSON when stored. Debug output redacts all secret fields so
/// the struct is safe to log.
#[derive(Clone, Serialize, Deserialize)]
pub struct McpTokenRecord {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// When the access token expires. `None` means no expiry was reported.
    pub expires_at: Option<DateTime<Utc>>,
    pub scope: Option<String>,
    pub client_id: String,
    /// Present when Dynamic Client Registration returned a client secret.
    pub client_secret: Option<String>,
    /// The token endpoint URL used to refresh this credential.
    ///
    /// Stored here so the refresh path does not need to re-run discovery.
    /// `None` for records created before this field was introduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
}

impl McpTokenRecord {
    /// Returns `true` when the access token will expire within `minutes`
    /// minutes (or has already expired).
    ///
    /// Returns `false` when no expiry timestamp was recorded — the token is
    /// assumed valid until proven otherwise.
    pub fn is_expiring_within(&self, minutes: u64) -> bool {
        match self.expires_at {
            None => false,
            Some(exp) => exp <= Utc::now() + Duration::minutes(minutes as i64),
        }
    }
}

impl fmt::Debug for McpTokenRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpTokenRecord")
            .field("access_token", &"REDACTED")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "REDACTED"))
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "REDACTED"))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Server key derivation
// ---------------------------------------------------------------------------

/// Derives a stable identifier for a server's token entry.
///
/// The key encodes both the server name and a hash of its connection config
/// (URL + transport string), so changing the endpoint invalidates any stored
/// credential automatically.
///
/// # Example
/// ```
/// use ao_engine_tools_provider_config::mcp_token_store::derive_server_key;
/// let key = derive_server_key("myserver", Some("https://api.example.com/mcp"), "http");
/// assert!(key.starts_with("myserver_"));
/// ```
pub fn derive_server_key(name: &str, url: Option<&str>, transport: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.unwrap_or("").as_bytes());
    hasher.update(b"\x00");
    hasher.update(transport.as_bytes());
    let digest = hasher.finalize();
    // 8 bytes → 16 hex chars; enough to distinguish configs without long keys
    let suffix: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{name}_{suffix}")
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Secure token store for MCP OAuth credentials.
///
/// Open with [`McpTokenStore::open`]. All operations are synchronous and
/// delegate to a shared [`SecretVault`] — see that module for backend
/// selection and legacy migration.
pub struct McpTokenStore {
    vault: SecretVault,
}

impl McpTokenStore {
    /// Open the token store using the best available backend.
    ///
    /// Set `LAUNCHPAD_MCP_STORE_FILE_FALLBACK` to any value to force the file
    /// backend (useful in CI where no keychain daemon is running) — honored
    /// by [`SecretVault::open`] as an alias for its own force-file variable.
    pub fn open() -> Result<Self, McpTokenStoreError> {
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

    /// Return the stored credential for `server_key`, or `None` if absent.
    pub fn get(&self, server_key: &str) -> Result<Option<McpTokenRecord>, McpTokenStoreError> {
        Ok(self.vault.get_mcp(server_key)?)
    }

    /// Return the stored credential for `server_key`, always re-reading the
    /// backing store first — see [`SecretVault::get_mcp_fresh`] for why this
    /// differs from [`Self::get`]. Used on the OAuth refresh-decision path
    /// (see `ao_engine_tools_runner::mcp::oauth_flow::OAuthEngine::current_access_token`),
    /// not on ordinary credential lookups.
    pub fn get_fresh(&self, server_key: &str) -> Result<Option<McpTokenRecord>, McpTokenStoreError> {
        Ok(self.vault.get_mcp_fresh(server_key)?)
    }

    /// Store or overwrite the credential for `server_key`.
    pub fn set(&self, server_key: &str, record: &McpTokenRecord) -> Result<(), McpTokenStoreError> {
        Ok(self.vault.set_mcp(server_key, record)?)
    }

    /// Remove the credential for `server_key`. No-op if not present.
    pub fn delete(&self, server_key: &str) -> Result<(), McpTokenStoreError> {
        Ok(self.vault.delete_mcp(server_key)?)
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

    fn sample_record() -> McpTokenRecord {
        McpTokenRecord {
            access_token: "at_test_value".to_owned(),
            refresh_token: Some("rt_test_value".to_owned()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            scope: Some("read write".to_owned()),
            client_id: "client_abc".to_owned(),
            client_secret: Some("cs_test_value".to_owned()),
            token_endpoint: Some("https://auth.example.com/token".to_owned()),
        }
    }

    // --- server key ---

    #[test]
    fn server_key_stable() {
        let a = derive_server_key("srv", Some("https://api.example.com/mcp"), "http");
        let b = derive_server_key("srv", Some("https://api.example.com/mcp"), "http");
        assert_eq!(a, b);
    }

    #[test]
    fn server_key_url_change_invalidates() {
        let a = derive_server_key("srv", Some("https://api.example.com/mcp"), "http");
        let b = derive_server_key("srv", Some("https://other.example.com/mcp"), "http");
        assert_ne!(a, b);
    }

    #[test]
    fn server_key_transport_change_invalidates() {
        let a = derive_server_key("srv", Some("https://api.example.com"), "http");
        let b = derive_server_key("srv", Some("https://api.example.com"), "sse");
        assert_ne!(a, b);
    }

    #[test]
    fn server_key_name_change_invalidates() {
        let a = derive_server_key("server_a", Some("https://api.example.com"), "http");
        let b = derive_server_key("server_b", Some("https://api.example.com"), "http");
        assert_ne!(a, b);
    }

    #[test]
    fn server_key_has_name_prefix() {
        let k = derive_server_key("myserver", Some("https://api.example.com"), "http");
        assert!(k.starts_with("myserver_"), "got: {k}");
    }

    // --- expiry helper ---

    #[test]
    fn is_expiring_within_already_expired() {
        let r = McpTokenRecord {
            expires_at: Some(Utc::now() - Duration::seconds(1)),
            ..sample_record()
        };
        assert!(r.is_expiring_within(5));
    }

    #[test]
    fn is_expiring_within_far_future() {
        let r = McpTokenRecord {
            expires_at: Some(Utc::now() + Duration::hours(24)),
            ..sample_record()
        };
        assert!(!r.is_expiring_within(5));
    }

    #[test]
    fn is_expiring_within_no_expiry_returns_false() {
        let r = McpTokenRecord { expires_at: None, ..sample_record() };
        assert!(!r.is_expiring_within(5));
    }

    // --- Debug redaction ---

    #[test]
    fn debug_redacts_secrets() {
        let r = sample_record();
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("at_test_value"), "access_token leaked");
        assert!(!dbg.contains("rt_test_value"), "refresh_token leaked");
        assert!(!dbg.contains("cs_test_value"), "client_secret leaked");
        assert!(dbg.contains("REDACTED"));
        assert!(dbg.contains("client_abc"), "client_id should not be redacted");
    }

    // --- Facade parity: file-backed round trip through the shared vault ---

    #[test]
    fn file_round_trip() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_MCP_STORE_FILE_FALLBACK", "1");

        let store = McpTokenStore::open().expect("open");
        let key = derive_server_key("testserver", Some("https://test.example.com"), "http");

        assert!(store.get(&key).expect("initial get").is_none());

        let record = sample_record();
        store.set(&key, &record).expect("set");

        let got = store.get(&key).expect("get after set").expect("present");
        assert_eq!(got.access_token, record.access_token);
        assert_eq!(got.refresh_token, record.refresh_token);
        assert_eq!(got.client_id, record.client_id);
        assert_eq!(got.scope, record.scope);
        assert_eq!(got.client_secret, record.client_secret);

        store.delete(&key).expect("delete");
        assert!(store.get(&key).expect("get after delete").is_none());
    }

    #[test]
    fn file_round_trip_with_client_secret() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_MCP_STORE_FILE_FALLBACK", "1");

        let store = McpTokenStore::open().expect("open");
        let key = "cs_round_trip_key";
        store.set(key, &sample_record()).expect("set");
        let got = store.get(key).expect("get").expect("present");
        assert_eq!(got.client_secret, Some("cs_test_value".to_owned()));
    }

    #[test]
    fn file_delete_nonexistent_is_noop() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_MCP_STORE_FILE_FALLBACK", "1");

        let store = McpTokenStore::open().expect("open");
        store.delete("no_such_key").expect("delete nonexistent must not error");
    }

    #[test]
    fn file_set_overwrites_existing() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_MCP_STORE_FILE_FALLBACK", "1");

        let store = McpTokenStore::open().expect("open");
        let key = "overwrite_key";

        store.set(key, &sample_record()).expect("set v1");

        let updated = McpTokenRecord {
            access_token: "new_access_token".to_owned(),
            ..sample_record()
        };
        store.set(key, &updated).expect("set v2");

        let got = store.get(key).expect("get").expect("present");
        assert_eq!(got.access_token, "new_access_token");
    }

    #[test]
    fn file_multiple_servers_isolated() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_MCP_STORE_FILE_FALLBACK", "1");

        let store = McpTokenStore::open().expect("open");
        let key_a = derive_server_key("server_a", Some("https://a.example.com"), "http");
        let key_b = derive_server_key("server_b", Some("https://b.example.com"), "sse");

        let r_a = McpTokenRecord { access_token: "token_a".to_owned(), ..sample_record() };
        let r_b = McpTokenRecord { access_token: "token_b".to_owned(), ..sample_record() };

        store.set(&key_a, &r_a).expect("set a");
        store.set(&key_b, &r_b).expect("set b");

        assert_eq!(store.get(&key_a).expect("get a").unwrap().access_token, "token_a");
        assert_eq!(store.get(&key_b).expect("get b").unwrap().access_token, "token_b");

        store.delete(&key_a).expect("delete a");
        assert!(store.get(&key_a).expect("get a after delete").is_none());
        assert!(store.get(&key_b).expect("get b still present").is_some());
    }

    #[test]
    fn new_with_file_fallback_round_trips_without_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = McpTokenStore::new_with_file_fallback(dir.path().to_path_buf());
        let key = "direct_fallback_key";

        store.set(key, &sample_record()).expect("set");
        let got = store.get(key).expect("get").expect("present");
        assert_eq!(got.access_token, "at_test_value");

        store.delete(key).expect("delete");
        assert!(store.get(key).expect("get after delete").is_none());
    }
}
