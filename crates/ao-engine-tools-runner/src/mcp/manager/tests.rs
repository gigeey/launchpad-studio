//! Unit tests for the MCP server manager.
//!
//! Declared from the parent module as `#[cfg(test)] mod tests;` — this is
//! the same module as the inline `mod tests` block it replaces, so private
//! items of the parent remain in scope here via `use super::*`.

use super::*;
use ao_engine_tools_core::skill_registry::{ContextMode, SkillEntry, SkillRegistry, SkillSource};
use ao_engine_tools_provider_config::mcp_servers::{McpServerEntry, McpServersConfig};
use std::collections::HashMap;
use std::time::Instant;

use crate::mcp::test_support::echo_server_bin;

fn make_config(entries: Vec<McpServerEntry>) -> McpServersConfig {
    McpServersConfig { servers: entries }
}

#[tokio::test]
async fn failure_isolation_one_good_two_bad() {
    let bin = echo_server_bin();
    let bin_str = bin.to_str().unwrap().to_string();

    let mut crash_env = HashMap::new();
    crash_env.insert("MCP_BEHAVIOR".to_string(), "crash".to_string());

    let entries = vec![
        McpServerEntry {
            name: "good".to_string(),
            command: Some(bin_str.clone()),
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Always,
            transport: McpTransportType::Stdio,
            url: None,
            auth: None,
        },
        McpServerEntry {
            name: "bad_command".to_string(),
            command: Some("/nonexistent/binary/path_that_does_not_exist".to_string()),
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Always,
            transport: McpTransportType::Stdio,
            url: None,
            auth: None,
        },
        McpServerEntry {
            name: "crashing".to_string(),
            command: Some(bin_str.clone()),
            args: vec![],
            env: crash_env,
            loading: McpLoadingPolicy::Always,
            transport: McpTransportType::Stdio,
            url: None,
            auth: None,
        },
    ];

    let config = make_config(entries);
    let manager = McpManager::from_config(&config).await;

    let client_count = manager.clients.lock().await.len();
    assert_eq!(client_count, 1, "exactly one server should have spawned successfully");

    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    assert!(
        registry.lookup("mcp__good__echo").is_some(),
        "good server's echo tool should be registered"
    );

    assert!(registry.lookup("mcp__bad_command__echo").is_none());
    assert!(registry.lookup("mcp__crashing__echo").is_none());

    manager.shutdown().await;
}

#[tokio::test]
async fn disabled_server_is_not_spawned() {
    let entries = vec![McpServerEntry {
        name: "disabled_srv".to_string(),
        command: Some("/nonexistent/binary/should_not_be_called".to_string()),
        args: vec![],
        env: HashMap::new(),
        loading: McpLoadingPolicy::Disabled,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    }];

    let config = make_config(entries);
    let manager = McpManager::from_config(&config).await;

    let count = manager.clients.lock().await.len();
    assert_eq!(count, 0, "disabled server should not be spawned");
    manager.shutdown().await;
}

#[tokio::test]
async fn all_servers_spawn_and_shutdown_returns() {
    let bin = echo_server_bin();
    let bin_str = bin.to_str().unwrap().to_string();

    let entries: Vec<McpServerEntry> = (0..5)
        .map(|i| McpServerEntry {
            name: format!("srv{i}"),
            command: Some(bin_str.clone()),
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Always,
            transport: McpTransportType::Stdio,
            url: None,
            auth: None,
        })
        .collect();

    let config = make_config(entries);
    let manager = McpManager::from_config(&config).await;

    let count = manager.clients.lock().await.len();
    assert_eq!(count, 5, "all 5 servers should spawn successfully");

    let start = Instant::now();
    manager.shutdown().await;
    let elapsed = start.elapsed();

    // WHAT THE BOUND CATCHES. `McpClientHandle::shutdown` gives a stdio client a 10 s
    // `shutdown`-RPC timeout, then waits up to 5 s for the child to exit before
    // SIGKILL. A client whose RPC timeout fires pushes elapsed past 6 s and fails
    // here. A client whose RPC answered but whose child hangs lands at ~5 s and
    // passes, so the bound is blind to that half of the shutdown path.
    //
    // WHAT THE BOUND DOES NOT CATCH. It cannot distinguish parallel shutdown from
    // serial. The echo fixture answers `shutdown` in ~2 ms, so five sequential
    // shutdowns and five concurrent ones both return in ~0.01 s; replacing the
    // `join_all` in `McpManager::shutdown` with a sequential loop leaves this test
    // passing.
    //
    // TODO(mcp-shutdown): to assert parallelism, teach the echo fixture a
    // shutdown-delay flag and bound elapsed under the serial cost (5 x delay).
    assert!(
        elapsed.as_secs() < 6,
        "shutdown of 5 servers returned in {elapsed:?}; bound catches a client hitting its 10 s shutdown-RPC timeout"
    );
}

