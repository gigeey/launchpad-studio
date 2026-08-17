//! HTTP/SSE transport for MCP servers.
//!
//! Implements MCP streamable-HTTP: JSON-RPC requests are sent as HTTP POST;
//! the server may reply with `application/json` (single response) or
//! `text/event-stream` (SSE stream carrying multiple JSON-RPC messages).
//!
//! Session continuity is maintained via the `Mcp-Session-Id` header that
//! the server echoes back after initialization.
//!
//! HTTP 401 responses surface as `McpError::AuthRequired` so that callers
//! can distinguish an auth failure from other transport errors.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use ao_engine_tools_provider_config::mcp_token_store::McpTokenStore;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::debug;

use super::client::{McpError, NotificationDispatch, ProgressNotification, ServerCapabilities};

// ── Transport inner state ─────────────────────────────────────────────────────

pub(super) struct HttpTransportInner {
    pub(super) name: String,
    url: String,
    http_client: reqwest::Client,
    /// Session ID received in the `Mcp-Session-Id` response header.
    pub(super) session_id: Mutex<Option<String>>,
    pub(super) next_id: AtomicU64,
    pub(super) notification_dispatch: Mutex<NotificationDispatch>,
    /// Bearer token for HTTP Authorization header, if set.
    bearer_token: StdMutex<Option<String>>,
    /// Token store + derived server key that [`super::client::McpClientHandle::reconnect`]
    /// uses to re-resolve a fresh bearer token before re-running the
    /// handshake. `None` when this connection was never given a token source
    /// (e.g. a server with no OAuth configuration at all).
    token_source: StdMutex<Option<(Arc<McpTokenStore>, String)>>,
}

impl HttpTransportInner {
    pub(super) fn new(name: &str, url: &str) -> Self {
        Self {
            name: name.to_string(),
            url: url.to_string(),
            http_client: reqwest::Client::new(),
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(1),
            notification_dispatch: Mutex::new(NotificationDispatch::new()),
            bearer_token: StdMutex::new(None),
            token_source: StdMutex::new(None),
        }
    }

    /// Set the bearer token used for HTTP Authorization headers.
    pub(super) fn set_bearer_token(&self, token: String) {
        *self.bearer_token.lock().unwrap() = Some(token);
    }

    /// Attach the token store + derived server key that `reconnect` should
    /// use to re-resolve a fresh bearer token before re-running the
    /// handshake.
    pub(super) fn set_token_source(&self, token_store: Arc<McpTokenStore>, server_key: String) {
        *self.token_source.lock().unwrap() = Some((token_store, server_key));
    }

    /// The attached token store + server key, if any.
    pub(super) fn token_source(&self) -> Option<(Arc<McpTokenStore>, String)> {
        self.token_source.lock().unwrap().clone()
    }

    pub(super) async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        dur: Duration,
    ) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        timeout(dur, self.execute_request(id, msg))
            .await
            .map_err(|_| McpError::Timeout)?
    }

    async fn execute_request(&self, id: u64, msg: Value) -> Result<Value, McpError> {
        let session_id = self.session_id.lock().await.clone();
        let bearer = self.bearer_token.lock().unwrap().clone();

        let mut req = self
            .http_client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        if let Some(sid) = session_id {
            req = req.header("Mcp-Session-Id", sid);
        }

        if let Some(token) = bearer {
            req = req.header("Authorization", format!("Bearer {token}"));
        }

        let response = req
            .json(&msg)
            .send()
            .await
            .map_err(|e| McpError::Transport(io::Error::new(io::ErrorKind::Other, e)))?;

        // Capture session ID for subsequent requests.
        if let Some(sid) = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().await = Some(sid.to_string());
        }

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(McpError::AuthRequired);
        }

        if !response.status().is_success() {
            return Err(McpError::Transport(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("HTTP {}", response.status()),
            )));
        }

        let content_type = response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/event-stream") {
            self.parse_sse_response(id, response).await
        } else {
            let body: Value = response
                .json()
                .await
                .map_err(|e| McpError::Protocol(e.to_string()))?;
            extract_rpc_result(&body, id)
        }
    }

    async fn parse_sse_response(
        &self,
        target_id: u64,
        response: reqwest::Response,
    ) -> Result<Value, McpError> {
        let mut stream = response.bytes_stream();
        let mut parser = SseParser::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| {
                McpError::Transport(io::Error::new(io::ErrorKind::Other, e))
            })?;

            for data in parser.feed(&bytes) {
                match serde_json::from_str::<Value>(&data) {
                    Ok(msg) => {
                        if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                            if id == target_id {
                                return extract_rpc_result(&msg, id);
                            }
                        } else {
                            self.dispatch_notification(&msg).await;
                        }
                    }
                    Err(e) => {
                        debug!(mcp_server = %self.name, "malformed SSE data: {e}");
                    }
                }
            }
        }

        Err(McpError::Closed)
    }

    async fn dispatch_notification(&self, msg: &Value) {
        let method = match msg.get("method").and_then(|m| m.as_str()) {
            Some(m) => m,
            None => return,
        };

        if method != "notifications/progress" {
            debug!(mcp_server = %self.name, "received notification '{method}' (no handler)");
            return;
        }

        let params = match msg.get("params") {
            Some(p) => p,
            None => return,
        };

        let token = params.get("progressToken").and_then(|t| {
            t.as_str()
                .map(|s| s.to_string())
                .or_else(|| t.as_u64().map(|n| n.to_string()))
        });

        let Some(token) = token else { return };

        let progress = params.get("progress").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let total = params.get("total").and_then(|v| v.as_f64());
        let message = params.get("message").and_then(|v| v.as_str()).map(|s| s.to_string());

        let notif = ProgressNotification { progress_token: token.clone(), progress, total, message };

        let dispatch = self.notification_dispatch.lock().await;
        if let Some(cb) = dispatch.progress.get(&token) {
            cb(notif);
        } else {
            debug!(
                mcp_server = %self.name,
                "progress notification for unknown token '{token}' (no callback registered)"
            );
        }
    }

    /// Send a JSON-RPC notification (no `id`, no response expected).
    pub(super) async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let session_id = self.session_id.lock().await.clone();
        let bearer = self.bearer_token.lock().unwrap().clone();

        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });

        let mut req = self
            .http_client
            .post(&self.url)
            .header("Content-Type", "application/json");

        if let Some(sid) = session_id {
            req = req.header("Mcp-Session-Id", sid);
        }

        if let Some(token) = bearer {
            req = req.header("Authorization", format!("Bearer {token}"));
        }

        let resp = req
            .json(&msg)
            .send()
            .await
            .map_err(|e| McpError::Transport(io::Error::new(io::ErrorKind::Other, e)))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(McpError::AuthRequired);
        }

        Ok(())
    }
}

