//! [`McpManager`] — owns all spawned MCP server subprocesses for the process
//! lifetime and registers their tools into the shared [`Registry`].
//!
//! Construction flow expected by callers:
//!
//! ```ignore
//! let cfg = McpServersConfig::load().unwrap_or_default();
//! let manager = McpManager::from_config(&cfg).await;
//! let manager = manager.register_into(&mut registry).await;
//! let manager = Arc::new(manager);
//! // hold `manager` alive for the process lifetime
//! ```

use std::sync::{Arc, OnceLock, Weak};

use ao_engine_tools_core::{
    policy::LoadPolicy,
    skill_registry::{
        ContextMode, SkillEntry, SkillProvenance, SkillRecord, SkillRegistry, SkillSource,
    },
    tool::IoTool,
    Registry,
};
use ao_engine_tools_provider_config::mcp_servers::{
    McpAuthConfig, McpLoadingPolicy, McpServerEntry, McpServersConfig, McpTransportType,
};
use ao_engine_tools_provider_config::mcp_token_store::{McpTokenStore, derive_server_key};
use futures_util::future::join_all;
use serde::Serialize;
use thiserror::Error;
use tracing::{info, warn};

use super::{
    adapter::McpToolAdapter,
    client::{McpClientHandle, McpError},
    list_resources::ListMcpResources,
    oauth_flow::{OAuthEngine, OAuthError},
    read_resource::ReadMcpResource,
    schema_fetch::{fetch_prompts, fetch_tools},
    server_auth::McpServerAuthTool,
};

// ── Public status types ───────────────────────────────────────────────────────

/// Connection state for a single MCP server.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    Connected,
    NeedsAuth,
    Error,
    Disabled,
}

/// Snapshot of one MCP server's status, suitable for serialization into a UI
/// connectors list.
#[derive(Debug, Clone, Serialize)]
pub struct McpServerStatus {
    pub name: String,
    /// Transport protocol: `"stdio"`, `"http"`, or `"sse"`.
    pub transport: String,
    /// For http/sse: the endpoint URL. For stdio: the command and args summary.
    pub endpoint: String,
    pub state: McpServerState,
    /// Set when `state` is [`McpServerState::Error`].
    pub error: Option<String>,
    /// Raw tool names advertised by the server. Empty for non-connected states.
    pub tool_names: Vec<String>,
    /// Origin of this server entry. Always `"config"` for servers loaded from
    /// `mcp_servers.toml`; future plugin-sourced servers will use `"plugin:<name>"`.
    pub source: String,
}

/// Error returned by [`McpManager::add_server`] and [`McpManager::remove_server`].
#[derive(Debug, Error)]
pub enum McpManagerError {
    /// A server with this name is already tracked by the manager.
    #[error("MCP server {0:?} is already registered")]
    DuplicateName(String),

    /// No server with this name was found in any tracking list.
    #[error("MCP server {0:?} not found")]
    NotFound(String),

    /// The transport connection attempt failed.
    #[error("MCP server connection failed: {0}")]
    ConnectionFailed(String),
}

// ── Internal ──────────────────────────────────────────────────────────────────

struct McpClientEntry {
    handle: McpClientHandle,
    loading: McpLoadingPolicy,
    transport: String,
    endpoint: String,
    tool_names: Vec<String>,
    source: String,
}

/// An MCP server that could not be connected during startup because it
/// requires OAuth authorization.
struct NeedsAuthEntry {
    name: String,
    url: String,
    auth_config: McpAuthConfig,
    loading: McpLoadingPolicy,
    transport: String,
    source: String,
}

struct FailedEntry {
    name: String,
    transport: String,
    endpoint: String,
    error: String,
    source: String,
    /// Retained so a server can be promoted straight to `clients` (with the
    /// right tool `LoadPolicy`) if a reauthorization attempt reconnects it —
    /// see [`McpManager::complete_authorization`].
    loading: McpLoadingPolicy,
}

struct DisabledEntry {
    name: String,
    transport: String,
    endpoint: String,
    source: String,
}

fn transport_to_str(t: &McpTransportType) -> String {
    match t {
        McpTransportType::Stdio => "stdio".to_string(),
        McpTransportType::Http => "http".to_string(),
        McpTransportType::Sse => "sse".to_string(),
    }
}