#[tokio::test]
async fn builtin_tools_survive_mcp_registration() {
    let bin = echo_server_bin();
    let bin_str = bin.to_str().unwrap().to_string();

    let entries = vec![McpServerEntry {
        name: "echo_srv".to_string(),
        command: Some(bin_str),
        args: vec![],
        env: HashMap::new(),
        loading: McpLoadingPolicy::Always,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    }];

    let config = make_config(entries);
    let manager = McpManager::from_config(&config).await;

    let mut registry = Registry::new();
    use ao_engine_tools_io::register_all as register_io_tools;
    register_io_tools(&mut registry);
    let builtin_count_before = registry.len();
    assert!(builtin_count_before > 0, "io tools should be registered");

    let manager = manager.register_into(&mut registry).await;

    let after = registry.len();
    assert!(after > builtin_count_before, "MCP tools were added on top of builtins");
    assert!(registry.lookup("mcp__echo_srv__echo").is_some());

    manager.shutdown().await;
}

// ── MCP skill tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn everything_server_prompts_appear_as_skills() {
    let bin = echo_server_bin();
    let bin_str = bin.to_str().unwrap().to_string();

    let mut env = HashMap::new();
    env.insert("MCP_BEHAVIOR".to_string(), "everything".to_string());

    let entries = vec![McpServerEntry {
        name: "everything".to_string(),
        command: Some(bin_str),
        args: vec![],
        env,
        loading: McpLoadingPolicy::Always,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    }];

    let config = make_config(entries);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    assert!(registry.lookup("mcp__everything__echo").is_some());

    let mut skill_registry = SkillRegistry::empty();
    manager.extend_skill_registry(&mut skill_registry);

    assert_eq!(skill_registry.len(), 2, "everything server exposes 2 MCP skills");
    assert!(skill_registry.get("greet").is_some(), "greet skill should be present");
    assert!(skill_registry.get("summarize").is_some(), "summarize skill should be present");

    let greet_entry = skill_registry.get("greet").unwrap();
    if let SkillEntry::Ok(r) = greet_entry {
        assert!(
            matches!(&r.source, SkillSource::Mcp { server_name } if server_name == "everything"),
            "source should be Mcp {{ server_name: 'everything' }}"
        );
        assert!(
            matches!(r.context, ContextMode::Inline),
            "MCP skills are inline"
        );
        assert!(!r.body.is_empty(), "skill body should be non-empty");
    } else {
        panic!("greet entry should be Ok");
    }

    manager.shutdown().await;
}

#[tokio::test]
async fn everything_skill_dispatches_inline_via_run_skill() {
    let bin = echo_server_bin();
    let bin_str = bin.to_str().unwrap().to_string();

    let mut env = HashMap::new();
    env.insert("MCP_BEHAVIOR".to_string(), "everything".to_string());

    let entries = vec![McpServerEntry {
        name: "everything".to_string(),
        command: Some(bin_str),
        args: vec![],
        env,
        loading: McpLoadingPolicy::Always,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    }];

    let config = make_config(entries);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    let mut skill_registry = SkillRegistry::empty();
    manager.extend_skill_registry(&mut skill_registry);

    let greet = skill_registry.get("greet").unwrap();
    if let SkillEntry::Ok(r) = greet {
        assert_eq!(r.name, "greet");
        assert!(matches!(&r.source, SkillSource::Mcp { server_name } if server_name == "everything"));
    } else {
        panic!("greet should be Ok");
    }

    manager.shutdown().await;
}

#[tokio::test]
async fn tool_only_server_yields_zero_mcp_skills() {
    let bin = echo_server_bin();
    let bin_str = bin.to_str().unwrap().to_string();

    let mut env = HashMap::new();
    env.insert("MCP_BEHAVIOR".to_string(), "tool_only".to_string());

    let entries = vec![McpServerEntry {
        name: "tool_only".to_string(),
        command: Some(bin_str),
        args: vec![],
        env,
        loading: McpLoadingPolicy::Always,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    }];

    let config = make_config(entries);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    assert!(registry.lookup("mcp__tool_only__echo").is_some());

    let mut skill_registry = SkillRegistry::empty();
    manager.extend_skill_registry(&mut skill_registry);
    assert!(
        skill_registry.is_empty(),
        "tool-only server should produce zero MCP skills"
    );

    manager.shutdown().await;
}

// ── anthropic/alwaysLoad override test ────────────────────────────────────

#[tokio::test]
async fn always_load_meta_hint_overrides_deferred_server_policy() {
    let bin = echo_server_bin();
    let bin_str = bin.to_str().unwrap().to_string();

    let mut env = HashMap::new();
    env.insert("MCP_BEHAVIOR".to_string(), "tools_with_meta".to_string());

    let entries = vec![McpServerEntry {
        name: "meta_srv".to_string(),
        command: Some(bin_str),
        args: vec![],
        env,
        loading: McpLoadingPolicy::Deferred,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    }];

    let config = make_config(entries);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    let smart = registry.lookup("mcp__meta_srv__smart_query")
        .expect("smart_query should be registered");
    let optional = registry.lookup("mcp__meta_srv__optional_tool")
        .expect("optional_tool should be registered");

    assert_eq!(
        smart.load_policy(),
        LoadPolicy::AlwaysLoad,
        "anthropic/alwaysLoad:true must force AlwaysLoad even when server is Deferred"
    );
    assert_eq!(
        optional.load_policy(),
        LoadPolicy::Deferred,
        "tool without alwaysLoad hint should inherit the server-level Deferred policy"
    );

    manager.shutdown().await;
}

// ── Auth-aware from_config_auth tests ─────────────────────────────────────

