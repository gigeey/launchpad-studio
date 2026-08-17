//! End-to-end integration test for Bash through the full runner pipeline.
//!
//! Exercises foreground execution, background mode, and cancellation via the
//! real BashTool registered through register_all. Proves cross-crate dispatch
//! and RunnerContext field plumbing work end-to-end.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ao_engine_tools_core::{DenialTracker, NoopDenialTracker, PermissionMode, Registry, RunnerContext, SessionKind};
use ao_engine_tools_io::register_all;
use ao_engine_tools_runner::hooks::config::RunnerSettings;
use ao_engine_tools_runner::prompt_bridge::{StubBridge, UserPromptBridge};
use ao_engine_tools_runner::message::{ContentBlock, Message};
use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};
use ao_engine_tools_runner::query_loop::{run_session, RunnerConfig, SessionOutcome};
use serde_json::{json, Value};
use tokio::time::timeout;

fn collect_tool_results(outcome: &SessionOutcome) -> Vec<Value> {
    outcome
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { tool_use_id, content, is_error } => {
                let content_str = content.iter().find_map(|b| {
                    if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None }
                }).unwrap_or("");
                // Parse as JSON for structured payloads; fall back to string.
                let content_val: Value = serde_json::from_str(content_str)
                    .unwrap_or_else(|_| Value::String(content_str.to_string()));
                Some(json!({
                    "tool_use_id": tool_use_id,
                    "content": content_val,
                    "is_error": is_error,
                }))
            }
            _ => None,
        })
        .collect()
}

fn make_ctx(session_id: &str) -> RunnerContext {
    let mut registry = Registry::new();
    register_all(&mut registry);
    let ctx = RunnerContext::new(session_id, "agent-bash-e2e")
        .expect("ctx")
        .with_registry(Arc::new(registry));
    ctx.permissions.set_mode(PermissionMode::BypassPermissions);
    ctx
}

fn make_config(script: Vec<Vec<CompletionEvent>>) -> RunnerConfig {
    let events: Vec<Vec<CompletionEvent>> = script;
    RunnerConfig {
        provider: Arc::new(MockProviderClient::new(events)),
        bridge: Arc::new(StubBridge) as Arc<dyn UserPromptBridge>,
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings: RunnerSettings::default(),
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    }
}

/// Foreground bash: `echo hello && exit 0` must yield stdout containing "hello",
/// exit_status == 0, cancelled == false, timed_out == false.
#[tokio::test]
#[cfg(unix)]
async fn bash_foreground_echo_hello() {
    let ctx = make_ctx("session-bash-foreground");
    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "b1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "echo hello && exit 0"}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let config = make_config(script);
    let outcome = timeout(
        Duration::from_secs(10),
        run_session(Vec::new(), ctx, config),
    )
    .await
    .expect("session did not finish in time")
    .expect("session ok");

    assert!(!outcome.cancelled);
    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 1, "one tool_result");
    assert_eq!(results[0]["is_error"], false);

    // `content` is the text the model receives. Bash renders its payload to a
    // flat form, so this is not JSON — indexing it as an object would only work
    // if the rendering were the payload's raw serialization, which it is not.
    let content = results[0]["content"]
        .as_str()
        .expect("bash content must be rendered text");
    assert!(
        content.contains("hello"),
        "stdout must reach the model unescaped: {content:?}"
    );
    // The footer carries exit status and the error states in one token, by the
    // precedence cancelled > timeout > signal=N > exit=N. `exit=0` therefore
    // asserts a clean exit *and* the absence of cancellation and timeout.
    assert!(
        content.contains("exit=0"),
        "footer must report a clean exit: {content:?}"
    );
}

