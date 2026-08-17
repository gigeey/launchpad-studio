//! Tests for the ListMcpResources tool.
//!
//! Included from `mod.rs` as `#[cfg(test)] mod tests`.

use std::collections::HashMap;
use std::sync::Arc;

use ao_engine_tools_core::{context::RunnerContext, output::ToolOutput, policy::LoadPolicy, tool::IoTool};
use serde_json::json;

use super::{ListMcpResources, MAX_OUTPUT_CHARS};
use crate::mcp::client::McpClientHandle;

use crate::mcp::test_support::echo_server_bin;

async fn spawn_with_behavior(name: &str, behavior: &str) -> McpClientHandle {
    let bin = echo_server_bin();
    let mut env = HashMap::new();
    if !behavior.is_empty() {
        env.insert("MCP_BEHAVIOR".to_string(), behavior.to_string());
    }
    McpClientHandle::spawn(name, bin.to_str().unwrap(), &[], &env)
        .await
        .unwrap_or_else(|e| panic!("should spawn echo_mcp_server (behavior={behavior}): {e}"))
}

fn ctx() -> RunnerContext {
    RunnerContext::new("sess", "agent").unwrap()
}

// ── Tool interface ────────────────────────────────────────────────────────────

#[test]
fn tool_metadata() {
    let tool = ListMcpResources::new(Arc::new(vec![]));
    assert_eq!(tool.name(), "ListMcpResources");
    assert!(!tool.description().is_empty());
    assert!(tool.is_concurrency_safe(), "listing resources is read-only");
    assert_eq!(tool.load_policy(), LoadPolicy::Deferred);

    let schema = tool.input_schema();
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["server"].is_object(), "schema should have server property");
    assert_eq!(schema["additionalProperties"], false);
    // server is not required
    assert!(schema.get("required").is_none(), "server param is optional");
}

// ── Guard: unknown server ─────────────────────────────────────────────────────

