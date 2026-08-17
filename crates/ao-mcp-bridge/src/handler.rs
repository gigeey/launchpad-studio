//! Request dispatch — maps incoming JSON-RPC methods to [`Registry`] actions.
//!
//! Public entry point is [`handle_request`], which is transport-agnostic and
//! returns `Option<JsonRpcResponse>` (`None` for notifications, per spec).

use std::sync::Arc;

use ao_engine_tools_core::context::RunnerContext;
use ao_engine_tools_core::output::ToolOutput;
use ao_engine_tools_core::registry::{Registry, ToolRef};
use serde_json::{json, Value};
use thiserror::Error;
use tracing::{debug, warn};

use crate::protocol::{
    CallToolContent, CallToolParams, CallToolResult, InitializeResult, JsonRpcError,
    JsonRpcRequest, JsonRpcResponse, ListToolsResult, McpTool, ServerCapabilities, ServerInfo,
    ToolAnnotations, ToolsCapability, JSONRPC_VERSION, MCP_PROTOCOL_VERSION,
};

/// Errors raised at the bridge layer (i.e. before a tool is even invoked).
/// These are surfaced as JSON-RPC errors, not as `CallToolResult { is_error: true }`,
/// because they indicate a malformed request rather than a tool's "no" answer.
#[derive(Debug, Error)]
pub enum McpBridgeError {
    #[error("unsupported jsonrpc version: {0:?}")]
    UnsupportedJsonRpcVersion(String),

    #[error("unknown method: {0}")]
    UnknownMethod(String),

    #[error("invalid params: {0}")]
    InvalidParams(String),
}

/// Handle a single inbound JSON-RPC request.
///
/// Returns `None` if the request is a notification (no `id`); otherwise
/// returns a fully formed response (success or error variant).
///
/// `ctx.tool_admission` is consulted by both `tools/list` (a denied tool is
/// omitted from the listing entirely) and `tools/call` (a denied tool is
/// rejected the same way an unregistered one would be) — a gated tool must
/// never be discoverable or callable through this bridge. The rest of `ctx`
/// is only consulted by `tools/call`. Callers building a per-request ctx may
/// pass a placeholder for non-call
/// methods, but in practice the cost of building one is negligible.
pub async fn handle_request(
    req: JsonRpcRequest,
    registry: &Registry,
    ctx: &RunnerContext,
) -> Option<JsonRpcResponse> {
    let is_notification = req.id.is_none();
    let id = req.id.clone();

    // Per JSON-RPC 2.0 we MUST accept `"jsonrpc": "2.0"` and nothing else.
    // A mismatch is reported, but only if the caller expects a response —
    // notifications swallow errors silently.
    if req.jsonrpc != JSONRPC_VERSION {
        warn!(
            got = req.jsonrpc,
            expected = JSONRPC_VERSION,
            "rejecting request with non-2.0 jsonrpc field"
        );
        if is_notification {
            return None;
        }
        return Some(JsonRpcResponse::err(
            id,
            JsonRpcError::new(
                JsonRpcError::INVALID_REQUEST,
                format!("Expected jsonrpc=\"2.0\", got {:?}", req.jsonrpc),
            ),
        ));
    }

    let response = match req.method.as_str() {
        "initialize" => Ok(handle_initialize()),
        "initialized" | "notifications/initialized" => {
            // Client confirmation that initialize completed. No-op on our side.
            debug!("received `initialized` notification");
            return None;
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(handle_tools_list(registry, ctx)),
        "tools/call" => handle_tools_call(req.params.unwrap_or(Value::Null), registry, ctx).await,
        other => Err(JsonRpcError::method_not_found(other)),
    };

    if is_notification {
        return None;
    }

    Some(match response {
        Ok(result) => JsonRpcResponse::ok(id, result),
        Err(err) => JsonRpcResponse::err(id, err),
    })
}

// ── Method handlers ────────────────────────────────────────────────────────

fn handle_initialize() -> Value {
    let result = InitializeResult {
        protocol_version: MCP_PROTOCOL_VERSION.to_string(),
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapability {
                // We do not push `notifications/tools/list_changed` today.
                // Once the registry grows runtime mutation (e.g. SkillRegister
                // adding tools mid-session), flip this to true and emit.
                list_changed: false,
            }),
        },
        server_info: ServerInfo {
            name: "launchpad-studio".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    };
    serde_json::to_value(result).expect("InitializeResult is always serializable")
}