/// Background bash: `sleep 30` with run_in_background=true must return a
/// short human-friendly process_id, status "running", an output_path, and
/// the handle must appear in ctx.background_commands.
#[tokio::test]
#[cfg(unix)]
async fn bash_background_returns_process_id() {
    let ctx = make_ctx("session-bash-background");
    let ctx_ref = ctx.clone();

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "b1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "sleep 30", "run_in_background": true}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let config = make_config(script);
    let outcome = timeout(
        Duration::from_secs(10),
        run_session(Vec::new(), ctx, config),
    )
    .await
    .expect("session did not finish in time")
    .expect("session ok");

    assert!(!outcome.cancelled);
    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 1, "one tool_result");
    assert_eq!(results[0]["is_error"], false);

    // Rendered text, not JSON — see the note in `bash_foreground_echo_hello`.
    let content = results[0]["content"]
        .as_str()
        .expect("bash content must be rendered text");
    // process_id must be a short human-friendly id like "bash_N", and must be
    // the first thing the model reads so a follow-up BashStatus/BashKill call
    // has it to hand.
    assert!(
        content.starts_with("process_id=bash_"),
        "summary must lead with a 'bash_N' process id: {content:?}"
    );
    assert!(
        content.contains("status=running"),
        "summary must state the process is running: {content:?}"
    );
    assert!(
        content.contains("output_path="),
        "output_path must be present in the background summary: {content:?}"
    );

    // Registry must have at least one entry.
    let live = ctx_ref.background_commands.len().await;
    assert!(live >= 1, "background_commands registry must have >= 1 entry");
}

/// Bare-sleep guard: a foreground `sleep 30` is rejected synchronously by
/// `detect_bare_sleep` (crates/ao-engine-tools-io/src/bash/mod.rs) before any
/// child process is spawned. This is a deliberate feature — a bare sleep in
/// foreground mode would occupy the tool slot for the full duration doing
/// nothing productive — so the guard firing is the expected, correct outcome,
/// not a byproduct of some other failure.
///
/// Split out from `bash_foreground_cancellation` (see that test's doc
/// comment): the two are mutually exclusive. A command the guard rejects
/// never spawns a process, so it can never be genuinely cancelled — asserting
/// both `is_error == true` (guard fired) and `cancelled == true` (a live
/// command was torn down) in one test is incoherent.
#[tokio::test]
#[cfg(unix)]
async fn bash_bare_sleep_guard_rejects_long_sleep() {
    let ctx = make_ctx("session-bash-bare-sleep-guard");

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "b1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "sleep 30"}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let config = make_config(script);

    let started = Instant::now();
    let outcome = timeout(
        Duration::from_secs(10),
        run_session(Vec::new(), ctx, config),
    )
    .await
    .expect("session did not finish in time")
    .expect("session ok");
    let elapsed = started.elapsed();

    // The guard returns from inside invoke() before execute::run_foreground
    // is ever called, so there is no child process to wait on. A real
    // `sleep 30` would still be running at this point; assert the round
    // trip finished in a small fraction of that, with generous headroom
    // for CI jitter.
    assert!(
        elapsed < Duration::from_secs(5),
        "guard must reject the command without spawning a child process; took {elapsed:?}"
    );

    // Nothing was ever in flight, so there is nothing to cancel — the
    // session ends via the natural no-more-tool-uses path once the "done"
    // turn arrives.
    assert!(
        !outcome.cancelled,
        "no cancellation was triggered in this test; session must finish naturally"
    );

    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 1, "one tool_result");
    assert_eq!(
        results[0]["is_error"], true,
        "bare sleep >= 2s in foreground mode must be rejected by detect_bare_sleep"
    );
    let content = results[0]["content"].as_str().unwrap_or_default();
    assert!(
        content.contains("Bare sleep"),
        "error message should name the bare-sleep guard; got: {content}"
    );
}

