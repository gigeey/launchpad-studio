//! Async JSON-RPC MCP client supporting stdio-subprocess and HTTP transports.
//!
//! The stdio transport uses line-delimited JSON (one JSON object per line).
//! The HTTP transport uses POST JSON-RPC with optional SSE streaming responses.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use ao_engine_tools_provider_config::mcp_token_store::McpTokenStore;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, warn};

use super::http_client::{run_http_handshake, HttpTransportInner};
use super::oauth_flow::{OAuthEngine, OAuthError};

#[derive(Debug, Error)]
pub enum McpError {
    #[error("spawn: {0}")]
    Spawn(io::Error),
    #[error("handshake failed: {0}")]
    HandshakeFailed(String),
    #[error("transport: {0}")]
    Transport(io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("call error (code {code}): {message}")]
    CallError {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("timeout")]
    Timeout,
    #[error("closed")]
    Closed,
    #[error("authentication required (HTTP 401)")]
    AuthRequired,

    /// HTTP 401 on reconnect with no stored credential to try: this server
    /// has either never been authorized, or its credential was deleted from
    /// the token store. Distinguished from [`McpError::CredentialRejected`]
    /// so a caller can tell "there was never a token to reject" from "the
    /// token we just installed was refused".
    #[error("MCP server '{0}' requires authorization — no stored credential is available")]
    NeverAuthorized(String),

    /// HTTP 401 on reconnect even after installing a token the store
    /// reported as current — the credential itself was refused by the
    /// server (revoked, or expired beyond what a refresh could repair), not
    /// merely stale in this process.
    #[error("MCP server '{0}' rejected its current credential — reauthorization is required")]
    CredentialRejected(String),

    /// Reconnect had a token source attached but the lookup/refresh call
    /// itself failed (e.g. a network error reaching the token endpoint) —
    /// authentication was never actually retried, so this is neither "never
    /// authorized" nor "the server rejected our credential".
    #[error("MCP server '{0}' credential refresh failed ({1}); authentication was not retried")]
    TokenRefreshFailed(String, String),

    /// The provider revoked the OAuth grant entirely (RFC 6749
    /// `invalid_grant` on a refresh attempt — e.g. Notion's
    /// refresh-token-reuse detection). Diagnosed straight from the token
    /// endpoint's structured error response rather than inferred from a
    /// generic HTTP failure, so it is trusted as terminal: retrying the
    /// refresh will not succeed, the user must re-authorize this connector
    /// from scratch. Distinct from [`McpError::CredentialRejected`] (a plain
    /// 401 on a credential the refresh path verified was current) and from
    /// [`McpError::TokenRefreshFailed`] (an unclassified refresh failure
    /// that might still be transient).
    #[error("MCP server '{0}' OAuth grant was revoked ({1}) — re-authorization is required")]
    GrantRevoked(String, String),
}

/// Outcome of attempting to re-resolve a bearer token during
/// [`McpClientHandle::reconnect`], used to pick the right error variant when
/// the subsequent handshake still comes back `AuthRequired`.
enum TokenRefreshOutcome {
    /// No token source was attached to this transport.
    NoSource,
    /// A token source was attached but the store has no credential.
    NoCredential,
    /// A fresh/current token was found and installed on the transport.
    Refreshed,
    /// The token source lookup/refresh call itself failed.
    RefreshFailed(String),
    /// The refresh call came back with a structured `invalid_grant` —
    /// the grant is revoked, not merely stale. Short-circuits `reconnect`
    /// before the handshake is even attempted, since no bearer token exists
    /// that could pass it and retrying would just resubmit the same
    /// rotated-out refresh token.
    GrantRevoked(String),
}

/// Capabilities advertised by an MCP server in its `initialize` response.
#[derive(Debug, Clone)]
pub struct ServerCapabilities {
    pub resources: bool,
    pub prompts: bool,
    pub tools: bool,
    /// Raw capabilities block for forward-compatibility.
    pub raw: Value,
}

/// A single `notifications/progress` event received from an MCP server.
#[derive(Debug, Clone)]
pub struct ProgressNotification {
    pub progress_token: String,
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<String>,
}

pub(crate) type ProgressCallback = Arc<dyn Fn(ProgressNotification) + Send + Sync>;

pub(crate) struct NotificationDispatch {
    pub(crate) progress: HashMap<String, ProgressCallback>,
}

impl NotificationDispatch {
    pub(crate) fn new() -> Self {
        Self { progress: HashMap::new() }
    }
}

// ── Stdio internal state ──────────────────────────────────────────────────────

struct ClientInner {
    name: String,
    writer: Mutex<Option<tokio::io::BufWriter<tokio::process::ChildStdin>>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>,
    next_id: AtomicU64,
    notification_dispatch: Mutex<NotificationDispatch>,
}