// ── Handshake ─────────────────────────────────────────────────────────────────

pub(super) async fn run_http_handshake(
    inner: &HttpTransportInner,
) -> Result<ServerCapabilities, McpError> {
    let resp = inner
        .call_with_timeout(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "clientInfo": { "name": "ao-engine", "version": "0.1.0" },
                "capabilities": {},
            }),
            Duration::from_secs(30),
        )
        .await
        .map_err(|e| match e {
            McpError::AuthRequired => McpError::AuthRequired,
            other => McpError::HandshakeFailed(other.to_string()),
        })?;

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
        .notify("notifications/initialized", json!({}))
        .await
        .map_err(|e| McpError::HandshakeFailed(format!("notifications/initialized: {e}")))?;

    Ok(caps)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn extract_rpc_result(msg: &Value, id: u64) -> Result<Value, McpError> {
    if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-32603);
        let message =
            err.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let data = err.get("data").cloned();
        return Err(McpError::CallError { code, message, data });
    }
    if let Some(result) = msg.get("result") {
        return Ok(result.clone());
    }
    Err(McpError::Protocol(format!(
        "response for id {id} has neither 'result' nor 'error'"
    )))
}

// ── SSE line parser ───────────────────────────────────────────────────────────

/// Stateful SSE byte-stream parser.
///
/// Feed raw bytes incrementally; each call returns zero or more complete
/// event `data` strings ready for JSON parsing. Lines that are not `data:`
/// fields (comments, `event:`, `id:`, `retry:`) are silently ignored.
struct SseParser {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
}

impl SseParser {
    fn new() -> Self {
        Self { buffer: Vec::new(), data_lines: Vec::new() }
    }

    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut completed = Vec::new();

        loop {
            let Some(nl) = self.buffer.iter().position(|&b| b == b'\n') else {
                break;
            };

            let raw = self.buffer.drain(..=nl).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&raw)
                .trim_end_matches(|c| c == '\n' || c == '\r')
                .to_string();

            if line.is_empty() {
                if !self.data_lines.is_empty() {
                    completed.push(self.data_lines.join("\n"));
                    self.data_lines.clear();
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data_lines.push(data.trim_start().to_string());
            }
        }

        completed
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_single_event() {
        let mut p = SseParser::new();
        let events = p.feed(b"data: {\"hello\":1}\n\n");
        assert_eq!(events, vec!["{\"hello\":1}"]);
    }

    #[test]
    fn sse_parser_two_events() {
        let mut p = SseParser::new();
        let events = p.feed(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], "{\"a\":1}");
        assert_eq!(events[1], "{\"b\":2}");
    }

    #[test]
    fn sse_parser_chunked_across_calls() {
        let mut p = SseParser::new();
        let e1 = p.feed(b"data: {\"x");
        assert!(e1.is_empty(), "incomplete line yields nothing");
        let e2 = p.feed(b"\":1}\n\n");
        assert_eq!(e2, vec!["{\"x\":1}"]);
    }

    #[test]
    fn sse_parser_ignores_non_data_fields() {
        let mut p = SseParser::new();
        let events = p.feed(b": comment\nevent: message\nid: 1\ndata: {\"ok\":true}\n\n");
        assert_eq!(events, vec!["{\"ok\":true}"]);
    }

    #[test]
    fn sse_parser_multi_line_data_joined() {
        let mut p = SseParser::new();
        let events = p.feed(b"data: line1\ndata: line2\n\n");
        assert_eq!(events, vec!["line1\nline2"]);
    }

    #[test]
    fn extract_rpc_result_success() {
        let msg = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"x":42}});
        let v = extract_rpc_result(&msg, 1).unwrap();
        assert_eq!(v["x"], 42);
    }

    #[test]
    fn extract_rpc_result_error() {
        let msg =
            serde_json::json!({"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"bad"}});
        let err = extract_rpc_result(&msg, 1).unwrap_err();
        assert!(matches!(err, McpError::CallError { code: -32600, .. }));
    }

    #[test]
    fn extract_rpc_result_malformed() {
        let msg = serde_json::json!({"jsonrpc":"2.0","id":1});
        let err = extract_rpc_result(&msg, 1).unwrap_err();
        assert!(matches!(err, McpError::Protocol(_)));
    }
}