fn handle_tools_list(registry: &Registry, ctx: &RunnerContext) -> Value {
    let mut tools: Vec<McpTool> = Vec::new();
    for name in registry.list() {
        // A tool the session's admission gate excludes must never be
        // advertised — the CLI process driving this session discovers its
        // available tools exclusively through this listing, so an excluded
        // name has to be invisible here, not merely rejected on `tools/call`.
        if let Some(gate) = ctx.tool_admission.as_ref() {
            if !gate.permits(&name) {
                continue;
            }
        }
        let Some(tool_ref) = registry.lookup(&name) else {
            continue;
        };
        tools.push(McpTool {
            name: tool_ref.name().to_string(),
            description: tool_ref.description().to_string(),
            input_schema: normalize_input_schema(tool_ref.input_schema()),
            annotations: build_tool_annotations(&tool_ref),
        });
    }
    let result = ListToolsResult { tools };
    serde_json::to_value(result).expect("ListToolsResult is always serializable")
}

/// Build the MCP `annotations` object for a tool, or return `None` when no
/// hints apply. A `None` result omits the `annotations` key entirely from the
/// wire, keeping the response identical to the pre-annotations behavior for
/// tools that declare no hints.
fn build_tool_annotations(tool: &ToolRef) -> Option<ToolAnnotations> {
    let read_only = tool.is_concurrency_safe();
    let open_world = tool.mcp_open_world_hint();

    if !read_only && !open_world {
        return None;
    }

    Some(ToolAnnotations {
        read_only_hint: if read_only { Some(true) } else { None },
        open_world_hint: if open_world { Some(true) } else { None },
        ..Default::default()
    })
}

async fn handle_tools_call(
    params: Value,
    registry: &Registry,
    ctx: &RunnerContext,
) -> Result<Value, JsonRpcError> {
    let parsed: CallToolParams = serde_json::from_value(params)
        .map_err(|e| JsonRpcError::invalid_params(format!("malformed tools/call params: {e}")))?;

    let Some(tool_ref) = registry.lookup(&parsed.name) else {
        // Tool not in registry. We surface this as a JSON-RPC error rather
        // than a `CallToolResult { is_error: true }` because the caller
        // (well-behaved client) shouldn't be issuing tools/call for tools
        // that weren't in the last tools/list. Misuse, not a tool "no".
        let mut err = JsonRpcError::invalid_params(format!(
            "unknown tool: {}",
            parsed.name
        ));
        if let Some(suggestion) = registry.nearest_name(&parsed.name) {
            err = err.with_data(json!({ "did_you_mean": suggestion }));
        }
        return Err(err);
    };

    // Backstop for a client that calls a tool from a stale `tools/list`
    // snapshot (fetched before the gate changed) or that skips discovery
    // entirely: a denied tool must be refused here too, with the identical
    // "unknown tool" shape `tools/list` already made it look like — nothing
    // in the response should hint that the tool exists but is blocked.
    if let Some(gate) = ctx.tool_admission.as_ref() {
        if !gate.permits(&parsed.name) {
            return Err(JsonRpcError::invalid_params(format!(
                "unknown tool: {}",
                parsed.name
            )));
        }
    }

    let arguments = parsed.arguments.unwrap_or_else(|| json!({}));

    let outcome = dispatch_one(tool_ref, arguments, ctx).await;

    match outcome {
        Ok(out) => {
            // Post-dispatch hook: an async `AskUserQuestionWithForm` returns
            // `{ posted: true, .. }` without ever touching the form bridge, so
            // unlike the sync path nothing has yet made the form visible to the
            // operator. Wire it up here (transcript entry + snapshot pointer +
            // FormPosted event) so the UI renders the form and takes over the
            // composer. No-op for every other tool / result.
            ao_engine_tools_core::wire_posted_async_form(ctx, &parsed.name, &out).await;
            Ok(tool_output_to_call_result(out))
        }
        Err(err_msg) => {
            // The tool's `invoke` returned `Result::Err`, which the IoTool/
            // EngineTool contract defines as a hard failure (panic, cancel,
            // invariant violation) — distinct from a recoverable refusal.
            // Surface as a JSON-RPC error so the client knows the call did
            // not produce a usable tool result.
            Err(JsonRpcError::internal(format!("tool failed: {err_msg}")))
        }
    }
}