#[tokio::test]
async fn needs_auth_http_server_gets_auth_pseudo_tool() {
    use axum::{routing::post, Router};
    use ao_engine_tools_provider_config::mcp_token_store::McpTokenStore;
    use tempfile::tempdir;

    let app = Router::new().route(
        "/mcp",
        post(|| async {
            (axum::http::StatusCode::UNAUTHORIZED, "Unauthorized")
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let url = format!("http://127.0.0.1:{}/mcp", addr.port());

    let dir = tempdir().unwrap();
    let token_store = Arc::new(McpTokenStore::new_with_file_fallback(
        dir.path().to_path_buf(),
    ));

    let entries = vec![McpServerEntry {
        name: "auth_srv".to_string(),
        transport: McpTransportType::Http,
        url: Some(url),
        loading: McpLoadingPolicy::Always,
        command: None,
        args: vec![],
        env: HashMap::new(),
        auth: Some(McpAuthConfig::default()),
    }];
    let config = McpServersConfig { servers: entries };

    let manager = McpManager::from_config_auth(&config, token_store).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    assert!(
        registry.lookup("mcp__auth_srv__authorize").is_some(),
        "auth pseudo-tool should be registered for a 401 server"
    );

    let list = registry.list();
    let real_tools: Vec<_> = list
        .iter()
        .filter(|n| n.starts_with("mcp__auth_srv__") && **n != "mcp__auth_srv__authorize")
        .collect();
    assert!(
        real_tools.is_empty(),
        "no real tools should be registered for a 401 server; got: {real_tools:?}"
    );

    manager.shutdown().await;
}

#[tokio::test]
async fn stored_token_enables_direct_http_connect() {
    use axum::{Router, routing::post};
    use axum::http::HeaderMap;
    use ao_engine_tools_provider_config::mcp_token_store::{McpTokenStore, McpTokenRecord, derive_server_key};
    use tempfile::tempdir;

    let app = Router::new().route(
        "/mcp",
        post(|headers: HeaderMap, body: axum::body::Bytes| async move {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !auth.starts_with("Bearer ") {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::Value::Null),
                );
            }

            let req_body: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let method = req_body
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("");
            let id = req_body.get("id").cloned().unwrap_or(serde_json::json!(1));

            let response = match method {
                "initialize" => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "serverInfo": { "name": "test-server", "version": "1.0" },
                        "capabilities": { "tools": {} }
                    }
                }),
                "notifications/initialized" => {
                    return (axum::http::StatusCode::OK, axum::Json(serde_json::Value::Null));
                }
                "tools/list" => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [
                            {
                                "name": "greet",
                                "description": "say hello",
                                "inputSchema": { "type": "object", "properties": {} }
                            }
                        ]
                    }
                }),
                _ => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {}
                }),
            };
            (axum::http::StatusCode::OK, axum::Json(response))
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let url = format!("http://127.0.0.1:{}/mcp", addr.port());

    let dir = tempdir().unwrap();
    let token_store = Arc::new(McpTokenStore::new_with_file_fallback(
        dir.path().to_path_buf(),
    ));

    let server_key = derive_server_key("tokened_srv", Some(&url), "http");
    let record = McpTokenRecord {
        access_token: "valid-bearer-token".to_string(),
        refresh_token: None,
        expires_at: Some(
            chrono::Utc::now() + chrono::Duration::hours(1),
        ),
        scope: None,
        client_id: "test-client".to_string(),
        client_secret: None,
        token_endpoint: None,
    };
    token_store.set(&server_key, &record).unwrap();

    let entries = vec![McpServerEntry {
        name: "tokened_srv".to_string(),
        transport: McpTransportType::Http,
        url: Some(url),
        loading: McpLoadingPolicy::Always,
        command: None,
        args: vec![],
        env: HashMap::new(),
        auth: Some(McpAuthConfig::default()),
    }];
    let config = McpServersConfig { servers: entries };

    let manager = McpManager::from_config_auth(&config, token_store).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    assert!(
        registry.lookup("mcp__tokened_srv__greet").is_some(),
        "real tool should be registered when a valid stored token is present"
    );

    assert!(
        registry.lookup("mcp__tokened_srv__authorize").is_none(),
        "auth pseudo-tool should NOT be registered when a valid token is present"
    );

    manager.shutdown().await;
}

// ── server_statuses tests ─────────────────────────────────────────────────

#[tokio::test]
async fn server_statuses_connected_includes_tool_names() {
    let bin = echo_server_bin();
    let bin_str = bin.to_str().unwrap().to_string();

    let entries = vec![McpServerEntry {
        name: "status_echo".to_string(),
        command: Some(bin_str),
        args: vec![],
        env: HashMap::new(),
        loading: McpLoadingPolicy::Always,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    }];

    let config = make_config(entries);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    let statuses = manager.server_statuses().await;
    assert_eq!(statuses.len(), 1);

    let s = &statuses[0];
    assert_eq!(s.name, "status_echo");
    assert_eq!(s.transport, "stdio");
    assert_eq!(s.state, McpServerState::Connected);
    assert!(s.error.is_none());
    assert!(!s.tool_names.is_empty(), "echo server should expose at least one tool");
    assert!(s.tool_names.contains(&"echo".to_string()), "echo tool should be listed");
    assert_eq!(s.source, "config");

    manager.shutdown().await;
}