impl ClientInner {
    async fn write_msg(&self, msg: Value) -> Result<(), McpError> {
        let mut line = serde_json::to_string(&msg)
            .map_err(|e| McpError::Protocol(e.to_string()))?;
        line.push('\n');

        let mut guard = self.writer.lock().await;
        let w = guard.as_mut().ok_or(McpError::Closed)?;
        w.write_all(line.as_bytes()).await.map_err(McpError::Transport)?;
        w.flush().await.map_err(McpError::Transport)?;
        Ok(())
    }

    async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        dur: Duration,
    ) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        if let Err(e) = self.write_msg(msg).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        match timeout(dur, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(McpError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(McpError::Timeout)
            }
        }
    }
}

// ── Stdio notification dispatch ───────────────────────────────────────────────

async fn dispatch_stdio_notification(inner: &Arc<ClientInner>, msg: &Value) {
    let method = match msg.get("method").and_then(|m| m.as_str()) {
        Some(m) => m,
        None => return,
    };

    match method {
        "notifications/progress" => {
            let params = match msg.get("params") {
                Some(p) => p,
                None => return,
            };

            let token = params.get("progressToken").and_then(|t| {
                t.as_str()
                    .map(|s| s.to_string())
                    .or_else(|| t.as_u64().map(|n| n.to_string()))
            });

            let Some(token) = token else {
                debug!(mcp_server = %inner.name, "progress notification missing progressToken");
                return;
            };

            let progress = params.get("progress").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let total = params.get("total").and_then(|v| v.as_f64());
            let message =
                params.get("message").and_then(|v| v.as_str()).map(|s| s.to_string());

            let notif = ProgressNotification {
                progress_token: token.clone(),
                progress,
                total,
                message,
            };

            let dispatch = inner.notification_dispatch.lock().await;
            if let Some(cb) = dispatch.progress.get(&token) {
                cb(notif);
            } else {
                debug!(
                    mcp_server = %inner.name,
                    "progress notification for unknown token '{token}'"
                );
            }
        }
        other => {
            debug!(mcp_server = %inner.name, "received notification '{other}' (no handler)");
        }
    }
}

// ── Stdio spawn config ────────────────────────────────────────────────────────

struct SpawnConfig {
    name: String,
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
}

// ── Stdio live session ────────────────────────────────────────────────────────

struct LiveSession {
    inner: Arc<ClientInner>,
    child: Child,
    stderr_task: JoinHandle<()>,
    reader_task: JoinHandle<()>,
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        if let Ok(mut g) = self.inner.writer.try_lock() {
            *g = None;
        }
        let _ = self.child.start_kill();
        self.stderr_task.abort();
        self.reader_task.abort();
    }
}

// ── Transport variants ────────────────────────────────────────────────────────

enum TransportState {
    Stdio {
        config: SpawnConfig,
        live: Mutex<Option<LiveSession>>,
    },
    Http(HttpTransportInner),
}

// ── Handle core ───────────────────────────────────────────────────────────────

struct McpHandleCore {
    transport: TransportState,
    server_caps: StdMutex<Option<ServerCapabilities>>,
}

// ── Stdio spawn and handshake ─────────────────────────────────────────────────

async fn spawn_live_session(config: &SpawnConfig) -> Result<LiveSession, McpError> {
    let mut child = Command::new(&config.command)
        .args(&config.args)
        .envs(&config.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(McpError::Spawn)?;

    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let inner = Arc::new(ClientInner {
        name: config.name.clone(),
        writer: Mutex::new(Some(tokio::io::BufWriter::new(stdin))),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        notification_dispatch: Mutex::new(NotificationDispatch::new()),
    });

    let name_for_stderr = config.name.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            warn!(mcp_server = %name_for_stderr, "{}", line);
        }
    });

    let inner_for_reader = Arc::clone(&inner);
    let reader_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) if !line.trim().is_empty() => {
                    match serde_json::from_str::<Value>(&line) {
                        Ok(msg) => {
                            if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                                let result = if let Some(err) = msg.get("error") {
                                    let code = err
                                        .get("code")
                                        .and_then(|c| c.as_i64())
                                        .unwrap_or(-32603);
                                    let message = err
                                        .get("message")
                                        .and_then(|m| m.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let data = err.get("data").cloned();
                                    Err(McpError::CallError { code, message, data })
                                } else if let Some(res) = msg.get("result") {
                                    Ok(res.clone())
                                } else {
                                    Err(McpError::Protocol(format!(
                                        "response for id {id} has neither 'result' nor 'error'"
                                    )))
                                };

                                let mut pending = inner_for_reader.pending.lock().await;
                                if let Some(sender) = pending.remove(&id) {
                                    let _ = sender.send(result);
                                } else {
                                    debug!(
                                        mcp_server = %inner_for_reader.name,
                                        "received response for unknown id {id}"
                                    );
                                }
                            } else {
                                dispatch_stdio_notification(&inner_for_reader, &msg).await;
                            }
                        }
                        Err(e) => {
                            warn!(
                                mcp_server = %inner_for_reader.name,
                                "malformed JSON from server: {e}: {line}"
                            );
                        }
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    *inner_for_reader.writer.lock().await = None;
                    break;
                }
            }
        }
        let mut pending = inner_for_reader.pending.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(McpError::Closed));
        }
    });

    Ok(LiveSession { inner, child, stderr_task, reader_task })
}

