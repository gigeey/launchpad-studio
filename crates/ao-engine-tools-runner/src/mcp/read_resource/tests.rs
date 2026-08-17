//! Tests for the ReadMcpResource tool.
//!
//! Included from `mod.rs` as `#[cfg(test)] mod tests`.

use std::collections::HashMap;
use std::sync::Arc;

use ao_engine_tools_core::{context::RunnerContext, output::ToolOutput, policy::LoadPolicy, tool::IoTool};
use serde_json::json;

use super::{ReadMcpResource, MAX_OUTPUT_CHARS};
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
    let tool = ReadMcpResource::new(Arc::new(vec![]));
    assert_eq!(tool.name(), "ReadMcpResource");
    assert!(!tool.description().is_empty());
    assert!(tool.is_concurrency_safe(), "reading a resource is read-only");
    assert_eq!(tool.load_policy(), LoadPolicy::Deferred);

    let schema = tool.input_schema();
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["server"].is_object());
    assert!(schema["properties"]["uri"].is_object());
    let required = schema["required"].as_array().expect("required should be array");
    assert!(required.iter().any(|v| v == "server"), "server should be required");
    assert!(required.iter().any(|v| v == "uri"), "uri should be required");
    assert_eq!(schema["additionalProperties"], false);
}

// ── Guard: missing required params ───────────────────────────────────────────

