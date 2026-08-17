//! ListMcpResources — model-facing tool that lists available resources
//! across connected MCP servers.
//!
//! The tool is deferred (`LoadPolicy::Deferred`) and concurrency-safe because
//! it issues read-only `resources/list` calls and performs no state mutation.

use std::sync::Arc;

use ao_engine_tools_core::{
    context::RunnerContext,
    output::ToolOutput,
    permissions::{PermissionContext, PermissionDecision},
    policy::LoadPolicy,
    tool::IoTool,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use futures_util::future::join_all;
use serde_json::{json, Value};
use tracing::warn;

pub mod prompt;
#[cfg(test)]
mod tests;

use super::client::{McpClientHandle, McpError};
use super::resource_fetch::{fetch_resources, McpResourceDescriptor};

/// Maximum output length in characters before the result is truncated.
pub(crate) const MAX_OUTPUT_CHARS: usize = 100_000;

/// A model-facing tool that lists resources available on connected MCP servers.
///
/// The optional `server` input parameter filters results to a single named
/// server.  When omitted, all servers that advertised resource support during
/// their handshake are queried concurrently and their results are merged into
/// one JSON array.
pub struct ListMcpResources {
    /// All configured server handles, ordered as they were registered.
    servers: Arc<Vec<(String, McpClientHandle)>>,
}

impl ListMcpResources {
    /// Create a new instance backed by the given server handle list.
    pub fn new(servers: Arc<Vec<(String, McpClientHandle)>>) -> Self {
        Self { servers }
    }
}

#[async_trait]
impl IoTool for ListMcpResources {
    fn name(&self) -> &str {
        "ListMcpResources"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Name of a specific MCP server to query. \
                                    Omit to list resources from all servers that support them."
                }
            },
            "additionalProperties": false
        })
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        _input: &Value,
        _ctx: &PermissionContext,
    ) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn invoke(&self, input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let server_filter = input.get("server").and_then(|s| s.as_str());

        // Resolve targets: named server or all resource-capable servers.
        let targets: Vec<(String, McpClientHandle)> = if let Some(filter) = server_filter {
            match self.servers.iter().find(|(name, _)| name == filter) {
                None => {
                    let known: Vec<&str> =
                        self.servers.iter().map(|(n, _)| n.as_str()).collect();
                    return Ok(ToolOutput::error(
                        format!(
                            "MCP server '{}' is not configured. Configured servers: {}",
                            filter,
                            if known.is_empty() {
                                "none".to_string()
                            } else {
                                known.join(", ")
                            }
                        ),
                        true,
                    ));
                }
                Some((name, handle)) => vec![(name.clone(), handle.clone())],
            }
        } else {
            // All servers that declared resources support.
            self.servers
                .iter()
                .filter(|(_, handle)| {
                    handle
                        .server_capabilities()
                        .map(|c| c.resources)
                        .unwrap_or(false)
                })
                .map(|(name, handle)| (name.clone(), handle.clone()))
                .collect()
        };

        if targets.is_empty() {
            return Ok(ToolOutput::text(
                "No MCP servers with resource support are currently available. \
                 Servers may still expose tools even when they do not publish resources."
                    .to_string(),
            ));
        }

        // Fetch from all targets concurrently; isolate per-server failures.
        let fetch_futs: Vec<_> = targets
            .iter()
            .map(|(server_name, handle)| {
                let server_name = server_name.clone();
                let handle = handle.clone();
                async move {
                    let result = fetch_with_reconnect(&handle).await;
                    (server_name, result)
                }
            })
            .collect();

        let results = join_all(fetch_futs).await;

        let mut all_resources: Vec<Value> = Vec::new();
        for (server_name, result) in results {
            match result {
                Ok(descriptors) => {
                    for d in descriptors {
                        all_resources.push(resource_to_json(d, &server_name));
                    }
                }
                Err(e) => {
                    warn!(mcp_server = %server_name, "failed to list resources: {e}");
                }
            }
        }

        if all_resources.is_empty() {
            return Ok(ToolOutput::text(
                "No resources found. MCP servers may still expose tools even when they do not \
                 publish resources."
                    .to_string(),
            ));
        }

        let json_text = serde_json::to_string_pretty(&all_resources)
            .unwrap_or_else(|_| format!("{:?}", all_resources));

        Ok(maybe_truncate(json_text))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Attempt `fetch_resources`; on `McpError::Closed`, reconnect once and retry.
async fn fetch_with_reconnect(
    handle: &McpClientHandle,
) -> Result<Vec<McpResourceDescriptor>, McpError> {
    match fetch_resources(handle).await {
        Err(McpError::Closed) => {
            handle.reconnect().await?;
            fetch_resources(handle).await
        }
        other => other,
    }
}

/// Serialize a resource descriptor into a JSON object with server attribution.
fn resource_to_json(d: McpResourceDescriptor, server_name: &str) -> Value {
    let mut entry = json!({
        "uri": d.uri,
        "server": server_name,
    });
    if let Some(name) = d.name {
        entry["name"] = json!(name);
    }
    if let Some(desc) = d.description {
        entry["description"] = json!(desc);
    }
    if let Some(mime) = d.mime_type {
        entry["mimeType"] = json!(mime);
    }
    entry
}

/// Truncate `text` at [`MAX_OUTPUT_CHARS`] and append a note when it exceeds
/// the cap.  Returns a [`ToolOutput::Text`] in either case.
fn maybe_truncate(text: String) -> ToolOutput {
    let total = text.chars().count();
    if total > MAX_OUTPUT_CHARS {
        let truncated: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
        let note = format!(
            "\n\n[Output truncated at {MAX_OUTPUT_CHARS} characters — \
             {MAX_OUTPUT_CHARS} of {total} total characters shown]"
        );
        ToolOutput::text(format!("{truncated}{note}"))
    } else {
        ToolOutput::text(text)
    }
}
