use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ao_protocol::data_root::resolve_data_root;
use ao_protocol::error::AoError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MCP_SERVERS_TOML: &str = "mcp_servers.toml";

#[derive(Debug, Error)]
pub enum McpConfigError {
    #[error("invalid MCP server name {0:?} (must match ^[a-z][a-z0-9_]*$)")]
    InvalidName(String),

    #[error("duplicate MCP server name {0:?}")]
    DuplicateName(String),

    #[error("MCP server not found: {0:?}")]
    EntryNotFound(String),

    #[error("invalid config for server {0:?}: {1}")]
    InvalidConfig(String, String),

    #[error("TOML parse error: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("TOML serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("data root resolver error: {0}")]
    Resolver(#[from] AoError),
}

/// Transport protocol for an MCP server connection.
///
/// Defaults to `stdio` for backward compatibility with existing configs that
/// only specify a `command` field.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportType {
    #[default]
    Stdio,
    Http,
    Sse,
}

impl McpTransportType {
    pub fn is_default(&self) -> bool {
        *self == McpTransportType::Stdio
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpLoadingPolicy {
    Always,
    Deferred,
    Disabled,
}

impl Default for McpLoadingPolicy {
    fn default() -> Self {
        McpLoadingPolicy::Deferred
    }
}

/// OAuth / bearer-auth configuration for a network-accessible MCP server.
///
/// These fields are placeholders for the OAuth flow engine — semantic
/// interpretation (PKCE flow, token exchange, keychain storage) is handled
/// by the auth engine layer. This struct only stores configuration surface.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct McpAuthConfig {
    /// Preferred local port for the OAuth redirect listener.
    /// The auth engine picks any available port if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_port_hint: Option<u16>,

    /// Pre-registered OAuth client ID, for servers that require one rather
    /// than Dynamic Client Registration (RFC 7591). Not secret — it appears
    /// in the authorization URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,

    /// Pre-registered OAuth client secret, for confidential clients (e.g.
    /// GitHub OAuth Apps) that authenticate at the token endpoint. Omit for
    /// public PKCE clients, which prove possession via the PKCE verifier
    /// alone. Stored in plaintext in `mcp_servers.toml`; migrating this to the
    /// OS keychain is a tracked follow-up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,

    /// Authorization server metadata discovery URL (RFC 9728 / RFC 8414).
    /// Overrides automatic discovery when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerEntry {
    pub name: String,

    /// Command to launch for stdio servers. Required when `transport` is `stdio`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,

    #[serde(default)]
    pub loading: McpLoadingPolicy,

    /// Transport protocol. Defaults to `stdio`.
    #[serde(default, skip_serializing_if = "McpTransportType::is_default")]
    pub transport: McpTransportType,

    /// Endpoint URL for `http` and `sse` transports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// OAuth authorization config for network transports. Ignored for stdio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<McpAuthConfig>,
}

/// Intermediate serde shape — TOML arrays-of-tables use the key `[[server]]`
/// (singular), not `[[servers]]`.
#[derive(Debug, Deserialize, Serialize)]
struct RawMcpServersConfig {
    #[serde(default, rename = "server")]
    servers: Vec<McpServerEntry>,
}

#[derive(Debug, Clone)]
pub struct McpServersConfig {
    pub servers: Vec<McpServerEntry>,
}

impl McpServersConfig {
    /// Load from `<resolve_data_root()>/mcp_servers.toml`.
    ///
    /// Missing file is not an error — returns an empty config.
    /// Malformed TOML, invalid names, duplicate names, or transport constraint
    /// violations are hard errors.
    pub fn load() -> Result<Self, McpConfigError> {
        Self::load_from(&Self::config_path()?)
    }

    /// Load from an explicit path. Missing file returns an empty config.
    pub fn load_from(path: &Path) -> Result<Self, McpConfigError> {
        if !path.exists() {
            return Ok(Self { servers: vec![] });
        }
        let raw = std::fs::read_to_string(path)?;
        Self::from_str(&raw)
    }