#[tokio::test]
async fn server_statuses_failed_server_included_with_error() {
    let entries = vec![
        McpServerEntry {
            name: "ok_srv".to_string(),
            command: Some(echo_server_bin().to_str().unwrap().to_string()),
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Always,
            transport: McpTransportType::Stdio,
            url: None,
            auth: None,
        },
        McpServerEntry {
            name: "bad_srv".to_string(),
            command: Some("/nonexistent/no_such_binary".to_string()),
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Always,
            transport: McpTransportType::Stdio,
            url: None,
            auth: None,
        },
    ];

    let config = make_config(entries);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    let statuses = manager.server_statuses().await;
    assert_eq!(statuses.len(), 2, "both servers should appear in statuses");

    let connected: Vec<_> = statuses.iter().filter(|s| s.state == McpServerState::Connected).collect();
    let errored: Vec<_> = statuses.iter().filter(|s| s.state == McpServerState::Error).collect();

    assert_eq!(connected.len(), 1);
    assert_eq!(connected[0].name, "ok_srv");

    assert_eq!(errored.len(), 1);
    assert_eq!(errored[0].name, "bad_srv");
    assert_eq!(errored[0].transport, "stdio");
    assert!(errored[0].error.is_some(), "error field must be set for failed server");
    assert!(errored[0].tool_names.is_empty());
    assert_eq!(errored[0].source, "config");

    manager.shutdown().await;
}

#[tokio::test]
async fn server_statuses_disabled_server_included() {
    // Deliberately not the echo fixture: every assertion below is about the
    // *disabled* entry, so whether `active_srv` spawns is irrelevant here.
    let entries = vec![
        McpServerEntry {
            name: "active_srv".to_string(),
            command: Some("/nonexistent/never_spawned".to_string()),
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Always,
            transport: McpTransportType::Stdio,
            url: None,
            auth: None,
        },
        McpServerEntry {
            name: "off_srv".to_string(),
            command: Some("/nonexistent/should_not_run".to_string()),
            args: vec![],
            env: HashMap::new(),
            loading: McpLoadingPolicy::Disabled,
            transport: McpTransportType::Stdio,
            url: None,
            auth: None,
        },
    ];

    let config = make_config(entries);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    let statuses = manager.server_statuses().await;
    assert_eq!(statuses.len(), 2);

    let disabled: Vec<_> = statuses.iter().filter(|s| s.state == McpServerState::Disabled).collect();
    assert_eq!(disabled.len(), 1);
    assert_eq!(disabled[0].name, "off_srv");
    assert_eq!(disabled[0].transport, "stdio");
    assert!(disabled[0].tool_names.is_empty());
    assert!(disabled[0].error.is_none());
    assert_eq!(disabled[0].source, "config");

    manager.shutdown().await;
}