#[tokio::test]
async fn missing_server_param_returns_error() {
    let tool = ReadMcpResource::new(Arc::new(vec![]));
    let out = tool.invoke(json!({ "uri": "resource://x" }), &ctx()).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(message.contains("server"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_uri_param_returns_error() {
    let tool = ReadMcpResource::new(Arc::new(vec![]));
    let out = tool.invoke(json!({ "server": "srv" }), &ctx()).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(message.contains("uri"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// ── Guard: unknown server ─────────────────────────────────────────────────────

#[tokio::test]
async fn unknown_server_lists_configured_servers_in_error() {
    let handle = McpClientHandle::unreachable_for_test("configured_srv");
    let tool =
        ReadMcpResource::new(Arc::new(vec![("configured_srv".to_string(), handle)]));

    let out = tool
        .invoke(json!({ "server": "unknown_srv", "uri": "resource://x" }), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(message.contains("not configured"), "{message}");
            assert!(
                message.contains("configured_srv"),
                "error should list known server names: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_server_empty_list_says_none() {
    let tool = ReadMcpResource::new(Arc::new(vec![]));

    let out = tool
        .invoke(json!({ "server": "any", "uri": "resource://x" }), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Error { message, .. } => {
            assert!(message.contains("none"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// ── Guard: no resources capability ───────────────────────────────────────────

#[tokio::test]
async fn no_resources_capability_returns_descriptive_error() {
    // Normal echo_mcp_server returns empty capabilities → resources: false.
    let handle = spawn_with_behavior("plain", "").await;
    let tool =
        ReadMcpResource::new(Arc::new(vec![("plain".to_string(), handle.clone())]));

    let out = tool
        .invoke(json!({ "server": "plain", "uri": "resource://x" }), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(
                message.contains("resources") || message.contains("capability"),
                "error should mention resources capability: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    handle.shutdown().await;
}

// ── Guard: server not connected ───────────────────────────────────────────────

#[tokio::test]
async fn dead_server_with_resources_cap_returns_not_connected_error() {
    // Handle that advertises resources capability but has no live session
    // and an unspawnable command — simulates a server that was once connected
    // but is now permanently unreachable.
    let dead = McpClientHandle::unreachable_with_resources_for_test("dead_srv");
    let tool = ReadMcpResource::new(Arc::new(vec![("dead_srv".to_string(), dead)]));

    let out = tool
        .invoke(json!({ "server": "dead_srv", "uri": "resource://x" }), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(recoverable);
            assert!(
                message.contains("not connected") || message.contains("reconnect"),
                "error should mention connection state: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// ── Happy path: text resource ─────────────────────────────────────────────────

#[tokio::test]
async fn reads_text_resource_and_returns_json_contents() {
    let handle = spawn_with_behavior("res_srv", "with_resources").await;
    let tool =
        ReadMcpResource::new(Arc::new(vec![("res_srv".to_string(), handle.clone())]));

    let out = tool
        .invoke(json!({ "server": "res_srv", "uri": "resource://notes.txt" }), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Text(s) => {
            let arr: serde_json::Value = serde_json::from_str(&s)
                .expect("output should be valid JSON array");
            let arr = arr.as_array().expect("top-level should be array");
            assert!(!arr.is_empty(), "should return at least one content item");
            assert!(
                arr[0]["text"].is_string(),
                "text resource should have text field: {s}"
            );
            assert!(
                arr[0]["uri"].is_string(),
                "each item should have uri: {s}"
            );
        }
        other => panic!("expected Text, got {other:?}"),
    }

    handle.shutdown().await;
}

// ── Happy path: blob resource persisted to disk ───────────────────────────────

#[tokio::test]
async fn reads_blob_resource_persists_to_disk_and_returns_path_note() {
    // Pin the data root to a tempdir for the WHOLE test (guard held across
    // the invoke and the assertions): the env var is process-global, so an
    // unguarded window lets a concurrent test clobber it mid-read.
    let guard = crate::test_env::DataDirGuard::new();

    let handle = spawn_with_behavior("blob_srv", "with_blob_resource").await;
    let tool =
        ReadMcpResource::new(Arc::new(vec![("blob_srv".to_string(), handle.clone())]));

    let out = tool
        .invoke(json!({ "server": "blob_srv", "uri": "resource://data.pdf" }), &ctx())
        .await
        .unwrap();

    match out {
        ToolOutput::Text(s) => {
            let arr: serde_json::Value = serde_json::from_str(&s)
                .expect("output should be valid JSON");
            let arr = arr.as_array().expect("should be array");
            assert!(!arr.is_empty(), "should return at least one content item");
            let text = arr[0]["text"].as_str().expect("item should have text field");
            assert!(
                text.contains("Saved to"),
                "blob text should be a path note starting with 'Saved to': {text}"
            );
            // The saved path should be under our redirected data root.
            let expected_prefix = guard.data_dir().join("mcp-output").display().to_string();
            assert!(
                text.contains(&expected_prefix),
                "path note should reference data_root/mcp-output: {text}"
            );
            // The file at the path should actually exist.
            if let Some(rest) = text.strip_prefix("Saved to ") {
                if let Some(end) = rest.rfind(" (") {
                    let path_str = &rest[..end];
                    assert!(
                        std::path::Path::new(path_str).exists(),
                        "blob file should exist at {path_str}"
                    );
                }
            }
        }
        other => panic!("expected Text, got {other:?}"),
    }

    handle.shutdown().await;
}

// ── Truncation cap ────────────────────────────────────────────────────────────

#[test]
fn truncation_note_appended_when_output_exceeds_cap() {
    // Build an oversized JSON string and feed it through maybe_truncate.
    let long_text = "x".repeat(MAX_OUTPUT_CHARS + 50);
    let items = vec![json!({ "uri": "r://x", "text": long_text })];
    let json_text = serde_json::to_string(&items).unwrap();
    let total = json_text.chars().count();
    assert!(total > MAX_OUTPUT_CHARS, "precondition: must exceed cap");

    let result = super::maybe_truncate(json_text);
    match result {
        ToolOutput::Text(s) => {
            assert!(s.contains("truncated"), "truncation note should be present: {s}");
            assert!(
                s.chars().count() > MAX_OUTPUT_CHARS,
                "result includes note chars, so total exceeds cap"
            );
        }
        other => panic!("expected Text, got {other:?}"),
    }
}
