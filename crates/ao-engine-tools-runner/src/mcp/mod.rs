//! MCP (Model Context Protocol) client infrastructure.
//!
//! Provides a stdio JSON-RPC client for spawning and communicating with
//! MCP server subprocesses. Used by [`McpManager`] to integrate
//! external tool servers into the runner's [`Registry`].
//!
//! [`McpManager`]: crate::mcp::manager::McpManager
//! [`Registry`]: ao_engine_tools_core::Registry

pub mod adapter;
pub mod blob_storage;
pub mod client;
pub mod http_client;
pub mod list_resources;
pub mod manager;
pub mod oauth_flow;
pub mod payload_stash;
pub mod read_resource;
pub mod resource_fetch;
pub mod schema_fetch;
pub mod server_auth;

#[cfg(test)]
pub(crate) mod test_support;

pub use adapter::McpToolAdapter;
pub use client::{McpClientHandle, McpError, ServerCapabilities};
pub use list_resources::ListMcpResources;
pub use manager::{McpManager, McpManagerError, McpServerState, McpServerStatus};
pub use oauth_flow::{AuthFlowHandle, AuthServerMetadata, OAuthEngine, OAuthError};
pub use read_resource::ReadMcpResource;
pub use resource_fetch::{fetch_resources, read_resource, McpResourceContent, McpResourceDescriptor};
pub use schema_fetch::{fetch_prompts, fetch_tools, McpPromptDescriptor, McpToolAnnotations, McpToolDescriptor};
pub use server_auth::McpServerAuthTool;
