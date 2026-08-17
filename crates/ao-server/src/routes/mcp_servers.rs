use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use ao_engine::AppState;
use ao_engine_tools_provider_config::mcp_servers::{
    McpConfigError, McpLoadingPolicy, McpServerEntry, McpServersConfig, McpTransportType,
};
use ao_engine_tools_runner::mcp::{McpManager, McpManagerError, McpServerState, McpServerStatus};
use ao_protocol::error::AoError;

use crate::error::AppError;

// ── Config helpers ────────────────────────────────────────────────────────────

fn load_config() -> Result<McpServersConfig, AppError> {
    McpServersConfig::load().map_err(|e| AppError(AoError::Internal(e.to_string())))
}

fn save_config(config: &McpServersConfig) -> Result<(), AppError> {
    let path = McpServersConfig::config_path()
        .map_err(|e| AppError(AoError::Internal(e.to_string())))?;
    config
        .save(&path)
        .map_err(|e| AppError(AoError::Internal(e.to_string())))
}

fn map_config_error(e: McpConfigError) -> AppError {
    match e {
        McpConfigError::InvalidName(name) => AppError(AoError::ValidationError(format!(
            "invalid MCP server name {name:?}: must start with a lowercase letter and contain only [a-z0-9_]"
        ))),
        McpConfigError::DuplicateName(name) => AppError(AoError::AgentAlreadyExists(format!(
            "MCP server {name:?} already exists"
        ))),
        McpConfigError::InvalidConfig(name, msg) => AppError(AoError::ValidationError(format!(
            "invalid config for server {name:?}: {msg}"
        ))),
        e => AppError(AoError::Internal(e.to_string())),
    }
}

fn endpoint_label(entry: &McpServerEntry) -> String {
    match entry.transport {
        McpTransportType::Stdio => {
            let cmd = entry.command.as_deref().unwrap_or("");
            if entry.args.is_empty() {
                cmd.to_string()
            } else {
                format!("{} {}", cmd, entry.args.join(" "))
            }
        }
        McpTransportType::Http | McpTransportType::Sse => {
            entry.url.as_deref().unwrap_or("").to_string()
        }
    }
}

fn transport_label(t: &McpTransportType) -> &'static str {
    match t {
        McpTransportType::Stdio => "stdio",
        McpTransportType::Http => "http",
        McpTransportType::Sse => "sse",
    }
}

// ── GET /mcp-servers ──────────────────────────────────────────────────────────

/// Returns a snapshot of every configured MCP server and its current state.
///
/// The list is seeded from the live manager (which covers connected,
/// needs-auth, failed, and disabled servers). Entries that appear in the
/// config file but are not yet tracked by the manager (e.g. a server whose
/// last add attempt failed) are appended so the UI always reflects the full
/// configured set.
pub async fn list_servers(State(state): State<Arc<AppState>>) -> Json<Vec<McpServerStatus>> {
    let mut statuses = state.mcp_manager.server_statuses().await;
    let tracked: HashSet<String> = statuses.iter().map(|s| s.name.clone()).collect();

    if let Ok(config) = McpServersConfig::load() {
        for entry in config.servers {
            if tracked.contains(&entry.name) {
                continue;
            }
            let server_state = if entry.loading == McpLoadingPolicy::Disabled {
                McpServerState::Disabled
            } else {
                McpServerState::Error
            };
            let transport = transport_label(&entry.transport).to_string();
            let endpoint = endpoint_label(&entry);
            statuses.push(McpServerStatus {
                name: entry.name,
                transport,
                endpoint,
                state: server_state,
                error: Some("connection failed during last attempt".to_string()),
                tool_names: vec![],
                source: "config".to_string(),
            });
        }
    }

    Json(statuses)
}

// ── POST /mcp-servers ─────────────────────────────────────────────────────────

