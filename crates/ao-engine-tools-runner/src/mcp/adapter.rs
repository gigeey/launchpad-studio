//! [`McpToolAdapter`] — bridges a single MCP server tool into the [`IoTool`] interface.
//!
//! Each adapter represents one tool from one MCP server. The adapter's
//! `invoke` proxies the call to the server via its [`McpClientHandle`] and
//! maps the MCP result content-block array back to a [`ToolOutput`].
//!
//! If the first call finds the connection closed (server crash, idle timeout)
//! or the server reports expired/missing auth, `invoke` attempts exactly one
//! reconnect+retry before surfacing a recoverable error output.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ao_engine_tools_core::{
    context::RunnerContext,
    output::{ToolBlock, ToolOutput},
    permissions::{PermissionContext, PermissionDecision},
    policy::LoadPolicy,
    tool::IoTool,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{debug, warn};

use super::blob_storage;
use super::client::{McpClientHandle, McpError, ProgressNotification};
use super::payload_stash;
use super::schema_fetch::McpToolAnnotations;

/// Process-wide monotonic counter for per-call correlation IDs.
///
/// Each `tools/call` outbound request includes `_meta['launchpad/toolUseId']`
/// built from this counter so that MCP servers can correlate requests with
/// their own telemetry or log systems. The ID is opaque to providers —
/// it never appears in the API request to the model.
static MCP_CALL_COUNTER: AtomicU64 = AtomicU64::new(1);

/// An [`IoTool`] adapter that proxies invocations to an MCP server tool.
pub struct McpToolAdapter {
    server_name: String,
    raw_name: String,
    qualified_name: String,
    description: String,
    input_schema: Value,
    client: McpClientHandle,
    loading_policy: LoadPolicy,
    /// Behavioural hints from the server's `annotations` object.
    /// All fields are `None` when the server did not include annotations.
    annotations: McpToolAnnotations,
    /// Optional search hint from `tool._meta['anthropic/searchHint']`.
    ///
    /// Stored here for future use in deferred-tool search ranking. Until
    /// the `IoTool` trait gains a dedicated search-hint surface, this is
    /// accessible via [`McpToolAdapter::search_hint`].
    search_hint: Option<String>,
}

impl McpToolAdapter {
    pub fn new(
        server_name: impl Into<String>,
        raw_name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        client: McpClientHandle,
        loading_policy: LoadPolicy,
        annotations: McpToolAnnotations,
        search_hint: Option<String>,
    ) -> Self {
        let server_name = server_name.into();
        let raw_name = raw_name.into();
        let qualified_name = format!("mcp__{}__{}", server_name, raw_name);
        Self {
            server_name,
            raw_name,
            qualified_name,
            description: description.into(),
            input_schema,
            client,
            loading_policy,
            annotations,
            search_hint,
        }
    }

    pub fn loading_policy(&self) -> LoadPolicy {
        self.loading_policy
    }

    /// Whether the MCP server declared this tool as potentially destructive.
    ///
    /// Returns `None` when the server omitted `destructiveHint` from its
    /// `annotations` object.  Callers should treat `None` as unknown rather
    /// than safe — absence of the hint does not mean the operation is benign.
    pub fn destructive_hint(&self) -> Option<bool> {
        self.annotations.destructive_hint
    }

    /// Whether the MCP server declared this tool as "open world" — capable of
    /// reaching state beyond the local environment (e.g. external APIs).
    ///
    /// Returns `None` when the server omitted `openWorldHint`.
    pub fn open_world_hint(&self) -> Option<bool> {
        self.annotations.open_world_hint
    }

    /// A human-readable display title for this tool, if the server provided one.
    ///
    /// The [`IoTool`] trait has no user-facing-name concept separate from
    /// `name()` (which returns the qualified `mcp__<server>__<tool>` string).
    /// Until that gap is addressed in the trait, callers that need a
    /// user-visible label can read this accessor directly.
    pub fn display_title(&self) -> Option<&str> {
        self.annotations.title.as_deref()
    }

    /// Optional search hint from `tool._meta['anthropic/searchHint']`, if the
    /// server provided one.
    ///
    /// Intended for deferred-tool search — the hint text describes extra
    /// keywords or context that help the model locate this tool by topic
    /// rather than by exact name. Not yet consumed by the `IoTool` trait;
    /// callers that want to use it must downcast to `McpToolAdapter`.
    pub fn search_hint(&self) -> Option<&str> {
        self.search_hint.as_deref()
    }

    /// Issue the underlying `tools/call` to the server.
    ///
    /// Injects `_meta['launchpad/toolUseId']` into the outbound params so that
    /// MCP servers can correlate this request with their own telemetry. The ID
    /// is a process-wide monotonic counter formatted as `"lp-{N}"` — unique
    /// within the process lifetime but not globally stable across restarts.
    /// `call_with_progress` merges `_meta['progressToken']` into the same
    /// object rather than overwriting it.
    async fn call_server(&self, input: &Value) -> Result<Value, McpError> {
        let server = self.server_name.clone();
        let tool = self.raw_name.clone();
        let on_progress = move |notif: ProgressNotification| {
            debug!(
                mcp_server = %server,
                mcp_tool = %tool,
                progress = notif.progress,
                total = ?notif.total,
                message = ?notif.message,
                "tool progress"
            );
        };

        // Build params with a correlation ID in _meta. call_with_progress will
        // merge progressToken into the same _meta object.
        let call_id = MCP_CALL_COUNTER.fetch_add(1, Ordering::Relaxed);
        let params = json!({
            "name": self.raw_name,
            "arguments": input,
            "_meta": { "launchpad/toolUseId": format!("lp-{call_id}") }
        });

        self.client
            .call_with_progress(
                "tools/call",
                params,
                Duration::from_secs(60),
                on_progress,
            )
            .await
    }
}

