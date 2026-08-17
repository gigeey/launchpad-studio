//! Concurrency partitioner — group contiguous concurrency-safe tool
//! invocations from a single assistant turn into batches without
//! reordering across batch boundaries.
//!
//! Pure, synchronous logic. Walks the input invocations in their
//! original order, looks each tool up in the registry, and groups
//! CONTIGUOUS runs of concurrency-safe tools into a single
//! [`Batch::Concurrent`]. Concurrency-unsafe tools (and tools missing
//! from the registry — the dispatcher will surface that error
//! downstream when it tries to invoke them) become individual
//! [`Batch::Serial`] batches.
//!
//! Critically, order is preserved across batches: an input shaped like
//! `[safe, safe, unsafe, safe, safe]` produces exactly three batches —
//! `Concurrent[s, s]`, `Serial(u)`, `Concurrent[s, s]` — never a single
//! larger concurrent group. The partitioner never moves an invocation
//! past another invocation.

use serde_json::Value;

use ao_engine_tools_core::registry::{Registry, ToolRef};

/// One `tool_use` block from the assistant turn, queued for dispatch.
///
/// Local to the runner crate; carries only the fields the dispatcher
/// pipeline needs (id for tool_result correlation, name for registry
/// lookup, raw input forwarded to validation/permissions/the tool).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInvocation {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// A grouping of [`ToolInvocation`]s the executor will run together.
///
/// [`Batch::Concurrent`] batches may be fanned out in parallel up to a
/// configured cap; [`Batch::Serial`] batches run as a single in-flight
/// call. The partitioner emits a single-element `Concurrent` for a lone
/// concurrency-safe invocation rather than collapsing it to `Serial`,
/// so the executor sees one consistent input shape.
#[derive(Debug, Clone, PartialEq)]
pub enum Batch {
    Concurrent(Vec<ToolInvocation>),
    Serial(ToolInvocation),
}

/// Partition `invocations` into batches per the concurrency contract.
///
/// Walks the slice in order, looking each tool up in `registry` and
/// reading its `is_concurrency_safe()` flag. Contiguous runs of safe
/// invocations collapse into a single [`Batch::Concurrent`]; each
/// unsafe (or unknown) invocation becomes its own [`Batch::Serial`].
///
/// An empty input returns an empty `Vec`. A single safe invocation
/// returns a one-element [`Batch::Concurrent`] (not a `Serial`) so the
/// executor's batch-shape contract stays uniform.
///
/// A tool name absent from the registry is treated conservatively as
/// unsafe (and emitted as `Serial`); the executor will surface the
/// missing-tool error when it tries to dispatch.
pub fn partition_invocations(
    invocations: &[ToolInvocation],
    registry: &Registry,
) -> Vec<Batch> {
    let mut batches: Vec<Batch> = Vec::new();
    let mut pending_concurrent: Vec<ToolInvocation> = Vec::new();

    for inv in invocations {
        let safe = match registry.lookup(&inv.name) {
            Some(ToolRef::Io(t)) => t.is_concurrency_safe(),
            Some(ToolRef::Engine(t)) => t.is_concurrency_safe(),
            None => false,
        };

        if safe {
            pending_concurrent.push(inv.clone());
        } else {
            if !pending_concurrent.is_empty() {
                batches.push(Batch::Concurrent(std::mem::take(&mut pending_concurrent)));
            }
            batches.push(Batch::Serial(inv.clone()));
        }
    }

    if !pending_concurrent.is_empty() {
        batches.push(Batch::Concurrent(pending_concurrent));
    }

    batches
}

#[cfg(test)]
mod tests;
