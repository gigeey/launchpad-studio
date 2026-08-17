//! Internal MCP (Model Context Protocol) bridge.
//!
//! This crate translates MCP JSON-RPC requests into dispatch calls against
//! a [`Registry`] of tools, and translates [`ToolOutput`] results back into
//! MCP wire-format responses. It is transport-agnostic on purpose: callers
//! own how requests arrive (HTTP, stdio, in-process) and feed parsed
//! [`JsonRpcRequest`] values into [`handle_request`].
//!
//! ## Wire shape
//!
//! All requests follow JSON-RPC 2.0. The methods understood today:
//!
//! - `initialize` — return server capabilities + identity.
//! - `tools/list` — enumerate every tool registered in the [`Registry`],
//!   producing one entry per tool with its name, human description, and
//!   JSON Schema for the `arguments` field.
//! - `tools/call` — invoke a named tool with the given `arguments`. The
//!   tool runs against the supplied [`RunnerContext`], and the resulting
//!   [`ToolOutput`] is mapped to an MCP `CallToolResult` (text content +
//!   `isError` flag).
//!
//! Notifications (requests without an `id` field) are accepted but produce
//! no response, per the JSON-RPC spec.
//!
//! ## What this crate does NOT do
//!
//! - Authentication: the caller (HTTP route, stdio transport, etc.) is
//!   responsible for validating any bearer token / session identity before
//!   constructing a [`RunnerContext`] and calling [`handle_request`].
//! - Context construction: [`RunnerContext`] is non-trivial to build and
//!   varies per session. The caller supplies a pre-built ctx whose identity
//!   already reflects the authenticated session.
//! - Streaming: [`tools/call`] returns a single result. Long-running tools
//!   block the call until completion.
//!
//! [`Registry`]: ao_engine_tools_core::Registry
//! [`RunnerContext`]: ao_engine_tools_core::context::RunnerContext
//! [`ToolOutput`]: ao_engine_tools_core::output::ToolOutput

pub mod handler;
pub mod protocol;

pub use handler::{handle_request, McpBridgeError};
pub use protocol::{
    CallToolContent, CallToolParams, CallToolResult, InitializeResult, JsonRpcError,
    JsonRpcRequest, JsonRpcResponse, McpTool, ServerInfo, ToolAnnotations, JSONRPC_VERSION,
    MCP_PROTOCOL_VERSION,
};