async fn run_stdio_handshake(inner: &Arc<ClientInner>) -> Result<ServerCapabilities, McpError> {
    let resp = inner
        .call_with_timeout(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "clientInfo": { "name": "ao-engine", "version": "0.1.0" },
                "capabilities": {}
            }),
            Duration::from_secs(30),
        )
        .await
        .map_err(|e| McpError::HandshakeFailed(e.to_string()))?;

    if resp.get("protocolVersion").is_none() {
        return Err(McpError::HandshakeFailed(format!(
            "initialize response missing protocolVersion: {resp}"
        )));
    }

    let raw_caps =
        resp.get("capabilities").cloned().unwrap_or(Value::Object(Default::default()));
    let caps = ServerCapabilities {
        resources: raw_caps.get("resources").is_some(),
        prompts: raw_caps.get("prompts").is_some(),
        tools: raw_caps.get("tools").is_some(),
        raw: raw_caps,
    };

    inner
        .write_msg(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }))
        .await
        .map_err(|e| McpError::HandshakeFailed(format!("notifications/initialized: {e}")))?;

    Ok(caps)
}

// ── Public handle ─────────────────────────────────────────────────────────────

/// Cheap-clone handle to an MCP server with reconnect-on-demand support.
///
/// Supports both stdio-subprocess and HTTP transports. Clones share the same
/// underlying connection state — a reconnect triggered by one clone is
/// visible to all others.
#[derive(Clone)]
pub struct McpClientHandle(Arc<McpHandleCore>);

impl std::fmt::Debug for McpClientHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClientHandle")
            .field("name", &self.name())
            .finish_non_exhaustive()
    }
}

