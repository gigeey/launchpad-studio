use super::*;
use ao_engine_tools_core::policy::LoadPolicy;
use ao_engine_tools_provider_config::mcp_servers::McpAuthConfig;
use ao_engine_tools_provider_config::mcp_token_store::McpTokenStore;
use std::sync::{Arc, OnceLock};

fn make_tool() -> McpServerAuthTool {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));
    McpServerAuthTool::new(
        "testserver",
        "http://localhost:9999/mcp",
        McpAuthConfig::default(),
        store,
        LoadPolicy::AlwaysLoad,
        Arc::new(OnceLock::new()),
    )
}

#[test]
fn name_follows_convention() {
    let t = make_tool();
    assert_eq!(t.name(), "mcp__testserver__authorize");
}

#[test]
fn not_concurrency_safe() {
    let t = make_tool();
    assert!(!t.is_concurrency_safe());
}

#[test]
fn empty_input_schema() {
    let t = make_tool();
    let schema = t.input_schema();
    assert_eq!(schema["type"], "object");
}

#[test]
fn inherits_load_policy() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(McpTokenStore::new_with_file_fallback(dir.path().to_path_buf()));
    let t = McpServerAuthTool::new(
        "deferred_srv",
        "http://localhost:9999/mcp",
        McpAuthConfig::default(),
        store,
        LoadPolicy::Deferred,
        Arc::new(OnceLock::new()),
    );
    assert_eq!(t.load_policy(), LoadPolicy::Deferred);
}
