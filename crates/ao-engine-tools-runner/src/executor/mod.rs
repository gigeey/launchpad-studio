//! Bounded concurrent executor — drives a single batch from
//! [`crate::partition`], capping in-flight invocations and returning
//! results in original-index order regardless of completion order.
//!
//! The caller supplies a `run_one` closure that turns one
//! [`ToolInvocation`] into the eventual [`InvocationResult`]. The
//! executor owns the concurrency, the cancellation propagation, and the
//! reorder-back-to-original step. Keeping the per-invocation pipeline
//! out of this module lets the query loop layer validation, hooks, and
//! permissions on top without the executor ever importing those
//! modules.

use std::future::Future;
use std::sync::Arc;

use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use ao_engine_tools_core::output::ToolOutput;

use crate::partition::{Batch, ToolInvocation};

/// One row of the executor's output Vec — paired with the original
/// invocation `id` (for `tool_result` correlation), the source `index`
/// inside the input batch, and the [`ToolOutput`] payload returned by
/// the caller's `run_one` closure (or a synthesized cancellation marker
/// if the batch was cancelled before this slot started).
#[derive(Debug, Clone)]
pub struct InvocationResult {
    pub id: String,
    pub index: usize,
    pub payload: ToolOutput,
}

/// Drive a single [`Batch`] to completion.
///
/// For [`Batch::Serial`] the lone invocation is awaited inline (the
/// returned Vec has length 1).
///
/// For [`Batch::Concurrent`], a [`tokio::sync::Semaphore`] caps the
/// number of in-flight `run_one` invocations to `cap`; results are
/// returned in original-index order regardless of completion order.
///
/// `cancel` is forwarded to every `run_one` call so in-flight tool
/// invocations can observe it. Invocations that have not yet acquired a
/// permit when `cancel` fires resolve to a
/// `ToolOutput::Error { message: "cancelled", recoverable: false }`
/// without ever calling `run_one`, so the one-result-per-tool_use
/// invariant the dispatcher relies on always holds.
pub async fn run_batch<F, Fut>(
    batch: &Batch,
    cap: usize,
    cancel: CancellationToken,
    run_one: F,
) -> Vec<InvocationResult>
where
    F: Fn(ToolInvocation, CancellationToken) -> Fut + Sync,
    Fut: Future<Output = InvocationResult> + Send,
{
    match batch {
        Batch::Serial(inv) => {
            let result = run_serial(inv.clone(), cancel, &run_one).await;
            vec![result]
        }
        Batch::Concurrent(invocations) => {
            run_concurrent(invocations, cap, cancel, run_one).await
        }
    }
}

async fn run_serial<F, Fut>(
    inv: ToolInvocation,
    cancel: CancellationToken,
    run_one: &F,
) -> InvocationResult
where
    F: Fn(ToolInvocation, CancellationToken) -> Fut,
    Fut: Future<Output = InvocationResult> + Send,
{
    if cancel.is_cancelled() {
        return cancelled_result(&inv, 0);
    }
    run_one(inv, cancel).await
}

async fn run_concurrent<F, Fut>(
    invocations: &[ToolInvocation],
    cap: usize,
    cancel: CancellationToken,
    run_one: F,
) -> Vec<InvocationResult>
where
    F: Fn(ToolInvocation, CancellationToken) -> Fut + Sync,
    Fut: Future<Output = InvocationResult> + Send,
{
    let semaphore = Arc::new(Semaphore::new(cap));
    let n = invocations.len();
    let mut slots: Vec<Option<InvocationResult>> = (0..n).map(|_| None).collect();
    let mut tasks = FuturesUnordered::new();

    for (index, inv) in invocations.iter().enumerate() {
        let inv = inv.clone();
        let semaphore = Arc::clone(&semaphore);
        let cancel = cancel.clone();
        let run_one_ref = &run_one;
        tasks.push(async move {
            // Cancellation observed before acquiring a permit short-
            // circuits without calling `run_one`. The biased select
            // guarantees the cancel branch wins ties so the bounded
            // exit time the dispatcher promises is observable in
            // tests.
            let payload = tokio::select! {
                biased;
                _ = cancel.cancelled() => return (index, cancelled_result(&inv, index)),
                permit = semaphore.acquire_owned() => permit,
            };
            let _permit = payload
                .expect("executor owns the semaphore and never closes it before draining tasks");
            // Re-check cancel after waiting — fairness can let us
            // acquire a permit just as cancel fires.
            if cancel.is_cancelled() {
                return (index, cancelled_result(&inv, index));
            }
            let result = run_one_ref(inv, cancel).await;
            (index, result)
        });
    }

    while let Some((index, result)) = tasks.next().await {
        slots[index] = Some(result);
    }

    slots
        .into_iter()
        .map(|s| s.expect("every concurrent slot is filled by the FuturesUnordered drain above"))
        .collect()
}

fn cancelled_result(inv: &ToolInvocation, index: usize) -> InvocationResult {
    InvocationResult {
        id: inv.id.clone(),
        index,
        payload: ToolOutput::Error {
            message: "cancelled".to_string(),
            recoverable: false,
        },
    }
}

#[cfg(test)]
mod tests;