/// Cancellation: fire ctx.cancel 100ms into a genuinely long-running
/// foreground bash invocation and prove a live child process is torn down
/// mid-flight. Wall-clock budget: the test must finish well under 10s.
///
/// # Command choice — `tail -f /dev/null`
///
/// `crates/ao-engine-tools-io/src/bash/mod.rs::invoke()` has exactly one
/// guard capable of returning before a child process is spawned:
/// `detect_bare_sleep`. It matches a bare `sleep N` (N >= 2s, no other
/// tokens) and nothing else — see its doc comment for why it deliberately
/// ignores pipelines and `&&`-chains. Every other branch in `invoke()`
/// (background mode, the explicit-timeout path via `execute::run`, and the
/// default path via `execute::run_foreground`) unconditionally spawns a real
/// child process once it's reached.
///
/// A command like `sleep 0.01 && sleep 30` would clear the guard today only
/// because it's a compound expression the guard's single-token match doesn't
/// parse — a fragile hole that a reasonable tightening of `detect_bare_sleep`
/// could close, silently breaking this test's coverage again. `tail -f
/// /dev/null` is not a `sleep` in any form, bare or chained, so no plausible
/// tightening of a guard scoped to the `sleep` keyword could ever start
/// matching it. It blocks on I/O (waiting for a file that never grows)
/// rather than busy-looping, so it holds no meaningful CPU while alive, and
/// it terminates immediately on SIGTERM (no signal handler installed), so
/// cancellation resolves quickly instead of falling through to the 5s
/// SIGKILL grace period in `execute::terminate_child`.
#[tokio::test]
#[cfg(unix)]
async fn bash_foreground_cancellation() {
    let ctx = make_ctx("session-bash-cancel");

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "b1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "tail -f /dev/null"}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let config = make_config(script);

    // Fire the session-level cancel 100ms after the session starts.
    // By that time the bash child is already blocked in `tail -f /dev/null`
    // and the tool's internal select! will pick up the cancellation, send
    // SIGTERM, and return ExecutionOutcome { cancelled: true }.
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });

    let started = Instant::now();
    let outcome = timeout(
        Duration::from_secs(10),
        run_session(Vec::new(), ctx, config),
    )
    .await
    .expect("session did not finish in time")
    .expect("session ok");
    let elapsed = started.elapsed();

    // Elapsed-time bounds prove this test exercises the path it claims:
    // - Lower bound: if the command had short-circuited before spawning (the
    //   bug this test used to have), the whole round trip would resolve in
    //   low single-digit milliseconds, not ~100ms. 80ms gives headroom below
    //   the 100ms cancel delay for scheduling jitter while still failing
    //   clearly on a short-circuit.
    // - Upper bound: left alone, `tail -f /dev/null` runs forever, and even
    //   a SIGTERM that were somehow ignored would still hit the 5s SIGKILL
    //   grace deadline in `terminate_child`. 3s comfortably separates
    //   "cancelled promptly" from either failure mode while tolerating CI
    //   scheduling jitter.
    assert!(
        elapsed >= Duration::from_millis(80),
        "run finished suspiciously fast ({elapsed:?}); command may not have been in flight when cancel fired"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "run took {elapsed:?}; cancellation should cut the command short well before the 5s SIGKILL grace period"
    );

    // The session is cancelled at the top of the next loop iteration.
    assert!(outcome.cancelled, "session must be marked cancelled");

    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 1, "one tool_result");

    // A cancelled command surfaces is_error=true so the model knows it was
    // interrupted. The payload is still Structured (not ToolOutput::Error),
    // and the content block carries cancelled=true.
    assert_eq!(
        results[0]["is_error"], true,
        "cancelled bash command must have is_error=true; content: {:?}",
        results[0]["content"]
    );
    // Rendered text, not JSON — see the note in `bash_foreground_echo_hello`.
    // `cancelled` is the highest-precedence footer token, so it replaces the
    // exit status outright rather than appearing alongside it.
    let content = results[0]["content"]
        .as_str()
        .expect("bash content must be rendered text");
    assert!(
        content.contains("cancelled"),
        "footer must report the cancellation to the model: {content:?}"
    );
    assert!(
        !content.contains("exit="),
        "a cancelled command must not also report an exit status: {content:?}"
    );
}