#[tokio::test]
async fn server_statuses_needs_auth_shows_correct_state() {
    use axum::{routing::post, Router};
    use ao_engine_tools_provider_config::mcp_token_store::McpTokenStore;
    use tempfile::tempdir;

    let app = Router::new().route(
        "/mcp",
        post(|| async { (axum::http::StatusCode::UNAUTHORIZED, "Unauthorized") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let url = format!("http://127.0.0.1:{}/mcp", addr.port());
    let dir = tempdir().unwrap();
    let token_store = Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));

    let entries = vec![McpServerEntry {
        name: "oauth_srv".to_string(),
        transport: McpTransportType::Http,
        url: Some(url.clone()),
        loading: McpLoadingPolicy::Always,
        command: None,
        args: vec![],
        env: HashMap::new(),
        auth: Some(McpAuthConfig::default()),
    }];
    let config = McpServersConfig { servers: entries };

    let manager = McpManager::from_config_auth(&config, token_store).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    let statuses = manager.server_statuses().await;
    assert_eq!(statuses.len(), 1);

    let s = &statuses[0];
    assert_eq!(s.name, "oauth_srv");
    assert_eq!(s.transport, "http");
    assert_eq!(s.state, McpServerState::NeedsAuth);
    assert!(s.error.is_none());
    assert!(s.tool_names.is_empty());
    assert_eq!(s.source, "config");
    assert_eq!(s.endpoint, url);

    manager.shutdown().await;
}

// ── shared post-auth promotion path ───────────────────────────────────────

/// Regression test for the bug where a server authorized through the
/// agent-facing `mcp__<name>__authorize` pseudo-tool stayed reported as
/// `NeedsAuth`. Both OAuth completion paths now funnel through
/// [`McpManager::complete_authorization`]; this exercises that shared path
/// directly and asserts the manager's tracked state flips to `Connected`
/// (the value the UI badge reads) in addition to swapping the registry tools.
#[tokio::test]
async fn complete_authorization_promotes_needs_auth_to_connected() {
    use axum::http::HeaderMap;
    use axum::{routing::post, Router};
    use ao_engine_tools_provider_config::mcp_token_store::{
        derive_server_key, McpTokenRecord, McpTokenStore,
    };
    use tempfile::tempdir;

    // Mock server: 401 without a bearer token (startup handshake), real MCP
    // responses once a bearer token is presented (post-auth reconnect).
    let app = Router::new().route(
        "/mcp",
        post(|headers: HeaderMap, body: axum::body::Bytes| async move {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !auth.starts_with("Bearer ") {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::Value::Null),
                );
            }
            let req_body: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let method = req_body.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let id = req_body.get("id").cloned().unwrap_or(serde_json::json!(1));
            let response = match method {
                "initialize" => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "serverInfo": { "name": "test-server", "version": "1.0" },
                        "capabilities": { "tools": {} }
                    }
                }),
                "notifications/initialized" => {
                    return (axum::http::StatusCode::OK, axum::Json(serde_json::Value::Null));
                }
                "tools/list" => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [
                            {
                                "name": "greet",
                                "description": "say hello",
                                "inputSchema": { "type": "object", "properties": {} }
                            }
                        ]
                    }
                }),
                _ => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            };
            (axum::http::StatusCode::OK, axum::Json(response))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let url = format!("http://127.0.0.1:{}/mcp", addr.port());
    let dir = tempdir().unwrap();
    let token_store = Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));

    // Startup with no stored token → server answers 401 → tracked NeedsAuth.
    let entries = vec![McpServerEntry {
        name: "promote_srv".to_string(),
        transport: McpTransportType::Http,
        url: Some(url.clone()),
        loading: McpLoadingPolicy::Always,
        command: None,
        args: vec![],
        env: HashMap::new(),
        auth: Some(McpAuthConfig::default()),
    }];
    let config = McpServersConfig { servers: entries };

    let manager = McpManager::from_config_auth(&config, Arc::clone(&token_store)).await;
    let mut registry = Registry::new();
    let manager = Arc::new(manager.register_into(&mut registry).await);
    let registry = Arc::new(registry);

    // Precondition: NeedsAuth, auth pseudo-tool present, no real tool yet.
    let before = manager.server_statuses().await;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].state, McpServerState::NeedsAuth);
    assert!(registry.lookup("mcp__promote_srv__authorize").is_some());
    assert!(registry.lookup("mcp__promote_srv__greet").is_none());

    // Simulate a completed OAuth flow: a valid token now sits in the store.
    let server_key = derive_server_key("promote_srv", Some(&url), "http");
    let record = McpTokenRecord {
        access_token: "valid-bearer-token".to_string(),
        refresh_token: None,
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        scope: None,
        client_id: "test-client".to_string(),
        client_secret: None,
        token_endpoint: None,
    };
    token_store.set(&server_key, &record).unwrap();

    // Drive the shared promotion path (the same call both OAuth entry points
    // make once the browser callback succeeds).
    manager
        .complete_authorization("promote_srv", Arc::clone(&registry))
        .await;

    // The manager must now report Connected — this is the regression guard.
    let after = manager.server_statuses().await;
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].state,
        McpServerState::Connected,
        "server must be promoted to Connected after authorization completes"
    );
    assert!(after[0].tool_names.contains(&"greet".to_string()));

    // Registry must have swapped the pseudo-tool for the real tool.
    assert!(
        registry.lookup("mcp__promote_srv__authorize").is_none(),
        "auth pseudo-tool should be removed after promotion"
    );
    assert!(
        registry.lookup("mcp__promote_srv__greet").is_some(),
        "real server tool should be registered after promotion"
    );
}

// ── state-independent reauthorization ─────────────────────────────────────
//
// A recovery action must not be gated behind the same health signal it
// exists to fix — these guard `resolve_reauth_target` and
// `complete_authorization` against regressing back to "only works from
// needs_auth", which was the bug this module was extended to fix (e.g. a
// server whose `tools/list` succeeds unauthenticated but every real call
// 401s, so the app never shows it as needing auth).

/// A bearer-gated MCP mock server: 401 without `Authorization: Bearer`,
/// full `initialize`/`tools/list` responses with any bearer value.
fn spawn_bearer_mcp_server() -> tokio::task::JoinHandle<std::net::SocketAddr> {
    use axum::http::HeaderMap;
    use axum::{routing::post, Router};

    tokio::spawn(async move {
        let app = Router::new().route(
            "/mcp",
            post(|headers: HeaderMap, body: axum::body::Bytes| async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if !auth.starts_with("Bearer ") {
                    return (
                        axum::http::StatusCode::UNAUTHORIZED,
                        axum::Json(serde_json::Value::Null),
                    );
                }
                let req_body: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                let method = req_body.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let id = req_body.get("id").cloned().unwrap_or(serde_json::json!(1));
                let response = match method {
                    "initialize" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "serverInfo": { "name": "test-server", "version": "1.0" },
                            "capabilities": { "tools": {} }
                        }
                    }),
                    "notifications/initialized" => {
                        return (axum::http::StatusCode::OK, axum::Json(serde_json::Value::Null));
                    }
                    "tools/list" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [
                                {
                                    "name": "greet",
                                    "description": "say hello",
                                    "inputSchema": { "type": "object", "properties": {} }
                                }
                            ]
                        }
                    }),
                    _ => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
                };
                (axum::http::StatusCode::OK, axum::Json(response))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    })
}