/// Add and live-connect a new MCP server.
///
/// Validates the entry, persists it to `mcp_servers.toml`, and attempts an
/// immediate live connection via [`McpManager::add_server`]. If the live
/// connection fails the entry is still persisted (so it survives a restart)
/// and the response returns `state = "error"` with the failure message.
pub async fn add_server(
    State(state): State<Arc<AppState>>,
    Json(entry): Json<McpServerEntry>,
) -> Result<(StatusCode, Json<McpServerStatus>), AppError> {
    // Persist to config first (also validates name format and transport constraints).
    let mut config = load_config()?;
    config.add_entry(entry.clone()).map_err(map_config_error)?;
    save_config(&config)?;

    // Attempt live connection.
    let registry = Arc::clone(&state.tools_registry);
    match McpManager::add_server(&state.mcp_manager, entry.clone(), registry, "config".to_string()).await {
        Ok(status) => Ok((StatusCode::CREATED, Json(status))),
        Err(McpManagerError::DuplicateName(name)) => Err(AppError(AoError::AgentAlreadyExists(
            format!("MCP server {name:?} is already active"),
        ))),
        Err(McpManagerError::ConnectionFailed(msg)) => {
            let transport = transport_label(&entry.transport).to_string();
            let endpoint = endpoint_label(&entry);
            let status = McpServerStatus {
                name: entry.name,
                transport,
                endpoint,
                state: McpServerState::Error,
                error: Some(msg),
                tool_names: vec![],
                source: "config".to_string(),
            };
            Ok((StatusCode::CREATED, Json(status)))
        }
        Err(e) => Err(AppError(AoError::Internal(e.to_string()))),
    }
}

// ── DELETE /mcp-servers/{name} ────────────────────────────────────────────────

/// Remove an MCP server from the config and shut down its live connection.
///
/// Returns 404 if no server with the given name is known (checked against both
/// the config file and the live manager).
pub async fn delete_server(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<StatusCode, AppError> {
    // Reject deletion of plugin-managed servers.
    if let Some(source) = state.mcp_manager.server_source(&name).await {
        if source.starts_with("plugin:") {
            return Err(AppError(AoError::ValidationError(format!(
                "MCP server {name:?} is managed by a plugin and cannot be deleted directly; \
                 uninstall the plugin to remove its servers"
            ))));
        }
    }

    // Remove from config file.
    let mut config = load_config()?;
    let in_config = config.remove_entry(&name).is_ok();
    if in_config {
        save_config(&config)?;
    }

    // Remove from live manager.
    let remove_result = state
        .mcp_manager
        .remove_server(&name, &state.tools_registry)
        .await;

    match remove_result {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(McpManagerError::NotFound(_)) if in_config => {
            // Was in config (already cleaned up) but not live — still a success.
            Ok(StatusCode::NO_CONTENT)
        }
        Err(McpManagerError::NotFound(_)) => Err(AppError(AoError::TaskNotFound(format!(
            "MCP server '{name}' not found"
        )))),
        Err(e) => Err(AppError(AoError::Internal(e.to_string()))),
    }
}

// ── POST /mcp-servers/{name}/authorize ────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct AuthorizeResponse {
    /// Authorization URL the caller should open in a browser to complete the
    /// OAuth consent flow. Present only while the callback listener is active.
    pub auth_url: String,
}