    /// Returns the path that `load()` and `save_to_config_path()` use.
    pub fn config_path() -> Result<PathBuf, McpConfigError> {
        let root = resolve_data_root()?;
        Ok(root.join(MCP_SERVERS_TOML))
    }

    /// Persist to the given path using an atomic write (temp file + rename).
    ///
    /// The parent directory is created if absent. TOML comments from a prior
    /// hand-edited file are not preserved — only structured field values round-trip.
    pub fn save(&self, path: &Path) -> Result<(), McpConfigError> {
        let raw = RawMcpServersConfig { servers: self.servers.clone() };
        let toml_str = toml::to_string(&raw)?;

        let dir = path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent directory")
        })?;
        std::fs::create_dir_all(dir)?;

        let tmp_path = dir.join(format!(".mcp_servers.{}.tmp", std::process::id()));
        std::fs::write(&tmp_path, &toml_str)?;
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }

    /// Persist to `<resolve_data_root()>/mcp_servers.toml`.
    pub fn save_to_config_path(&self) -> Result<(), McpConfigError> {
        self.save(&Self::config_path()?)
    }

    /// Add a new server entry, rejecting duplicate names or invalid configs.
    pub fn add_entry(&mut self, entry: McpServerEntry) -> Result<(), McpConfigError> {
        if !is_valid_name(&entry.name) {
            return Err(McpConfigError::InvalidName(entry.name.clone()));
        }
        if self.servers.iter().any(|s| s.name == entry.name) {
            return Err(McpConfigError::DuplicateName(entry.name.clone()));
        }
        validate_entry(&entry)?;
        self.servers.push(entry);
        Ok(())
    }

    /// Remove an entry by name, returning `EntryNotFound` if absent.
    pub fn remove_entry(&mut self, name: &str) -> Result<(), McpConfigError> {
        let before = self.servers.len();
        self.servers.retain(|s| s.name != name);
        if self.servers.len() == before {
            return Err(McpConfigError::EntryNotFound(name.to_string()));
        }
        Ok(())
    }

    /// Replace an existing entry by name, returning `EntryNotFound` if absent.
    pub fn update_entry(&mut self, entry: McpServerEntry) -> Result<(), McpConfigError> {
        if !is_valid_name(&entry.name) {
            return Err(McpConfigError::InvalidName(entry.name.clone()));
        }
        validate_entry(&entry)?;
        let pos = self
            .servers
            .iter()
            .position(|s| s.name == entry.name)
            .ok_or_else(|| McpConfigError::EntryNotFound(entry.name.clone()))?;
        self.servers[pos] = entry;
        Ok(())
    }

    fn from_str(s: &str) -> Result<Self, McpConfigError> {
        let raw: RawMcpServersConfig = toml::from_str(s)?;
        let servers = raw.servers;

        for entry in &servers {
            if !is_valid_name(&entry.name) {
                return Err(McpConfigError::InvalidName(entry.name.clone()));
            }
        }

        let mut seen = HashSet::new();
        for entry in &servers {
            if !seen.insert(entry.name.clone()) {
                return Err(McpConfigError::DuplicateName(entry.name.clone()));
            }
        }

        for entry in &servers {
            validate_entry(entry)?;
        }

        Ok(Self { servers })
    }
}

