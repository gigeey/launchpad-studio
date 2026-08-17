//! JSON-RPC 2.0 + MCP wire-format types.
//!
//! Pure data: serde-tagged structs that map 1:1 to the bytes on the wire.
//! No business logic lives here — see [`crate::handler`] for dispatch.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The JSON-RPC version string we accept and emit.
pub const JSONRPC_VERSION: &str = "2.0";

/// The MCP protocol version we advertise during `initialize`. Bumped when
/// we adopt a newer revision of the spec.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

// ── JSON-RPC envelope ─────────────────────────────────────────────────────

/// An inbound JSON-RPC request or notification. Notifications omit `id`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    /// `None` means this is a notification — the caller expects no response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// An outbound JSON-RPC response. Exactly one of `result` or `error` is set.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    /// Echoes the request id. `None` is only legal when the request itself
    /// had no id AND an error must be reported (rare; e.g. parse error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<Value>, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    /// Standard JSON-RPC error codes, per the spec.
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            Self::METHOD_NOT_FOUND,
            format!("Method not found: {method}"),
        )
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(Self::INVALID_PARAMS, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(Self::INTERNAL_ERROR, message)
    }
}

// ── MCP `initialize` ──────────────────────────────────────────────────────

/// Result body for the `initialize` request. The client uses `capabilities`
/// to decide which subsequent methods to call (e.g. only call `tools/list`
/// if `capabilities.tools` is present).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilities {
    /// Present when the server exposes tools. The `list_changed` field
    /// signals whether the server will push `notifications/tools/list_changed`
    /// (we do not today — set to `false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsCapability {
    #[serde(rename = "listChanged", default)]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

// ── MCP `tools/list` ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListToolsResult {
    pub tools: Vec<McpTool>,
}

/// Behavioral hints for a tool in the `tools/list` response. Clients use
/// these to decide scheduling policy (e.g. whether to run multiple calls
/// concurrently) and permission gating. All fields are optional; the entire
/// object is omitted from the wire when no hints apply, so clients that do
/// not understand annotations see no change.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// Human-readable display name, distinct from the machine-readable `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// When `true`, the tool does not modify any state and is safe to run
    /// concurrently with other read-only calls in the same turn. Clients
    /// typically batch consecutive read-only tool_use blocks and issue them
    /// in parallel. When absent or `false`, the client serialises calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// When `true`, the tool may perform irreversible or destructive actions.
    /// Clients may present additional confirmation prompts before issuing such
    /// calls. Absent means the client should assume the default (not
    /// destructive unless the tool's description says otherwise).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// When `true`, repeating the same call with the same arguments produces
    /// the same outcome (no additional side effects on retry).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// When `true`, the tool interacts with external or unpredictable systems
    /// (network calls, subagent spawning, OS side-channels). Clients may
    /// apply looser permission-caching for such tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// One entry in the `tools/list` response. `input_schema` is serialized as
/// `inputSchema` (camelCase) to match the MCP spec. `annotations` is omitted
/// when no hints apply so existing clients see no change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

// ── MCP `tools/call` ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    /// Per the MCP spec, `arguments` is an object whose shape is validated
    /// against the tool's `inputSchema`. Absent means the tool takes no args.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolResult {
    pub content: Vec<CallToolContent>,
    /// `true` if the tool reported a recoverable error. The client sees the
    /// error text in `content[0]`. Hard failures (panic, transport) surface
    /// as JSON-RPC-level errors instead, not here.
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// One chunk of content in a tool result. Today we only emit `Text`; future
/// variants (`image`, `resource`) will expand this enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CallToolContent {
    Text { text: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_with_id_round_trips() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        });
        let req: JsonRpcRequest = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(req.method, "tools/list");
        assert_eq!(req.id, Some(json!(1)));
        let back = serde_json::to_value(&req).unwrap();
        assert_eq!(back["method"], "tools/list");
    }

    #[test]
    fn notification_has_no_id() {
        let raw = json!({"jsonrpc": "2.0", "method": "ping"});
        let req: JsonRpcRequest = serde_json::from_value(raw).unwrap();
        assert!(req.id.is_none());
    }

    #[test]
    fn response_omits_unset_fields() {
        let resp = JsonRpcResponse::ok(Some(json!(7)), json!({"ok": true}));
        let s = serde_json::to_string(&resp).unwrap();
        // No `error` field on the wire.
        assert!(!s.contains("\"error\""));
        assert!(s.contains("\"id\":7"));
    }

    #[test]
    fn mcp_tool_uses_camel_case_input_schema() {
        let tool = McpTool {
            name: "Read".to_string(),
            description: "Read a file".to_string(),
            input_schema: json!({"type": "object"}),
            annotations: None,
        };
        let s = serde_json::to_string(&tool).unwrap();
        assert!(s.contains("\"inputSchema\""));
        assert!(!s.contains("\"input_schema\""));
    }

    #[test]
    fn tool_annotations_omitted_when_none() {
        let tool = McpTool {
            name: "Bash".to_string(),
            description: "Run a command".to_string(),
            input_schema: json!({"type": "object"}),
            annotations: None,
        };
        let s = serde_json::to_string(&tool).unwrap();
        assert!(!s.contains("\"annotations\""), "annotations must be absent for non-annotated tools");
    }

    #[test]
    fn tool_annotations_camel_case_and_partial_fields() {
        let ann = ToolAnnotations {
            read_only_hint: Some(true),
            open_world_hint: Some(true),
            ..Default::default()
        };
        let tool = McpTool {
            name: "Delegate".to_string(),
            description: "Spawn a child agent".to_string(),
            input_schema: json!({"type": "object"}),
            annotations: Some(ann),
        };
        let v = serde_json::to_value(&tool).unwrap();
        let ann_obj = &v["annotations"];
        assert!(ann_obj.is_object(), "annotations must be an object");
        assert_eq!(ann_obj["readOnlyHint"], true, "readOnlyHint must use camelCase");
        assert_eq!(ann_obj["openWorldHint"], true, "openWorldHint must use camelCase");
        // Fields not set must be absent.
        assert!(ann_obj.get("destructiveHint").is_none(), "unset fields must be absent");
        assert!(ann_obj.get("idempotentHint").is_none(), "unset fields must be absent");
        assert!(ann_obj.get("title").is_none(), "unset fields must be absent");
        // Snake-case keys must never appear on the wire.
        let raw = serde_json::to_string(&tool).unwrap();
        assert!(!raw.contains("read_only_hint"), "snake_case must not appear on wire");
    }

    #[test]
    fn call_tool_content_text_variant_serializes_with_type_field() {
        let c = CallToolContent::Text {
            text: "hi".to_string(),
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["type"], "text");
        assert_eq!(v["text"], "hi");
    }

    #[test]
    fn error_codes_match_spec() {
        assert_eq!(JsonRpcError::PARSE_ERROR, -32700);
        assert_eq!(JsonRpcError::INVALID_REQUEST, -32600);
        assert_eq!(JsonRpcError::METHOD_NOT_FOUND, -32601);
        assert_eq!(JsonRpcError::INVALID_PARAMS, -32602);
        assert_eq!(JsonRpcError::INTERNAL_ERROR, -32603);
    }
}