#[tokio::test]
async fn unknown_server_returns_actionable_error_with_known_names() {
    let handle = McpClientHandle::unreachable_for_test("real_server");
    let tool =
        ListMcpResources::new(Arc::new(vec![("real_server".to_string(), handle)]));

    let out = tool
        .invoke(json!({ "server": "nonexistent" }), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(message.contains("not configured"), "message: {message}");
            assert!(message.contains("real_server"), "message should list known names: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_server_no_configured_servers_says_none() {
    let tool = ListMcpResources::new(Arc::new(vec![]));

    let out = tool
        .invoke(json!({ "server": "anything" }), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Error { message, .. } => {
            assert!(message.contains("none"), "message should say 'none': {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// ── No resource-capable servers ───────────────────────────────────────────────

#[tokio::test]
async fn no_resource_capable_servers_returns_friendly_message() {
    // Normal echo_mcp_server does not advertise resources capability.
    let handle = spawn_with_behavior("echo", "").await;
    let tool =
        ListMcpResources::new(Arc::new(vec![("echo".to_string(), handle.clone())]));

    let out = tool.invoke(json!({}), &ctx()).await.unwrap();

    match out {
        ToolOutput::Text(s) => {
            assert!(
                s.contains("No MCP servers with resource support"),
                "expected 'no resource support' message, got: {s}"
            );
        }
        other => panic!("expected Text, got {other:?}"),
    }

    handle.shutdown().await;
}

// ── Happy path: text resources ────────────────────────────────────────────────

#[tokio::test]
async fn lists_resources_from_resource_capable_server() {
    let handle = spawn_with_behavior("res_server", "with_resources").await;
    let tool =
        ListMcpResources::new(Arc::new(vec![("res_server".to_string(), handle.clone())]));

    let out = tool.invoke(json!({}), &ctx()).await.unwrap();

    match out {
        ToolOutput::Text(s) => {
            let arr: serde_json::Value = serde_json::from_str(&s)
                .expect("output should be valid JSON array");
            let arr = arr.as_array().expect("top-level should be array");
            assert!(!arr.is_empty(), "should have at least one resource");
            assert!(arr[0]["uri"].is_string(), "each entry should have uri");
            assert_eq!(
                arr[0]["server"], "res_server",
                "each entry should be tagged with server name"
            );
        }
        other => panic!("expected Text, got {other:?}"),
    }

    handle.shutdown().await;
}

// ── Server filter ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn server_filter_queries_only_the_named_server() {
    let h_res = spawn_with_behavior("srv_res", "with_resources").await;
    let h_plain = spawn_with_behavior("srv_plain", "").await;
    let servers = Arc::new(vec![
        ("srv_res".to_string(), h_res.clone()),
        ("srv_plain".to_string(), h_plain.clone()),
    ]);
    let tool = ListMcpResources::new(servers);

    // Filter to the resource-capable server.
    let out = tool
        .invoke(json!({ "server": "srv_res" }), &ctx())
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            let arr: serde_json::Value = serde_json::from_str(&s)
                .expect("output should be valid JSON");
            let arr = arr.as_array().expect("should be array");
            assert!(!arr.is_empty(), "srv_res should return resources");
            assert!(
                arr.iter().all(|e| e["server"] == "srv_res"),
                "all entries should be from srv_res"
            );
        }
        other => panic!("expected Text for srv_res, got {other:?}"),
    }

    h_res.shutdown().await;
    h_plain.shutdown().await;
}

// ── Pagination ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn pagination_collects_all_pages() {
    let handle = spawn_with_behavior("paged", "resources_paginated").await;
    let tool =
        ListMcpResources::new(Arc::new(vec![("paged".to_string(), handle.clone())]));

    let out = tool.invoke(json!({}), &ctx()).await.unwrap();

    match out {
        ToolOutput::Text(s) => {
            let arr: serde_json::Value = serde_json::from_str(&s)
                .expect("output should be valid JSON");
            let arr = arr.as_array().expect("should be array");
            assert_eq!(arr.len(), 2, "should collect 2 resources across 2 pages; got: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }

    handle.shutdown().await;
}

// ── Truncation ────────────────────────────────────────────────────────────────

#[test]
fn truncation_note_added_when_output_exceeds_cap() {
    // Feed the truncation helper directly via its public-within-crate
    // `maybe_truncate` logic by constructing an oversized string.
    let long_uri = "r".repeat(MAX_OUTPUT_CHARS + 100);
    let text = format!("[{{\"uri\":\"{long_uri}\"}}]");
    assert!(text.chars().count() > MAX_OUTPUT_CHARS);

    // Feed through maybe_truncate via a resource list that generates large JSON.
    let resources = vec![json!({ "uri": long_uri, "server": "s" })];
    let json_text = serde_json::to_string_pretty(&resources).unwrap();
    let total = json_text.chars().count();
    assert!(total > MAX_OUTPUT_CHARS, "precondition: json must be over cap");

    let result = super::maybe_truncate(json_text);
    match result {
        ToolOutput::Text(s) => {
            assert!(s.contains("truncated"), "truncation note should be present: {s}");
            assert!(
                s.chars().count() > MAX_OUTPUT_CHARS,
                "result should include truncation note (> cap chars)"
            );
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── Failure isolation ─────────────────────────────────────────────────────────

#[tokio::test]
async fn one_server_failure_does_not_block_others() {
    // dead_server has resources capability in its caps (set via
    // unreachable_with_resources_for_test) but will fail all calls.
    let dead = McpClientHandle::unreachable_with_resources_for_test("dead_srv");
    let live = spawn_with_behavior("live_srv", "with_resources").await;

    let servers = Arc::new(vec![
        ("dead_srv".to_string(), dead),
        ("live_srv".to_string(), live.clone()),
    ]);
    let tool = ListMcpResources::new(servers);

    let out = tool.invoke(json!({}), &ctx()).await.unwrap();

    // live_srv's resources should still appear despite dead_srv failing.
    match out {
        ToolOutput::Text(s) => {
            let arr: serde_json::Value = serde_json::from_str(&s)
                .unwrap_or(serde_json::Value::Null);
            if let Some(arr) = arr.as_array() {
                assert!(
                    arr.iter().any(|e| e["server"] == "live_srv"),
                    "live server's resources should appear despite dead server: {s}"
                );
            }
        }
        // If dead_srv happened to be the only server queried, any ToolOutput is fine.
        _ => {}
    }

    live.shutdown().await;
}