#[tokio::test]
async fn resolve_reauth_target_finds_already_connected_http_server() {
    use ao_engine_tools_provider_config::mcp_token_store::{
        derive_server_key, McpTokenRecord, McpTokenStore,
    };
    use tempfile::tempdir;

    let addr = spawn_bearer_mcp_server().await.unwrap();
    let url = format!("http://127.0.0.1:{}/mcp", addr.port());

    let dir = tempdir().unwrap();
    let token_store = Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));
    let server_key = derive_server_key("connected_srv", Some(&url), "http");
    token_store
        .set(&server_key, &McpTokenRecord {
            access_token: "valid-bearer-token".to_string(),
            refresh_token: None,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            scope: None,
            client_id: "test-client".to_string(),
            client_secret: None,
            token_endpoint: None,
        })
        .unwrap();

    let entries = vec![McpServerEntry {
        name: "connected_srv".to_string(),
        transport: McpTransportType::Http,
        url: Some(url.clone()),
        loading: McpLoadingPolicy::Always,
        command: None,
        args: vec![],
        env: HashMap::new(),
        auth: Some(McpAuthConfig::default()),
    }];
    let config = McpServersConfig { servers: entries };

    let manager = McpManager::from_config_auth(&config, token_store).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    // Precondition: the server connected directly (valid stored token), so
    // it never touched `needs_auth` — this is the Notion-shaped bug case.
    let statuses = manager.server_statuses().await;
    assert_eq!(statuses[0].state, McpServerState::Connected);

    let (resolved_url, _auth_config) = manager
        .resolve_reauth_target("connected_srv")
        .await
        .expect("a connected server must resolve a reauth target, not just needs_auth ones");
    assert_eq!(resolved_url, url);

    manager.shutdown().await;
}

#[tokio::test]
async fn resolve_reauth_target_rejects_stdio_transport() {
    // Deliberately not the echo fixture: the rejection under test keys off the
    // transport type alone, so this server never needs to spawn successfully.
    let entries = vec![McpServerEntry {
        name: "stdio_srv".to_string(),
        command: Some("/nonexistent/never_spawned".to_string()),
        args: vec![],
        env: HashMap::new(),
        loading: McpLoadingPolicy::Always,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    }];

    let config = make_config(entries);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    let err = manager
        .resolve_reauth_target("stdio_srv")
        .await
        .expect_err("a stdio server has no browser-based auth flow");
    assert!(err.contains("stdio"), "error should name the offending transport: {err}");

    manager.shutdown().await;
}

#[tokio::test]
async fn resolve_reauth_target_errors_for_unknown_server() {
    let config = make_config(vec![]);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = manager.register_into(&mut registry).await;

    assert!(manager.resolve_reauth_target("ghost").await.is_err());
    manager.shutdown().await;
}

/// The core fix: reauthorizing a server that is already `Connected` must
/// rotate its credential in place (single entry, state stays `Connected`,
/// tools stay registered) rather than being rejected because it never
/// entered `needs_auth`, and without leaving a duplicate entry behind.
#[tokio::test]
async fn complete_authorization_rotates_credential_for_connected_server() {
    use ao_engine_tools_provider_config::mcp_token_store::{
        derive_server_key, McpTokenRecord, McpTokenStore,
    };
    use tempfile::tempdir;

    let addr = spawn_bearer_mcp_server().await.unwrap();
    let url = format!("http://127.0.0.1:{}/mcp", addr.port());

    let dir = tempdir().unwrap();
    let token_store = Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));
    let server_key = derive_server_key("rotate_srv", Some(&url), "http");
    token_store
        .set(&server_key, &McpTokenRecord {
            access_token: "old-bearer-token".to_string(),
            refresh_token: None,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            scope: None,
            client_id: "test-client".to_string(),
            client_secret: None,
            token_endpoint: None,
        })
        .unwrap();

    let entries = vec![McpServerEntry {
        name: "rotate_srv".to_string(),
        transport: McpTransportType::Http,
        url: Some(url.clone()),
        loading: McpLoadingPolicy::Always,
        command: None,
        args: vec![],
        env: HashMap::new(),
        auth: Some(McpAuthConfig::default()),
    }];
    let config = McpServersConfig { servers: entries };

    let manager = McpManager::from_config_auth(&config, Arc::clone(&token_store)).await;
    let mut registry = Registry::new();
    let manager = Arc::new(manager.register_into(&mut registry).await);
    let registry = Arc::new(registry);

    let before = manager.server_statuses().await;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].state, McpServerState::Connected, "starts connected with the old token");
    assert!(registry.lookup("mcp__rotate_srv__greet").is_some());

    // Simulate a completed reauth: a freshly rotated token now sits in the
    // store (the old credential is never cleared up front — it's simply
    // superseded once the new one is persisted, same as the needs_auth path).
    token_store
        .set(&server_key, &McpTokenRecord {
            access_token: "rotated-bearer-token".to_string(),
            refresh_token: None,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            scope: None,
            client_id: "test-client".to_string(),
            client_secret: None,
            token_endpoint: None,
        })
        .unwrap();

    manager.complete_authorization("rotate_srv", Arc::clone(&registry)).await;

    let after = manager.server_statuses().await;
    assert_eq!(
        after.len(),
        1,
        "reauthorizing an already-connected server must not create a duplicate tracked entry"
    );
    assert_eq!(
        after[0].state,
        McpServerState::Connected,
        "an already-connected server must stay Connected through reauth, not bounce through another state"
    );
    assert!(
        registry.lookup("mcp__rotate_srv__greet").is_some(),
        "tools must still be bound after the in-place credential swap"
    );
}

