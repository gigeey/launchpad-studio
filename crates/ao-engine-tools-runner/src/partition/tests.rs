//! Unit tests for the concurrency partitioner. Declared from `mod.rs`
//! as `#[cfg(test)] mod tests;` so private items remain in scope.

use std::sync::Arc;

use ao_engine_tools_core::context::RunnerContext;
use ao_engine_tools_core::output::ToolOutput;
use ao_engine_tools_core::registry::Registry;
use ao_engine_tools_core::tool::IoTool;
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{partition_invocations, Batch, ToolInvocation};

/// Test fixture: an `IoTool` whose name and concurrency flag are
/// configurable so we can assemble registries shaped like the
/// safe/unsafe mix each test wants.
struct StubTool {
    name: String,
    safe: bool,
}

#[async_trait]
impl IoTool for StubTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "stub tool"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn is_concurrency_safe(&self) -> bool {
        self.safe
    }
    async fn invoke(
        &self,
        _input: Value,
        _ctx: &RunnerContext,
    ) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("ok"))
    }
}

fn make_registry() -> Registry {
    let mut r = Registry::new();
    r.register_io(Arc::new(StubTool {
        name: "Read".into(),
        safe: true,
    }));
    r.register_io(Arc::new(StubTool {
        name: "Glob".into(),
        safe: true,
    }));
    r.register_io(Arc::new(StubTool {
        name: "Grep".into(),
        safe: true,
    }));
    r.register_io(Arc::new(StubTool {
        name: "Edit".into(),
        safe: false,
    }));
    r.register_io(Arc::new(StubTool {
        name: "Bash".into(),
        safe: false,
    }));
    r
}

fn inv(id: &str, name: &str) -> ToolInvocation {
    ToolInvocation {
        id: id.into(),
        name: name.into(),
        input: json!({}),
    }
}

fn ids_of(items: &[ToolInvocation]) -> Vec<&str> {
    items.iter().map(|i| i.id.as_str()).collect()
}

#[test]
fn empty_input_returns_empty_vec() {
    let registry = make_registry();
    let batches = partition_invocations(&[], &registry);
    assert!(batches.is_empty());
}

#[test]
fn all_safe_returns_single_concurrent_batch_in_order() {
    let registry = make_registry();
    let invs = vec![inv("a", "Read"), inv("b", "Glob"), inv("c", "Grep")];

    let batches = partition_invocations(&invs, &registry);

    assert_eq!(batches.len(), 1);
    match &batches[0] {
        Batch::Concurrent(items) => assert_eq!(ids_of(items), vec!["a", "b", "c"]),
        Batch::Serial(_) => panic!("expected a single Concurrent batch"),
    }
}

#[test]
fn all_unsafe_returns_n_serial_batches_in_order() {
    let registry = make_registry();
    let invs = vec![inv("a", "Edit"), inv("b", "Bash"), inv("c", "Edit")];

    let batches = partition_invocations(&invs, &registry);

    assert_eq!(batches.len(), 3);
    let collected: Vec<&str> = batches
        .iter()
        .map(|b| match b {
            Batch::Serial(item) => item.id.as_str(),
            Batch::Concurrent(_) => panic!("expected Serial batch"),
        })
        .collect();
    assert_eq!(collected, vec!["a", "b", "c"]);
}

#[test]
fn mixed_safe_unsafe_safe_yields_exactly_three_batches() {
    let registry = make_registry();
    // [safe, safe, unsafe, safe, safe] — the canonical example from
    // the partitioner contract.
    let invs = vec![
        inv("a", "Read"),
        inv("b", "Glob"),
        inv("c", "Edit"),
        inv("d", "Read"),
        inv("e", "Grep"),
    ];

    let batches = partition_invocations(&invs, &registry);

    assert_eq!(batches.len(), 3);
    match &batches[0] {
        Batch::Concurrent(items) => assert_eq!(ids_of(items), vec!["a", "b"]),
        Batch::Serial(_) => panic!("expected first batch Concurrent[a, b]"),
    }
    match &batches[1] {
        Batch::Serial(item) => assert_eq!(item.id, "c"),
        Batch::Concurrent(_) => panic!("expected second batch Serial(c)"),
    }
    match &batches[2] {
        Batch::Concurrent(items) => assert_eq!(ids_of(items), vec!["d", "e"]),
        Batch::Serial(_) => panic!("expected third batch Concurrent[d, e]"),
    }
}

#[test]
fn single_safe_invocation_returns_one_element_concurrent_batch() {
    // Locked-in design call: a lone safe invocation is emitted as Concurrent
    // (single-element), not Serial — keeps the executor's input-shape
    // contract uniform.
    let registry = make_registry();
    let batches = partition_invocations(&[inv("a", "Read")], &registry);

    assert_eq!(batches.len(), 1);
    match &batches[0] {
        Batch::Concurrent(items) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].id, "a");
        }
        Batch::Serial(_) => {
            panic!("a single safe invocation must be Concurrent (single-element)")
        }
    }
}

#[test]
fn single_unsafe_invocation_returns_serial_batch() {
    let registry = make_registry();
    let batches = partition_invocations(&[inv("a", "Edit")], &registry);

    assert_eq!(batches.len(), 1);
    match &batches[0] {
        Batch::Serial(item) => assert_eq!(item.id, "a"),
        Batch::Concurrent(_) => panic!("expected Serial"),
    }
}

#[test]
fn unknown_tool_is_treated_as_serial() {
    // Conservative behavior: missing-from-registry counts as unsafe so
    // the executor surfaces the dispatch error one-at-a-time instead of
    // contaminating a concurrent batch.
    let registry = make_registry();
    let invs = vec![
        inv("a", "Read"),
        inv("b", "DoesNotExist"),
        inv("c", "Read"),
    ];

    let batches = partition_invocations(&invs, &registry);

    assert_eq!(batches.len(), 3);
    match &batches[0] {
        Batch::Concurrent(items) => assert_eq!(ids_of(items), vec!["a"]),
        Batch::Serial(_) => panic!("expected first batch Concurrent[a]"),
    }
    match &batches[1] {
        Batch::Serial(item) => {
            assert_eq!(item.id, "b");
            assert_eq!(item.name, "DoesNotExist");
        }
        Batch::Concurrent(_) => {
            panic!("expected unknown tool to land in its own Serial batch")
        }
    }
    match &batches[2] {
        Batch::Concurrent(items) => assert_eq!(ids_of(items), vec!["c"]),
        Batch::Serial(_) => panic!("expected last batch Concurrent[c]"),
    }
}

#[test]
fn safe_tail_after_unsafe_starts_a_new_concurrent_batch() {
    // Edge case: the trailing-safe-run flush at end of input.
    let registry = make_registry();
    let invs = vec![inv("a", "Edit"), inv("b", "Read"), inv("c", "Glob")];

    let batches = partition_invocations(&invs, &registry);

    assert_eq!(batches.len(), 2);
    match &batches[0] {
        Batch::Serial(item) => assert_eq!(item.id, "a"),
        Batch::Concurrent(_) => panic!("expected leading Serial(a)"),
    }
    match &batches[1] {
        Batch::Concurrent(items) => assert_eq!(ids_of(items), vec!["b", "c"]),
        Batch::Serial(_) => panic!("expected trailing Concurrent[b, c]"),
    }
}
