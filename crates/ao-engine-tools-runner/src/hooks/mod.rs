//! Hook subsystem — `settings.json` loader (see [`config`]) plus the
//! bash subprocess runner that drives pre- and post-tool hooks.
//!
//! Each hook is a shell command bound to a permission-style match
//! string. The runner spawns the command via `bash -c`, writes a JSON
//! [`HookRequest`] to its stdin, reads its stdout to EOF, and parses the
//! result into a [`HookOutcome`]. Stderr and non-zero exits are emitted
//! as `tracing::warn` events so operators can debug hook failures
//! without the runner aborting the turn.
//!
//! Pre-tool hooks run sequentially in declaration order. The first
//! non-`Continue` outcome wins and remaining pre-hooks are skipped, so
//! a `Deny` returned by an early hook short-circuits the gate. Post-tool
//! hooks all run; their outcomes are intentionally ignored (post-hooks
//! are side-effect channels — logging, audit trails — not gates).
//!
//! Cancellation is honored on pre-hooks: when the supplied
//! [`CancellationToken`] fires, the in-flight child is dropped (with
//! `kill_on_drop`) and the runner returns [`HookOutcome::Continue`]
//! promptly so the caller can finish unwinding the turn.

use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub mod config;

pub use config::{
    HookConfig, HookEntry, PermissionsConfig, RawPermissionRule, RunnerSettings, SettingsError,
    load_runner_settings,
};

/// Request payload written to a hook's stdin. The wire format is the
/// JSON serialization of this struct, so hook scripts can `jq` the input
/// directly to read `tool_name`, `input`, `agent_id`, or `session_id`.
#[derive(Debug, Clone, Serialize)]
pub struct HookRequest {
    pub tool_name: String,
    pub input: Value,
    pub agent_id: String,
    pub session_id: String,
}

/// Decision returned by a hook on stdout. The serde representation
/// matches [`crate::hooks::HookRequest`]'s sibling decision types — a
/// tagged enum on the `decision` field with snake_case discriminants —
/// so hook scripts can emit `{"decision":"deny","reason":"..."}` and
/// have it round-trip through `serde_json::from_str`.
///
/// `Continue` is the implicit fall-through: when a hook's stdout is
/// empty or fails to parse, the runner treats it as `Continue` and lets
/// subsequent hooks (or, for the last hook, the underlying tool
/// permission decision) take over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum HookOutcome {
    Allow,
    Deny { reason: String },
    Ask { reason: String },
    Mutate { updated_input: Value },
    Continue,
}

/// Run the supplied pre-tool hooks sequentially in declaration order.
///
/// The first hook that returns a non-[`HookOutcome::Continue`] outcome
/// wins; remaining hooks are skipped. If every hook returns `Continue`
/// (the default for empty / malformed stdout) the function returns
/// `Continue`, signaling the gate to fall through to the tool's own
/// [`PermissionDecision`](ao_engine_tools_core::PermissionDecision).
///
/// Cancellation: when `cancel` fires while a hook subprocess is in
/// flight, the child is dropped (and SIGKILLed via `kill_on_drop`) and
/// the function returns `Continue` promptly.
pub async fn run_pre_hooks(
    matched: &[&HookEntry],
    request: &HookRequest,
    cancel: CancellationToken,
) -> HookOutcome {
    for entry in matched {
        if cancel.is_cancelled() {
            return HookOutcome::Continue;
        }
        let outcome = run_one_hook(entry, request, cancel.clone()).await;
        match outcome {
            HookOutcome::Continue => continue,
            other => return other,
        }
    }
    HookOutcome::Continue
}

/// Run every supplied post-tool hook sequentially. Outcomes are
/// intentionally discarded — post-hooks are side-effect channels (audit
/// logs, metrics emitters, mirror writes) and never gate execution.
pub async fn run_post_hooks(matched: &[&HookEntry], request: &HookRequest) {
    let cancel = CancellationToken::new();
    for entry in matched {
        let _ = run_one_hook(entry, request, cancel.clone()).await;
    }
}

async fn run_one_hook(
    entry: &HookEntry,
    request: &HookRequest,
    cancel: CancellationToken,
) -> HookOutcome {
    let request_json = match serde_json::to_string(request) {
        Ok(s) => s,
        Err(error) => {
            tracing::warn!(
                command = %entry.command,
                %error,
                "failed to serialize hook request payload"
            );
            return HookOutcome::Continue;
        }
    };

    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(&entry.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(error) => {
            tracing::warn!(
                command = %entry.command,
                %error,
                "failed to spawn hook subprocess"
            );
            return HookOutcome::Continue;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(error) = stdin.write_all(request_json.as_bytes()).await {
            tracing::debug!(
                command = %entry.command,
                %error,
                "hook stdin write failed; continuing to read stdout"
            );
        }
        // Dropping `stdin` here closes the pipe so the child observes EOF.
    }

    let timeout_dur = Duration::from_millis(entry.timeout_ms);

    // Race the cancel token against the timed wait so cancellation kills
    // the child via `kill_on_drop` (the wait future drops the `Child` on
    // unwind). `biased` is load-bearing: without it the cancel arm and a
    // simultaneously-ready wait arm would race randomly, making the
    // bounded-time-after-cancel test flaky.
    let wait_result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            tracing::debug!(command = %entry.command, "hook cancelled");
            return HookOutcome::Continue;
        }
        result = tokio::time::timeout(timeout_dur, child.wait_with_output()) => result,
    };

    let output = match wait_result {
        Ok(Ok(o)) => o,
        Ok(Err(error)) => {
            tracing::warn!(
                command = %entry.command,
                %error,
                "hook subprocess I/O error"
            );
            return HookOutcome::Continue;
        }
        Err(_elapsed) => {
            tracing::warn!(
                command = %entry.command,
                timeout_ms = entry.timeout_ms,
                "hook subprocess timed out and was killed"
            );
            return HookOutcome::Continue;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            command = %entry.command,
            status = output.status.code().unwrap_or(-1),
            stderr = %stderr.trim(),
            "hook exited with non-zero status"
        );
    }

    let stdout_text = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout_text.trim();
    if trimmed.is_empty() {
        return HookOutcome::Continue;
    }
    match serde_json::from_str::<HookOutcome>(trimmed) {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(
                command = %entry.command,
                stdout = %trimmed,
                %error,
                "hook stdout did not parse as a HookOutcome"
            );
            HookOutcome::Continue
        }
    }
}

#[cfg(test)]
mod tests;
