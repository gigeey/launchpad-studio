//! Auth pseudo-tool that drives the OAuth PKCE flow for MCP servers that
//! return HTTP 401 during the initial handshake.
//!
//! When the manager discovers that a server requires authorization it registers
//! a [`McpServerAuthTool`] in the registry. The model can call this tool to
//! receive an authorization URL; once the user completes the browser redirect,
//! a background task swaps the auth pseudo-tool out of the registry and inserts
//! the server's real tools.

mod prompt;
#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock, Weak};

use ao_engine_tools_core::{
    context::RunnerContext,
    output::ToolOutput,
    policy::LoadPolicy,
    tool::IoTool,
    Registry,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::warn;

use ao_engine_tools_provider_config::mcp_servers::McpAuthConfig;
use ao_engine_tools_provider_config::mcp_token_store::{McpTokenStore, derive_server_key};

use super::{
    adapter::McpToolAdapter,
    client::McpClientHandle,
    manager::McpManager,
    oauth_flow::OAuthEngine,
    schema_fetch::{fetch_tools, McpToolDescriptor},
};

// ── Auth pseudo-tool ──────────────────────────────────────────────────────────

/// An [`IoTool`] that drives the OAuth PKCE flow for an MCP server.
///
/// Registered in the [`Registry`] in place of the server's real tools when
/// the initial HTTP handshake returns 401. Calling this tool returns the
/// authorization URL to the model; a background task monitors the callback
/// and, on success, replaces this pseudo-tool with the server's real tools.
pub struct McpServerAuthTool {
    tool_name: String,
    server_name: String,
    server_url: String,
    server_key: String,
    auth_config: McpAuthConfig,
    token_store: Arc<McpTokenStore>,
    loading_policy: LoadPolicy,
    /// Weak handle back to the owning [`McpManager`], shared with the manager's
    /// own cell. Empty until the manager calls `attach_self_reference` after
    /// Arc-wrapping. When set, a completed OAuth flow promotes the server to
    /// `Connected` in the manager so its status stays in sync with the UI;
    /// when empty (e.g. unit tests), the flow falls back to registry-only
    /// tool registration.
    manager_ref: Arc<OnceLock<Weak<McpManager>>>,
}

impl McpServerAuthTool {
    /// Create a new auth pseudo-tool for the given server.
    ///
    /// `manager_ref` is the owning manager's shared self-reference cell; pass a
    /// clone of `McpManager::self_ref` so the post-auth callback can reach the
    /// manager and update its tracked connection state.
    pub fn new(
        server_name: impl Into<String>,
        server_url: impl Into<String>,
        auth_config: McpAuthConfig,
        token_store: Arc<McpTokenStore>,
        loading_policy: LoadPolicy,
        manager_ref: Arc<OnceLock<Weak<McpManager>>>,
    ) -> Self {
        let server_name = server_name.into();
        let server_url = server_url.into();
        let server_key = derive_server_key(&server_name, Some(&server_url), "http");
        let tool_name = format!("mcp__{server_name}__authorize");
        Self {
            tool_name,
            server_name,
            server_url,
            server_key,
            auth_config,
            token_store,
            loading_policy,
            manager_ref,
        }
    }
}

#[async_trait]
impl IoTool for McpServerAuthTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    fn load_policy(&self) -> LoadPolicy {
        self.loading_policy
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    async fn invoke(&self, _input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let engine = OAuthEngine::new(reqwest::Client::new());
        match engine
            .begin_authorization_flow(
                &self.server_key,
                &self.server_url,
                &self.auth_config,
                Arc::clone(&self.token_store),
            )
            .await
        {
            Ok(flow_handle) => {
                let auth_url = flow_handle.auth_url.clone();
                let registry = Arc::clone(&ctx.registry);
                let server_name = self.server_name.clone();
                let server_url = self.server_url.clone();
                let server_key = self.server_key.clone();
                let token_store = Arc::clone(&self.token_store);
                let loading_policy = self.loading_policy;
                let manager_ref = Arc::clone(&self.manager_ref);

                tokio::spawn(async move {
                    match flow_handle.wait.await {
                        Ok(Ok(())) => {
                            // Preferred path: promote through the manager so its
                            // tracked state flips to Connected (and the UI badge
                            // updates), unifying with the in-app button path.
                            // Falls back to registry-only registration when the
                            // manager reference is unavailable (e.g. tests).
                            match manager_ref.get().and_then(Weak::upgrade) {
                                Some(manager) => {
                                    manager
                                        .complete_authorization(&server_name, registry)
                                        .await;
                                }
                                None => {
                                    post_auth_reconnect(
                                        registry,
                                        server_name,
                                        server_url,
                                        server_key,
                                        token_store,
                                        loading_policy,
                                    )
                                    .await;
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            warn!("OAuth flow failed: {e}");
                        }
                        Err(e) => {
                            warn!("OAuth flow task panicked: {e}");
                        }
                    }
                });

                Ok(ToolOutput::text(format!(
                    "Authorization required for the '{}' MCP server.\n\nPlease open this URL in your browser:\n\n{}\n\nAfter completing authorization in the browser, the server's tools will become available on the next turn. Share this URL with the user now.",
                    self.server_name, auth_url
                )))
            }
            Err(e) => Ok(ToolOutput::text(format!(
                "Could not start authorization for the '{}' server: {}",
                self.server_name, e
            ))),
        }
    }
}

// ── Post-auth reconnect ───────────────────────────────────────────────────────

async fn post_auth_reconnect(
    registry: Arc<Registry>,
    server_name: String,
    server_url: String,
    server_key: String,
    token_store: Arc<McpTokenStore>,
    loading_policy: LoadPolicy,
) {
    let engine = OAuthEngine::new(reqwest::Client::new());
    let token = match engine.current_access_token(&server_key, &token_store).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            warn!(
                server = %server_name,
                "post-auth reconnect: no access token found after OAuth flow"
            );
            return;
        }
        Err(e) => {
            warn!(server = %server_name, "post-auth reconnect: token lookup failed: {e}");
            return;
        }
    };

    let handle =
        match McpClientHandle::connect_http_with_bearer(&server_name, &server_url, &token).await {
            Ok(h) => h,
            Err(e) => {
                warn!(server = %server_name, "post-auth reconnect: connect failed: {e}");
                return;
            }
        };

    let descriptors: Vec<McpToolDescriptor> = match fetch_tools(&handle).await {
        Ok(d) => d,
        Err(e) => {
            warn!(server = %server_name, "post-auth reconnect: fetch_tools failed: {e}");
            return;
        }
    };

    // Swap out the auth pseudo-tool and any stale tools for this server.
    registry.remove_by_prefix(&format!("mcp__{server_name}__"));

    for desc in descriptors {
        let effective_policy = if desc.always_load {
            LoadPolicy::AlwaysLoad
        } else {
            loading_policy
        };
        let adapter = McpToolAdapter::new(
            &server_name,
            desc.raw_name,
            desc.description,
            desc.input_schema,
            handle.clone(),
            effective_policy,
            desc.annotations,
            desc.search_hint,
        );
        registry.register_io_dynamic(Arc::new(adapter));
    }
}