impl McpClientHandle {
    /// Spawn an MCP server subprocess and complete the `initialize` handshake.
    pub async fn spawn(
        name: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<McpClientHandle, McpError> {
        let config = SpawnConfig {
            name: name.to_string(),
            command: command.to_string(),
            args: args.to_vec(),
            env: env.clone(),
        };
        let core = Arc::new(McpHandleCore {
            transport: TransportState::Stdio {
                config,
                live: Mutex::new(None),
            },
            server_caps: StdMutex::new(None),
        });
        let handle = McpClientHandle(core);
        handle.reconnect().await?;
        Ok(handle)
    }

    /// Connect to an HTTP/SSE MCP server and complete the `initialize` handshake.
    pub async fn connect_http(name: &str, url: &str) -> Result<McpClientHandle, McpError> {
        let inner = HttpTransportInner::new(name, url);
        let caps = run_http_handshake(&inner)
            .await
            .map_err(|e| match e {
                McpError::AuthRequired => McpError::AuthRequired,
                other => McpError::HandshakeFailed(other.to_string()),
            })?;
        let core = Arc::new(McpHandleCore {
            transport: TransportState::Http(inner),
            server_caps: StdMutex::new(Some(caps)),
        });
        Ok(McpClientHandle(core))
    }

    /// Connect to an HTTP/SSE MCP server using a bearer token for authentication.
    ///
    /// Sets the `Authorization: Bearer <token>` header on all requests before
    /// initiating the `initialize` handshake.
    pub async fn connect_http_with_bearer(
        name: &str,
        url: &str,
        token: &str,
    ) -> Result<McpClientHandle, McpError> {
        let inner = HttpTransportInner::new(name, url);
        inner.set_bearer_token(token.to_string());
        let caps = run_http_handshake(&inner).await?;
        let core = Arc::new(McpHandleCore {
            transport: TransportState::Http(inner),
            server_caps: StdMutex::new(Some(caps)),
        });
        Ok(McpClientHandle(core))
    }

    /// Attach the token store and derived server key that [`reconnect`]
    /// should use to re-resolve a fresh bearer token before re-running the
    /// HTTP handshake.
    ///
    /// A no-op for stdio-transport handles — reconnect there always
    /// re-spawns the subprocess and has no concept of a bearer token.
    ///
    /// [`reconnect`]: Self::reconnect
    pub fn attach_http_token_source(&self, token_store: Arc<McpTokenStore>, server_key: String) {
        if let TransportState::Http(http) = &self.0.transport {
            http.set_token_source(token_store, server_key);
        }
    }

    /// Reconnect and redo the `initialize` handshake.
    ///
    /// For stdio: kills the old subprocess and spawns a fresh one.
    ///
    /// For HTTP: resets the session, and if a token source was attached via
    /// [`attach_http_token_source`], re-resolves the current access token
    /// from the store (running the same proactive-refresh logic the initial
    /// connect uses) and installs it on the transport before repeating the
    /// initialize call. This is what lets a handle that was created before a
    /// credential existed pick one up once the user finishes authorizing,
    /// without a process restart. If the handshake still comes back with
    /// HTTP 401 afterwards, the returned error distinguishes why: no
    /// credential was ever available ([`McpError::NeverAuthorized`]), the
    /// freshly-installed credential was itself refused
    /// ([`McpError::CredentialRejected`]), or the token lookup/refresh call
    /// failed before a retry was even possible
    /// ([`McpError::TokenRefreshFailed`]). A fourth, more direct case skips
    /// the handshake attempt entirely: if the refresh call itself came back
    /// with a structured `invalid_grant` response, the grant is revoked and
    /// [`McpError::GrantRevoked`] is returned immediately — there is no
    /// bearer token to try, and attempting the handshake anyway would just
    /// waste a round trip on a connection that cannot succeed.
    ///
    /// [`attach_http_token_source`]: Self::attach_http_token_source
    pub async fn reconnect(&self) -> Result<(), McpError> {
        match &self.0.transport {
            TransportState::Stdio { config, live } => {
                { *live.lock().await = None; }
                let session = spawn_live_session(config).await?;
                let caps = run_stdio_handshake(&session.inner).await?;
                *self.0.server_caps.lock().unwrap() = Some(caps);
                *live.lock().await = Some(session);
                Ok(())
            }
            TransportState::Http(http) => {
                *http.session_id.lock().await = None;

                let refresh_outcome = match http.token_source() {
                    None => TokenRefreshOutcome::NoSource,
                    Some((store, key)) => {
                        let engine = OAuthEngine::new(reqwest::Client::new());
                        match engine.current_access_token(&key, &store).await {
                            Ok(Some(token)) => {
                                http.set_bearer_token(token);
                                TokenRefreshOutcome::Refreshed
                            }
                            Ok(None) => TokenRefreshOutcome::NoCredential,
                            Err(OAuthError::GrantRevoked { description }) => {
                                TokenRefreshOutcome::GrantRevoked(description)
                            }
                            Err(e) => TokenRefreshOutcome::RefreshFailed(e.to_string()),
                        }
                    }
                };

                if let TokenRefreshOutcome::GrantRevoked(description) = refresh_outcome {
                    return Err(McpError::GrantRevoked(http.name.clone(), description));
                }

                match run_http_handshake(http).await {
                    Ok(caps) => {
                        *self.0.server_caps.lock().unwrap() = Some(caps);
                        Ok(())
                    }
                    Err(McpError::AuthRequired) => Err(match refresh_outcome {
                        TokenRefreshOutcome::Refreshed => {
                            McpError::CredentialRejected(http.name.clone())
                        }
                        TokenRefreshOutcome::RefreshFailed(reason) => {
                            McpError::TokenRefreshFailed(http.name.clone(), reason)
                        }
                        TokenRefreshOutcome::NoCredential | TokenRefreshOutcome::NoSource => {
                            McpError::NeverAuthorized(http.name.clone())
                        }
                        TokenRefreshOutcome::GrantRevoked(_) => {
                            unreachable!("returned early above")
                        }
                    }),
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// The server name supplied at construction time.
    pub fn name(&self) -> &str {
        match &self.0.transport {
            TransportState::Stdio { config, .. } => &config.name,
            TransportState::Http(http) => &http.name,
        }
    }

    /// Capabilities the server advertised during the most recent `initialize`.
    pub fn server_capabilities(&self) -> Option<ServerCapabilities> {
        self.0.server_caps.lock().unwrap().clone()
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, McpError> {
        self.call_with_timeout(method, params, Duration::from_secs(60)).await
    }

    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        dur: Duration,
    ) -> Result<Value, McpError> {
        match &self.0.transport {
            TransportState::Stdio { live, .. } => {
                let inner = live
                    .lock()
                    .await
                    .as_ref()
                    .map(|s| Arc::clone(&s.inner))
                    .ok_or(McpError::Closed)?;
                inner.call_with_timeout(method, params, dur).await
            }
            TransportState::Http(http) => {
                http.call_with_timeout(method, params, dur).await
            }
        }
    }

    /// Call a method and route `notifications/progress` from the server to `on_progress`.
    ///
    /// Injects `_meta.progressToken` into `params` so the server can correlate
    /// notifications with this call. The callback is unregistered when the call
    /// completes (success, error, or timeout).
    pub async fn call_with_progress<F>(
        &self,
        method: &str,
        mut params: Value,
        dur: Duration,
        on_progress: F,
    ) -> Result<Value, McpError>
    where
        F: Fn(ProgressNotification) + Send + Sync + 'static,
    {
        match &self.0.transport {
            TransportState::Stdio { live, .. } => {
                let inner = live
                    .lock()
                    .await
                    .as_ref()
                    .map(|s| Arc::clone(&s.inner))
                    .ok_or(McpError::Closed)?;

                let token =
                    format!("pt-{}", inner.next_id.fetch_add(1, Ordering::SeqCst));

                inject_progress_token(&mut params, &token);
                inner
                    .notification_dispatch
                    .lock()
                    .await
                    .progress
                    .insert(token.clone(), Arc::new(on_progress));

                let result = inner.call_with_timeout(method, params, dur).await;
                inner.notification_dispatch.lock().await.progress.remove(&token);
                result
            }
            TransportState::Http(http) => {
                let token =
                    format!("pt-{}", http.next_id.fetch_add(1, Ordering::SeqCst));

                inject_progress_token(&mut params, &token);
                http.notification_dispatch
                    .lock()
                    .await
                    .progress
                    .insert(token.clone(), Arc::new(on_progress));

                let result = http.call_with_timeout(method, params, dur).await;
                http.notification_dispatch.lock().await.progress.remove(&token);
                result
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        match &self.0.transport {
            TransportState::Stdio { live, .. } => {
                let inner = live
                    .lock()
                    .await
                    .as_ref()
                    .map(|s| Arc::clone(&s.inner))
                    .ok_or(McpError::Closed)?;
                inner
                    .write_msg(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
                    .await
            }
            TransportState::Http(http) => http.notify(method, params).await,
        }
    }

    /// Graceful shutdown: send `shutdown`, close the transport, and for stdio
    /// wait for the subprocess to exit (5 s deadline, then SIGKILL).
    pub async fn shutdown(self) {
        match &self.0.transport {
            TransportState::Stdio { live, .. } => {
                let session = live.lock().await.take();
                if let Some(mut s) = session {
                    let _ = timeout(
                        Duration::from_secs(10),
                        s.inner.call_with_timeout(
                            "shutdown",
                            json!({}),
                            Duration::from_secs(10),
                        ),
                    )
                    .await;
                    *s.inner.writer.lock().await = None;
                    if timeout(Duration::from_secs(5), s.child.wait()).await.is_err() {
                        let _ = s.child.start_kill();
                    }
                    s.stderr_task.abort();
                    s.reader_task.abort();
                }
            }
            TransportState::Http(http) => {
                let _ = http.notify("shutdown", json!({})).await;
            }
        }
    }
}

fn inject_progress_token(params: &mut Value, token: &str) {
    if let Value::Object(ref mut map) = params {
        let meta = map.entry("_meta".to_string()).or_insert_with(|| json!({}));
        if let Value::Object(ref mut meta_map) = meta {
            meta_map.insert("progressToken".to_string(), json!(token));
        }
    }
}

// ── Test helpers (crate-private) ──────────────────────────────────────────────

#[cfg(test)]
impl McpClientHandle {
    pub(crate) async fn simulate_connection_death_for_test(&self) {
        if let TransportState::Stdio { live, .. } = &self.0.transport {
            let guard = live.lock().await;
            if let Some(session) = guard.as_ref() {
                *session.inner.writer.lock().await = None;
            }
        }
    }

    pub(crate) fn unreachable_for_test(name: &str) -> Self {
        McpClientHandle(Arc::new(McpHandleCore {
            transport: TransportState::Stdio {
                config: SpawnConfig {
                    name: name.to_string(),
                    command: "/nonexistent/binary/that/does/not/exist".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                },
                live: Mutex::new(None),
            },
            server_caps: StdMutex::new(None),
        }))
    }

    pub(crate) fn unreachable_with_resources_for_test(name: &str) -> Self {
        let caps = ServerCapabilities {
            resources: true,
            prompts: false,
            tools: false,
            raw: serde_json::json!({ "resources": {} }),
        };
        McpClientHandle(Arc::new(McpHandleCore {
            transport: TransportState::Stdio {
                config: SpawnConfig {
                    name: name.to_string(),
                    command: "/nonexistent/binary/that/does/not/exist".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                },
                live: Mutex::new(None),
            },
            server_caps: StdMutex::new(Some(caps)),
        }))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use crate::mcp::test_support::echo_server_bin;

    #[tokio::test]
    async fn spawn_call_shutdown_roundtrip() {
        let bin = echo_server_bin();
        let handle = McpClientHandle::spawn("echo", bin.to_str().unwrap(), &[], &HashMap::new())
            .await
            .expect("should spawn echo_mcp_server");

        let result = handle
            .call("tools/call", json!({ "name": "echo", "arguments": { "x": 42 } }))
            .await
            .expect("call should succeed");

        assert!(result.get("content").is_some(), "result has content array");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn exits_during_handshake_gives_handshake_failed() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "crash".to_string());

        let err = McpClientHandle::spawn("crash", bin.to_str().unwrap(), &[], &env)
            .await
            .expect_err("should fail for a crashing server");

        assert!(matches!(err, McpError::HandshakeFailed(_)));
    }

    #[tokio::test]
    async fn malformed_jsonrpc_gives_protocol() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "bad_protocol".to_string());

        let handle = McpClientHandle::spawn("bad_protocol", bin.to_str().unwrap(), &[], &env)
            .await
            .expect("should spawn");

        let err = handle.call("tools/call", json!({})).await.expect_err("should fail");
        assert!(matches!(err, McpError::Protocol(_)));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn error_response_gives_call_error() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "error_response".to_string());

        let handle = McpClientHandle::spawn("error_response", bin.to_str().unwrap(), &[], &env)
            .await
            .expect("should spawn");

        let err = handle.call("tools/call", json!({})).await.expect_err("should return error");
        match err {
            McpError::CallError { code, .. } => assert_eq!(code, -32603),
            other => panic!("expected CallError, got {other:?}"),
        }
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn timeout_fires_when_server_hangs() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "hang_after_init".to_string());

        let handle = McpClientHandle::spawn("hang", bin.to_str().unwrap(), &[], &env)
            .await
            .expect("should spawn");

        let err = handle
            .call_with_timeout("tools/call", json!({}), Duration::from_millis(200))
            .await
            .expect_err("should time out");

        assert!(matches!(err, McpError::Timeout));
    }

    #[tokio::test]
    async fn capabilities_captured_after_handshake() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "with_capabilities".to_string());

        let handle = McpClientHandle::spawn("caps", bin.to_str().unwrap(), &[], &env)
            .await
            .expect("should spawn");

        let caps = handle.server_capabilities().expect("capabilities must be present");
        assert!(caps.resources && caps.tools && caps.prompts);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn empty_capabilities_still_captured() {
        let bin = echo_server_bin();
        let handle = McpClientHandle::spawn("echo", bin.to_str().unwrap(), &[], &HashMap::new())
            .await
            .expect("should spawn");

        let caps = handle.server_capabilities().expect("capabilities present even when empty");
        assert!(!caps.resources && !caps.tools && !caps.prompts);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn progress_notification_reaches_callback() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "send_progress".to_string());

        let handle = McpClientHandle::spawn("progress", bin.to_str().unwrap(), &[], &env)
            .await
            .expect("should spawn");

        let received = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
        let received_clone = Arc::clone(&received);

        let result = handle
            .call_with_progress(
                "tools/call",
                json!({ "name": "echo", "arguments": {} }),
                Duration::from_secs(5),
                move |notif| {
                    received_clone.lock().unwrap().push(notif.progress);
                },
            )
            .await
            .expect("call should succeed");

        assert!(result.get("content").is_some());
        let steps = received.lock().unwrap().clone();
        assert_eq!(steps, vec![1.0, 2.0, 3.0]);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn reconnect_restores_closed_connection() {
        let bin = echo_server_bin();
        let handle = McpClientHandle::spawn("echo", bin.to_str().unwrap(), &[], &HashMap::new())
            .await
            .expect("should spawn");

        handle.simulate_connection_death_for_test().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.reconnect().await.expect("reconnect should succeed");

        let result = handle
            .call("tools/call", json!({ "name": "echo", "arguments": { "x": 1 } }))
            .await
            .expect("call after reconnect should succeed");

        assert!(result.get("content").is_some());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn capabilities_refreshed_after_reconnect() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "with_capabilities".to_string());

        let handle = McpClientHandle::spawn("caps", bin.to_str().unwrap(), &[], &env)
            .await
            .expect("should spawn");

        handle.reconnect().await.expect("reconnect should succeed");

        let caps = handle.server_capabilities().expect("caps present after reconnect");
        assert!(caps.resources && caps.tools && caps.prompts);
        handle.shutdown().await;
    }

    // ── HTTP reconnect: token re-resolution ───────────────────────────────────

    use ao_engine_tools_provider_config::mcp_token_store::{derive_server_key, McpTokenRecord};

    fn sample_token_record(access_token: &str) -> McpTokenRecord {
        McpTokenRecord {
            access_token: access_token.to_string(),
            refresh_token: None,
            expires_at: None,
            scope: None,
            client_id: "test-client".to_string(),
            client_secret: None,
            token_endpoint: None,
        }
    }

    /// Mock streamable-HTTP MCP server where `initialize` always succeeds
    /// regardless of auth, but `tools/call` requires `Authorization: Bearer
    /// good-token` and 401s otherwise. Models the real-world shape of the bug
    /// this module fixes: a server whose handshake doesn't require auth but
    /// whose tool calls do, so a handle can exist perfectly well without ever
    /// having a bearer token installed.
    async fn spawn_bearer_gated_mock_server() -> String {
        use axum::{routing::post, Router};

        let app = Router::new().route(
            "/mcp",
            post(|headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
                let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let id = req.get("id").cloned().unwrap_or(json!(1));

                match method {
                    "initialize" => (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2024-11-05",
                                "serverInfo": { "name": "bearer-gated-server", "version": "1.0" },
                                "capabilities": {}
                            }
                        })),
                    ),
                    "notifications/initialized" => {
                        (axum::http::StatusCode::OK, axum::Json(Value::Null))
                    }
                    "tools/call" => {
                        let authorized = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(|v| v == "Bearer good-token")
                            .unwrap_or(false);
                        if !authorized {
                            return (axum::http::StatusCode::UNAUTHORIZED, axum::Json(Value::Null));
                        }
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "content": [{ "type": "text", "text": "authenticated-ok" }],
                                    "isError": false
                                }
                            })),
                        )
                    }
                    _ => (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
                    ),
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        format!("http://127.0.0.1:{}/mcp", addr.port())
    }

    /// Mock server whose first `initialize` call always succeeds (so a handle
    /// can be constructed) but every subsequent `initialize` call — i.e. every
    /// call made by `reconnect()` — returns HTTP 401 regardless of the
    /// Authorization header. Used to exercise the error-classification paths
    /// once the handshake itself keeps failing on reconnect.
    async fn spawn_initialize_flaky_mock_server() -> String {
        use axum::{routing::post, Router};
        use std::sync::atomic::AtomicUsize;

        let init_calls = Arc::new(AtomicUsize::new(0));

        let app = Router::new().route(
            "/mcp",
            post(move |body: axum::body::Bytes| {
                let init_calls = Arc::clone(&init_calls);
                async move {
                    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let id = req.get("id").cloned().unwrap_or(json!(1));

                    match method {
                        "initialize" => {
                            let n = init_calls.fetch_add(1, Ordering::SeqCst);
                            if n == 0 {
                                (
                                    axum::http::StatusCode::OK,
                                    axum::Json(json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": {
                                            "protocolVersion": "2024-11-05",
                                            "serverInfo": { "name": "flaky-server", "version": "1.0" },
                                            "capabilities": {}
                                        }
                                    })),
                                )
                            } else {
                                (axum::http::StatusCode::UNAUTHORIZED, axum::Json(Value::Null))
                            }
                        }
                        "notifications/initialized" => {
                            (axum::http::StatusCode::OK, axum::Json(Value::Null))
                        }
                        _ => (
                            axum::http::StatusCode::OK,
                            axum::Json(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
                        ),
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        format!("http://127.0.0.1:{}/mcp", addr.port())
    }

    #[tokio::test]
    async fn reconnect_installs_credential_that_became_available_after_connect() {
        let url = spawn_bearer_gated_mock_server().await;

        // Connect before any credential exists — matches the from_config_auth
        // Ok(None) path: initialize doesn't require auth, so the handle comes
        // up fine with no bearer token installed.
        let handle = McpClientHandle::connect_http("cred_test_srv", &url)
            .await
            .expect("initial unauthenticated connect should succeed");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));
        let server_key = derive_server_key("cred_test_srv", Some(&url), "http");
        handle.attach_http_token_source(Arc::clone(&store), server_key.clone());

        // Before any credential exists, tool calls fail — no bearer token.
        let err = handle
            .call("tools/call", json!({ "name": "x", "arguments": {} }))
            .await
            .expect_err("call should fail before any credential exists");
        assert!(matches!(err, McpError::AuthRequired));

        // The credential becomes available (e.g. the user finishes an OAuth
        // flow elsewhere in the app) while this handle is still alive.
        store
            .set(&server_key, &sample_token_record("good-token"))
            .expect("store credential");

        // reconnect() must re-resolve the now-available token and install it
        // — no process restart required.
        handle.reconnect().await.expect("reconnect should succeed");

        let result = handle
            .call("tools/call", json!({ "name": "x", "arguments": {} }))
            .await
            .expect("call should succeed after reconnect installs the fresh credential");
        assert_eq!(
            result["content"][0]["text"], "authenticated-ok",
            "server should have accepted the bearer token installed by reconnect()"
        );
    }

    #[tokio::test]
    async fn reconnect_without_any_token_source_surfaces_never_authorized() {
        let url = spawn_initialize_flaky_mock_server().await;
        let handle = McpClientHandle::connect_http("never_auth_srv", &url)
            .await
            .expect("first initialize succeeds unauthenticated");

        // No token source attached — nothing to refresh, so a subsequent
        // 401'ing handshake must be reported as "never authorized", not
        // confused with a credential having been rejected.
        let err = handle.reconnect().await.expect_err("reconnect should fail");
        match err {
            McpError::NeverAuthorized(name) => assert_eq!(name, "never_auth_srv"),
            other => panic!("expected NeverAuthorized, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reconnect_with_credential_server_still_rejects_surfaces_credential_rejected() {
        let url = spawn_initialize_flaky_mock_server().await;
        let handle = McpClientHandle::connect_http("revoked_srv", &url)
            .await
            .expect("first initialize succeeds unauthenticated");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));
        let server_key = derive_server_key("revoked_srv", Some(&url), "http");
        store
            .set(&server_key, &sample_token_record("revoked-token"))
            .expect("seed credential");
        handle.attach_http_token_source(store, server_key);

        // A credential exists and gets installed, but the server keeps
        // rejecting the handshake — that's a genuinely revoked/expired
        // credential, distinguishable from "never authorized".
        let err = handle.reconnect().await.expect_err("reconnect should fail");
        match err {
            McpError::CredentialRejected(name) => assert_eq!(name, "revoked_srv"),
            other => panic!("expected CredentialRejected, got {other:?}"),
        }
    }

    /// Mock `/mcp` endpoint whose `initialize` call always succeeds and
    /// counts how many times it was called — used to prove `reconnect()`
    /// short-circuits on a revoked grant instead of still attempting (and
    /// thereby implicitly retrying) the handshake.
    async fn spawn_bearer_gated_mock_server_with_init_counter() -> (String, Arc<AtomicUsize>) {
        use axum::{routing::post, Router};

        let init_calls = Arc::new(AtomicUsize::new(0));
        let init_calls_for_route = Arc::clone(&init_calls);

        let app = Router::new().route(
            "/mcp",
            post(move |body: axum::body::Bytes| {
                let init_calls = Arc::clone(&init_calls_for_route);
                async move {
                    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let id = req.get("id").cloned().unwrap_or(json!(1));
                    match method {
                        "initialize" => {
                            init_calls.fetch_add(1, Ordering::SeqCst);
                            (
                                axum::http::StatusCode::OK,
                                axum::Json(json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "protocolVersion": "2024-11-05",
                                        "serverInfo": { "name": "revoked-grant-server", "version": "1.0" },
                                        "capabilities": {}
                                    }
                                })),
                            )
                        }
                        "notifications/initialized" => {
                            (axum::http::StatusCode::OK, axum::Json(Value::Null))
                        }
                        _ => (
                            axum::http::StatusCode::OK,
                            axum::Json(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
                        ),
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("http://127.0.0.1:{}/mcp", addr.port()), init_calls)
    }

    /// Mock `/token` endpoint that always returns RFC 6749 `invalid_grant`,
    /// modeling a provider (e.g. Notion) that has revoked the grant behind a
    /// refresh token.
    async fn spawn_invalid_grant_token_endpoint() -> String {
        use axum::{routing::post, Router};

        let app = Router::new().route(
            "/token",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({
                        "error": "invalid_grant",
                        "error_description": "Refresh token reuse detected",
                    })),
                )
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        format!("http://127.0.0.1:{}/token", addr.port())
    }

    #[tokio::test]
    async fn reconnect_with_revoked_grant_surfaces_grant_revoked_and_skips_handshake_retry() {
        let (mcp_url, init_calls) = spawn_bearer_gated_mock_server_with_init_counter().await;
        let token_url = spawn_invalid_grant_token_endpoint().await;

        let handle = McpClientHandle::connect_http("revoked_grant_srv", &mcp_url)
            .await
            .expect("initial unauthenticated connect should succeed");
        assert_eq!(init_calls.load(Ordering::SeqCst), 1, "one initialize call for the initial connect");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));
        let server_key = derive_server_key("revoked_grant_srv", Some(&mcp_url), "http");
        let mut expiring = sample_token_record("stale-access-token");
        expiring.refresh_token = Some("reused-refresh-token".to_string());
        expiring.expires_at = Some(chrono::Utc::now() - chrono::Duration::minutes(1));
        expiring.token_endpoint = Some(token_url);
        store.set(&server_key, &expiring).expect("seed expiring credential with refresh token");
        handle.attach_http_token_source(Arc::clone(&store), server_key);

        let err = handle.reconnect().await.expect_err("reconnect must fail for a revoked grant");
        match err {
            McpError::GrantRevoked(name, reason) => {
                assert_eq!(name, "revoked_grant_srv");
                assert!(reason.contains("reuse"), "reason should carry the provider's description: {reason}");
            }
            other => panic!("expected GrantRevoked, got {other:?}"),
        }

        // reconnect() must short-circuit on a revoked grant rather than
        // still attempting the handshake with no usable credential — that
        // would be an implicit retry of a connection that cannot succeed.
        assert_eq!(
            init_calls.load(Ordering::SeqCst),
            1,
            "a revoked grant must not fall through to a handshake attempt"
        );
    }
}