#[async_trait]
impl IoTool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn load_policy(&self) -> LoadPolicy {
        self.loading_policy
    }

    /// Returns `true` only when the MCP server explicitly declared
    /// `readOnlyHint: true` in the tool's `annotations`.
    ///
    /// The default is `false` — if the server omits `annotations` or sets
    /// `readOnlyHint: false`, the tool is assumed to be write-capable and
    /// will not be issued concurrently.  This is the safe conservative
    /// choice: a write-heavy tool called concurrently can corrupt server-side
    /// state, while a read-only tool called serially merely loses throughput.
    fn is_concurrency_safe(&self) -> bool {
        self.annotations.read_only_hint == Some(true)
    }

    async fn check_permissions(
        &self,
        _input: &Value,
        _ctx: &PermissionContext,
    ) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn invoke(&self, input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        // Spawn a background task that logs every 30 s while the call is in
        // flight. Slow MCP servers can be indistinguishable from hangs without
        // this signal — the heartbeat lets operators tell "still running" from
        // "stuck" well before the 60-second timeout fires.
        let server_for_log = self.server_name.clone();
        let tool_for_log = self.qualified_name.clone();
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                debug!(
                    mcp_server = %server_for_log,
                    mcp_tool = %tool_for_log,
                    "tool call still in flight — no response yet; continuing to wait"
                );
            }
        });

        let mut res = self.call_server(&input).await;

        // One-shot reconnect+retry on connection closure or expired auth.
        // Reconnect re-resolves the OAuth bearer token from the token store
        // before re-running the handshake for HTTP-transport servers — the
        // point in the adapter's lifecycle where a handle that predates a
        // credential (or whose credential just expired) picks up a current
        // one. Without retrying here, such a handle would stay dead until the
        // whole process restarts. A second consecutive failure means the
        // reconnect attempt itself could not authenticate; the resulting
        // error distinguishes never-authorized, credential-rejected, and
        // refresh-failed below rather than collapsing all three into one
        // bare 401.
        if matches!(res, Err(McpError::Closed) | Err(McpError::AuthRequired)) {
            res = match self.client.reconnect().await {
                Ok(()) => self.call_server(&input).await,
                Err(reconnect_err) => Err(reconnect_err),
            };
        }

        // The call completed — stop the heartbeat task regardless of outcome.
        heartbeat.abort();

        Ok(match res {
            Ok(value) => {
                mcp_result_to_tool_output(&self.server_name, &self.raw_name, &input, value)
            }
            Err(McpError::CallError { message, .. }) => {
                ToolOutput::error(format!("MCP call failed: {message}"), true)
            }
            Err(McpError::Timeout) => ToolOutput::error(
                format!("MCP tool '{}' timed out", self.qualified_name),
                true,
            ),
            Err(McpError::Transport(e)) => ToolOutput::error(
                format!("MCP transport error for '{}': {e}", self.qualified_name),
                true,
            ),
            Err(McpError::Closed) => ToolOutput::error(
                format!("MCP server '{}' connection closed", self.server_name),
                true,
            ),
            Err(McpError::NeverAuthorized(name)) => ToolOutput::error(
                format!(
                    "MCP server '{name}' needs authorization — no stored credential was found. Use the connector's authorize action, then retry."
                ),
                true,
            ),
            Err(McpError::CredentialRejected(name)) => ToolOutput::error(
                format!(
                    "MCP server '{name}' rejected its stored credential — reauthorize this connector, then retry."
                ),
                true,
            ),
            Err(McpError::TokenRefreshFailed(name, reason)) => ToolOutput::error(
                format!(
                    "MCP server '{name}' credential refresh failed ({reason}); authentication could not be retried."
                ),
                true,
            ),
            Err(McpError::GrantRevoked(name, reason)) => ToolOutput::error(
                format!(
                    "MCP server '{name}' authorization was revoked by the provider ({reason}). This will not resolve on retry — re-authorize this connector, then try again."
                ),
                true,
            ),
            Err(e) => ToolOutput::error(format!("MCP error for '{}': {e}", self.server_name), true),
        })
    }
}

