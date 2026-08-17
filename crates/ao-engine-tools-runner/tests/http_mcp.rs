//! Integration tests for the HTTP/SSE MCP transport.
//!
//! Spins up in-process axum mock servers and exercises the full
//! initialize → tools/list → tools/call sequence over HTTP, plus
//! a 401-auth-required error path.

use std::collections::HashMap;

use ao_engine_tools_provider_config::mcp_servers::{
    McpLoadingPolicy, McpServersConfig, McpServerEntry, McpTransportType,
};
use ao_engine_tools_runner::mcp::{McpClientHandle, McpError, McpManager};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

// ── Mock server helpers ───────────────────────────────────────────────────────

#[derive(Clone)]
struct MockState {
    /// When true, every request returns HTTP 401.
    always_401: bool,
}

async fn mcp_handler(
    State(state): State<MockState>,
    _headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if state.always_401 {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mock-http-mcp", "version": "1.0" }
        }),
        "notifications/initialized" => {
            // Notification — no response body needed.
            return (StatusCode::OK, axum::body::Body::empty()).into_response();
        }
        "tools/list" => json!({
            "tools": [{
                "name": "greet",
                "description": "Greet someone",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" }
                    }
                }
            }]
        }),
        "tools/call" => {
            let tool_name = msg
                .pointer("/params/name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            json!({
                "content": [{ "type": "text", "text": format!("called {tool_name}") }]
            })
        }
        "shutdown" => {
            return StatusCode::OK.into_response();
        }
        _ => {
            return (
                StatusCode::OK,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "method not found" }
                })),
            )
                .into_response();
        }
    };

    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    }))
    .into_response()
}

async fn start_mock_server(always_401: bool) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}/mcp");

    let state = MockState { always_401 };
    let router = Router::new().route("/mcp", post(mcp_handler)).with_state(state);

    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    (url, handle)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_initialize_tools_list_call() {
    let (url, _server) = start_mock_server(false).await;

    let handle = McpClientHandle::connect_http("mock", &url)
        .await
        .expect("connect_http should succeed");

    // Capabilities should be captured during initialize.
    let caps = handle.server_capabilities().expect("caps after handshake");
    assert!(caps.tools, "mock server advertises tools capability");

    // tools/list
    let list_result = handle
        .call("tools/list", json!({}))
        .await
        .expect("tools/list should succeed");

    let tools = list_result.get("tools").and_then(|t| t.as_array()).expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "greet");

    // tools/call
    let call_result = handle
        .call("tools/call", json!({ "name": "greet", "arguments": { "name": "World" } }))
        .await
        .expect("tools/call should succeed");

    let text = call_result.pointer("/content/0/text").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(text, "called greet");

    handle.shutdown().await;
}

#[tokio::test]
async fn http_401_surfaces_auth_required_error() {
    let (url, _server) = start_mock_server(true).await;

    let err = McpClientHandle::connect_http("auth-test", &url)
        .await
        .expect_err("should fail when server returns 401");

    // The handshake sends initialize which gets a 401 back.
    // AuthRequired is propagated directly (not wrapped in HandshakeFailed) so
    // callers can distinguish it from other connection errors.
    assert!(
        matches!(err, McpError::AuthRequired),
        "expected AuthRequired from 401 during handshake, got {err:?}"
    );
}

#[tokio::test]
async fn http_direct_call_returns_auth_required() {
    let (url, _server) = start_mock_server(true).await;
    let err = McpClientHandle::connect_http("direct-401", &url)
        .await
        .expect_err("should fail");

    // AuthRequired propagates through connect_http unchanged — callers can
    // match on it to detect authorization-required servers.
    assert!(matches!(err, McpError::AuthRequired));
}

#[tokio::test]
async fn http_reconnect_re_handshakes() {
    let (url, _server) = start_mock_server(false).await;

    let handle = McpClientHandle::connect_http("reconnect-test", &url)
        .await
        .expect("initial connect");

    // Reconnect clears the session and re-runs initialize.
    handle.reconnect().await.expect("reconnect should succeed");

    let caps = handle.server_capabilities().expect("caps present after reconnect");
    assert!(caps.tools, "caps refreshed after reconnect");

    handle.shutdown().await;
}

#[tokio::test]
async fn manager_from_config_dispatches_http_transport() {
    let (url, _server) = start_mock_server(false).await;

    let entries = vec![McpServerEntry {
        name: "httptest".to_string(),
        command: None,
        args: vec![],
        env: HashMap::new(),
        loading: McpLoadingPolicy::Always,
        transport: McpTransportType::Http,
        url: Some(url),
        auth: None,
    }];

    let config = McpServersConfig { servers: entries };
    let manager = McpManager::from_config(&config).await;

    let mut registry = ao_engine_tools_core::Registry::new();
    let manager = manager.register_into(&mut registry).await;

    // The mock server exposes one tool: "greet".
    assert!(
        registry.lookup("mcp__httptest__greet").is_some(),
        "greet tool should be registered from HTTP server"
    );

    manager.shutdown().await;
}

#[tokio::test]
async fn stdio_config_backward_compat_in_manager() {
    // A stdio config (command present, no transport / url / auth) must be
    // accepted by McpManager and result in a disabled (not-spawned) server
    // without any panic or error propagating to the caller.
    let entries = vec![McpServerEntry {
        name: "compat".to_string(),
        command: Some("echo".to_string()),
        args: vec![],
        env: HashMap::new(),
        loading: McpLoadingPolicy::Disabled, // disabled so we don't actually exec echo
        transport: McpTransportType::Stdio,  // default transport
        url: None,
        auth: None,
    }];

    let config = McpServersConfig { servers: entries };
    let manager = McpManager::from_config(&config).await;

    // Verify it shuts down cleanly without errors (disabled servers don't spawn).

    manager.shutdown().await;
}
