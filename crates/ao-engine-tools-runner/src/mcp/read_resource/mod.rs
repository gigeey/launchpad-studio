//! ReadMcpResource — model-facing tool that fetches a resource by URI
//! from a named MCP server.
//!
//! The tool is deferred (`LoadPolicy::Deferred`) and concurrency-safe because
//! it issues read-only `resources/read` calls and performs no state mutation
//! beyond blob persistence (writes are to a private data directory, not to
//! model state).

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
use serde_json::{json, Value};

pub mod prompt;
#[cfg(test)]
mod tests;

use super::blob_storage;
use super::client::{McpClientHandle, McpError};
use super::resource_fetch::read_resource;

/// Maximum output length in characters before the result is truncated.
pub(crate) const MAX_OUTPUT_CHARS: usize = 100_000;

/// A model-facing tool that reads a named resource from a specific MCP server.
///
/// Both `server` and `uri` are required.  The tool validates that the server
/// is configured and that it advertised resource support during initialization
/// before issuing the `resources/read` request.  Binary blob payloads are
/// decoded and persisted to disk; the model receives a path-note in place of
/// raw base64 data.
pub struct ReadMcpResource {
    /// All configured server handles, ordered as they were registered.
    servers: Arc<Vec<(String, McpClientHandle)>>,
}

impl ReadMcpResource {
    /// Create a new instance backed by the given server handle list.
    pub fn new(servers: Arc<Vec<(String, McpClientHandle)>>) -> Self {
        Self { servers }
    }
}

#[async_trait]
impl IoTool for ReadMcpResource {
    fn name(&self) -> &str {
        "ReadMcpResource"
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
                    "description": "Name of the MCP server that owns the resource."
                },
                "uri": {
                    "type": "string",
                    "description": "Resource URI to read, as returned by ListMcpResources."
                }
            },
            "required": ["server", "uri"],
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
        // ── Required input extraction ─────────────────────────────────────────
        let server_name = match input.get("server").and_then(|s| s.as_str()) {
            Some(s) => s,
            None => {
                return Ok(ToolOutput::error(
                    "Missing required parameter: 'server'",
                    true,
                ));
            }
        };
        let uri = match input.get("uri").and_then(|u| u.as_str()) {
            Some(u) => u,
            None => {
                return Ok(ToolOutput::error("Missing required parameter: 'uri'", true));
            }
        };

        // ── Guard: unknown server ─────────────────────────────────────────────
        let handle = match self.servers.iter().find(|(name, _)| name == server_name) {
            Some((_, h)) => h.clone(),
            None => {
                let known: Vec<&str> =
                    self.servers.iter().map(|(n, _)| n.as_str()).collect();
                return Ok(ToolOutput::error(
                    format!(
                        "MCP server '{}' is not configured. Configured servers: {}",
                        server_name,
                        if known.is_empty() {
                            "none".to_string()
                        } else {
                            known.join(", ")
                        }
                    ),
                    true,
                ));
            }
        };

        // ── Guard: resources capability ───────────────────────────────────────
        match handle.server_capabilities() {
            None => {
                return Ok(ToolOutput::error(
                    format!(
                        "MCP server '{}' has not completed initialization; cannot read resources.",
                        server_name
                    ),
                    true,
                ));
            }
            Some(caps) if !caps.resources => {
                return Ok(ToolOutput::error(
                    format!(
                        "MCP server '{}' does not support resource access. The server did not \
                         advertise the 'resources' capability during initialization.",
                        server_name
                    ),
                    true,
                ));
            }
            _ => {}
        }

        // ── Call resources/read with one-shot reconnect on connection loss ────
        let contents = match read_resource(&handle, uri).await {
            Ok(c) => c,
            Err(McpError::Closed) => {
                match handle.reconnect().await {
                    Ok(()) => match read_resource(&handle, uri).await {
                        Ok(c) => c,
                        Err(e) => {
                            return Ok(ToolOutput::error(
                                format!(
                                    "MCP server '{}' is not connected: {e}",
                                    server_name
                                ),
                                true,
                            ));
                        }
                    },
                    Err(reconnect_err) => {
                        return Ok(ToolOutput::error(
                            format!(
                                "MCP server '{}' is not connected and could not reconnect: \
                                 {reconnect_err}",
                                server_name
                            ),
                            true,
                        ));
                    }
                }
            }
            Err(McpError::CallError { message, .. }) => {
                return Ok(ToolOutput::error(
                    format!(
                        "MCP server '{}' failed to read resource '{}': {message}",
                        server_name, uri
                    ),
                    true,
                ));
            }
            Err(e) => {
                return Ok(ToolOutput::error(
                    format!(
                        "Error reading resource '{}' from server '{}': {e}",
                        uri, server_name
                    ),
                    true,
                ));
            }
        };

        // ── Map contents: text passes through; blobs are persisted to disk ────
        let mapped: Vec<Value> = contents
            .into_iter()
            .map(|c| {
                let text = if let Some(t) = c.text {
                    // Text resource — pass through as-is.
                    t
                } else if let Some(b) = c.blob {
                    // Binary resource — decode and persist; model receives path note.
                    let mime = c.mime_type.as_deref().unwrap_or("application/octet-stream");
                    blob_storage::decode_and_persist(&b, mime)
                } else {
                    // Empty resource — return empty string rather than nothing.
                    String::new()
                };

                let mut entry = json!({
                    "uri": c.uri,
                    "text": text,
                });
                if let Some(mime) = c.mime_type {
                    entry["mimeType"] = json!(mime);
                }
                entry
            })
            .collect();

        let json_text = serde_json::to_string(&mapped)
            .unwrap_or_else(|_| format!("{:?}", mapped));

        Ok(maybe_truncate(json_text))
    }
}

// ── Helper ────────────────────────────────────────────────────────────────────

/// Truncate `text` at [`MAX_OUTPUT_CHARS`] and append a note when it exceeds
/// the cap.  Returns a [`ToolOutput::Text`] in either case.
pub(crate) fn maybe_truncate(text: String) -> ToolOutput {
    let total = text.chars().count();
    if total > MAX_OUTPUT_CHARS {
        let truncated: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
        let note = format!(
            " ... [output truncated at {MAX_OUTPUT_CHARS} characters — \
             {MAX_OUTPUT_CHARS} of {total} total characters shown]"
        );
        ToolOutput::text(format!("{truncated}{note}"))
    } else {
        ToolOutput::text(text)
    }
}