/// Parse an MCP `tools/call` result value into a [`ToolOutput`].
///
/// Handles every content block type defined in the MCP spec:
///
/// - `text` — passed through as [`ToolBlock::Text`].
/// - `image` — passed through inline as [`ToolBlock::Image`]; providers that
///   do not support inline images (OpenAI, Gemini) receive a text placeholder
///   from their respective message normalizers.
/// - `resource` with `text` — emitted as text prefixed by the resource URI.
/// - `resource` with `blob` and an image MIME type — same treatment as an
///   `image` block.
/// - `resource` with `blob` and a non-image MIME type — decoded and persisted
///   to disk; the model receives a "Saved to …" path note.
/// - `audio` — decoded and persisted to disk; the model receives a path note.
/// - `resource_link` — formatted as text showing name, URI, and description.
/// - `structuredContent` (top-level field) — included as a JSON text block.
///
/// Write failures are handled gracefully: the model receives an informative
/// text note rather than a hard error or a silent omission.
///
/// Before the `content` blocks and `structuredContent` collapse into the
/// returned [`ToolOutput`], a copy of both is recorded into the in-process
/// [`payload_stash`], keyed by `(server_name, raw_name, hash_args(args))`.
/// This has no effect on the value returned here — see
/// [`payload_stash`] for why the capture exists and how it is scoped.
pub fn mcp_result_to_tool_output(
    server_name: &str,
    raw_name: &str,
    args: &Value,
    value: Value,
) -> ToolOutput {
    let is_error = value.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);

    let content_array = value
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let mut blocks: Vec<ToolBlock> = Vec::new();

    for block in &content_array {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match block_type {
            "text" => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    blocks.push(ToolBlock::text(text));
                }
            }
            "image" => {
                let mime = block
                    .get("mimeType")
                    .and_then(|m| m.as_str())
                    .unwrap_or("image/png");
                match block.get("data").and_then(|d| d.as_str()) {
                    Some(data) => blocks.push(ToolBlock::image(mime, data)),
                    None => warn!(
                        mcp_server = %server_name,
                        mcp_tool = %raw_name,
                        "image content block is missing the 'data' field — skipped",
                    ),
                }
            }
            "resource" => {
                match block.get("resource") {
                    Some(resource) => {
                        let uri = resource
                            .get("uri")
                            .and_then(|u| u.as_str())
                            .unwrap_or("");
                        let mime = resource
                            .get("mimeType")
                            .and_then(|m| m.as_str())
                            .unwrap_or("application/octet-stream");

                        if let Some(text) = resource.get("text").and_then(|t| t.as_str()) {
                            // Text resource — prefix with URI so the model
                            // knows the source location.
                            let body = if uri.is_empty() {
                                text.to_string()
                            } else {
                                format!("[{uri}]\n{text}")
                            };
                            blocks.push(ToolBlock::text(body));
                        } else if let Some(blob) =
                            resource.get("blob").and_then(|b| b.as_str())
                        {
                            if blob_storage::is_image_mime(mime) {
                                // Image blob — pass through inline.
                                blocks.push(ToolBlock::image(mime, blob));
                            } else {
                                // Non-image blob — persist and emit a path note.
                                let note = blob_storage::decode_and_persist(blob, mime);
                                blocks.push(ToolBlock::text(note));
                            }
                        } else {
                            warn!(
                                mcp_server = %server_name,
                                mcp_tool = %raw_name,
                                uri = %uri,
                                "resource content block has neither 'text' nor 'blob' — skipped",
                            );
                        }
                    }
                    None => warn!(
                        mcp_server = %server_name,
                        mcp_tool = %raw_name,
                        "resource content block is missing the inner 'resource' object — skipped",
                    ),
                }
            }
            "audio" => {
                let mime = block
                    .get("mimeType")
                    .and_then(|m| m.as_str())
                    .unwrap_or("audio/mpeg");
                match block.get("data").and_then(|d| d.as_str()) {
                    Some(data) => {
                        let note = blob_storage::decode_and_persist(data, mime);
                        blocks.push(ToolBlock::text(note));
                    }
                    None => warn!(
                        mcp_server = %server_name,
                        mcp_tool = %raw_name,
                        "audio content block is missing the 'data' field — skipped",
                    ),
                }
            }
            "resource_link" => {
                let uri = block.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                let name = block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(uri);
                let description = block.get("description").and_then(|d| d.as_str());
                let mut lines = vec![format!("[Resource: {name}]"), format!("URI: {uri}")];
                if let Some(desc) = description {
                    lines.push(desc.to_string());
                }
                blocks.push(ToolBlock::text(lines.join("\n")));
            }
            other => {
                warn!(
                    mcp_server = %server_name,
                    mcp_tool = %raw_name,
                    "unrecognized MCP content block type '{}' — skipped", other
                );
            }
        }
    }

    // Stash the raw pre-flatten values before `structuredContent` is
    // stringified and merged into `blocks` below. This is purely a side
    // channel for future consumers — it does not influence anything
    // computed from this point on.
    let stashed_structured = value.get("structuredContent").filter(|sc| !sc.is_null()).cloned();
    let stashed_text = {
        let joined = blocks
            .iter()
            .filter_map(|b| match b {
                ToolBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if joined.is_empty() { None } else { Some(joined) }
    };
    if stashed_structured.is_some() || stashed_text.is_some() {
        payload_stash::global().record(payload_stash::StashedPayload {
            server: server_name.to_string(),
            tool: raw_name.to_string(),
            args: args.clone(),
            args_hash: payload_stash::hash_args(args),
            captured_at: chrono::Utc::now(),
            structured: stashed_structured,
            text: stashed_text,
        });
    }

    // `structuredContent` is a top-level field on the call result, parallel to
    // the `content` array.  Include it as a JSON text block when present.
    if let Some(sc) = value.get("structuredContent") {
        if !sc.is_null() {
            let json_text = serde_json::to_string_pretty(sc)
                .unwrap_or_else(|_| sc.to_string());
            blocks.push(ToolBlock::text(format!("Structured result:\n{json_text}")));
        }
    }

    if is_error {
        // Error tool results must be text-only; summarise any inline binary
        // blocks rather than forwarding raw base64 data.
        let text = blocks
            .iter()
            .map(|b| match b {
                ToolBlock::Text { text } => text.clone(),
                ToolBlock::Image { media_type, .. } => format!("[image: {media_type}]"),
                ToolBlock::Document { media_type, .. } => format!("[document: {media_type}]"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        ToolOutput::error(text, true)
    } else if blocks.iter().all(|b| matches!(b, ToolBlock::Text { .. })) {
        // All text — use the simpler ToolOutput::Text rather than wrapping in Blocks.
        let text = blocks
            .into_iter()
            .map(|b| match b {
                ToolBlock::Text { text } => text,
                _ => unreachable!(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        ToolOutput::text(text)
    } else {
        ToolOutput::Blocks(blocks)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ao_engine_tools_core::context::RunnerContext;
    use ao_engine_tools_core::output::ToolBlock;
    use crate::mcp::schema_fetch::McpToolAnnotations;
    use std::collections::HashMap;

    use crate::mcp::test_support::echo_server_bin;

    async fn spawn_adapter(behavior: &str) -> (McpClientHandle, McpToolAdapter) {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        if !behavior.is_empty() {
            env.insert("MCP_BEHAVIOR".to_string(), behavior.to_string());
        }
        let handle = McpClientHandle::spawn("echo", bin.to_str().unwrap(), &[], &env)
            .await
            .expect("should spawn echo_mcp_server");
        let adapter = McpToolAdapter::new(
            "echo",
            "echo",
            "echoes back its arguments",
            serde_json::json!({ "type": "object", "properties": { "x": { "type": "integer" } } }),
            handle.clone(),
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations::default(),
            None,
        );
        (handle, adapter)
    }

    #[tokio::test]
    async fn invoke_normal_returns_text() {
        let (handle, adapter) = spawn_adapter("").await;
        let ctx = RunnerContext::new("sess", "agent").unwrap();

        let output = adapter
            .invoke(serde_json::json!({ "x": 42 }), &ctx)
            .await
            .expect("invoke should not return Err");

        match output {
            ToolOutput::Text(s) => {
                assert!(s.contains("\"x\"") || s.contains("x"), "text contains arg: {s}");
            }
            other => panic!("expected ToolOutput::Text, got {other:?}"),
        }

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn invoke_is_error_returns_error_recoverable() {
        let (handle, adapter) = spawn_adapter("is_error").await;
        let ctx = RunnerContext::new("sess", "agent").unwrap();

        let output = adapter
            .invoke(serde_json::json!({}), &ctx)
            .await
            .expect("invoke should not return Err");

        match output {
            ToolOutput::Error { message: _, recoverable } => {
                assert!(recoverable, "isError: true should produce recoverable: true");
            }
            other => panic!("expected ToolOutput::Error, got {other:?}"),
        }

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn invoke_jsonrpc_error_returns_error() {
        let (handle, adapter) = spawn_adapter("error_response").await;
        let ctx = RunnerContext::new("sess", "agent").unwrap();

        let output = adapter
            .invoke(serde_json::json!({}), &ctx)
            .await
            .expect("invoke should not return Err");

        match output {
            ToolOutput::Error { message, recoverable } => {
                assert!(recoverable);
                assert!(
                    message.contains("MCP call failed"),
                    "message should mention MCP call failed: {message}"
                );
            }
            other => panic!("expected ToolOutput::Error, got {other:?}"),
        }

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn qualified_name_has_mcp_prefix() {
        let (handle, adapter) = spawn_adapter("").await;
        assert_eq!(adapter.name(), "mcp__echo__echo");
        // No annotations supplied → default FALSE (conservative: assume write-capable).
        assert!(!adapter.is_concurrency_safe());
        assert_eq!(adapter.load_policy(), LoadPolicy::AlwaysLoad);
        assert_eq!(adapter.loading_policy(), LoadPolicy::AlwaysLoad);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn reconnect_after_connection_death_succeeds() {
        let (handle, adapter) = spawn_adapter("").await;
        let ctx = RunnerContext::new("sess", "agent").unwrap();

        // Simulate connection death: close stdin so child exits, reader detects EOF
        handle.simulate_connection_death_for_test().await;
        // Give the reader task time to process EOF and close the writer on its side
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // invoke detects Closed, reconnects transparently, retries → success
        let output = adapter
            .invoke(serde_json::json!({ "x": 99 }), &ctx)
            .await
            .expect("invoke should not return Err");

        assert!(
            matches!(output, ToolOutput::Text(_)),
            "reconnect should succeed transparently: {output:?}"
        );

        handle.shutdown().await;
    }

    // ── Annotation-driven concurrency tests ───────────────────────────────────

    #[test]
    fn is_concurrency_safe_default_false_when_no_annotations() {
        let handle = McpClientHandle::unreachable_for_test("srv");
        let adapter = McpToolAdapter::new(
            "srv",
            "tool",
            "a tool",
            serde_json::json!({ "type": "object" }),
            handle,
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations::default(),
            None,
        );
        assert!(
            !adapter.is_concurrency_safe(),
            "absent annotations should default to NOT concurrency-safe"
        );
    }

    #[test]
    fn is_concurrency_safe_true_when_readonly_hint_true() {
        let handle = McpClientHandle::unreachable_for_test("srv");
        let adapter = McpToolAdapter::new(
            "srv",
            "read_tool",
            "reads only",
            serde_json::json!({ "type": "object" }),
            handle,
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations {
                read_only_hint: Some(true),
                ..McpToolAnnotations::default()
            },
            None,
        );
        assert!(
            adapter.is_concurrency_safe(),
            "readOnlyHint:true should make the adapter concurrency-safe"
        );
    }

    #[test]
    fn is_concurrency_safe_false_when_readonly_hint_false() {
        let handle = McpClientHandle::unreachable_for_test("srv");
        let adapter = McpToolAdapter::new(
            "srv",
            "write_tool",
            "writes state",
            serde_json::json!({ "type": "object" }),
            handle,
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations {
                read_only_hint: Some(false),
                ..McpToolAnnotations::default()
            },
            None,
        );
        assert!(
            !adapter.is_concurrency_safe(),
            "readOnlyHint:false should NOT be concurrency-safe"
        );
    }

    #[test]
    fn annotation_accessors_exposed_correctly() {
        let handle = McpClientHandle::unreachable_for_test("srv");
        let adapter = McpToolAdapter::new(
            "srv",
            "rich_tool",
            "tool with all annotations",
            serde_json::json!({ "type": "object" }),
            handle,
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations {
                read_only_hint: Some(false),
                destructive_hint: Some(true),
                open_world_hint: Some(true),
                title: Some("Rich Tool".to_string()),
            },
            None,
        );
        assert_eq!(adapter.destructive_hint(), Some(true));
        assert_eq!(adapter.open_world_hint(), Some(true));
        assert_eq!(adapter.display_title(), Some("Rich Tool"));
    }

    #[test]
    fn annotation_accessors_none_when_absent() {
        let handle = McpClientHandle::unreachable_for_test("srv");
        let adapter = McpToolAdapter::new(
            "srv",
            "bare_tool",
            "no annotations",
            serde_json::json!({ "type": "object" }),
            handle,
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations::default(),
            None,
        );
        assert_eq!(adapter.destructive_hint(), None);
        assert_eq!(adapter.open_world_hint(), None);
        assert_eq!(adapter.display_title(), None);
    }

    #[tokio::test]
    async fn permanently_dead_server_yields_recoverable_error() {
        // Handle with no live session and a bad spawn command: the first call
        // returns Closed, the one-shot reconnect attempt fails (spawn error),
        // and the adapter surfaces a recoverable ToolOutput::Error.
        let dead_handle = McpClientHandle::unreachable_for_test("dead");
        let adapter = McpToolAdapter::new(
            "dead",
            "some_tool",
            "desc",
            serde_json::json!({ "type": "object" }),
            dead_handle,
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations::default(),
            None,
        );
        let ctx = RunnerContext::new("sess", "agent").unwrap();

        let output = adapter.invoke(serde_json::json!({}), &ctx).await.unwrap();

        match output {
            ToolOutput::Error { recoverable, .. } => {
                assert!(recoverable, "dead server should yield recoverable error");
            }
            other => panic!("expected recoverable error, got {other:?}"),
        }
    }

    // ── mcp_result_to_tool_output unit tests ──────────────────────────────────

    fn call_result(content: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "content": content, "isError": false })
    }

    fn call_result_error(content: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "content": content, "isError": true })
    }

    // ── text block ─────────────────────────────────────────────────────────

    #[test]
    fn text_block_passes_through() {
        let v = call_result(serde_json::json!([
            { "type": "text", "text": "hello from tool" }
        ]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Text(s) => assert_eq!(s, "hello from tool"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn multiple_text_blocks_joined_with_newline() {
        let v = call_result(serde_json::json!([
            { "type": "text", "text": "line one" },
            { "type": "text", "text": "line two" }
        ]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Text(s) => {
                assert!(s.contains("line one"), "{s}");
                assert!(s.contains("line two"), "{s}");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // ── image block ─────────────────────────────────────────────────────────

    #[test]
    fn image_block_returns_blocks_with_inline_image() {
        // "AAAA" is valid base64 (three zero bytes); we pass it through as-is.
        let v = call_result(serde_json::json!([
            { "type": "image", "data": "AAAA", "mimeType": "image/png" }
        ]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1, "should have exactly one block");
                match &blocks[0] {
                    ToolBlock::Image { media_type, data } => {
                        assert_eq!(media_type, "image/png");
                        assert_eq!(data, "AAAA");
                    }
                    other => panic!("expected Image block, got {other:?}"),
                }
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn text_plus_image_returns_mixed_blocks() {
        let v = call_result(serde_json::json!([
            { "type": "text", "text": "screenshot below" },
            { "type": "image", "data": "AAAA", "mimeType": "image/webp" }
        ]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                assert!(matches!(&blocks[0], ToolBlock::Text { text } if text == "screenshot below"));
                assert!(matches!(&blocks[1], ToolBlock::Image { media_type, .. } if media_type == "image/webp"));
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    // ── resource block (text) ───────────────────────────────────────────────

    #[test]
    fn resource_text_block_prefixes_uri() {
        let v = call_result(serde_json::json!([{
            "type": "resource",
            "resource": {
                "uri": "file:///workspace/notes.txt",
                "mimeType": "text/plain",
                "text": "contents of the file"
            }
        }]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Text(s) => {
                assert!(
                    s.contains("file:///workspace/notes.txt"),
                    "URI should appear in output: {s}"
                );
                assert!(
                    s.contains("contents of the file"),
                    "text should appear in output: {s}"
                );
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn resource_text_block_without_uri_omits_prefix() {
        let v = call_result(serde_json::json!([{
            "type": "resource",
            "resource": {
                "uri": "",
                "mimeType": "text/plain",
                "text": "bare text"
            }
        }]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Text(s) => assert_eq!(s, "bare text"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // ── resource block (blob, image mime) ──────────────────────────────────

    #[test]
    fn resource_blob_image_passes_through_inline() {
        let v = call_result(serde_json::json!([{
            "type": "resource",
            "resource": {
                "uri": "file:///screenshot.png",
                "mimeType": "image/png",
                "blob": "AAAA"
            }
        }]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert!(
                    matches!(&blocks[0], ToolBlock::Image { media_type, .. } if media_type == "image/png")
                );
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    // ── resource block (blob, non-image mime) ───────────────────────────────

    #[test]
    fn resource_blob_non_image_attempts_persistence() {
        // The function tries to persist; if the data root is unavailable in
        // the test environment, it returns an informative fallback text.
        // Either way, the output must NOT be empty and must NOT silently drop
        // the block.
        let v = call_result(serde_json::json!([{
            "type": "resource",
            "resource": {
                "uri": "file:///report.pdf",
                "mimeType": "application/pdf",
                "blob": "AAAA"   // valid base64
            }
        }]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        let text = match out {
            ToolOutput::Text(s) => s,
            ToolOutput::Blocks(blocks) if blocks.len() == 1 => match &blocks[0] {
                ToolBlock::Text { text } => text.clone(),
                other => panic!("unexpected block: {other:?}"),
            },
            other => panic!("expected Text or single text block, got {other:?}"),
        };
        // The result must reference either a saved path or an error message —
        // never a silent drop.
        assert!(
            text.contains("Saved to") || text.contains("could not"),
            "output should describe persistence outcome, got: {text}"
        );
    }

    // ── audio block ─────────────────────────────────────────────────────────

    #[test]
    fn audio_block_attempts_persistence() {
        let v = call_result(serde_json::json!([{
            "type": "audio",
            "data": "AAAA",          // valid base64
            "mimeType": "audio/mpeg"
        }]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        let text = match out {
            ToolOutput::Text(s) => s,
            ToolOutput::Blocks(blocks) if blocks.len() == 1 => match &blocks[0] {
                ToolBlock::Text { text } => text.clone(),
                other => panic!("unexpected block: {other:?}"),
            },
            other => panic!("expected Text result for audio block, got {other:?}"),
        };
        assert!(
            text.contains("Saved to") || text.contains("could not"),
            "audio output should describe persistence outcome: {text}"
        );
    }

    // ── resource_link block ─────────────────────────────────────────────────

    #[test]
    fn resource_link_formats_name_uri_description() {
        let v = call_result(serde_json::json!([{
            "type": "resource_link",
            "uri": "https://example.com/data",
            "name": "Example Dataset",
            "description": "A dataset for testing"
        }]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Text(s) => {
                assert!(s.contains("Example Dataset"), "{s}");
                assert!(s.contains("https://example.com/data"), "{s}");
                assert!(s.contains("A dataset for testing"), "{s}");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn resource_link_without_description_still_includes_uri() {
        let v = call_result(serde_json::json!([{
            "type": "resource_link",
            "uri": "https://example.com/minimal",
            "name": "Minimal"
        }]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Text(s) => {
                assert!(s.contains("Minimal"), "{s}");
                assert!(s.contains("https://example.com/minimal"), "{s}");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // ── structuredContent ───────────────────────────────────────────────────

    #[test]
    fn structured_content_included_as_json_text_block() {
        let v = serde_json::json!({
            "content": [{ "type": "text", "text": "done" }],
            "isError": false,
            "structuredContent": { "items": [1, 2, 3], "total": 3 }
        });
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Text(s) => {
                assert!(s.contains("done"), "{s}");
                assert!(
                    s.contains("Structured result:"),
                    "structuredContent should be labelled: {s}"
                );
                assert!(s.contains("total"), "JSON fields should appear: {s}");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn structured_content_only_no_regular_content() {
        let v = serde_json::json!({
            "content": [],
            "isError": false,
            "structuredContent": { "key": "value" }
        });
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Text(s) => {
                assert!(s.contains("Structured result:"), "{s}");
                assert!(s.contains("\"key\""), "{s}");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn null_structured_content_is_ignored() {
        let v = serde_json::json!({
            "content": [{ "type": "text", "text": "result" }],
            "isError": false,
            "structuredContent": null
        });
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Text(s) => {
                assert_eq!(s, "result");
            }
            other => panic!("expected plain Text, got {other:?}"),
        }
    }

    // ── isError flag ────────────────────────────────────────────────────────

    #[test]
    fn is_error_true_produces_recoverable_error_output() {
        let v = call_result_error(serde_json::json!([
            { "type": "text", "text": "tool failed" }
        ]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Error { message, recoverable } => {
                assert!(recoverable, "MCP isError should be recoverable");
                assert!(message.contains("tool failed"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn is_error_with_image_block_downgrades_to_text() {
        // An error result containing an image block should not forward raw
        // base64 — the image is described as text in the error message.
        let v = call_result_error(serde_json::json!([
            { "type": "image", "data": "AAAA", "mimeType": "image/jpeg" }
        ]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);
        match out {
            ToolOutput::Error { message, .. } => {
                assert!(
                    message.contains("[image: image/jpeg]"),
                    "error message should describe the image: {message}"
                );
                assert!(!message.contains("AAAA"), "error should not leak base64: {message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ── blob persistence under data root ────────────────────────────────────

    #[test]
    fn audio_blob_persisted_under_data_root_and_path_in_output() {
        // Pin the data root to a tempdir; the guard serializes against every
        // other test in this binary that touches the same process-global var.
        let guard = crate::test_env::DataDirGuard::new();

        // "hello" encoded as standard base64
        let hello_b64 = "aGVsbG8=";
        let v = call_result(serde_json::json!([{
            "type": "audio",
            "data": hello_b64,
            "mimeType": "audio/mpeg"
        }]));
        let out = mcp_result_to_tool_output("srv", "tool", &serde_json::json!({}), v);

        let text = match out {
            ToolOutput::Text(s) => s,
            ToolOutput::Blocks(blocks) if blocks.len() == 1 => match &blocks[0] {
                ToolBlock::Text { text } => text.clone(),
                other => panic!("unexpected block: {other:?}"),
            },
            other => panic!("expected Text result for audio, got {other:?}"),
        };

        // The output must reference a path under data_root/mcp-output/
        assert!(
            text.contains("Saved to"),
            "text should start with 'Saved to': {text}"
        );
        let expected_prefix = guard.data_dir().join("mcp-output").display().to_string();
        assert!(
            text.contains(&expected_prefix),
            "output path should be under data_root/mcp-output, got: {text}"
        );

        // The file must actually exist at the reported path.
        // Extract the path from "Saved to <path> (...)".
        if let Some(rest) = text.strip_prefix("Saved to ") {
            if let Some(end) = rest.rfind(" (") {
                let path_str = &rest[..end];
                let path = std::path::Path::new(path_str);
                assert!(path.exists(), "blob file should exist at {path_str}");
                assert_eq!(
                    std::fs::read(path).unwrap(),
                    b"hello",
                    "file contents should match the decoded base64"
                );
            }
        }
    }

    // ── Outbound _meta correlation tests ──────────────────────────────────────

    /// Verify that a `tools/call` issued by the adapter includes `_meta` with
    /// `launchpad/toolUseId`.  The echo_meta fixture echoes `params._meta`
    /// back as text, so we can inspect what the server received.
    #[tokio::test]
    async fn outbound_params_include_meta_correlation_id() {
        let bin = echo_server_bin();
        let mut env = HashMap::new();
        env.insert("MCP_BEHAVIOR".to_string(), "echo_meta".to_string());
        let handle = McpClientHandle::spawn("meta_srv", bin.to_str().unwrap(), &[], &env)
            .await
            .expect("should spawn echo_mcp_server");
        let adapter = McpToolAdapter::new(
            "meta_srv",
            "echo",
            "echo meta",
            serde_json::json!({ "type": "object" }),
            handle.clone(),
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations::default(),
            None,
        );
        let ctx = RunnerContext::new("sess", "agent").unwrap();

        let output = adapter.invoke(serde_json::json!({}), &ctx).await.unwrap();

        let text = match output {
            ToolOutput::Text(s) => s,
            other => panic!("expected Text, got {other:?}"),
        };

        assert!(
            text.contains("launchpad/toolUseId"),
            "outbound _meta must include launchpad/toolUseId; server echoed: {text}"
        );
        assert!(
            text.contains("progressToken"),
            "outbound _meta must also contain progressToken; server echoed: {text}"
        );

        handle.shutdown().await;
    }

    // ── search_hint accessor test ─────────────────────────────────────────────

    #[test]
    fn search_hint_accessor_returns_stored_value() {
        let handle = McpClientHandle::unreachable_for_test("srv");
        let adapter = McpToolAdapter::new(
            "srv",
            "hinted_tool",
            "a tool with a search hint",
            serde_json::json!({ "type": "object" }),
            handle,
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations::default(),
            Some("database query engine".to_string()),
        );
        assert_eq!(adapter.search_hint(), Some("database query engine"));
    }

    #[test]
    fn search_hint_accessor_none_when_absent() {
        let handle = McpClientHandle::unreachable_for_test("srv");
        let adapter = McpToolAdapter::new(
            "srv",
            "plain_tool",
            "no hint",
            serde_json::json!({ "type": "object" }),
            handle,
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations::default(),
            None,
        );
        assert_eq!(adapter.search_hint(), None);
    }

    // ── AuthRequired self-heal ─────────────────────────────────────────────────

    /// Builds a minimal streamable-HTTP MCP mock: `initialize` and
    /// `notifications/initialized` always succeed; `tools/call` returns HTTP
    /// 401 for the first `fail_calls` attempts and a successful text result
    /// thereafter. Returns the bound URL plus shared counters for asserting
    /// on call counts.
    async fn spawn_auth_mock_server(
        fail_calls: usize,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use axum::{routing::post, Router};
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let initialize_calls = Arc::new(AtomicUsize::new(0));
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let init_counter = initialize_calls.clone();
        let call_counter = tool_calls.clone();

        let app = Router::new().route(
            "/mcp",
            post(move |body: axum::body::Bytes| {
                let init_counter = init_counter.clone();
                let call_counter = call_counter.clone();
                async move {
                    let req: serde_json::Value =
                        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let id = req.get("id").cloned().unwrap_or(serde_json::json!(1));

                    match method {
                        "initialize" => {
                            init_counter.fetch_add(1, Ordering::SeqCst);
                            (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "protocolVersion": "2024-11-05",
                                        "serverInfo": { "name": "auth-test-server", "version": "1.0" },
                                        "capabilities": { "tools": {} }
                                    }
                                })),
                            )
                        }
                        "notifications/initialized" => {
                            (axum::http::StatusCode::OK, axum::Json(serde_json::Value::Null))
                        }
                        "tools/call" => {
                            let n = call_counter.fetch_add(1, Ordering::SeqCst);
                            if n < fail_calls {
                                return (
                                    axum::http::StatusCode::UNAUTHORIZED,
                                    axum::Json(serde_json::Value::Null),
                                );
                            }
                            (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "content": [{ "type": "text", "text": "ok-after-reconnect" }],
                                        "isError": false
                                    }
                                })),
                            )
                        }
                        _ => (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
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

        (format!("http://127.0.0.1:{}/mcp", addr.port()), initialize_calls, tool_calls)
    }

    #[tokio::test]
    async fn auth_required_triggers_one_reconnect_and_retry_then_succeeds() {
        // First tools/call attempt returns 401; the adapter should reconnect
        // (re-running the handshake) and retry exactly once, succeeding on
        // the second attempt.
        let (url, initialize_calls, tool_calls) = spawn_auth_mock_server(1).await;

        let handle = McpClientHandle::connect_http("auth_test_srv", &url)
            .await
            .expect("initial connect should succeed");
        let adapter = McpToolAdapter::new(
            "auth_test_srv",
            "some_tool",
            "desc",
            serde_json::json!({ "type": "object" }),
            handle,
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations::default(),
            None,
        );
        let ctx = RunnerContext::new("sess", "agent").unwrap();

        let output = adapter
            .invoke(serde_json::json!({}), &ctx)
            .await
            .expect("invoke must never return Err — fail-open even on auth failure");

        match output {
            ToolOutput::Text(s) => assert!(
                s.contains("ok-after-reconnect"),
                "expected the retried call's success text: {s}"
            ),
            other => panic!("expected successful text output after reconnect, got {other:?}"),
        }

        assert_eq!(
            tool_calls.load(Ordering::SeqCst),
            2,
            "tools/call should be attempted exactly twice: once failing with 401, once succeeding after reconnect"
        );
        assert_eq!(
            initialize_calls.load(Ordering::SeqCst),
            2,
            "initialize should run once for the initial connect and once for the reconnect"
        );
    }

    #[tokio::test]
    async fn auth_required_persisting_after_reconnect_does_not_loop() {
        // tools/call returns 401 unconditionally. The adapter must attempt
        // exactly one reconnect+retry (not loop forever) and fail open with
        // a recoverable error, per McpToolAdapter::invoke's historical
        // contract of never returning Err.
        let (url, initialize_calls, tool_calls) = spawn_auth_mock_server(usize::MAX).await;

        let handle = McpClientHandle::connect_http("auth_test_srv", &url)
            .await
            .expect("initial connect should succeed");
        let adapter = McpToolAdapter::new(
            "auth_test_srv",
            "some_tool",
            "desc",
            serde_json::json!({ "type": "object" }),
            handle,
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations::default(),
            None,
        );
        let ctx = RunnerContext::new("sess", "agent").unwrap();

        let output = adapter
            .invoke(serde_json::json!({}), &ctx)
            .await
            .expect("invoke must never return Err even when auth never recovers");

        match output {
            ToolOutput::Error { recoverable, .. } => {
                assert!(recoverable, "persisting auth failure should surface as a recoverable error");
            }
            other => panic!("expected recoverable Error output, got {other:?}"),
        }

        assert_eq!(
            tool_calls.load(Ordering::SeqCst),
            2,
            "tools/call must be attempted exactly twice total — no retry loop beyond the single reconnect"
        );
        assert_eq!(
            initialize_calls.load(Ordering::SeqCst),
            2,
            "exactly one reconnect (second initialize) should be attempted, never more"
        );
    }

    /// Bearer-gated MCP mock server for the live self-heal test: `initialize`
    /// always succeeds regardless of auth (the real-world shape of the
    /// onboarding bug — the handshake doesn't require a credential, only
    /// `tools/call` does), and `tools/call` 401s unless the `Authorization`
    /// header exactly matches `Bearer <expected_token>`. Returns the bound
    /// URL plus shared counters for asserting on call counts.
    async fn spawn_expected_bearer_mock_server(
        expected_token: &str,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use axum::http::HeaderMap;
        use axum::{routing::post, Router};
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let initialize_calls = Arc::new(AtomicUsize::new(0));
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let init_counter = initialize_calls.clone();
        let call_counter = tool_calls.clone();
        let expected_header = format!("Bearer {expected_token}");

        let app = Router::new().route(
            "/mcp",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let init_counter = init_counter.clone();
                let call_counter = call_counter.clone();
                let expected_header = expected_header.clone();
                async move {
                    let req: serde_json::Value =
                        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let id = req.get("id").cloned().unwrap_or(serde_json::json!(1));

                    match method {
                        "initialize" => {
                            init_counter.fetch_add(1, Ordering::SeqCst);
                            (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "protocolVersion": "2024-11-05",
                                        "serverInfo": { "name": "live-401-test-server", "version": "1.0" },
                                        "capabilities": { "tools": {} }
                                    }
                                })),
                            )
                        }
                        "notifications/initialized" => {
                            (axum::http::StatusCode::OK, axum::Json(serde_json::Value::Null))
                        }
                        "tools/call" => {
                            call_counter.fetch_add(1, Ordering::SeqCst);
                            let authorized = headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .map(|v| v == expected_header)
                                .unwrap_or(false);
                            if !authorized {
                                return (
                                    axum::http::StatusCode::UNAUTHORIZED,
                                    axum::Json(serde_json::Value::Null),
                                );
                            }
                            (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({
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
                            axum::Json(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
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

        (format!("http://127.0.0.1:{}/mcp", addr.port()), initialize_calls, tool_calls)
    }

    #[tokio::test]
    async fn live_401_self_heals_after_credential_written_mid_flight() {
        // Reproduces the onboarding first-run bug end-to-end through the
        // PUBLIC invoke() path (composing the two halves that are otherwise
        // only tested separately): a real McpClientHandle connects before
        // any credential exists — the running-process shape where the
        // handle predates a user finishing OAuth — a real McpTokenStore has
        // the credential written to it AFTER the handle (and adapter)
        // already exist, and the very next tool call must self-heal via the
        // AUTOMATIC 401 -> reconnect -> retry path inside
        // McpToolAdapter::invoke. reconnect() is never called by hand here.
        use ao_engine_tools_provider_config::mcp_token_store::{
            derive_server_key, McpTokenRecord, McpTokenStore,
        };
        use std::sync::Arc;

        let (url, initialize_calls, tool_calls) =
            spawn_expected_bearer_mock_server("fresh-token").await;

        // Connect with NO credential present in the token store — mirrors
        // from_config_auth's Ok(None) branch: the handshake doesn't require
        // auth, so the handle comes up fine with no bearer token installed.
        let handle = McpClientHandle::connect_http("live_401_srv", &url)
            .await
            .expect("initial unauthenticated connect should succeed");

        let dir = tempfile::tempdir().expect("tempdir");
        let token_store =
            Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));
        let server_key = derive_server_key("live_401_srv", Some(&url), "http");

        // Attach the token source exactly the way from_config_auth does —
        // unconditionally on Ok, even though no credential exists yet.
        handle.attach_http_token_source(Arc::clone(&token_store), server_key.clone());

        let adapter = McpToolAdapter::new(
            "live_401_srv",
            "some_tool",
            "desc",
            serde_json::json!({ "type": "object" }),
            handle,
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations::default(),
            None,
        );

        // The credential arrives AFTER the handle and adapter already
        // exist — the new user finishes connecting their account
        // mid-session, elsewhere in the running app.
        token_store
            .set(
                &server_key,
                &McpTokenRecord {
                    access_token: "fresh-token".to_string(),
                    refresh_token: None,
                    expires_at: None,
                    scope: None,
                    client_id: "test-client".to_string(),
                    client_secret: None,
                    token_endpoint: None,
                },
            )
            .expect("store credential");

        let ctx = RunnerContext::new("sess", "agent").unwrap();

        // Drive it through the adapter's public invoke() so the automatic
        // 401 -> reconnect -> retry path fires — not a hand-called reconnect().
        let output = adapter
            .invoke(serde_json::json!({}), &ctx)
            .await
            .expect("invoke must never return Err");

        match output {
            ToolOutput::Text(s) => assert!(
                s.contains("authenticated-ok"),
                "expected the retried call's success text: {s}"
            ),
            other => panic!("expected successful text output after self-heal, got {other:?}"),
        }

        assert_eq!(
            tool_calls.load(Ordering::SeqCst),
            2,
            "tools/call should be attempted exactly twice: once 401ing with no token installed, once succeeding after the automatic reconnect picked up the credential written mid-flight"
        );
        assert_eq!(
            initialize_calls.load(Ordering::SeqCst),
            2,
            "initialize should run once for the initial connect and exactly once more for the single automatic reconnect"
        );
    }

    /// A `/token` endpoint that always returns RFC 6749 `invalid_grant`,
    /// modeling a provider (e.g. Notion) that has revoked the grant behind a
    /// refresh token. Returns the bound URL plus a call counter.
    async fn spawn_invalid_grant_token_endpoint() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use axum::{routing::post, Router};
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let call_count = Arc::new(AtomicUsize::new(0));
        let counter = call_count.clone();

        let app = Router::new().route(
            "/token",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(serde_json::json!({
                            "error": "invalid_grant",
                            "error_description": "Refresh token reuse detected",
                        })),
                    )
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("http://127.0.0.1:{}/token", addr.port()), call_count)
    }

    #[tokio::test]
    async fn live_revoked_grant_surfaces_terminal_error_without_extra_retry() {
        // End-to-end through the PUBLIC invoke() path, the same shape as
        // `live_401_self_heals_after_credential_written_mid_flight` above,
        // but for a grant Notion has revoked rather than one that's merely
        // stale: tools/call 401s unconditionally (no valid bearer was ever
        // installed), the adapter's self-heal reconnects, and the refresh
        // attempt inside that reconnect hits a `/token` endpoint returning
        // `invalid_grant`. The user must see a distinct, actionable message
        // — not the generic "credential refresh failed" bucket — and the
        // adapter must not perform a second tools/call attempt on top of
        // the one that already 401'd.
        use ao_engine_tools_provider_config::mcp_token_store::{
            derive_server_key, McpTokenRecord, McpTokenStore,
        };
        use std::sync::Arc;

        let (mcp_url, initialize_calls, tool_calls) = spawn_auth_mock_server(usize::MAX).await;
        let (token_url, token_calls) = spawn_invalid_grant_token_endpoint().await;

        let handle = McpClientHandle::connect_http("revoked_grant_live_srv", &mcp_url)
            .await
            .expect("initial connect should succeed");

        let dir = tempfile::tempdir().expect("tempdir");
        let token_store = Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));
        let server_key = derive_server_key("revoked_grant_live_srv", Some(&mcp_url), "http");
        token_store
            .set(
                &server_key,
                &McpTokenRecord {
                    access_token: "stale-access-token".to_string(),
                    refresh_token: Some("reused-refresh-token".to_string()),
                    expires_at: Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
                    scope: None,
                    client_id: "test-client".to_string(),
                    client_secret: None,
                    token_endpoint: Some(token_url),
                },
            )
            .expect("seed expiring credential with refresh token");
        handle.attach_http_token_source(Arc::clone(&token_store), server_key);

        let adapter = McpToolAdapter::new(
            "revoked_grant_live_srv",
            "some_tool",
            "desc",
            serde_json::json!({ "type": "object" }),
            handle,
            LoadPolicy::AlwaysLoad,
            McpToolAnnotations::default(),
            None,
        );
        let ctx = RunnerContext::new("sess", "agent").unwrap();

        let output = adapter
            .invoke(serde_json::json!({}), &ctx)
            .await
            .expect("invoke must never return Err — fail-open even on a revoked grant");

        match output {
            ToolOutput::Error { message, recoverable } => {
                assert!(recoverable, "the model should be told to ask the user to re-authorize, not treat this as fatal");
                assert!(message.contains("revoked"), "message should name the terminal state: {message}");
                assert!(
                    message.contains("revoked_grant_live_srv"),
                    "message should name the server: {message}"
                );
            }
            other => panic!("expected a recoverable Error output, got {other:?}"),
        }

        assert_eq!(token_calls.load(Ordering::SeqCst), 1, "the revoked grant must not be retried");
        assert_eq!(
            tool_calls.load(Ordering::SeqCst),
            1,
            "no second tools/call attempt — reconnect short-circuited on the revoked grant before any retry"
        );
        assert_eq!(
            initialize_calls.load(Ordering::SeqCst),
            1,
            "no second initialize — reconnect must not fall through to a handshake attempt"
        );
    }

    // ── payload stash: zero agent-facing change ───────────────────────────────

    #[test]
    fn snapshot_text_only_output_byte_identical() {
        let v = call_result(serde_json::json!([
            { "type": "text", "text": "line one" },
            { "type": "text", "text": "line two" }
        ]));
        let out = mcp_result_to_tool_output(
            "snap-srv",
            "snap-tool-text",
            &serde_json::json!({ "q": 1 }),
            v,
        );
        match out {
            ToolOutput::Text(s) => assert_eq!(s, "line one\nline two"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_structured_content_output_byte_identical() {
        let v = serde_json::json!({
            "content": [{ "type": "text", "text": "done" }],
            "isError": false,
            "structuredContent": { "items": [1, 2, 3], "total": 3 }
        });
        let out = mcp_result_to_tool_output(
            "snap-srv",
            "snap-tool-structured",
            &serde_json::json!({ "q": 2 }),
            v,
        );
        let expected_json = serde_json::to_string_pretty(&serde_json::json!({
            "items": [1, 2, 3],
            "total": 3
        }))
        .unwrap();
        let expected = format!("done\nStructured result:\n{expected_json}");
        match out {
            ToolOutput::Text(s) => assert_eq!(s, expected),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn stash_records_structured_and_text_before_flatten() {
        let v = serde_json::json!({
            "content": [{ "type": "text", "text": "done" }],
            "isError": false,
            "structuredContent": { "items": [1, 2, 3], "total": 3 }
        });
        let args = serde_json::json!({ "b": 2, "a": 1 });
        let _ = mcp_result_to_tool_output("stash-test-srv", "stash-test-tool", &args, v);

        let hash = payload_stash::hash_args(&args);
        let stashed = payload_stash::global()
            .get("stash-test-srv", "stash-test-tool", &hash)
            .expect("stash entry should be recorded");
        assert_eq!(stashed.text.as_deref(), Some("done"));
        assert_eq!(
            stashed.structured,
            Some(serde_json::json!({ "items": [1, 2, 3], "total": 3 }))
        );
    }

    #[test]
    fn stash_write_does_not_change_returned_tool_output() {
        // Calling twice with the same key (so the second call overwrites the
        // stash entry) must not change the returned ToolOutput at all.
        let build_result = || {
            serde_json::json!({
                "content": [{ "type": "text", "text": "done" }],
                "isError": false,
                "structuredContent": { "n": 1 }
            })
        };
        let args = serde_json::json!({ "k": "same" });
        let first = mcp_result_to_tool_output(
            "stash-idempotent-srv",
            "tool",
            &args,
            build_result(),
        );
        let second = mcp_result_to_tool_output(
            "stash-idempotent-srv",
            "tool",
            &args,
            build_result(),
        );
        match (first, second) {
            (ToolOutput::Text(a), ToolOutput::Text(b)) => assert_eq!(a, b),
            other => panic!("expected two Text outputs, got {other:?}"),
        }
    }
}
