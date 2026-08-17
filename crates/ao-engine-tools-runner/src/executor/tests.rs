//! Unit tests for the bounded executor. Declared from `mod.rs` as
//! `#[cfg(test)] mod tests;` so private items remain in scope.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

use ao_engine_tools_core::output::ToolOutput;

use super::{run_batch, InvocationResult};
use crate::partition::{Batch, ToolInvocation};

fn make_inv(id: &str) -> ToolInvocation {
    ToolInvocation {
        id: id.to_string(),
        name: "Stub".to_string(),
        input: Value::Null,
    }
}

fn text_payload(s: &str) -> ToolOutput {
    ToolOutput::text(s)
}

fn make_result(id: &str, index: usize, payload: ToolOutput) -> InvocationResult {
    InvocationResult {
        id: id.to_string(),
        index,
        payload,
    }
}

fn unwrap_text(payload: &ToolOutput) -> &str {
    match payload {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text payload, got {:?}", other),
    }
}

#[tokio::test]
async fn concurrent_results_returned_in_input_order_not_completion_order() {
    // Reverse-order delays: invocation 0 sleeps longest, 2 finishes first.
    // The result Vec must still come back as [a, b, c] regardless of
    // who completed when.
    let invocations = vec![make_inv("a"), make_inv("b"), make_inv("c")];
    let batch = Batch::Concurrent(invocations);

    let results = run_batch(&batch, 4, CancellationToken::new(), |inv, _cancel| async move {
        let delay_ms = match inv.id.as_str() {
            "a" => 30,
            "b" => 20,
            "c" => 10,
            _ => 0,
        };
        sleep(Duration::from_millis(delay_ms)).await;
        // Set a deliberately bogus index here so we also prove the
        // executor's slot ordering is what places results in the
        // output Vec — not the closure's self-reported index.
        make_result(&inv.id, 999, text_payload(&inv.id))
    })
    .await;

    assert_eq!(results.len(), 3);
    assert_eq!(unwrap_text(&results[0].payload), "a");
    assert_eq!(unwrap_text(&results[1].payload), "b");
    assert_eq!(unwrap_text(&results[2].payload), "c");
    assert_eq!(results[0].id, "a");
    assert_eq!(results[1].id, "b");
    assert_eq!(results[2].id, "c");
}

#[tokio::test]
async fn cap_of_one_serializes_execution() {
    let invocations: Vec<_> = (0..5).map(|i| make_inv(&i.to_string())).collect();
    let batch = Batch::Concurrent(invocations);

    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let in_flight_for_closure = Arc::clone(&in_flight);
    let peak_for_closure = Arc::clone(&peak);

    let results = run_batch(&batch, 1, CancellationToken::new(), move |inv, _cancel| {
        let in_flight = Arc::clone(&in_flight_for_closure);
        let peak = Arc::clone(&peak_for_closure);
        async move {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            // peak = max(peak, now)
            let mut prev = peak.load(Ordering::SeqCst);
            while now > prev {
                match peak.compare_exchange(prev, now, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => break,
                    Err(actual) => prev = actual,
                }
            }
            sleep(Duration::from_millis(5)).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            make_result(&inv.id, 0, text_payload("ok"))
        }
    })
    .await;

    assert_eq!(results.len(), 5);
    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "cap=1 must serialize execution"
    );
}

#[tokio::test]
async fn cap_of_ten_with_twelve_invocations_keeps_peak_at_or_below_cap() {
    let invocations: Vec<_> = (0..12).map(|i| make_inv(&i.to_string())).collect();
    let batch = Batch::Concurrent(invocations);

    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let in_flight_for_closure = Arc::clone(&in_flight);
    let peak_for_closure = Arc::clone(&peak);

    let results = run_batch(&batch, 10, CancellationToken::new(), move |inv, _cancel| {
        let in_flight = Arc::clone(&in_flight_for_closure);
        let peak = Arc::clone(&peak_for_closure);
        async move {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            let mut prev = peak.load(Ordering::SeqCst);
            while now > prev {
                match peak.compare_exchange(prev, now, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => break,
                    Err(actual) => prev = actual,
                }
            }
            // Hold the permit long enough to fully fill the pool before
            // the first invocation releases.
            sleep(Duration::from_millis(20)).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            make_result(&inv.id, 0, text_payload("ok"))
        }
    })
    .await;

    assert_eq!(results.len(), 12);
    let observed_peak = peak.load(Ordering::SeqCst);
    assert!(
        observed_peak <= 10,
        "peak in-flight {} exceeded cap 10",
        observed_peak
    );
    // The lower bound is what discriminates a concurrent executor from a
    // serial one: a fully serial run produces peak == 1. The 20 ms permit
    // hold above fills the pool before any invocation releases, so peak
    // reaches the full cap of 10 in practice; 2 is asserted instead so a
    // loaded machine cannot turn a working executor red.
    assert!(
        observed_peak >= 2,
        "peak in-flight was {}, so nothing overlapped — the executor ran serially",
        observed_peak
    );
}

