//! Client helpers for the MCP `resources/list` and `resources/read` endpoints.
//!
//! These complement the tool-fetch helpers in [`super::schema_fetch`] by
//! covering the resource half of the MCP protocol.  Both helpers operate over
//! the same [`McpClientHandle`] used for `tools/list` and `tools/call`, so no
//! additional transport setup is required.

use serde_json::json;
use tracing::warn;

use super::client::{McpClientHandle, McpError};

// ── Resource descriptor ───────────────────────────────────────────────────────

/// Metadata for a single resource entry returned by `resources/list`.
#[derive(Debug, Clone)]
pub struct McpResourceDescriptor {
    /// Canonical URI identifying this resource on the server.
    pub uri: String,
    /// Optional human-readable label.
    pub name: Option<String>,
    /// Optional summary text intended for display to the model.
    pub description: Option<String>,
    /// Optional MIME type of the resource content.
    pub mime_type: Option<String>,
}

// ── Resource content ──────────────────────────────────────────────────────────

/// A single content entry from a `resources/read` response.
///
/// Exactly one of `text` or `blob` is expected to be present for non-empty
/// resources.  Both may be absent for resources that exist but are empty.
#[derive(Debug, Clone)]
pub struct McpResourceContent {
    /// Resource URI echoed back by the server.
    pub uri: String,
    /// MIME type of the content, when provided.
    pub mime_type: Option<String>,
    /// Plain-text content, present for text-based resources.
    pub text: Option<String>,
    /// Base64-encoded binary content, present for binary resources.
    pub blob: Option<String>,
}

// ── resources/list helper ─────────────────────────────────────────────────────

/// Call `resources/list` on `client`, following `nextCursor` pagination until
/// all pages have been retrieved.
///
/// Malformed per-resource entries (missing `uri`) are skipped with a
/// `tracing::warn` line; other resources from the same page are still
/// included.
///
/// When the server does not implement `resources/list` it returns JSON-RPC
/// error -32601 (method not found).  Callers that want to treat this as an
/// empty list should match on `McpError::CallError { code: -32601, .. }`.
pub async fn fetch_resources(
    client: &McpClientHandle,
) -> Result<Vec<McpResourceDescriptor>, McpError> {
    let server_name = client.name().to_string();
    let mut resources: Vec<McpResourceDescriptor> = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };

        let result = client.call("resources/list", params).await?;

        let page = match result.get("resources").and_then(|r| r.as_array()) {
            Some(arr) => arr.clone(),
            None => {
                warn!(
                    mcp_server = %server_name,
                    "resources/list response missing 'resources' array"
                );
                break;
            }
        };

        for entry in &page {
            let uri = match entry.get("uri").and_then(|u| u.as_str()) {
                Some(u) => u.to_string(),
                None => {
                    warn!(
                        mcp_server = %server_name,
                        "resource entry missing 'uri' field — skipping"
                    );
                    continue;
                }
            };
            resources.push(McpResourceDescriptor {
                uri,
                name: entry.get("name").and_then(|n| n.as_str()).map(str::to_string),
                description: entry
                    .get("description")
                    .and_then(|d| d.as_str())
                    .map(str::to_string),
                mime_type: entry
                    .get("mimeType")
                    .and_then(|m| m.as_str())
                    .map(str::to_string),
            });
        }

        cursor = result
            .get("nextCursor")
            .and_then(|c| c.as_str())
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }

    Ok(resources)
}

// ── resources/read helper ─────────────────────────────────────────────────────