fn validate_entry(entry: &McpServerEntry) -> Result<(), McpConfigError> {
    match entry.transport {
        McpTransportType::Stdio => {
            if entry.command.is_none() {
                return Err(McpConfigError::InvalidConfig(
                    entry.name.clone(),
                    "stdio transport requires 'command'".to_string(),
                ));
            }
        }
        McpTransportType::Http | McpTransportType::Sse => {
            if entry.url.is_none() {
                return Err(McpConfigError::InvalidConfig(
                    entry.name.clone(),
                    "http/sse transport requires 'url'".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use ao_protocol::data_root::DATA_DIR_ENV_VAR;

    use crate::test_env::{lock_env, EnvGuard};

    use super::*;

    const FULL_FIXTURE: &str = r#"
[[server]]
name = "github"
command = "github-mcp"
args = ["--verbose", "--port", "8080"]
loading = "always"

[server.env]
GITHUB_TOKEN = "ghp_test"
LOG_LEVEL = "debug"

[[server]]
name = "slack"
command = "slack-mcp"
"#;

    #[test]
    fn round_trip_two_servers() {
        let cfg = McpServersConfig::from_str(FULL_FIXTURE).expect("parse fixture");
        assert_eq!(cfg.servers.len(), 2);

        let github = &cfg.servers[0];
        assert_eq!(github.name, "github");
        assert_eq!(github.command.as_deref(), Some("github-mcp"));
        assert_eq!(github.args, vec!["--verbose", "--port", "8080"]);
        assert_eq!(github.loading, McpLoadingPolicy::Always);
        assert_eq!(github.transport, McpTransportType::Stdio);
        assert_eq!(github.env.get("GITHUB_TOKEN").map(String::as_str), Some("ghp_test"));

        let slack = &cfg.servers[1];
        assert_eq!(slack.name, "slack");
        assert_eq!(slack.command.as_deref(), Some("slack-mcp"));
        assert!(slack.args.is_empty());
        assert!(slack.env.is_empty());
        assert_eq!(slack.loading, McpLoadingPolicy::Deferred);
        assert_eq!(slack.transport, McpTransportType::Stdio);
    }

    #[test]
    fn existing_stdio_configs_backward_compatible() {
        // Configs with only name/command/args/env/loading must parse unchanged.
        let toml = "[[server]]\nname = \"minimal\"\ncommand = \"cmd\"\n";
        let cfg = McpServersConfig::from_str(toml).expect("backward compat");
        let entry = &cfg.servers[0];
        assert_eq!(entry.transport, McpTransportType::Stdio);
        assert_eq!(entry.command.as_deref(), Some("cmd"));
        assert!(entry.url.is_none());
        assert!(entry.auth.is_none());
    }

    #[test]
    fn http_server_parses_correctly() {
        let toml = r#"
[[server]]
name = "myapi"
transport = "http"
url = "https://example.com/mcp"
"#;
        let cfg = McpServersConfig::from_str(toml).expect("parse http server");
        assert_eq!(cfg.servers.len(), 1);
        let entry = &cfg.servers[0];
        assert_eq!(entry.transport, McpTransportType::Http);
        assert_eq!(entry.url.as_deref(), Some("https://example.com/mcp"));
        assert!(entry.command.is_none());
    }

    #[test]
    fn sse_server_parses_correctly() {
        let toml = r#"
[[server]]
name = "mysse"
transport = "sse"
url = "https://example.com/sse"
"#;
        let cfg = McpServersConfig::from_str(toml).expect("parse sse server");
        assert_eq!(cfg.servers[0].transport, McpTransportType::Sse);
    }

    #[test]
    fn http_server_with_auth_config() {
        let toml = r#"
[[server]]
name = "secure"
transport = "http"
url = "https://example.com/mcp"

[server.auth]
callback_port_hint = 9876
client_id = "my-client"
client_secret = "my-secret"
metadata_url = "https://auth.example.com/.well-known/oauth-authorization-server"
"#;
        let cfg = McpServersConfig::from_str(toml).expect("parse with auth");
        let auth = cfg.servers[0].auth.as_ref().expect("auth present");
        assert_eq!(auth.callback_port_hint, Some(9876));
        assert_eq!(auth.client_id.as_deref(), Some("my-client"));
        assert_eq!(auth.client_secret.as_deref(), Some("my-secret"));
        assert!(auth.metadata_url.is_some());
    }

    #[test]
    fn auth_config_client_secret_optional() {
        // A public PKCE client configures client_id without a secret.
        let toml = r#"
[[server]]
name = "publicpkce"
transport = "http"
url = "https://example.com/mcp"

[server.auth]
client_id = "public-client"
"#;
        let cfg = McpServersConfig::from_str(toml).expect("parse public client");
        let auth = cfg.servers[0].auth.as_ref().expect("auth present");
        assert_eq!(auth.client_id.as_deref(), Some("public-client"));
        assert!(auth.client_secret.is_none(), "secret absent for public client");
    }

    #[test]
    fn stdio_without_command_rejected() {
        // A server with no transport field (defaults to stdio) but no command.
        let toml = "[[server]]\nname = \"nope\"\n";
        let err = McpServersConfig::from_str(toml).expect_err("should reject missing command");
        assert!(matches!(err, McpConfigError::InvalidConfig(n, _) if n == "nope"));
    }

    #[test]
    fn explicit_stdio_without_command_rejected() {
        let toml = "[[server]]\nname = \"nope\"\ntransport = \"stdio\"\n";
        let err = McpServersConfig::from_str(toml).expect_err("explicit stdio needs command");
        assert!(matches!(err, McpConfigError::InvalidConfig(n, _) if n == "nope"));
    }

    #[test]
    fn http_without_url_rejected() {
        let toml = "[[server]]\nname = \"nope\"\ntransport = \"http\"\n";
        let err = McpServersConfig::from_str(toml).expect_err("http needs url");
        assert!(matches!(err, McpConfigError::InvalidConfig(n, _) if n == "nope"));
    }

    #[test]
    fn sse_without_url_rejected() {
        let toml = "[[server]]\nname = \"nope\"\ntransport = \"sse\"\n";
        let err = McpServersConfig::from_str(toml).expect_err("sse needs url");
        assert!(matches!(err, McpConfigError::InvalidConfig(n, _) if n == "nope"));
    }

    #[test]
    fn name_validation_rejects_uppercase() {
        let toml = "[[server]]\nname = \"GitHub\"\ncommand = \"x\"\n";
        let err = McpServersConfig::from_str(toml).expect_err("should reject uppercase");
        assert!(matches!(err, McpConfigError::InvalidName(n) if n == "GitHub"));
    }

    #[test]
    fn name_validation_rejects_digit_leading() {
        let toml = "[[server]]\nname = \"1github\"\ncommand = \"x\"\n";
        let err = McpServersConfig::from_str(toml).expect_err("should reject digit-leading");
        assert!(matches!(err, McpConfigError::InvalidName(n) if n == "1github"));
    }

    #[test]
    fn duplicate_name_rejected() {
        let toml = "[[server]]\nname = \"github\"\ncommand = \"a\"\n\n[[server]]\nname = \"github\"\ncommand = \"b\"\n";
        let err = McpServersConfig::from_str(toml).expect_err("should reject duplicate");
        assert!(matches!(err, McpConfigError::DuplicateName(n) if n == "github"));
    }

    #[test]
    fn missing_file_returns_empty_config() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());

        let cfg = McpServersConfig::load().expect("should succeed on missing file");
        assert!(cfg.servers.is_empty());
    }

    #[test]
    fn load_reads_from_data_dir_env_var() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(MCP_SERVERS_TOML);
        std::fs::write(&path, FULL_FIXTURE).expect("write fixture");
        let _guard = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());

        let cfg = McpServersConfig::load().expect("load succeeds");
        assert_eq!(cfg.servers.len(), 2);
        assert_eq!(cfg.servers[0].name, "github");
    }

    #[test]
    fn malformed_toml_returns_parse_error() {
        let err = McpServersConfig::from_str("[[[ not valid toml").expect_err("parse error");
        assert!(matches!(err, McpConfigError::Parse(_)));
    }

    #[test]
    fn loading_policy_defaults_to_deferred() {
        let toml = "[[server]]\nname = \"minimal\"\ncommand = \"cmd\"\n";
        let cfg = McpServersConfig::from_str(toml).expect("parse");
        assert_eq!(cfg.servers[0].loading, McpLoadingPolicy::Deferred);
    }

    #[test]
    fn valid_names_accepted() {
        for name in &["a", "abc", "abc123", "a1b2c3", "a_b_c", "a0_1z"] {
            assert!(is_valid_name(name), "{name} should be valid");
        }
    }

    #[test]
    fn invalid_names_rejected() {
        for name in &["", "A", "1abc", "_abc", "abc-def", "ABC"] {
            assert!(!is_valid_name(name), "{name} should be invalid");
        }
    }

    // --- write-back tests ---

    fn make_stdio_entry(name: &str, command: &str) -> McpServerEntry {
        McpServerEntry {
            name: name.to_string(),
            command: Some(command.to_string()),
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Always,
            transport: McpTransportType::Stdio,
            url: None,
            auth: None,
        }
    }

    fn make_http_entry(name: &str, url: &str) -> McpServerEntry {
        McpServerEntry {
            name: name.to_string(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Deferred,
            transport: McpTransportType::Http,
            url: Some(url.to_string()),
            auth: None,
        }
    }

    #[test]
    fn save_then_load_from_preserves_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(MCP_SERVERS_TOML);

        let mut cfg = McpServersConfig { servers: vec![] };
        cfg.add_entry(make_stdio_entry("alpha", "alpha-cmd")).unwrap();
        cfg.add_entry(make_http_entry("beta", "https://example.com/mcp")).unwrap();
        cfg.save(&path).expect("save");

        let loaded = McpServersConfig::load_from(&path).expect("load_from");
        assert_eq!(loaded.servers.len(), 2);
        assert_eq!(loaded.servers[0].name, "alpha");
        assert_eq!(loaded.servers[1].name, "beta");
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(MCP_SERVERS_TOML);

        let mut env = HashMap::new();
        env.insert("TOKEN".to_string(), "secret".to_string());

        let entry = McpServerEntry {
            name: "full".to_string(),
            command: Some("fullcmd".to_string()),
            args: vec!["--flag".to_string(), "val".to_string()],
            env,
            loading: McpLoadingPolicy::Always,
            transport: McpTransportType::Stdio,
            url: None,
            auth: None,
        };

        let cfg = McpServersConfig { servers: vec![entry] };
        cfg.save(&path).unwrap();

        let loaded = McpServersConfig::load_from(&path).unwrap();
        let e = &loaded.servers[0];
        assert_eq!(e.name, "full");
        assert_eq!(e.command.as_deref(), Some("fullcmd"));
        assert_eq!(e.args, vec!["--flag", "val"]);
        assert_eq!(e.env.get("TOKEN").map(String::as_str), Some("secret"));
        assert_eq!(e.loading, McpLoadingPolicy::Always);
        assert_eq!(e.transport, McpTransportType::Stdio);
    }

    #[test]
    fn round_trip_http_with_auth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(MCP_SERVERS_TOML);

        let entry = McpServerEntry {
            name: "secure".to_string(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Deferred,
            transport: McpTransportType::Http,
            url: Some("https://api.example.com/mcp".to_string()),
            auth: Some(McpAuthConfig {
                callback_port_hint: Some(9000),
                client_id: Some("cid".to_string()),
                client_secret: Some("csecret".to_string()),
                metadata_url: Some("https://auth.example.com/.well-known/oauth-authorization-server".to_string()),
            }),
        };

        McpServersConfig { servers: vec![entry] }.save(&path).unwrap();

        let loaded = McpServersConfig::load_from(&path).unwrap();
        let auth = loaded.servers[0].auth.as_ref().unwrap();
        assert_eq!(auth.callback_port_hint, Some(9000));
        assert_eq!(auth.client_id.as_deref(), Some("cid"));
        assert_eq!(auth.client_secret.as_deref(), Some("csecret"));
        assert!(auth.metadata_url.is_some());
    }

    #[test]
    fn save_omits_defaults_and_empty_collections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(MCP_SERVERS_TOML);

        let cfg = McpServersConfig {
            servers: vec![McpServerEntry {
                name: "minimal".to_string(),
                command: Some("my-cmd".to_string()),
                args: vec![],
                env: HashMap::new(),
                loading: McpLoadingPolicy::Deferred,
                transport: McpTransportType::Stdio,
                url: None,
                auth: None,
            }],
        };
        cfg.save(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        // transport = "stdio" is default — should not appear
        assert!(!raw.contains("transport"), "default transport should not be written: {raw}");
        // empty env map — should not produce a [server.env] header
        assert!(!raw.contains("env"), "empty env should not be written: {raw}");
        // empty args — should not appear
        assert!(!raw.contains("args"), "empty args should not be written: {raw}");
    }

    #[test]
    fn add_entry_rejects_duplicate() {
        let mut cfg = McpServersConfig { servers: vec![] };
        cfg.add_entry(make_stdio_entry("srv", "cmd")).unwrap();
        let err = cfg.add_entry(make_stdio_entry("srv", "cmd2")).expect_err("duplicate");
        assert!(matches!(err, McpConfigError::DuplicateName(n) if n == "srv"));
    }

    #[test]
    fn add_entry_rejects_invalid_name() {
        let mut cfg = McpServersConfig { servers: vec![] };
        let err = cfg.add_entry(make_stdio_entry("Bad-Name", "cmd")).expect_err("bad name");
        assert!(matches!(err, McpConfigError::InvalidName(_)));
    }

    #[test]
    fn add_entry_rejects_invalid_transport() {
        let mut cfg = McpServersConfig { servers: vec![] };
        // stdio entry without command
        let bad = McpServerEntry {
            name: "broken".to_string(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Always,
            transport: McpTransportType::Stdio,
            url: None,
            auth: None,
        };
        let err = cfg.add_entry(bad).expect_err("missing command");
        assert!(matches!(err, McpConfigError::InvalidConfig(n, _) if n == "broken"));
    }

    #[test]
    fn remove_entry_removes_server() {
        let mut cfg = McpServersConfig { servers: vec![] };
        cfg.add_entry(make_stdio_entry("alpha", "cmd1")).unwrap();
        cfg.add_entry(make_stdio_entry("beta", "cmd2")).unwrap();
        cfg.remove_entry("alpha").unwrap();
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name, "beta");
    }

    #[test]
    fn remove_entry_errors_on_missing() {
        let mut cfg = McpServersConfig { servers: vec![] };
        let err = cfg.remove_entry("ghost").expect_err("not found");
        assert!(matches!(err, McpConfigError::EntryNotFound(n) if n == "ghost"));
    }

    #[test]
    fn update_entry_replaces_by_name() {
        let mut cfg = McpServersConfig { servers: vec![] };
        cfg.add_entry(make_stdio_entry("srv", "old-cmd")).unwrap();
        let updated = make_stdio_entry("srv", "new-cmd");
        cfg.update_entry(updated).unwrap();
        assert_eq!(cfg.servers[0].command.as_deref(), Some("new-cmd"));
    }

    #[test]
    fn update_entry_errors_on_missing() {
        let mut cfg = McpServersConfig { servers: vec![] };
        let err = cfg.update_entry(make_stdio_entry("ghost", "cmd")).expect_err("not found");
        assert!(matches!(err, McpConfigError::EntryNotFound(n) if n == "ghost"));
    }

    #[test]
    fn save_creates_parent_dir_if_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("subdir").join(MCP_SERVERS_TOML);
        let cfg = McpServersConfig { servers: vec![] };
        cfg.save(&path).expect("save into missing subdir");
        assert!(path.exists());
    }

    #[test]
    fn add_remove_save_load_integration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(MCP_SERVERS_TOML);

        let mut cfg = McpServersConfig { servers: vec![] };
        cfg.add_entry(make_stdio_entry("alpha", "cmd-a")).unwrap();
        cfg.add_entry(make_stdio_entry("beta", "cmd-b")).unwrap();
        cfg.add_entry(make_stdio_entry("gamma", "cmd-g")).unwrap();
        cfg.remove_entry("beta").unwrap();
        cfg.save(&path).unwrap();

        let loaded = McpServersConfig::load_from(&path).unwrap();
        assert_eq!(loaded.servers.len(), 2);
        let names: Vec<_> = loaded.servers.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"gamma"));
        assert!(!names.contains(&"beta"));
    }
}
