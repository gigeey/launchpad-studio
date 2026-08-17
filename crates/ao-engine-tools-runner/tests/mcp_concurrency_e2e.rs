//! End-to-end reachability: a server-declared `readOnlyHint` reaches the
//! dispatcher's batching decision.
//!
//! Every hop of this chain already had a unit test, and no test crossed the
//! seams between them:
//!
//! 1. `tools/list` JSON → `McpToolAnnotations` (`mcp::schema_fetch` tests)
//! 2. `McpToolAnnotations` → `is_concurrency_safe()` (`mcp::adapter` tests)
//! 3. `is_concurrency_safe()` → `Batch` shape (`partition` tests, using
//!    built-in tools — never an MCP adapter)
//!
//! The untested seams are the registration sites, where the parsed
//! annotations are handed to the adapter constructor. Replacing
//! `desc.annotations` with `Default::default()` at `mcp/manager.rs:496`
//! leaves the rest of this crate green (604 passed, 1 failed — this file)
//! while turning every read-only MCP tool in the product serial: a silent
//! throughput loss with no failing test anywhere. These tests spawn a real
//! server subprocess, register it through the live paths, and assert the
//! batch shape the dispatcher would actually get.
//!
//! Two of the three registration sites are covered — `register_into`
//! (startup) and `add_server` (runtime add). The third, the post-auth
//! re-registration at `mcp/manager.rs:1230`, is not: reaching it needs a
//! completed OAuth flow, and the fixture server has no auth behavior. It
//! carries the same hazard.
//!
//! Also not covered here: that a `Batch::Concurrent` is in fact fanned out
//! in parallel. That is asserted separately, against an observed in-flight
//! peak, in `executor::tests`.

use std::collections::HashMap;

use ao_engine_tools_core::Registry;
use ao_engine_tools_provider_config::mcp_servers::{
    McpLoadingPolicy, McpServerEntry, McpServersConfig, McpTransportType,
};
use ao_engine_tools_runner::mcp::{McpManager, McpServerState};
use ao_engine_tools_runner::partition::{partition_invocations, Batch, ToolInvocation};
use serde_json::json;

/// Cargo sets `CARGO_BIN_EXE_echo_mcp_server` for integration tests and
/// guarantees the fixture binary is built first, so editing the fixture
/// rebuilds it for this test.
fn echo_server_bin() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_echo_mcp_server").into()
}

fn inv(id: &str, name: &str) -> ToolInvocation {
    ToolInvocation {
        id: id.to_string(),
        name: name.to_string(),
        input: json!({}),
    }
}

fn ids_of(items: &[ToolInvocation]) -> Vec<&str> {
    items.iter().map(|i| i.id.as_str()).collect()
}

/// A stdio server entry running the fixture in `with_annotations` mode,
/// which serves two tools: `read_file` with `readOnlyHint: true` and
/// `write_db` with `readOnlyHint: false`.
fn annotated_entry(name: &str) -> McpServerEntry {
    let bin = echo_server_bin();
    let mut env = HashMap::new();
    env.insert("MCP_BEHAVIOR".to_string(), "with_annotations".to_string());

    McpServerEntry {
        name: name.to_string(),
        command: Some(bin.to_str().expect("fixture path is valid UTF-8").to_string()),
        args: vec![],
        env,
        loading: McpLoadingPolicy::Always,
        transport: McpTransportType::Stdio,
        url: None,
        auth: None,
    }
}

/// Assert that a registry holding the fixture's two tools under `server`
/// partitions them by the hints the server declared.
///
/// The interleaving also pins the ordering contract: a read-only tool after
/// a write must open a NEW concurrent batch rather than joining the one
/// before the write.
fn assert_partitions_by_declared_hints(registry: &Registry, server: &str) {
    let read_only = format!("mcp__{server}__read_file");
    let write = format!("mcp__{server}__write_db");

    assert!(
        registry.lookup(&read_only).is_some(),
        "{read_only} should be registered; without it the batch assertions \
         below would pass for the wrong reason, since an unknown tool is \
         also partitioned as Serial"
    );
    assert!(
        registry.lookup(&write).is_some(),
        "{write} should be registered"
    );

    let invocations = vec![
        inv("a", &read_only),
        inv("b", &read_only),
        inv("c", &write),
        inv("d", &read_only),
    ];
    let batches = partition_invocations(&invocations, registry);

    assert_eq!(
        batches.len(),
        3,
        "expected Concurrent[a, b], Serial(c), Concurrent[d]; got {batches:?}"
    );

    match &batches[0] {
        Batch::Concurrent(items) => assert_eq!(
            ids_of(items),
            vec!["a", "b"],
            "both readOnlyHint:true calls should share one concurrent batch"
        ),
        Batch::Serial(item) => panic!(
            "readOnlyHint:true did not survive registration — {read_only} was \
             partitioned as Serial({}), so the annotation was lost somewhere \
             between tools/list and the registry",
            item.id
        ),
    }

    match &batches[1] {
        Batch::Serial(item) => assert_eq!(item.id, "c"),
        Batch::Concurrent(items) => panic!(
            "readOnlyHint:false was treated as concurrency-safe — {write} was \
             batched concurrently as {:?}. Write-capable MCP tools must never \
             fan out.",
            ids_of(items)
        ),
    }

    match &batches[2] {
        Batch::Concurrent(items) => assert_eq!(ids_of(items), vec!["d"]),
        Batch::Serial(item) => panic!(
            "the read-only call after the write was partitioned as Serial({}); \
             it should open a fresh concurrent batch",
            item.id
        ),
    }
}

/// Startup path: `McpManager::from_config` → `register_into`. This is what
/// `AppState` runs for every configured server (`ao-engine/src/state.rs:355`).
#[tokio::test]
async fn read_only_hint_reaches_the_partitioner_via_register_into() {
    let config = McpServersConfig {
        servers: vec![annotated_entry("annotated")],
    };

    let mut registry = Registry::new();
    let manager = McpManager::from_config(&config).await;
    let manager = manager.register_into(&mut registry).await;

    assert_partitions_by_declared_hints(&registry, "annotated");

    manager.shutdown().await;
}

/// Runtime path: `add_server` on a live manager, which registers into the
/// registry's dynamic slot rather than the static map. A separate call to
/// `McpToolAdapter::new`, so it can lose the annotations independently.
#[tokio::test]
async fn read_only_hint_reaches_the_partitioner_via_add_server() {
    let empty = McpServersConfig { servers: vec![] };
    let manager = McpManager::from_config(&empty).await;

    let registry = std::sync::Arc::new(Registry::new());
    let status = manager
        .add_server(
            annotated_entry("added_at_runtime"),
            std::sync::Arc::clone(&registry),
            "config".to_string(),
        )
        .await
        .expect("the fixture server should connect");
    assert_eq!(status.state, McpServerState::Connected);

    assert_partitions_by_declared_hints(&registry, "added_at_runtime");

    manager.shutdown().await;
}