/// Call `resources/read` on `client` for the given `uri`.
///
/// Returns the parsed `contents` array.  The wire shape for `resources/read`
/// (`{ contents: [...] }`) differs from `tools/call` (`{ content: [...], isError }`)
/// and is intentionally handled here rather than in `mcp_result_to_tool_output`,
/// which is scoped to tool-call results only.
pub async fn read_resource(
    client: &McpClientHandle,
    uri: &str,
) -> Result<Vec<McpResourceContent>, McpError> {
    let server_name = client.name().to_string();

    let result = client.call("resources/read", json!({ "uri": uri })).await?;

    let contents_array = match result.get("contents").and_then(|c| c.as_array()) {
        Some(arr) => arr.clone(),
        None => {
            warn!(
                mcp_server = %server_name,
                "resources/read response missing 'contents' array"
            );
            return Ok(vec![]);
        }
    };

    let mut contents: Vec<McpResourceContent> = Vec::with_capacity(contents_array.len());
    for entry in &contents_array {
        contents.push(McpResourceContent {
            uri: entry.get("uri").and_then(|u| u.as_str()).unwrap_or("").to_string(),
            mime_type: entry
                .get("mimeType")
                .and_then(|m| m.as_str())
                .map(str::to_string),
            text: entry.get("text").and_then(|t| t.as_str()).map(str::to_string),
            blob: entry.get("blob").and_then(|b| b.as_str()).map(str::to_string),
        });
    }

    Ok(contents)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::mcp::test_support::echo_server_bin;

    // ── fetch_resources ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_resources_returns_descriptors() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "with_resources".to_string());

        let client = McpClientHandle::spawn("res", bin.to_str().unwrap(), &[], &env)
            .await
            .expect("should spawn");

        let resources = fetch_resources(&client).await.expect("fetch should succeed");

        assert!(!resources.is_empty(), "with_resources fixture should return at least one resource");
        assert!(!resources[0].uri.is_empty(), "resource should have a URI");

        client.shutdown().await;
    }

    #[tokio::test]
    async fn fetch_resources_follows_pagination() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "resources_paginated".to_string());

        let client = McpClientHandle::spawn("paged", bin.to_str().unwrap(), &[], &env)
            .await
            .expect("should spawn");

        let resources = fetch_resources(&client).await.expect("fetch should succeed");

        assert_eq!(resources.len(), 2, "paginated fixture returns 2 resources across 2 pages");
        let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
        assert!(uris.contains(&"resource://page1/item1"), "page 1 item should be present");
        assert!(uris.contains(&"resource://page2/item1"), "page 2 item should be present");

        client.shutdown().await;
    }

    #[tokio::test]
    async fn fetch_resources_no_support_returns_error() {
        let bin = echo_server_bin();
        // Default behavior: resources/list returns method-not-found.
        let client = McpClientHandle::spawn("echo", bin.to_str().unwrap(), &[], &HashMap::new())
            .await
            .expect("should spawn");

        let err = fetch_resources(&client).await.expect_err("should fail for unsupported server");

        assert!(
            matches!(err, McpError::CallError { code: -32601, .. }),
            "expected method-not-found error, got {err:?}"
        );

        client.shutdown().await;
    }

    // ── read_resource ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn read_resource_returns_text_content() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "with_resources".to_string());

        let client = McpClientHandle::spawn("res", bin.to_str().unwrap(), &[], &env)
            .await
            .expect("should spawn");

        let contents = read_resource(&client, "resource://notes.txt")
            .await
            .expect("read should succeed");

        assert!(!contents.is_empty(), "should return at least one content item");
        assert!(
            contents[0].text.is_some(),
            "text resource should have text content"
        );

        client.shutdown().await;
    }

    #[tokio::test]
    async fn read_resource_returns_blob_content() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "with_blob_resource".to_string());

        let client = McpClientHandle::spawn("blob", bin.to_str().unwrap(), &[], &env)
            .await
            .expect("should spawn");

        let contents = read_resource(&client, "resource://data.pdf")
            .await
            .expect("read should succeed");

        assert!(!contents.is_empty(), "should return at least one content item");
        assert!(
            contents[0].blob.is_some(),
            "blob resource should have blob field"
        );
        assert_eq!(
            contents[0].mime_type.as_deref(),
            Some("application/pdf"),
            "blob resource should have mime type"
        );

        client.shutdown().await;
    }
}