/// A server whose last connection attempt failed (tracked in `failed`, not
/// `needs_auth`) must also be reachable for reauthorization and promoted
/// to `Connected` on success — the same recovery path `needs_auth` gets.
#[tokio::test]
async fn complete_authorization_promotes_failed_server_to_connected() {
    use ao_engine_tools_provider_config::mcp_token_store::{
        derive_server_key, McpTokenRecord, McpTokenStore,
    };
    use tempfile::tempdir;

    // Reserve a port, then drop the listener so the manager's initial
    // connect attempt is refused and the entry lands in `failed`.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let url = format!("http://127.0.0.1:{port}/mcp");

    let dir = tempdir().unwrap();
    let token_store = Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));

    let entries = vec![McpServerEntry {
        name: "failed_srv".to_string(),
        transport: McpTransportType::Http,
        url: Some(url.clone()),
        loading: McpLoadingPolicy::Always,
        command: None,
        args: vec![],
        env: HashMap::new(),
        auth: Some(McpAuthConfig::default()),
    }];
    let config = McpServersConfig { servers: entries };

    let manager = McpManager::from_config_auth(&config, Arc::clone(&token_store)).await;
    let mut registry = Registry::new();
    let manager = Arc::new(manager.register_into(&mut registry).await);
    let registry = Arc::new(registry);

    let before = manager.server_statuses().await;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].state, McpServerState::Error, "nothing was listening on the port yet");

    // Now bring the real server up on the same port and store a token, as
    // if the user reauthorized a server that previously failed to connect.
    let app = {
        use axum::http::HeaderMap;
        use axum::{routing::post, Router};
        Router::new().route(
            "/mcp",
            post(|headers: HeaderMap, body: axum::body::Bytes| async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if !auth.starts_with("Bearer ") {
                    return (axum::http::StatusCode::UNAUTHORIZED, axum::Json(serde_json::Value::Null));
                }
                let req_body: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                let method = req_body.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let id = req_body.get("id").cloned().unwrap_or(serde_json::json!(1));
                let response = match method {
                    "initialize" => serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "serverInfo": { "name": "test-server", "version": "1.0" },
                            "capabilities": { "tools": {} }
                        }
                    }),
                    "notifications/initialized" => {
                        return (axum::http::StatusCode::OK, axum::Json(serde_json::Value::Null));
                    }
                    "tools/list" => serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "tools": [
                            { "name": "greet", "description": "say hello", "inputSchema": { "type": "object", "properties": {} } }
                        ] }
                    }),
                    _ => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
                };
                (axum::http::StatusCode::OK, axum::Json(response))
            }),
        )
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let server_key = derive_server_key("failed_srv", Some(&url), "http");
    token_store
        .set(&server_key, &McpTokenRecord {
            access_token: "valid-bearer-token".to_string(),
            refresh_token: None,
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            scope: None,
            client_id: "test-client".to_string(),
            client_secret: None,
            token_endpoint: None,
        })
        .unwrap();

    manager.complete_authorization("failed_srv", Arc::clone(&registry)).await;

    let after = manager.server_statuses().await;
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].state,
        McpServerState::Connected,
        "a previously-failed server must be promoted to Connected after a successful reauth"
    );
    assert!(registry.lookup("mcp__failed_srv__greet").is_some());
}

// ── add_server / remove_server lifecycle tests ────────────────────────────

#[tokio::test]
async fn add_server_stdio_lifecycle() {
    let bin = echo_server_bin();
    let bin_str = bin.to_str().unwrap().to_string();

    // Start with an empty manager.
    let config = make_config(vec![]);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = Arc::new(manager.register_into(&mut registry).await);
    let registry = Arc::new(registry);

    // Statuses should be empty at start.
    assert!(manager.server_statuses().await.is_empty());

    // Add a stdio server.
    let entry = McpServerEntry {
        name: "rt_echo".to_string(),
        command: Some(bin_str),
        args: vec![],
        env: HashMap::new(),
        loading: McpLoadingPolicy::Always,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    };

    let status = manager
        .add_server(entry, Arc::clone(&registry), "config".to_string())
        .await
        .expect("add_server should succeed for a valid echo server");

    assert_eq!(status.name, "rt_echo");
    assert_eq!(status.state, McpServerState::Connected);
    assert!(!status.tool_names.is_empty(), "echo server should advertise tools");

    // Tool should be visible in the registry.
    assert!(
        registry.lookup("mcp__rt_echo__echo").is_some(),
        "echo tool should be registered after add_server"
    );

    // Status list should reflect the new server.
    let statuses = manager.server_statuses().await;
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].name, "rt_echo");
    assert_eq!(statuses[0].state, McpServerState::Connected);

    // Remove the server.
    manager
        .remove_server("rt_echo", &registry)
        .await
        .expect("remove_server should succeed for a tracked server");

    // Tool should be gone from the registry.
    assert!(
        registry.lookup("mcp__rt_echo__echo").is_none(),
        "echo tool should be unregistered after remove_server"
    );

    // Status list should be empty again.
    assert!(manager.server_statuses().await.is_empty());
}