/// Dispatch a single tool by reference. Returns the [`ToolOutput`] on success
/// or a stringified error message on hard failure.
async fn dispatch_one(
    tool_ref: ToolRef,
    arguments: Value,
    ctx: &RunnerContext,
) -> Result<ToolOutput, String> {
    let name = tool_ref.name().to_string();
    let start = std::time::Instant::now();

    let result = match tool_ref {
        ToolRef::Io(tool) => tool.invoke(arguments, ctx).await,
        ToolRef::Engine(tool) => tool.invoke(arguments, ctx).await,
    };

    let elapsed_ms = start.elapsed().as_millis() as u64;
    match &result {
        Ok(out) => {
            debug!(tool = %name, elapsed_ms, ok = true, kind = ?std::mem::discriminant(out), "tool dispatch ok");
        }
        Err(e) => {
            warn!(tool = %name, elapsed_ms, error = %e, "tool dispatch failed");
        }
    }

    result.map_err(|e| e.to_string())
}

/// Map a [`ToolOutput`] to the MCP `CallToolResult` shape.
fn tool_output_to_call_result(out: ToolOutput) -> Value {
    let result = match out {
        ToolOutput::Text(s) => CallToolResult {
            content: vec![CallToolContent::Text { text: s }],
            is_error: false,
        },
        ToolOutput::Structured(v) => CallToolResult {
            // MCP clients expect text content. Tools that carry a compact
            // `text_fallback` rendering (e.g. Glob) surface that so the model
            // reads a clean list rather than a JSON blob; others fall back to
            // their JSON form. A future iteration could grow a dedicated
            // `resource` content variant for structured returns.
            content: vec![CallToolContent::Text {
                text: ToolOutput::structured_to_text(&v),
            }],
            is_error: false,
        },
        ToolOutput::Error {
            message,
            recoverable: _,
        } => CallToolResult {
            content: vec![CallToolContent::Text { text: message }],
            is_error: true,
        },
        // `CallToolContent` carries only text today, so multimodal block
        // results are flattened to their textual summary (binary payloads are
        // described, not inlined). A future iteration can grow an image content
        // variant and map the blocks one-to-one.
        out @ ToolOutput::Blocks(_) => CallToolResult {
            content: vec![CallToolContent::Text {
                text: out.as_text(),
            }],
            is_error: false,
        },
    };
    serde_json::to_value(result).expect("CallToolResult is always serializable")
}

/// Normalize a tool's input schema so it always has an `"object"` top-level
/// type and a `properties` field, even when the tool returned a bare schema
/// fragment. MCP clients reject schemas missing these fields.
fn normalize_input_schema(mut schema: Value) -> Value {
    if !schema.is_object() {
        // Tool gave us something weird (array, scalar). Wrap into a permissive
        // object schema so the wire format is at least valid.
        return json!({
            "type": "object",
            "properties": {},
            "additionalProperties": true,
        });
    }
    let obj = schema.as_object_mut().expect("checked is_object above");
    if !obj.contains_key("type") {
        obj.insert("type".to_string(), Value::String("object".to_string()));
    }
    if !obj.contains_key("properties") {
        obj.insert("properties".to_string(), json!({}));
    }
    schema
}

// ── Re-export for use in tests/integration crates ──────────────────────────

/// Convenience alias for callers that want to hold the registry + ctx via Arc.
pub type SharedRegistry = Arc<Registry>;