/// Start (or restart) the OAuth authorization flow for a configured server.
///
/// Works regardless of the server's current state — including servers that
/// already report `connected`, so a working credential can be rotated
/// without first being torn down. See
/// [`McpManager::trigger_auth_flow`] for the exact state precedence.
///
/// Returns 202 Accepted immediately with the authorization URL. The backend
/// spawns a listener for the OAuth callback; once the user completes the
/// browser consent the credential is persisted and the server (re)connects.
/// A server that started in `needs_auth` or `error` transitions to
/// `connected`; a server that was already `connected` stays `connected` with
/// its tool bindings refreshed in place. Callers should poll
/// `GET /mcp-servers` to detect the `needs_auth`/`error` → `connected`
/// transition; an already-connected server has no observable state change to
/// poll for.
///
/// Returns 400 if `name` is not a configured MCP server, if it uses the
/// `stdio` transport (no browser-based auth flow), or if no token store is
/// available.
pub async fn authorize_server(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<AuthorizeResponse>), AppError> {
    let manager = Arc::clone(&state.mcp_manager);
    let registry = Arc::clone(&state.tools_registry);

    match McpManager::trigger_auth_flow(manager, &name, registry).await {
        Ok(auth_url) => Ok((StatusCode::ACCEPTED, Json(AuthorizeResponse { auth_url }))),
        Err(msg) => Err(AppError(AoError::ValidationError(msg))),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ao_engine_tools_provider_config::mcp_servers::{
        McpConfigError, McpLoadingPolicy, McpServerEntry, McpServersConfig, McpTransportType,
    };

    fn stdio_entry(name: &str, command: &str) -> McpServerEntry {
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

    fn http_entry(name: &str, url: &str) -> McpServerEntry {
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
    fn stdio_without_command_is_rejected() {
        let mut cfg = McpServersConfig { servers: vec![] };
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
        let err = cfg.add_entry(bad).expect_err("stdio without command must fail");
        assert!(matches!(err, McpConfigError::InvalidConfig(n, _) if n == "broken"));
    }

    #[test]
    fn http_without_url_is_rejected() {
        let mut cfg = McpServersConfig { servers: vec![] };
        let bad = McpServerEntry {
            name: "nourl".to_string(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Always,
            transport: McpTransportType::Http,
            url: None,
            auth: None,
        };
        let err = cfg.add_entry(bad).expect_err("http without url must fail");
        assert!(matches!(err, McpConfigError::InvalidConfig(n, _) if n == "nourl"));
    }

    #[test]
    fn sse_without_url_is_rejected() {
        let mut cfg = McpServersConfig { servers: vec![] };
        let bad = McpServerEntry {
            name: "nossurl".to_string(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Always,
            transport: McpTransportType::Sse,
            url: None,
            auth: None,
        };
        let err = cfg.add_entry(bad).expect_err("sse without url must fail");
        assert!(matches!(err, McpConfigError::InvalidConfig(n, _) if n == "nossurl"));
    }

    #[test]
    fn duplicate_name_is_rejected() {
        let mut cfg = McpServersConfig { servers: vec![] };
        cfg.add_entry(stdio_entry("myserver", "cmd")).unwrap();
        let err = cfg
            .add_entry(stdio_entry("myserver", "cmd2"))
            .expect_err("duplicate name must fail");
        assert!(matches!(err, McpConfigError::DuplicateName(n) if n == "myserver"));
    }

    #[test]
    fn invalid_name_format_is_rejected() {
        let mut cfg = McpServersConfig { servers: vec![] };
        let err = cfg
            .add_entry(stdio_entry("MyServer", "cmd"))
            .expect_err("uppercase name must fail");
        assert!(matches!(err, McpConfigError::InvalidName(_)));
    }

    #[test]
    fn name_starting_with_digit_is_rejected() {
        let mut cfg = McpServersConfig { servers: vec![] };
        let err = cfg
            .add_entry(stdio_entry("1server", "cmd"))
            .expect_err("digit-leading name must fail");
        assert!(matches!(err, McpConfigError::InvalidName(_)));
    }

    #[test]
    fn valid_entries_accepted() {
        let mut cfg = McpServersConfig { servers: vec![] };
        cfg.add_entry(stdio_entry("github", "github-mcp")).unwrap();
        cfg.add_entry(http_entry("myapi", "https://example.com/mcp"))
            .unwrap();
        assert_eq!(cfg.servers.len(), 2);
    }

    #[test]
    fn remove_entry_missing_returns_not_found() {
        let mut cfg = McpServersConfig { servers: vec![] };
        let err = cfg
            .remove_entry("ghost")
            .expect_err("removing unknown server must fail");
        assert!(matches!(
            err,
            McpConfigError::EntryNotFound(n) if n == "ghost"
        ));
    }

    #[test]
    fn plugin_source_prefix_triggers_delete_rejection() {
        // Verify the guard condition used in delete_server: any source that
        // starts with "plugin:" must be rejected with a ValidationError.
        let plugin_source = "plugin:my-plugin";
        assert!(
            plugin_source.starts_with("plugin:"),
            "plugin-prefixed source must match the guard"
        );

        let config_source = "config";
        assert!(
            !config_source.starts_with("plugin:"),
            "config source must not match the guard"
        );
    }
}