#[tokio::test]
async fn cancellation_mid_batch_resolves_promptly_with_cancelled_payloads() {
    let n = 8;
    let cap = 2;
    let invocations: Vec<_> = (0..n).map(|i| make_inv(&i.to_string())).collect();
    let batch = Batch::Concurrent(invocations);
    let batch = Arc::new(batch);

    let cancel = CancellationToken::new();
    let cancel_for_run = cancel.clone();

    let started = Arc::new(AtomicUsize::new(0));
    let started_for_closure = Arc::clone(&started);

    let batch_for_task = Arc::clone(&batch);
    let task = tokio::spawn(async move {
        run_batch(
            &batch_for_task,
            cap,
            cancel_for_run,
            move |inv, cancel| {
                let started = Arc::clone(&started_for_closure);
                async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    tokio::select! {
                        _ = cancel.cancelled() => InvocationResult {
                            id: inv.id.clone(),
                            index: 0,
                            payload: ToolOutput::Error {
                                message: "cancelled".to_string(),
                                recoverable: false,
                            },
                        },
                        _ = sleep(Duration::from_secs(60)) => unreachable!(
                            "sleep should be aborted by cancel before completing"
                        ),
                    }
                }
            },
        )
        .await
    });

    // Give the executor time to fill the permit pool.
    sleep(Duration::from_millis(20)).await;
    cancel.cancel();

    let results = timeout(Duration::from_millis(100), task)
        .await
        .expect("run_batch did not return within 100ms after cancel")
        .expect("task panicked");

    assert_eq!(results.len(), n);
    for (i, r) in results.iter().enumerate() {
        match &r.payload {
            ToolOutput::Error {
                message,
                recoverable,
            } => {
                assert_eq!(message, "cancelled", "result {i} should be cancelled");
                assert!(!*recoverable, "cancelled results are not recoverable");
            }
            other => panic!("result {i}: expected cancelled error, got {:?}", other),
        }
        assert_eq!(r.id, i.to_string());
    }

    // Only invocations that acquired a permit ever entered run_one;
    // the rest short-circuited via the executor's biased cancel arm
    // and never called the closure at all.
    let started_count = started.load(Ordering::SeqCst);
    assert!(
        started_count <= cap,
        "run_one was entered {started_count} times, expected ≤ cap ({cap})"
    );
}

#[tokio::test]
async fn cancellation_before_run_short_circuits_serial_batch() {
    let inv = make_inv("only");
    let batch = Batch::Serial(inv);
    let cancel = CancellationToken::new();
    cancel.cancel();

    let called = Arc::new(AtomicUsize::new(0));
    let called_for_closure = Arc::clone(&called);

    let results = run_batch(&batch, 4, cancel, move |inv, _cancel| {
        let called = Arc::clone(&called_for_closure);
        async move {
            called.fetch_add(1, Ordering::SeqCst);
            make_result(&inv.id, 0, text_payload("should not run"))
        }
    })
    .await;

    assert_eq!(results.len(), 1);
    assert_eq!(called.load(Ordering::SeqCst), 0);
    match &results[0].payload {
        ToolOutput::Error { message, .. } => assert_eq!(message, "cancelled"),
        other => panic!("expected cancelled, got {:?}", other),
    }
}

#[tokio::test]
async fn serial_batch_runs_single_invocation_through_closure() {
    let inv = make_inv("only");
    let batch = Batch::Serial(inv);

    let results = run_batch(
        &batch,
        4,
        CancellationToken::new(),
        |inv, _cancel| async move { make_result(&inv.id, 0, text_payload(&inv.id)) },
    )
    .await;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "only");
    assert_eq!(unwrap_text(&results[0].payload), "only");
}