#[cfg(test)]
mod tests {
    use super::*;
    use ao_engine_tools_core::context::RunnerContext;
    use ao_engine_tools_core::output::ToolOutput;
    use ao_engine_tools_core::policy::LoadPolicy;
    use ao_engine_tools_core::registry::Registry;
    use ao_engine_tools_core::tool::IoTool;
    use ao_engine_tools_core::ToolAdmission;
    use ao_protocol::error::AoError;
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::sync::Arc;

    /// Test tool that is read-only (concurrency-safe, no open-world hint).
    struct ReadOnlyTool;

    #[async_trait]
    impl IoTool for ReadOnlyTool {
        fn name(&self) -> &str { "read_only_tool" }
        fn description(&self) -> &str { "A read-only tool." }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn load_policy(&self) -> LoadPolicy { LoadPolicy::AlwaysLoad }
        fn is_concurrency_safe(&self) -> bool { true }
        async fn invoke(&self, _input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    /// Test tool that spawns subagents (read-only + open-world).
    struct SubagentTool;

    #[async_trait]
    impl IoTool for SubagentTool {
        fn name(&self) -> &str { "subagent_tool" }
        fn description(&self) -> &str { "A subagent-spawning tool." }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn load_policy(&self) -> LoadPolicy { LoadPolicy::AlwaysLoad }
        fn is_concurrency_safe(&self) -> bool { true }
        fn mcp_open_world_hint(&self) -> bool { true }
        async fn invoke(&self, _input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
            Ok(ToolOutput::text("spawned"))
        }
    }

    /// Test tool that mutates state (no concurrency safety, no open-world hint).
    struct MutatingTool;

    #[async_trait]
    impl IoTool for MutatingTool {
        fn name(&self) -> &str { "mutating_tool" }
        fn description(&self) -> &str { "A state-mutating tool." }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn load_policy(&self) -> LoadPolicy { LoadPolicy::AlwaysLoad }
        async fn invoke(&self, _input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
            Ok(ToolOutput::text("mutated"))
        }
    }

    /// Test tool that echoes its input back as text. Always-load so it
    /// shows up in tools/list without ToolSearch.
    struct EchoTool;

    #[async_trait]
    impl IoTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo the provided message back as text."
        }
        fn input_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": { "msg": { "type": "string" } },
                "required": ["msg"]
            })
        }
        fn load_policy(&self) -> LoadPolicy {
            LoadPolicy::AlwaysLoad
        }
        async fn invoke(
            &self,
            input: Value,
            _ctx: &RunnerContext,
        ) -> Result<ToolOutput, AoError> {
            let msg = input
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("<no msg>");
            Ok(ToolOutput::text(format!("echo: {msg}")))
        }
    }

    /// Test tool that always returns a recoverable error.
    struct RefuseTool;

    #[async_trait]
    impl IoTool for RefuseTool {
        fn name(&self) -> &str {
            "refuse"
        }
        fn description(&self) -> &str {
            "Always refuses with a recoverable error."
        }
        fn input_schema(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        async fn invoke(
            &self,
            _input: Value,
            _ctx: &RunnerContext,
        ) -> Result<ToolOutput, AoError> {
            Ok(ToolOutput::error("computed no", true))
        }
    }

    fn test_registry() -> Registry {
        let mut r = Registry::new();
        r.register_io(Arc::new(EchoTool));
        r.register_io(Arc::new(RefuseTool));
        r
    }

    fn test_ctx() -> RunnerContext {
        RunnerContext::new("sess", "agent").expect("RunnerContext::new should succeed in tests")
    }

    #[tokio::test]
    async fn initialize_returns_server_info_and_capabilities() {
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(json!(1)),
            method: "initialize".to_string(),
            params: None,
        };
        let registry = test_registry();
        let ctx = test_ctx();
        let resp = handle_request(req, &registry, &ctx).await.expect("response");
        let result = resp.result.expect("ok response");
        assert_eq!(result["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], "launchpad-studio");
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn tools_list_includes_registered_tools_with_camel_case_schema() {
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(json!(2)),
            method: "tools/list".to_string(),
            params: None,
        };
        let registry = test_registry();
        let ctx = test_ctx();
        let resp = handle_request(req, &registry, &ctx).await.expect("response");
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        let names: Vec<String> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"echo".to_string()));
        assert!(names.contains(&"refuse".to_string()));

        // Every tool entry must use camelCase `inputSchema` (not snake_case).
        for t in &tools {
            assert!(t.get("inputSchema").is_some());
            assert!(t.get("input_schema").is_none());
        }
    }

    #[tokio::test]
    async fn tools_call_invokes_tool_and_returns_text_content() {
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(json!(3)),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "echo",
                "arguments": { "msg": "hello" }
            })),
        };
        let registry = test_registry();
        let ctx = test_ctx();
        let resp = handle_request(req, &registry, &ctx).await.expect("response");
        let result = resp.result.expect("ok");
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], "echo: hello");
    }

    #[tokio::test]
    async fn tools_call_recoverable_error_surfaces_as_is_error_true() {
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(json!(4)),
            method: "tools/call".to_string(),
            params: Some(json!({ "name": "refuse", "arguments": {} })),
        };
        let registry = test_registry();
        let ctx = test_ctx();
        let resp = handle_request(req, &registry, &ctx).await.expect("response");
        let result = resp.result.expect("ok (the JSON-RPC envelope is ok; isError is on the payload)");
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "computed no");
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_returns_jsonrpc_error_with_suggestion() {
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(json!(5)),
            method: "tools/call".to_string(),
            params: Some(json!({ "name": "ech", "arguments": {} })),
        };
        let registry = test_registry();
        let ctx = test_ctx();
        let resp = handle_request(req, &registry, &ctx).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, JsonRpcError::INVALID_PARAMS);
        assert!(err.message.contains("ech"));
        let data = err.data.expect("data field with suggestion");
        assert_eq!(data["did_you_mean"], "echo");
    }

    #[tokio::test]
    async fn tools_list_omits_tool_denied_by_admission_gate() {
        // Simulates a channel-bridge session: the ctx carries a `Deny` gate
        // naming a registered tool. It must not appear in tools/list at all —
        // the CLI process on the other end of this bridge discovers its tool
        // set exclusively through this listing.
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(json!(7)),
            method: "tools/list".to_string(),
            params: None,
        };
        let registry = test_registry();
        let mut deny = std::collections::HashSet::new();
        deny.insert("echo".to_string());
        let ctx = test_ctx().with_tool_admission(Some(ToolAdmission::Deny(deny)));
        let resp = handle_request(req, &registry, &ctx).await.expect("response");
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        let names: Vec<String> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(!names.contains(&"echo".to_string()));
        assert!(names.contains(&"refuse".to_string()));
    }

    #[tokio::test]
    async fn tools_call_rejects_tool_denied_by_admission_gate() {
        // Backstop: even if a client still calls a denied tool (stale
        // tools/list snapshot, or skipped discovery), the call must be
        // refused with the same shape as an unregistered tool — never
        // dispatched, and never hinting that the tool exists but is blocked.
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(json!(8)),
            method: "tools/call".to_string(),
            params: Some(json!({ "name": "echo", "arguments": { "msg": "hi" } })),
        };
        let registry = test_registry();
        let mut deny = std::collections::HashSet::new();
        deny.insert("echo".to_string());
        let ctx = test_ctx().with_tool_admission(Some(ToolAdmission::Deny(deny)));
        let resp = handle_request(req, &registry, &ctx).await.expect("response");
        let err = resp.error.expect("denied tool call must return a JSON-RPC error");
        assert_eq!(err.code, JsonRpcError::INVALID_PARAMS);
        assert!(err.message.contains("echo"));
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(json!(6)),
            method: "does/not/exist".to_string(),
            params: None,
        };
        let registry = test_registry();
        let ctx = test_ctx();
        let resp = handle_request(req, &registry, &ctx).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, JsonRpcError::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn notifications_return_none_even_on_unknown_method() {
        // No `id` field — notification per JSON-RPC 2.0 spec.
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: None,
            method: "anything".to_string(),
            params: None,
        };
        let registry = test_registry();
        let ctx = test_ctx();
        let resp = handle_request(req, &registry, &ctx).await;
        assert!(resp.is_none());
    }

    #[tokio::test]
    async fn initialized_notification_is_silently_accepted() {
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: None,
            method: "notifications/initialized".to_string(),
            params: None,
        };
        let registry = test_registry();
        let ctx = test_ctx();
        let resp = handle_request(req, &registry, &ctx).await;
        assert!(resp.is_none());
    }

    #[tokio::test]
    async fn wrong_jsonrpc_version_is_rejected() {
        let req = JsonRpcRequest {
            jsonrpc: "1.0".to_string(),
            id: Some(json!(7)),
            method: "tools/list".to_string(),
            params: None,
        };
        let registry = test_registry();
        let ctx = test_ctx();
        let resp = handle_request(req, &registry, &ctx).await.expect("response");
        let err = resp.error.expect("error");
        assert_eq!(err.code, JsonRpcError::INVALID_REQUEST);
    }

    #[test]
    fn normalize_input_schema_adds_missing_type_and_properties() {
        let schema = normalize_input_schema(json!({"required": ["x"]}));
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());
    }

    #[test]
    fn normalize_input_schema_leaves_well_formed_schema_alone() {
        let schema = normalize_input_schema(json!({
            "type": "object",
            "properties": { "x": { "type": "string" } }
        }));
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["x"]["type"], "string");
    }

    #[test]
    fn normalize_input_schema_handles_non_object_input() {
        let schema = normalize_input_schema(json!("a bare string"));
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());
        assert_eq!(schema["additionalProperties"], true);
    }

    // ── Annotations in tools/list ────────────────────────────────────────────

    fn annotation_registry() -> Registry {
        let mut r = Registry::new();
        r.register_io(Arc::new(ReadOnlyTool));
        r.register_io(Arc::new(SubagentTool));
        r.register_io(Arc::new(MutatingTool));
        r
    }

    fn find_tool_in_list<'a>(tools: &'a Vec<Value>, name: &str) -> Option<&'a Value> {
        tools.iter().find(|t| t["name"].as_str() == Some(name))
    }

    #[tokio::test]
    async fn tools_list_read_only_tool_gets_read_only_hint() {
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(json!(1)),
            method: "tools/list".to_string(),
            params: None,
        };
        let registry = annotation_registry();
        let ctx = test_ctx();
        let resp = handle_request(req, &registry, &ctx).await.expect("response");
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();

        let tool = find_tool_in_list(&tools, "read_only_tool").expect("tool in list");
        assert_eq!(
            tool["annotations"]["readOnlyHint"],
            true,
            "read-only tool must carry readOnlyHint=true"
        );
        assert!(
            tool["annotations"].get("openWorldHint").is_none()
                || tool["annotations"]["openWorldHint"] == Value::Null,
            "read-only tool must not carry openWorldHint"
        );
    }

    #[tokio::test]
    async fn tools_list_subagent_tool_gets_read_only_and_open_world() {
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(json!(2)),
            method: "tools/list".to_string(),
            params: None,
        };
        let registry = annotation_registry();
        let ctx = test_ctx();
        let resp = handle_request(req, &registry, &ctx).await.expect("response");
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();

        let tool = find_tool_in_list(&tools, "subagent_tool").expect("tool in list");
        assert_eq!(
            tool["annotations"]["readOnlyHint"], true,
            "subagent tool must carry readOnlyHint=true"
        );
        assert_eq!(
            tool["annotations"]["openWorldHint"], true,
            "subagent tool must carry openWorldHint=true"
        );
    }

    #[tokio::test]
    async fn tools_list_mutating_tool_has_no_annotations() {
        let req = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(json!(3)),
            method: "tools/list".to_string(),
            params: None,
        };
        let registry = annotation_registry();
        let ctx = test_ctx();
        let resp = handle_request(req, &registry, &ctx).await.expect("response");
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();

        let tool = find_tool_in_list(&tools, "mutating_tool").expect("tool in list");
        assert!(
            tool.get("annotations").is_none() || tool["annotations"] == Value::Null,
            "mutating tool must not carry any annotations; got: {:?}",
            tool.get("annotations")
        );
    }
}