#[tokio::test]
async fn add_server_duplicate_name_rejected() {
    let bin = echo_server_bin();
    let bin_str = bin.to_str().unwrap().to_string();

    let entry = McpServerEntry {
        name: "dup_srv".to_string(),
        command: Some(bin_str.clone()),
        args: vec![],
        env: HashMap::new(),
        loading: McpLoadingPolicy::Always,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    };

    let config = make_config(vec![]);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = Arc::new(manager.register_into(&mut registry).await);
    let registry = Arc::new(registry);

    // First add succeeds.
    manager
        .add_server(entry.clone(), Arc::clone(&registry), "config".to_string())
        .await
        .expect("first add_server should succeed");

    // Second add with the same name should fail.
    let err = manager
        .add_server(entry, Arc::clone(&registry), "config".to_string())
        .await
        .expect_err("duplicate name should be rejected");

    assert!(
        matches!(err, McpManagerError::DuplicateName(ref n) if n == "dup_srv"),
        "expected DuplicateName error, got: {err:?}"
    );

    manager.remove_server("dup_srv", &registry).await.unwrap();
}

#[tokio::test]
async fn remove_server_not_found_returns_error() {
    let config = make_config(vec![]);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = Arc::new(manager.register_into(&mut registry).await);

    let err = manager
        .remove_server("nonexistent_srv", &registry)
        .await
        .expect_err("removing an unknown server should return an error");

    assert!(
        matches!(err, McpManagerError::NotFound(ref n) if n == "nonexistent_srv"),
        "expected NotFound error, got: {err:?}"
    );
}

#[tokio::test]
async fn add_server_connection_failure_returns_error() {
    let config = make_config(vec![]);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = Arc::new(manager.register_into(&mut registry).await);
    let registry = Arc::new(registry);

    let entry = McpServerEntry {
        name: "bad_rt".to_string(),
        command: Some("/nonexistent/binary_that_does_not_exist".to_string()),
        args: vec![],
        env: HashMap::new(),
        loading: McpLoadingPolicy::Always,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    };

    let err = manager
        .add_server(entry, Arc::clone(&registry), "config".to_string())
        .await
        .expect_err("connecting to a nonexistent binary should fail");

    assert!(
        matches!(err, McpManagerError::ConnectionFailed(_)),
        "expected ConnectionFailed, got: {err:?}"
    );

    // Nothing should have been added to the manager.
    assert!(manager.server_statuses().await.is_empty());
}

#[tokio::test]
async fn remove_server_clears_needs_auth_entry() {
    use axum::{routing::post, Router};
    use ao_engine_tools_provider_config::mcp_token_store::McpTokenStore;
    use tempfile::tempdir;

    let app = Router::new().route(
        "/mcp",
        post(|| async { (axum::http::StatusCode::UNAUTHORIZED, "Unauthorized") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let url = format!("http://127.0.0.1:{}/mcp", addr.port());
    let dir = tempdir().unwrap();
    let token_store = Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));

    // Bootstrap manager with auth token store.
    let config = McpServersConfig { servers: vec![] };
    let manager = McpManager::from_config_auth(&config, Arc::clone(&token_store)).await;
    let mut registry = Registry::new();
    let manager = Arc::new(manager.register_into(&mut registry).await);
    let registry = Arc::new(registry);

    // Add an HTTP server that requires auth (returns 401).
    let entry = McpServerEntry {
        name: "auth_rt".to_string(),
        transport: McpTransportType::Http,
        url: Some(url.clone()),
        loading: McpLoadingPolicy::Always,
        command: None,
        args: vec![],
        env: HashMap::new(),
        auth: Some(McpAuthConfig::default()),
    };

    let status = manager
        .add_server(entry, Arc::clone(&registry), "config".to_string())
        .await
        .expect("add_server should return NeedsAuth status, not an error");

    assert_eq!(status.state, McpServerState::NeedsAuth);

    // Auth pseudo-tool should be in the registry.
    assert!(
        registry.lookup("mcp__auth_rt__authorize").is_some(),
        "auth pseudo-tool should be registered for a 401 server"
    );

    // Status list should show NeedsAuth.
    let statuses = manager.server_statuses().await;
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].state, McpServerState::NeedsAuth);

    // Remove the server.
    manager
        .remove_server("auth_rt", &registry)
        .await
        .expect("remove_server should succeed for a NeedsAuth server");

    // Auth pseudo-tool should be gone.
    assert!(
        registry.lookup("mcp__auth_rt__authorize").is_none(),
        "auth pseudo-tool should be removed after remove_server"
    );

    // Status list should be empty.
    assert!(manager.server_statuses().await.is_empty());
}

#[tokio::test]
async fn server_source_reflects_add_source() {
    let bin = echo_server_bin();
    let bin_str = bin.to_str().unwrap().to_string();

    let config = make_config(vec![]);
    let manager = McpManager::from_config(&config).await;
    let mut registry = Registry::new();
    let manager = Arc::new(manager.register_into(&mut registry).await);
    let registry = Arc::new(registry);

    let entry = McpServerEntry {
        name: "my-plugin:search".to_string(),
        command: Some(bin_str),
        args: vec![],
        env: HashMap::new(),
        loading: McpLoadingPolicy::Always,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    };

    manager
        .add_server(entry, Arc::clone(&registry), "plugin:my-plugin".to_string())
        .await
        .expect("add_server should succeed");

    let source = manager.server_source("my-plugin:search").await;
    assert_eq!(source.as_deref(), Some("plugin:my-plugin"));

    let statuses = manager.server_statuses().await;
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].source, "plugin:my-plugin");

    manager.remove_server("my-plugin:search", &registry).await.unwrap();
}