fn make_endpoint(
    t: &McpTransportType,
    command: &Option<String>,
    args: &[String],
    url: &Option<String>,
) -> String {
    match t {
        McpTransportType::Stdio => {
            let cmd = command.as_deref().unwrap_or("");
            if args.is_empty() {
                cmd.to_string()
            } else {
                format!("{} {}", cmd, args.join(" "))
            }
        }
        McpTransportType::Http | McpTransportType::Sse => {
            url.as_deref().unwrap_or("").to_string()
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Owns all spawned MCP server subprocesses and provides process-lifetime
/// registration and shutdown.
///
/// Stored as `Arc<McpManager>` in `AppState`. Wrap in `Arc` only after
/// `register_into` completes — the returned `McpManager` from `register_into`
/// is the value to Arc-wrap.
pub struct McpManager {
    // `Mutex` makes McpManager `Send + Sync` regardless of whether `McpClientHandle`
    // is `Sync`, since `Mutex<T>: Send + Sync` whenever `T: Send`.
    clients: tokio::sync::Mutex<Vec<McpClientEntry>>,
    /// Skill records built from MCP server prompts during `register_into`.
    /// Populated once at startup; read by `extend_skill_registry` per session.
    prompt_skills: std::sync::Mutex<Vec<SkillRecord>>,
    /// Servers that returned HTTP 401 during startup and need OAuth authorization.
    needs_auth: std::sync::Mutex<Vec<NeedsAuthEntry>>,
    /// Token store used for auth-aware connections. `None` when auth is not configured.
    token_store: Option<Arc<McpTokenStore>>,
    /// Servers that failed to connect during startup (connection error or handshake
    /// failure). Retained so `server_statuses` can report them with state="error".
    failed: std::sync::Mutex<Vec<FailedEntry>>,
    /// Servers explicitly disabled in config. Never spawned; reported with
    /// state="disabled" in `server_statuses`. Wrapped in `Mutex` so
    /// `remove_server` can purge entries through a shared reference.
    disabled: std::sync::Mutex<Vec<DisabledEntry>>,
    /// Weak self-reference, populated by [`attach_self_reference`] immediately
    /// after the manager is wrapped in `Arc`. Auth pseudo-tools registered into
    /// the shared registry hold a clone of this same cell, so once it is set
    /// they can upgrade it to call back into the manager and update connection
    /// state when an OAuth flow completes. Empty until `attach_self_reference`
    /// runs (e.g. in unit tests that never Arc-wrap the manager).
    ///
    /// [`attach_self_reference`]: McpManager::attach_self_reference
    self_ref: Arc<OnceLock<Weak<McpManager>>>,
}

impl McpManager {
    /// Spawn all non-disabled servers in `config` concurrently with failure
    /// isolation: a server that fails to spawn or complete the MCP handshake
    /// logs a `warn` line and is retained as a failed entry; the remaining
    /// servers are unaffected.
    ///
    /// Servers with [`McpLoadingPolicy::Disabled`] are skipped (logged at
    /// `info`) and retained as disabled entries for status reporting.
    pub async fn from_config(config: &McpServersConfig) -> Self {
        enum Outcome {
            Connected(McpClientEntry),
            Failed(FailedEntry),
            Disabled(DisabledEntry),
        }

        let futs: Vec<_> = config
            .servers
            .iter()
            .map(|entry| {
                let name = entry.name.clone();
                let transport_label = transport_to_str(&entry.transport);
                let endpoint_label =
                    make_endpoint(&entry.transport, &entry.command, &entry.args, &entry.url);
                let transport = entry.transport.clone();
                let command = entry.command.clone();
                let args = entry.args.clone();
                let env = entry.env.clone();
                let url = entry.url.clone();
                let loading = entry.loading.clone();
                async move {
                    if loading == McpLoadingPolicy::Disabled {
                        info!(mcp_server = %name, "MCP server disabled; skipping spawn");
                        return Outcome::Disabled(DisabledEntry {
                            name,
                            transport: transport_label,
                            endpoint: endpoint_label,
                            source: "config".to_string(),
                        });
                    }
                    let result = match transport {
                        McpTransportType::Stdio => {
                            let cmd = command.unwrap_or_default();
                            McpClientHandle::spawn(&name, &cmd, &args, &env).await
                        }
                        McpTransportType::Http | McpTransportType::Sse => {
                            let endpoint = url.unwrap_or_default();
                            McpClientHandle::connect_http(&name, &endpoint).await
                        }
                    };
                    match result {
                        Ok(handle) => Outcome::Connected(McpClientEntry {
                            handle,
                            loading,
                            transport: transport_label,
                            endpoint: endpoint_label,
                            tool_names: Vec::new(),
                            source: "config".to_string(),
                        }),
                        Err(e) => {
                            warn!(mcp_server = %name, "failed to connect MCP server: {e}");
                            Outcome::Failed(FailedEntry {
                                name,
                                transport: transport_label,
                                endpoint: endpoint_label,
                                error: e.to_string(),
                                source: "config".to_string(),
                                loading,
                            })
                        }
                    }
                }
            })
            .collect();

        let outcomes: Vec<Outcome> = join_all(futs).await;
        let mut entries = Vec::new();
        let mut failed_entries = Vec::new();
        let mut disabled_entries = Vec::new();

        for outcome in outcomes {
            match outcome {
                Outcome::Connected(e) => entries.push(e),
                Outcome::Failed(f) => failed_entries.push(f),
                Outcome::Disabled(d) => disabled_entries.push(d),
            }
        }

        McpManager {
            clients: tokio::sync::Mutex::new(entries),
            prompt_skills: std::sync::Mutex::new(Vec::new()),
            needs_auth: std::sync::Mutex::new(Vec::new()),
            token_store: None,
            failed: std::sync::Mutex::new(failed_entries),
            disabled: std::sync::Mutex::new(disabled_entries),
            self_ref: Arc::new(OnceLock::new()),
        }
    }

    /// Spawn all non-disabled servers in `config` concurrently, using the
    /// provided `token_store` for bearer-token authentication on HTTP/SSE servers.
    ///
    /// For each HTTP/SSE server:
    /// - If a non-expired token is stored, connects with `Authorization: Bearer` header.
    /// - If no token is stored, attempts an unauthenticated connection.
    /// - If the server returns HTTP 401 (`McpError::AuthRequired`), records it in
    ///   `needs_auth` so `register_into` can inject an auth pseudo-tool.
    ///
    /// Stdio servers behave identically to [`from_config`].
    pub async fn from_config_auth(
        config: &McpServersConfig,
        token_store: Arc<McpTokenStore>,
    ) -> Self {
        enum Outcome {
            Connected(McpClientEntry),
            NeedsAuth(NeedsAuthEntry),
            Failed(FailedEntry),
            Disabled(DisabledEntry),
        }

        let futs: Vec<_> = config
            .servers
            .iter()
            .map(|entry| {
                let name = entry.name.clone();
                let transport_label = transport_to_str(&entry.transport);
                let endpoint_label =
                    make_endpoint(&entry.transport, &entry.command, &entry.args, &entry.url);
                let transport = entry.transport.clone();
                let command = entry.command.clone();
                let args = entry.args.clone();
                let env = entry.env.clone();
                let url = entry.url.clone();
                let loading = entry.loading.clone();
                let auth_config = entry.auth.clone().unwrap_or_default();
                let ts = Arc::clone(&token_store);
                async move {
                    if loading == McpLoadingPolicy::Disabled {
                        info!(mcp_server = %name, "MCP server disabled; skipping spawn");
                        return Outcome::Disabled(DisabledEntry {
                            name,
                            transport: transport_label,
                            endpoint: endpoint_label,
                            source: "config".to_string(),
                        });
                    }
                    let result = match transport {
                        McpTransportType::Stdio => {
                            let cmd = command.unwrap_or_default();
                            McpClientHandle::spawn(&name, &cmd, &args, &env).await
                        }
                        McpTransportType::Http | McpTransportType::Sse => {
                            let endpoint = url.clone().unwrap_or_default();
                            let server_key = derive_server_key(&name, Some(&endpoint), "http");

                            // Prefer a stored credential, transparently refreshing an
                            // expired/expiring access token via its refresh token before
                            // falling back to an anonymous connect (which triggers auth).
                            let engine = OAuthEngine::new(reqwest::Client::new());
                            let connect_result = match engine.current_access_token(&server_key, &ts).await {
                                Ok(Some(token)) => {
                                    McpClientHandle::connect_http_with_bearer(
                                        &name, &endpoint, &token,
                                    )
                                    .await
                                }
                                Ok(None) => {
                                    info!(
                                        mcp_server = %name,
                                        "no stored OAuth credential found; connecting unauthenticated"
                                    );
                                    McpClientHandle::connect_http(&name, &endpoint).await
                                }
                                Err(OAuthError::GrantRevoked { description }) => {
                                    warn!(
                                        mcp_server = %name,
                                        "OAuth grant revoked ({description}) — user must re-authorize this \
                                         connector; connecting without bearer for now"
                                    );
                                    McpClientHandle::connect_http(&name, &endpoint).await
                                }
                                Err(e) => {
                                    warn!(
                                        mcp_server = %name,
                                        "stored token refresh failed ({e}); connecting without bearer"
                                    );
                                    McpClientHandle::connect_http(&name, &endpoint).await
                                }
                            };

                            // Attach the token source regardless of whether a credential
                            // was available just now — a credential completed later (the
                            // user finishes OAuth while this handle is already live) must
                            // be reachable by a future `reconnect()` without a restart.
                            if let Ok(ref handle) = connect_result {
                                handle.attach_http_token_source(Arc::clone(&ts), server_key.clone());
                            }

                            connect_result
                        }
                    };
                    match result {
                        Ok(handle) => Outcome::Connected(McpClientEntry {
                            handle,
                            loading,
                            transport: transport_label,
                            endpoint: endpoint_label,
                            tool_names: Vec::new(),
                            source: "config".to_string(),
                        }),
                        Err(McpError::AuthRequired) => {
                            let endpoint = url.unwrap_or_default();
                            info!(
                                mcp_server = %name,
                                "MCP server requires authorization; registering auth pseudo-tool"
                            );
                            Outcome::NeedsAuth(NeedsAuthEntry {
                                name,
                                url: endpoint,
                                auth_config,
                                loading,
                                transport: transport_label,
                                source: "config".to_string(),
                            })
                        }
                        Err(e) => {
                            warn!(mcp_server = %name, "failed to connect MCP server: {e}");
                            Outcome::Failed(FailedEntry {
                                name,
                                transport: transport_label,
                                endpoint: endpoint_label,
                                error: e.to_string(),
                                source: "config".to_string(),
                                loading,
                            })
                        }
                    }
                }
            })
            .collect();

        let outcomes: Vec<Outcome> = join_all(futs).await;
        let mut entries = Vec::new();
        let mut needs_auth_entries = Vec::new();
        let mut failed_entries = Vec::new();
        let mut disabled_entries = Vec::new();

        for outcome in outcomes {
            match outcome {
                Outcome::Connected(e) => entries.push(e),
                Outcome::NeedsAuth(a) => needs_auth_entries.push(a),
                Outcome::Failed(f) => failed_entries.push(f),
                Outcome::Disabled(d) => disabled_entries.push(d),
            }
        }

        McpManager {
            clients: tokio::sync::Mutex::new(entries),
            prompt_skills: std::sync::Mutex::new(Vec::new()),
            needs_auth: std::sync::Mutex::new(needs_auth_entries),
            token_store: Some(token_store),
            failed: std::sync::Mutex::new(failed_entries),
            disabled: std::sync::Mutex::new(disabled_entries),
            self_ref: Arc::new(OnceLock::new()),
        }
    }

    /// Fetch the tool list from every live server and register each tool as a
    /// [`McpToolAdapter`] in `registry`.
    ///
    /// Returns `self` so the caller can chain and retain process-lifetime
    /// ownership of the subprocesses.
    ///
    /// Failures per server (e.g. `tools/list` returning an error) are logged as
    /// warnings; other servers' tools are still registered.
    pub async fn register_into(self, registry: &mut Registry) -> Self {
        let mut clients = self.clients.into_inner();
        let mut prompt_skill_records: Vec<SkillRecord> = Vec::new();
        let mut tool_names_by_idx: Vec<Vec<String>> = vec![Vec::new(); clients.len()];

        for (idx, entry) in clients.iter().enumerate() {
            let server_name = entry.handle.name().to_string();
            let loading_policy = match entry.loading {
                McpLoadingPolicy::Always => LoadPolicy::AlwaysLoad,
                McpLoadingPolicy::Deferred => LoadPolicy::Deferred,
                McpLoadingPolicy::Disabled => continue,
            };

            match fetch_tools(&entry.handle).await {
                Ok(descriptors) => {
                    // Record raw tool names before consuming the descriptor vec.
                    tool_names_by_idx[idx] =
                        descriptors.iter().map(|d| d.raw_name.clone()).collect();

                    for desc in descriptors {
                        let effective_policy = if desc.always_load {
                            LoadPolicy::AlwaysLoad
                        } else {
                            loading_policy
                        };
                        let adapter = McpToolAdapter::new(
                            &server_name,
                            desc.raw_name,
                            desc.description,
                            desc.input_schema,
                            entry.handle.clone(),
                            effective_policy,
                            desc.annotations,
                            desc.search_hint,
                        );
                        let qualified = adapter.name().to_string();
                        if registry.lookup(&qualified).is_some() {
                            warn!(
                                mcp_tool = %qualified,
                                "MCP tool qualified name collides with existing registration; skipping"
                            );
                        } else {
                            registry.register_io(Arc::new(adapter));
                        }
                    }
                }
                Err(e) => {
                    warn!(mcp_server = %server_name, "fetch_tools failed: {e}");
                }
            }

            match fetch_prompts(&entry.handle).await {
                Ok(prompt_descs) => {
                    for desc in prompt_descs {
                        let record = SkillRecord {
                            name: desc.raw_name,
                            description: desc.description,
                            context: ContextMode::Inline,
                            agent: None,
                            allowed_tools: Vec::new(),
                            arguments: Vec::new(),
                            body: desc.body,
                            source: SkillSource::Mcp { server_name: server_name.clone() },
                            when_to_use: None,
                            model: None,
                            disable_model_invocation: false,
                            provenance: SkillProvenance::UserAuthored,
                            retired: false,
                            retired_reason: None,
                            superseded_by: None,
                            distilled_from: Vec::new(),
                            version: 1,
                        };
                        prompt_skill_records.push(record);
                    }
                }
                Err(e) => {
                    warn!(mcp_server = %server_name, "fetch_prompts failed: {e}");
                }
            }
        }

        // Apply collected tool names to each entry.
        for (entry, names) in clients.iter_mut().zip(tool_names_by_idx) {
            entry.tool_names = names;
        }

        // Register the built-in resource tools once, shared across all servers.
        if !clients.is_empty() {
            let server_handles: std::sync::Arc<Vec<(String, McpClientHandle)>> =
                std::sync::Arc::new(
                    clients
                        .iter()
                        .map(|e| (e.handle.name().to_string(), e.handle.clone()))
                        .collect(),
                );
            registry.register_io(std::sync::Arc::new(ListMcpResources::new(
                std::sync::Arc::clone(&server_handles),
            )));
            registry.register_io(std::sync::Arc::new(ReadMcpResource::new(
                std::sync::Arc::clone(&server_handles),
            )));
        }

        // Inject auth pseudo-tools for servers that need authorization.
        let needs_auth_vec = self.needs_auth.into_inner().unwrap_or_default();
        if let Some(ref ts) = self.token_store {
            for auth_entry in &needs_auth_vec {
                let loading_policy = match auth_entry.loading {
                    McpLoadingPolicy::Always => LoadPolicy::AlwaysLoad,
                    McpLoadingPolicy::Deferred => LoadPolicy::Deferred,
                    McpLoadingPolicy::Disabled => continue,
                };
                let tool = McpServerAuthTool::new(
                    &auth_entry.name,
                    &auth_entry.url,
                    auth_entry.auth_config.clone(),
                    Arc::clone(ts),
                    loading_policy,
                    Arc::clone(&self.self_ref),
                );
                registry.register_io_dynamic(Arc::new(tool));
            }
        }

        McpManager {
            clients: tokio::sync::Mutex::new(clients),
            prompt_skills: std::sync::Mutex::new(prompt_skill_records),
            needs_auth: std::sync::Mutex::new(needs_auth_vec),
            token_store: self.token_store,
            failed: self.failed,
            disabled: self.disabled,
            self_ref: self.self_ref,
        }
    }

    /// Connect and register a new MCP server at runtime, without restarting
    /// the server process.
    ///
    /// Behavior by outcome:
    /// - **Success**: tools are fetched and inserted into `registry` as dynamic
    ///   tools (visible on the next model turn). The server appears as
    ///   [`McpServerState::Connected`] in subsequent [`server_statuses`] calls.
    /// - **AuthRequired**: the `mcp__<name>__authorize` pseudo-tool is inserted
    ///   into `registry` (if a token store is configured) and the server is
    ///   tracked as [`McpServerState::NeedsAuth`].
    /// - **Other error**: returns [`McpManagerError::ConnectionFailed`]; no
    ///   state change is recorded.
    ///
    /// Returns [`McpManagerError::DuplicateName`] if a server with `entry.name`
    /// is already tracked in any list (connected, needs-auth, failed, or disabled).
    ///
    /// # Registry semantics
    ///
    /// `registry` must be the same `Arc<Registry>` that the live server's sessions
    /// read (i.e. `AppState.tools_registry`). Tools are inserted into the
    /// runtime-dynamic slot, which is visible through shared references without
    /// a restart.
    /// `source` identifies the origin of this server entry — `"config"` for
    /// user-configured entries and `"plugin:<name>"` for entries that a plugin
    /// provides. The value is stored and reflected in [`McpServerStatus::source`].
    pub async fn add_server(
        &self,
        entry: McpServerEntry,
        registry: Arc<Registry>,
        source: String,
    ) -> Result<McpServerStatus, McpManagerError> {
        let name = entry.name.clone();

        // Duplicate check across all tracking lists.
        {
            let clients = self.clients.lock().await;
            if clients.iter().any(|e| e.handle.name() == name) {
                return Err(McpManagerError::DuplicateName(name));
            }
        }
        {
            let needs_auth = self.needs_auth.lock().unwrap_or_else(|p| p.into_inner());
            if needs_auth.iter().any(|e| e.name == name) {
                return Err(McpManagerError::DuplicateName(name));
            }
        }
        {
            let failed = self.failed.lock().unwrap_or_else(|p| p.into_inner());
            if failed.iter().any(|e| e.name == name) {
                return Err(McpManagerError::DuplicateName(name));
            }
        }
        {
            let disabled = self.disabled.lock().unwrap_or_else(|p| p.into_inner());
            if disabled.iter().any(|e| e.name == name) {
                return Err(McpManagerError::DuplicateName(name));
            }
        }

        let transport_label = transport_to_str(&entry.transport);
        let endpoint_label =
            make_endpoint(&entry.transport, &entry.command, &entry.args, &entry.url);

        // Disabled loading policy — track but do not connect.
        if entry.loading == McpLoadingPolicy::Disabled {
            info!(mcp_server = %name, "add_server: disabled loading policy; tracking without connecting");
            let status = McpServerStatus {
                name: name.clone(),
                transport: transport_label.clone(),
                endpoint: endpoint_label.clone(),
                state: McpServerState::Disabled,
                error: None,
                tool_names: vec![],
                source: source.clone(),
            };
            self.disabled
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(DisabledEntry {
                    name,
                    transport: transport_label,
                    endpoint: endpoint_label,
                    source,
                });
            return Ok(status);
        }

        // Attempt connection.
        let connect_result = match entry.transport {
            McpTransportType::Stdio => {
                let cmd = entry.command.clone().unwrap_or_default();
                McpClientHandle::spawn(&name, &cmd, &entry.args, &entry.env).await
            }
            McpTransportType::Http | McpTransportType::Sse => {
                let endpoint = entry.url.clone().unwrap_or_default();
                if let Some(ref ts) = self.token_store {
                    let server_key = derive_server_key(&name, Some(&endpoint), "http");
                    let engine = OAuthEngine::new(reqwest::Client::new());
                    let connect_result = match engine.current_access_token(&server_key, ts).await {
                        Ok(Some(token)) => {
                            McpClientHandle::connect_http_with_bearer(&name, &endpoint, &token).await
                        }
                        Ok(None) => {
                            info!(
                                mcp_server = %name,
                                "no stored OAuth credential found; connecting unauthenticated"
                            );
                            McpClientHandle::connect_http(&name, &endpoint).await
                        }
                        Err(OAuthError::GrantRevoked { description }) => {
                            warn!(
                                mcp_server = %name,
                                "OAuth grant revoked ({description}) — user must re-authorize this \
                                 connector; connecting without bearer for now"
                            );
                            McpClientHandle::connect_http(&name, &endpoint).await
                        }
                        Err(e) => {
                            warn!(
                                mcp_server = %name,
                                "stored token refresh failed ({e}); connecting without bearer"
                            );
                            McpClientHandle::connect_http(&name, &endpoint).await
                        }
                    };

                    // See the equivalent comment in `from_config_auth`: attach
                    // regardless of outcome so a later credential is reachable by
                    // a future `reconnect()` without a restart.
                    if let Ok(ref handle) = connect_result {
                        handle.attach_http_token_source(Arc::clone(ts), server_key.clone());
                    }

                    connect_result
                } else {
                    McpClientHandle::connect_http(&name, &endpoint).await
                }
            }
        };

        let loading_policy = match entry.loading {
            McpLoadingPolicy::Always => LoadPolicy::AlwaysLoad,
            McpLoadingPolicy::Deferred => LoadPolicy::Deferred,
            McpLoadingPolicy::Disabled => unreachable!("handled above"),
        };

        match connect_result {
            Ok(handle) => {
                let mut registered_names: Vec<String> = Vec::new();

                match fetch_tools(&handle).await {
                    Ok(descriptors) => {
                        registered_names =
                            descriptors.iter().map(|d| d.raw_name.clone()).collect();
                        for desc in descriptors {
                            let effective = if desc.always_load {
                                LoadPolicy::AlwaysLoad
                            } else {
                                loading_policy
                            };
                            let adapter = McpToolAdapter::new(
                                &name,
                                desc.raw_name,
                                desc.description,
                                desc.input_schema,
                                handle.clone(),
                                effective,
                                desc.annotations,
                                desc.search_hint,
                            );
                            let qualified = adapter.name().to_string();
                            if registry.lookup(&qualified).is_some() {
                                warn!(
                                    mcp_tool = %qualified,
                                    "runtime add: tool name collides with existing registration; skipping"
                                );
                            } else {
                                registry.register_io_dynamic(Arc::new(adapter));
                            }
                        }
                    }
                    Err(e) => {
                        warn!(mcp_server = %name, "runtime add: fetch_tools failed: {e}");
                    }
                }

                let status = McpServerStatus {
                    name: name.clone(),
                    transport: transport_label.clone(),
                    endpoint: endpoint_label.clone(),
                    state: McpServerState::Connected,
                    error: None,
                    tool_names: registered_names.clone(),
                    source: source.clone(),
                };

                self.clients.lock().await.push(McpClientEntry {
                    handle,
                    loading: entry.loading,
                    transport: transport_label,
                    endpoint: endpoint_label,
                    tool_names: registered_names,
                    source,
                });

                Ok(status)
            }

            Err(McpError::AuthRequired) => {
                let url = entry.url.clone().unwrap_or_default();
                let auth_config = entry.auth.clone().unwrap_or_default();

                if let Some(ref ts) = self.token_store {
                    let tool = McpServerAuthTool::new(
                        &name,
                        &url,
                        auth_config.clone(),
                        Arc::clone(ts),
                        loading_policy,
                        Arc::clone(&self.self_ref),
                    );
                    registry.register_io_dynamic(Arc::new(tool));
                } else {
                    warn!(
                        mcp_server = %name,
                        "add_server: server requires OAuth but no token store is configured; \
                         auth pseudo-tool will not be registered"
                    );
                }

                info!(
                    mcp_server = %name,
                    "add_server: server requires authorization; tracking as NeedsAuth"
                );

                let status = McpServerStatus {
                    name: name.clone(),
                    transport: transport_label.clone(),
                    endpoint: url.clone(),
                    state: McpServerState::NeedsAuth,
                    error: None,
                    tool_names: vec![],
                    source: source.clone(),
                };

                self.needs_auth
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(NeedsAuthEntry {
                        name,
                        url,
                        auth_config,
                        loading: entry.loading,
                        transport: transport_label,
                        source,
                    });

                Ok(status)
            }

            Err(e) => {
                warn!(mcp_server = %name, "runtime add: connection failed: {e}");
                Err(McpManagerError::ConnectionFailed(e.to_string()))
            }
        }
    }

    /// Shut down an MCP server and remove its tools from `registry`.
    ///
    /// The server is identified by `name` and is looked up across all tracking
    /// lists (connected, needs-auth, failed, disabled). If found in multiple
    /// lists (which should not happen in normal operation) all occurrences are
    /// removed.
    ///
    /// Dynamic tools whose names match the `mcp__<name>__` prefix are removed
    /// from `registry`. Static tools (registered via [`Registry::register_io`])
    /// are never touched.
    ///
    /// Returns [`McpManagerError::NotFound`] if the name is not tracked in any list.
    pub async fn remove_server(
        &self,
        name: &str,
        registry: &Registry,
    ) -> Result<(), McpManagerError> {
        let mut any_found = false;

        // Extract the client handle (if any) after releasing the async lock so we
        // can call `shutdown()` outside the critical section.
        let handle_to_shutdown = {
            let mut clients = self.clients.lock().await;
            let idx = clients.iter().position(|e| e.handle.name() == name);
            idx.map(|i| {
                any_found = true;
                clients.remove(i).handle
            })
        };

        {
            let mut needs_auth = self.needs_auth.lock().unwrap_or_else(|p| p.into_inner());
            let before = needs_auth.len();
            needs_auth.retain(|e| e.name != name);
            if needs_auth.len() < before {
                any_found = true;
            }
        }

        {
            let mut failed = self.failed.lock().unwrap_or_else(|p| p.into_inner());
            let before = failed.len();
            failed.retain(|e| e.name != name);
            if failed.len() < before {
                any_found = true;
            }
        }

        {
            let mut disabled = self.disabled.lock().unwrap_or_else(|p| p.into_inner());
            let before = disabled.len();
            disabled.retain(|e| e.name != name);
            if disabled.len() < before {
                any_found = true;
            }
        }

        if !any_found {
            return Err(McpManagerError::NotFound(name.to_string()));
        }

        // Graceful shutdown after all sync locks are released.
        if let Some(handle) = handle_to_shutdown {
            handle.shutdown().await;
        }

        // Remove all dynamic tools for this server from the registry.
        let prefix = format!("mcp__{name}__");
        registry.remove_by_prefix(&prefix);

        Ok(())
    }

    /// Returns a snapshot of every configured server's current status.
    ///
    /// The `source` field reflects how the entry was added: `"config"` for
    /// servers loaded from `mcp_servers.toml` at startup, `"plugin:<name>"`
    /// for servers added by an installed plugin.
    pub async fn server_statuses(&self) -> Vec<McpServerStatus> {
        let mut statuses = Vec::new();

        {
            let clients = self.clients.lock().await;
            for entry in clients.iter() {
                statuses.push(McpServerStatus {
                    name: entry.handle.name().to_string(),
                    transport: entry.transport.clone(),
                    endpoint: entry.endpoint.clone(),
                    state: McpServerState::Connected,
                    error: None,
                    tool_names: entry.tool_names.clone(),
                    source: entry.source.clone(),
                });
            }
        }

        {
            let needs_auth = self.needs_auth.lock().unwrap_or_else(|p| p.into_inner());
            for entry in needs_auth.iter() {
                statuses.push(McpServerStatus {
                    name: entry.name.clone(),
                    transport: entry.transport.clone(),
                    endpoint: entry.url.clone(),
                    state: McpServerState::NeedsAuth,
                    error: None,
                    tool_names: vec![],
                    source: entry.source.clone(),
                });
            }
        }

        {
            let failed = self.failed.lock().unwrap_or_else(|p| p.into_inner());
            for entry in failed.iter() {
                statuses.push(McpServerStatus {
                    name: entry.name.clone(),
                    transport: entry.transport.clone(),
                    endpoint: entry.endpoint.clone(),
                    state: McpServerState::Error,
                    error: Some(entry.error.clone()),
                    tool_names: vec![],
                    source: entry.source.clone(),
                });
            }
        }

        {
            let disabled = self.disabled.lock().unwrap_or_else(|p| p.into_inner());
            for entry in disabled.iter() {
                statuses.push(McpServerStatus {
                    name: entry.name.clone(),
                    transport: entry.transport.clone(),
                    endpoint: entry.endpoint.clone(),
                    state: McpServerState::Disabled,
                    error: None,
                    tool_names: vec![],
                    source: entry.source.clone(),
                });
            }
        }

        statuses
    }

    /// Return the source label for a tracked server, or `None` if no server
    /// with that name is registered.
    ///
    /// This is used to guard lifecycle operations: plugin-sourced servers
    /// (source starts with `"plugin:"`) must not be deleted through the
    /// user-facing config API.
    pub async fn server_source(&self, name: &str) -> Option<String> {
        {
            let clients = self.clients.lock().await;
            if let Some(e) = clients.iter().find(|e| e.handle.name() == name) {
                return Some(e.source.clone());
            }
        }
        {
            let needs_auth = self.needs_auth.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(e) = needs_auth.iter().find(|e| e.name == name) {
                return Some(e.source.clone());
            }
        }
        {
            let failed = self.failed.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(e) = failed.iter().find(|e| e.name == name) {
                return Some(e.source.clone());
            }
        }
        {
            let disabled = self.disabled.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(e) = disabled.iter().find(|e| e.name == name) {
                return Some(e.source.clone());
            }
        }
        None
    }

    /// Fetch a clone of the live client handle for a connected server, for
    /// callers that need to issue their own MCP calls outside the normal
    /// tool-registry dispatch path (e.g. the assignment poll loop calling
    /// `tools/call` directly against a connector on a timer).
    ///
    /// Returns `None` if the server isn't currently connected — covers
    /// unknown names, `needs_auth`, `failed`, and `disabled` states alike,
    /// since none of those have a live handle to hand back.
    pub async fn client_handle(&self, name: &str) -> Option<McpClientHandle> {
        let clients = self.clients.lock().await;
        clients
            .iter()
            .find(|e| e.handle.name() == name)
            .map(|e| e.handle.clone())
    }

    /// Install the weak self-reference used by auth pseudo-tools to call back
    /// into the manager when an OAuth flow completes.
    ///
    /// Must be called exactly once, immediately after the manager is wrapped in
    /// `Arc` (the value returned by [`register_into`]). Auth pseudo-tools that
    /// were registered during `register_into` share the same underlying cell, so
    /// setting it here makes the agent-driven `mcp__<name>__authorize` path able
    /// to promote a server to `Connected` — keeping the manager's status in sync
    /// with the in-app "Authorize" button path.
    ///
    /// Calling it more than once is a no-op (the cell is write-once).
    ///
    /// [`register_into`]: McpManager::register_into
    pub fn attach_self_reference(self: &Arc<Self>) {
        let _ = self.self_ref.set(Arc::downgrade(self));
    }

    /// Complete an OAuth authorization for `server_name`, wherever it currently
    /// lives: load the freshly persisted access token, reconnect to the
    /// server, and swap the registry's tool set for it — the exact state
    /// transition depends on where the server was found.
    ///
    /// - **`needs_auth`** — the original case this promotes into `clients` so
    ///   [`server_statuses`] reports [`McpServerState::Connected`], matching
    ///   the startup path.
    /// - **`clients` (already connected)** — a reauthorization of a working
    ///   server (token rotation, expired refresh token, wrong account). The
    ///   existing entry is updated in place: its old handle is shut down
    ///   after the new one takes its slot, so the state never leaves
    ///   `Connected` and the swap is invisible to [`server_statuses`] beyond
    ///   the refreshed tool set.
    /// - **`failed`** — a server whose last connection attempt errored is
    ///   promoted into `clients` on success, the same as `needs_auth`.
    ///
    /// This is the single completion path shared by every OAuth trigger — the
    /// in-app authorize/reauthorize actions (via [`trigger_auth_flow`]) and
    /// the agent-facing `mcp__<name>__authorize` pseudo-tool — keeping the
    /// manager's tracked state and the tools registry consistent regardless
    /// of which entry point initiated the flow.
    ///
    /// On any failure (server not tracked in any list, missing token store,
    /// token lookup/connect error) it logs and returns without mutating
    /// state — a failed reauthorization of an already-connected server never
    /// tears down its existing working connection. `tools/list` failing
    /// after a successful reconnect still completes the transition with an
    /// empty tool list, matching the startup path.
    ///
    /// [`server_statuses`]: McpManager::server_statuses
    /// [`trigger_auth_flow`]: McpManager::trigger_auth_flow
    pub async fn complete_authorization(&self, server_name: &str, registry: Arc<Registry>) {
        /// Which tracking list `server_name` was found on, captured before the
        /// reconnect so the post-reconnect transition can be applied without
        /// re-scanning (and without assuming the server is still there).
        enum PromotionSource {
            NeedsAuth { source: String },
            Connected,
            Failed { source: String },
        }

        let needs_auth_hit = {
            let guard = self.needs_auth.lock().unwrap_or_else(|p| p.into_inner());
            guard.iter().find(|e| e.name == server_name).map(|e| {
                (e.url.clone(), e.loading.clone(), PromotionSource::NeedsAuth { source: e.source.clone() })
            })
        };

        let (url, loading, promotion) = match needs_auth_hit {
            Some(hit) => hit,
            None => {
                let connected_hit = {
                    let clients = self.clients.lock().await;
                    clients
                        .iter()
                        .find(|e| e.handle.name() == server_name)
                        .map(|e| (e.endpoint.clone(), e.loading.clone()))
                };
                match connected_hit {
                    Some((url, loading)) => (url, loading, PromotionSource::Connected),
                    None => {
                        let failed_hit = {
                            let failed = self.failed.lock().unwrap_or_else(|p| p.into_inner());
                            failed.iter().find(|e| e.name == server_name).map(|e| {
                                (e.endpoint.clone(), e.loading.clone(), PromotionSource::Failed {
                                    source: e.source.clone(),
                                })
                            })
                        };
                        match failed_hit {
                            Some(hit) => hit,
                            None => {
                                warn!(
                                    server = %server_name,
                                    "post-auth: server is not tracked in any known state; skipping promotion"
                                );
                                return;
                            }
                        }
                    }
                }
            }
        };

        let token_store = match self.token_store.as_ref() {
            Some(ts) => Arc::clone(ts),
            None => {
                warn!(
                    server = %server_name,
                    "post-auth: no token store configured; cannot complete authorization"
                );
                return;
            }
        };

        let server_key = derive_server_key(server_name, Some(&url), "http");
        let engine = OAuthEngine::new(reqwest::Client::new());
        let token = match engine.current_access_token(&server_key, &token_store).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                warn!(server = %server_name, "post-auth: no access token found after OAuth flow");
                return;
            }
            Err(OAuthError::GrantRevoked { description }) => {
                warn!(
                    server = %server_name,
                    "post-auth: OAuth grant revoked immediately after authorizing ({description}) — \
                     user must re-authorize again"
                );
                return;
            }
            Err(e) => {
                warn!(server = %server_name, "post-auth: token lookup failed: {e}");
                return;
            }
        };

        let handle =
            match McpClientHandle::connect_http_with_bearer(server_name, &url, &token).await {
                Ok(h) => h,
                Err(e) => {
                    warn!(server = %server_name, "post-auth: reconnect failed: {e}");
                    return;
                }
            };
        handle.attach_http_token_source(Arc::clone(&token_store), server_key.clone());

        let loading_policy = match &loading {
            McpLoadingPolicy::Always => LoadPolicy::AlwaysLoad,
            McpLoadingPolicy::Deferred | McpLoadingPolicy::Disabled => LoadPolicy::Deferred,
        };

        // Swap the auth pseudo-tool (and any stale tools) for the real tool set.
        registry.remove_by_prefix(&format!("mcp__{server_name}__"));
        let mut tool_names: Vec<String> = Vec::new();
        match fetch_tools(&handle).await {
            Ok(descriptors) => {
                tool_names = descriptors.iter().map(|d| d.raw_name.clone()).collect();
                for desc in descriptors {
                    let effective = if desc.always_load {
                        LoadPolicy::AlwaysLoad
                    } else {
                        loading_policy
                    };
                    let adapter = McpToolAdapter::new(
                        server_name,
                        desc.raw_name,
                        desc.description,
                        desc.input_schema,
                        handle.clone(),
                        effective,
                        desc.annotations,
                        desc.search_hint,
                    );
                    registry.register_io_dynamic(Arc::new(adapter));
                }
            }
            Err(e) => {
                warn!(server = %server_name, "post-auth: fetch_tools failed: {e}");
            }
        }

        match promotion {
            // Move the server from `needs_auth` → `clients` so its status flips
            // to Connected. This is the step the agent-driven pseudo-tool path
            // was previously missing, which left the UI badge stuck on "Needs auth".
            PromotionSource::NeedsAuth { source } => {
                {
                    let mut guard = self.needs_auth.lock().unwrap_or_else(|p| p.into_inner());
                    guard.retain(|e| e.name != server_name);
                }
                self.clients.lock().await.push(McpClientEntry {
                    handle,
                    loading,
                    transport: "http".to_string(),
                    endpoint: url,
                    tool_names,
                    source,
                });
                info!(mcp_server = %server_name, "post-auth: server reconnected and promoted to Connected");
            }
            // Already connected — rotate the handle in place so the entry's
            // state, loading policy, and source never change; only the live
            // connection and its tool set do. The old handle is shut down
            // after being swapped out so the previous credential's session
            // closes cleanly.
            PromotionSource::Connected => {
                let old_handle = {
                    let mut clients = self.clients.lock().await;
                    clients.iter_mut().find(|e| e.handle.name() == server_name).map(|e| {
                        e.tool_names = tool_names;
                        std::mem::replace(&mut e.handle, handle)
                    })
                };
                if let Some(old) = old_handle {
                    old.shutdown().await;
                }
                info!(mcp_server = %server_name, "post-auth: server credential rotated and reconnected");
            }
            // Move the server from `failed` → `clients`, the same promotion
            // `needs_auth` gets — a server can fail with a stale/invalid
            // credential just as easily as it can start out unauthorized.
            PromotionSource::Failed { source } => {
                {
                    let mut guard = self.failed.lock().unwrap_or_else(|p| p.into_inner());
                    guard.retain(|e| e.name != server_name);
                }
                self.clients.lock().await.push(McpClientEntry {
                    handle,
                    loading,
                    transport: "http".to_string(),
                    endpoint: url,
                    tool_names,
                    source,
                });
                info!(mcp_server = %server_name, "post-auth: previously-failed server reconnected and promoted to Connected");
            }
        }
    }

    /// Resolve the connection URL and OAuth client configuration needed to
    /// (re)start an authorization flow for `name`, regardless of the server's
    /// current tracked state.
    ///
    /// A recovery action must not be gated behind the same health signal it
    /// exists to fix, so this is deliberately state-independent: token
    /// rotation, an expiring refresh token, or a wrong account connected are
    /// all legitimate reasons to reauthorize a server that already looks
    /// `connected`, and a server can just as easily need it after a failed
    /// reconnect attempt as while awaiting its first authorization.
    ///
    /// Checked in order: `needs_auth`, then `clients` (connected), then
    /// `failed`. `auth_config` for the latter two is re-read from the on-disk
    /// config by server name, since only `needs_auth` entries retain it in
    /// memory; a server absent from the config file (e.g. plugin-managed)
    /// falls back to [`McpAuthConfig::default()`]. A server on any list that
    /// uses the `stdio` transport is rejected — there is no browser-based
    /// auth flow for a local subprocess. Returns an error if `name` is not
    /// tracked in any of the three lists.
    async fn resolve_reauth_target(&self, name: &str) -> Result<(String, McpAuthConfig), String> {
        {
            let guard = self.needs_auth.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(e) = guard.iter().find(|e| e.name == name) {
                return Ok((e.url.clone(), e.auth_config.clone()));
            }
        }

        let non_http_transport_err = |transport: &str| {
            format!(
                "server {name:?} uses the {transport} transport, which has no browser-based authorization flow"
            )
        };
        let configured_auth = || -> McpAuthConfig {
            McpServersConfig::load()
                .ok()
                .and_then(|cfg| cfg.servers.into_iter().find(|e| e.name == name))
                .and_then(|e| e.auth)
                .unwrap_or_default()
        };

        {
            let clients = self.clients.lock().await;
            if let Some(e) = clients.iter().find(|e| e.handle.name() == name) {
                return if e.transport == "http" || e.transport == "sse" {
                    Ok((e.endpoint.clone(), configured_auth()))
                } else {
                    Err(non_http_transport_err(&e.transport))
                };
            }
        }

        {
            let failed = self.failed.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(e) = failed.iter().find(|e| e.name == name) {
                return if e.transport == "http" || e.transport == "sse" {
                    Ok((e.endpoint.clone(), configured_auth()))
                } else {
                    Err(non_http_transport_err(&e.transport))
                };
            }
        }

        Err(format!("server {name:?} is not a configured MCP server"))
    }

    /// Start (or restart) the OAuth authorization flow for a server.
    ///
    /// Works for a server in any tracked state — see
    /// [`resolve_reauth_target`] for the precedence and the stdio exclusion.
    /// Begins the OAuth PKCE flow, spawns a background task that waits for the
    /// browser callback, exchanges the authorization code for tokens, and
    /// reconnects the server — see [`complete_authorization`] for how the
    /// manager's tracked state is updated once the flow finishes. Returns the
    /// authorization URL so the caller can open a browser or pass it through
    /// to the user.
    ///
    /// Returns an error if `name` is not a tracked/configured MCP server, if
    /// it uses the `stdio` transport, if no token store is configured, or if
    /// the OAuth discovery/setup step fails.
    ///
    /// [`resolve_reauth_target`]: McpManager::resolve_reauth_target
    /// [`complete_authorization`]: McpManager::complete_authorization
    pub async fn trigger_auth_flow(
        manager: Arc<Self>,
        name: &str,
        registry: Arc<Registry>,
    ) -> Result<String, String> {
        let (url, auth_config) = manager.resolve_reauth_target(name).await?;

        let token_store = manager
            .token_store
            .as_ref()
            .ok_or_else(|| "no OAuth token store is configured for this session".to_string())?
            .clone();

        let server_name = name.to_string();
        let server_key = derive_server_key(&server_name, Some(&url), "http");

        let engine = OAuthEngine::new(reqwest::Client::new());
        let flow_handle = engine
            .begin_authorization_flow(&server_key, &url, &auth_config, Arc::clone(&token_store))
            .await
            .map_err(|e| e.to_string())?;

        let auth_url = flow_handle.auth_url.clone();

        tokio::spawn(async move {
            match flow_handle.wait.await {
                Ok(Ok(())) => {
                    manager.complete_authorization(&server_name, registry).await;
                }
                Ok(Err(e)) => {
                    warn!(server = %server_name, "OAuth flow failed: {e}");
                }
                Err(e) => {
                    warn!(server = %server_name, "OAuth flow task panicked: {e}");
                }
            }
        });

        Ok(auth_url)
    }

    /// Append all MCP-sourced prompt skills into an existing [`SkillRegistry`].
    pub fn extend_skill_registry(&self, skill_registry: &mut SkillRegistry) {
        let records = self.prompt_skills.lock().unwrap();
        for record in records.iter() {
            if skill_registry.get(&record.name).is_none() {
                skill_registry.insert(record.name.clone(), SkillEntry::Ok(record.clone()));
            }
        }
    }

    /// Gracefully shut down all MCP server subprocesses in parallel.
    pub async fn shutdown(self) {
        let clients = self.clients.into_inner();
        join_all(clients.into_iter().map(|e| e.handle.shutdown())).await;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
