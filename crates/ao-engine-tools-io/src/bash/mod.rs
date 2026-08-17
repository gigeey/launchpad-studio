//! Bash tool — shell execution for the native engine.
//!
//! Spawns a fresh `/bin/bash -c` per invocation; no persistent shell session.
//! Full behaviour (spawn/capture, timeout, cancellation, env scrubbing,
//! cwd capture, background mode) is implemented across the submodules below.

use ao_engine_tools_core::{
    IoTool, PermissionContext, PermissionDecision, Registry, RunnerContext, ToolOutput,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

pub mod auto_approve;
pub mod background;
pub mod command_classifier;
pub mod execute;
pub mod image;
pub mod persist;
pub mod prompt;
pub mod shell_snapshot;

#[cfg(test)]
mod tests;

/// Duration after which a still-running foreground command is automatically moved
/// to the background registry. Only applies when no explicit timeout was provided.
///
/// A shorter value is used in test builds so auto-backgrounding tests complete
/// quickly without waiting the full production threshold. It is not shortened
/// further than this: the threshold doubles as the bound for
/// `auto_bg_fast_command_unaffected`, which asserts a trivial command finishes
/// below it. At 500 ms a loaded machine could push `echo hello` past the
/// threshold and fail that test without any defect present.
#[cfg(not(test))]
const AUTO_BG_THRESHOLD_MS: u64 = 15_000;
#[cfg(test)]
const AUTO_BG_THRESHOLD_MS: u64 = 3_000;

/// Bash tool — executes shell commands in a fresh `/bin/bash -c` subprocess.
#[derive(Default)]
pub struct BashTool;

#[derive(Deserialize)]
struct BashInput {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    // description is consumed via the raw Value in check_permissions; we still
    // include it here so serde accepts the field without a parse error.
    #[serde(default, rename = "description")]
    _description: Option<String>,
    #[serde(default)]
    run_in_background: Option<bool>,
}

#[async_trait]
impl IoTool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        serde_json::from_str(prompt::INPUT_SCHEMA).expect("INPUT_SCHEMA is a valid JSON literal")
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    async fn check_permissions(
        &self,
        input: &Value,
        _ctx: &PermissionContext,
    ) -> PermissionDecision {
        let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let (_, rest) = execute::split_leading_cd(cmd);
        let variant = command_classifier::classify(rest);

        // Safe read-only commands are auto-approved to reduce prompt friction
        // and to let read-only inspection run in unattended/autonomous sessions.
        // `auto_approve::is_auto_approvable` rejects execution-smuggling constructs
        // (command substitution, chaining, redirection, background ops), so this
        // is safe to bypass the prompt. Settings rules and hooks still override
        // this decision at higher precedence.
        if variant == command_classifier::Classification::ReadOnly
            && auto_approve::is_auto_approvable(cmd)
        {
            return PermissionDecision::Allow;
        }

        let label = match variant {
            command_classifier::Classification::ReadOnly => "ReadOnly",
            command_classifier::Classification::Destructive => "Destructive",
            command_classifier::Classification::NetworkTouching => "NetworkTouching",
            command_classifier::Classification::GitMutating => "GitMutating",
            command_classifier::Classification::Unclassified => "Unclassified",
        };
        let desc = input
            .get("description")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let reason = match desc {
            Some(d) => format!("[classification: {label}] {d} — execute bash: {rest}"),
            None => format!("[classification: {label}] execute bash: {rest}"),
        };
        PermissionDecision::Ask { reason }
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let input: BashInput = serde_json::from_value(input)
            .map_err(|e| AoError::ValidationError(format!("invalid Bash input: {e}")))?;

        if input.run_in_background == Some(true) {
            // Spawn in the background; cwd changes inside background processes
            // are not tracked (background cwd is not read back).
            let (process_id, output_path) =
                background::spawn_and_register(&input.command, ctx).await?;
            let mut payload = serde_json::json!({
                "process_id": process_id.to_string(),
                "status": "running",
                "command": input.command,
                "output_path": output_path.to_string_lossy().as_ref(),
            });
            attach_text_fallback(&mut payload);
            return Ok(ToolOutput::structured(payload));
        }

        // Resolve timeout: default 120 000 ms, max 600 000 ms. Treat 0 as "use default".
        let timeout_ms = match input.timeout {
            None | Some(0) => 120_000,
            Some(t) => t.min(600_000),
        };

        // Sleep guard: a bare foreground `sleep N` (N ≥ 2 s) occupies the tool slot
        // for the full duration with nothing productive happening. Block it and direct
        // the caller toward background mode + BashStatus polling.
        if let Some(secs) = detect_bare_sleep(&input.command) {
            return Ok(ToolOutput::error(
                format!(
                    "Bare sleep of {secs:.0} s in foreground mode blocks the tool slot for its \
                     full duration. Use `run_in_background: true` to spawn the sleep in the \
                     background, then poll its status with BashStatus. For poll-until-done \
                     patterns: spawn the work with run_in_background, check state with BashStatus, \
                     and abort with BashKill if needed."
                ),
                true,
            ));
        }

        // When an explicit timeout was provided, honour it strictly (no auto-background).
        // When using the default timeout, a command still running after AUTO_BG_THRESHOLD_MS
        // is promoted to background rather than blocking until the hard deadline.
        let explicit_timeout = matches!(input.timeout, Some(t) if t > 0);

        let outcome = if explicit_timeout {
            execute::run(&input.command, ctx, timeout_ms).await?
        } else {
            match execute::run_foreground(
                &input.command,
                ctx,
                timeout_ms,
                Some(AUTO_BG_THRESHOLD_MS),
            )
            .await?
            {
                execute::ForegroundOutcome::Done(o) => o,
                execute::ForegroundOutcome::Backgrounded {
                    pre_stdout,
                    pre_stderr,
                    child,
                    stdout,
                    stderr,
                } => {
                    let (process_id, output_path) = background::register_running_process(
                        &input.command,
                        child,
                        stdout,
                        stderr,
                        pre_stdout,
                        pre_stderr,
                        ctx,
                    )
                    .await?;
                    let mut payload = serde_json::json!({
                        "process_id": process_id.to_string(),
                        "status": "running",
                        "command": input.command,
                        "output_path": output_path.to_string_lossy().as_ref(),
                        "auto_backgrounded": true,
                        "note": format!(
                            "Command ran past the {}s limit and was moved to the background. \
                             Poll with BashStatus(\"{}\") or stop with BashKill(\"{}\").",
                            AUTO_BG_THRESHOLD_MS / 1000,
                            process_id,
                            process_id,
                        ),
                    });
                    attach_text_fallback(&mut payload);
                    return Ok(ToolOutput::structured(payload));
                }
            }
        };

        let is_error = outcome.cancelled || outcome.timed_out;

        // Compute a human-readable note for well-known non-zero exit codes. Only
        // produced for commands that completed normally (not cancelled or timed out).
        let exit_note = if !is_error {
            interpret_exit_code(outcome.exit_status, outcome.signal)
        } else {
            None
        };

        // Image detection wins over all other output paths, including persistence.
        // A large base64 image in stdout is delivered as an image content block
        // rather than being truncated or written to disk as opaque text.
        if !is_error {
            if let Some((media_type, base64_data)) = image::detect_image(&outcome.stdout) {
                return Ok(ToolOutput::image(media_type, base64_data));
            }
        }

        if outcome.needs_persistence {
            match persist::write_output(&outcome.stdout, &outcome.stderr).await {
                Ok(persisted) => {
                    let footer = exit_footer(is_error, outcome.cancelled, outcome.timed_out, outcome.signal, outcome.exit_status);
                    let text_fallback = match &exit_note {
                        Some(note) => format!("{}\n{}\n{}\n", persisted.envelope, footer, note),
                        None => format!("{}\n{}", persisted.envelope, footer),
                    };
                    let mut payload = serde_json::json!({
                        "persisted_output_path": persisted.path.to_string_lossy().as_ref(),
                        "persisted_output_size": persisted.size,
                        "persisted_output_lines": persisted.lines,
                        "exit_status": outcome.exit_status,
                        "signal": outcome.signal,
                        "timed_out": outcome.timed_out,
                        "cancelled": outcome.cancelled,
                        "is_error": is_error,
                        "text_fallback": text_fallback,
                    });
                    if let Some(ref note) = exit_note {
                        payload["exit_code_note"] = serde_json::Value::String(note.clone());
                    }
                    return Ok(ToolOutput::structured(payload));
                }
                Err(e) => {
                    tracing::warn!("bash output persistence failed, falling back to truncation: {e}");
                    // Fall through to the normal payload path with whatever bytes we have.
                }
            }
        }

        let mut payload = serde_json::json!({
            "stdout": String::from_utf8_lossy(&outcome.stdout).into_owned(),
            "stderr": String::from_utf8_lossy(&outcome.stderr).into_owned(),
            "exit_status": outcome.exit_status,
            "signal": outcome.signal,
            "timed_out": outcome.timed_out,
            "cancelled": outcome.cancelled,
            "is_error": is_error,
        });
        if let Some(ref note) = exit_note {
            payload["exit_code_note"] = serde_json::Value::String(note.clone());
        }
        attach_text_fallback(&mut payload);
        Ok(ToolOutput::structured(payload))
    }
}

/// Render `payload` with [`render_text`] and store the result back into the
/// payload's `text_fallback` field.
///
/// # Why the rendering is carried in the payload rather than applied by callers
///
/// [`ToolOutput::structured_to_text`] is the single conversion every transport
/// (native, XML, MCP, CLI) uses to turn a structured result into model-facing
/// text, deliberately so the rendering cannot drift between them. It takes only
/// the payload — it has no tool identity — so the sole way for a tool to choose
/// its own rendering is to ship it in `text_fallback`. Having the transports
/// call [`render_text`] directly would give Bash a private path around that
/// single conversion, which is the drift it exists to prevent.
///
/// # The cost, and why it is worth paying
///
/// For an inline result this holds stdout/stderr twice in the payload: once as
/// the structured fields, once inside the rendered string. The duplication is
/// transient (the payload is converted to text and dropped; only the rendered
/// text is persisted to the transcript) and bounded by `PERSISTENCE_THRESHOLD`
/// — larger output takes the persisted path, where the payload carries a file
/// path instead of the bytes, so `text_fallback` duplicates nothing.
///
/// The duplication buys roughly a halving of the model-facing bytes, on the
/// most frequently called tool in the set: the structured form escapes every
/// newline in stdout and repeats each field name. Transient bytes are cheaper
/// than context bytes.
fn attach_text_fallback(payload: &mut serde_json::Value) {
    let rendered = render_text(payload);
    payload["text_fallback"] = serde_json::Value::String(rendered);
}

/// Return a human-readable note for well-known non-zero exit codes and signal kills.
///
/// Covers exit codes 126, 127, 130, 137, 143 and OS-level signals 2, 9, 15.
/// Returns `None` for exit code 0 (success).
/// Returns a generic advisory for any other non-zero code or signal.
///
/// # Scope note
///
/// Command-specific "exit-1-means-success" interpretation (e.g., `grep` returning
/// exit 1 when no lines match, `diff` returning 1 when files differ, `test` returning
/// 1 when the condition is false) is intentionally out of scope here. Such commands
/// are correct to exit non-zero; distinguishing them from genuine failures would require
/// parsing the command string to identify the binary, which is fragile and error-prone.
/// The generic catch-all below ("inspect the output…") is the safe default. A model that
/// understands grep semantics will not be confused by the note; one that doesn't would
/// benefit from more output context anyway.
pub fn interpret_exit_code(exit_status: i32, signal: Option<i32>) -> Option<String> {
    if let Some(sig) = signal {
        let note = match sig {
            2 => "Process killed by SIGINT (signal 2 — keyboard interrupt or Ctrl-C).",
            9 => "Process killed by SIGKILL (signal 9 — forced termination, often the OOM killer).",
            15 => "Process killed by SIGTERM (signal 15 — graceful-stop signal).",
            _ => "Process was killed by a signal before it could exit normally.",
        };
        return Some(note.to_string());
    }

    let note = match exit_status {
        0 => return None,
        126 => "Exit 126: command found but not executable — check file permissions or binary format.",
        127 => "Exit 127: command not found — the executable is missing from PATH or the name is misspelled.",
        130 => "Exit 130: terminated by SIGINT (signal 2 — keyboard interrupt or Ctrl-C).",
        137 => "Exit 137: killed by SIGKILL (signal 9 — forced kill, commonly the OOM killer or ulimit).",
        143 => "Exit 143: terminated by SIGTERM (signal 15 — standard graceful-stop signal).",
        _ => "Non-zero exit status — inspect the output and stderr above for the error cause.",
    };
    Some(note.to_string())
}

/// Detect a foreground bare-sleep command: `sleep N` where N ≥ 2 seconds.
///
/// Only matches when the entire command (after whitespace stripping) is a single
/// `sleep` invocation with a numeric argument and no other tokens. Returns the
/// sleep duration if the pattern matches and the duration is at or above the
/// threshold; None otherwise.
///
/// # Design note — "bare" not "leading"
///
/// The spec describes blocking a "leading `sleep N`" pattern, which could be read
/// as blocking `sleep 5 && start-server.sh` too. This implementation intentionally
/// restricts the guard to commands where sleep is the ONLY token (no pipeline,
/// no `&&`, no trailing commands). Blocking a leading sleep in a pipeline would be
/// overly aggressive — `sleep 2 && curl ...` is a legitimate retry-with-delay pattern.
/// The `run_in_background` path already handles long-duration sleeps correctly, so
/// the guard's purpose is narrowly to prevent wasteful single-token `sleep N` calls.
fn detect_bare_sleep(command: &str) -> Option<f64> {
    const THRESHOLD_SECS: f64 = 2.0;

    let s = command.trim();
    let after_kw = s.strip_prefix("sleep")?;
    if !after_kw.starts_with(|c: char| c.is_ascii_whitespace()) {
        return None;
    }
    // The argument must be a bare numeric token with no trailing content.
    // If there's a pipeline, &&, or anything else, parse fails → None.
    let arg = after_kw.trim();
    let secs: f64 = arg.parse().ok()?;
    if secs >= THRESHOLD_SECS { Some(secs) } else { None }
}

/// Build the exit-status footer line for Bash tool results.
///
/// Precedence: `cancelled` > `timed_out` > `signal=N` > `exit=N`.
pub fn exit_footer(
    is_error: bool,
    cancelled: bool,
    timed_out: bool,
    signal: Option<i32>,
    exit_status: i32,
) -> String {
    let _ = is_error;
    if cancelled {
        "cancelled".to_string()
    } else if timed_out {
        "timeout".to_string()
    } else if let Some(sig) = signal {
        format!("signal={sig}")
    } else {
        format!("exit={exit_status}")
    }
}

/// Render a Bash tool structured payload as the flat text string the model reads.
///
/// Reached on every Bash call: `invoke` passes each payload it builds through
/// [`attach_text_fallback`], and the transports read that field back out via
/// [`ToolOutput::structured_to_text`]. Without it the model receives the raw
/// JSON serialization of the payload.
///
/// Idempotent. When the payload already carries a `text_fallback` field — the
/// persisted-output path assembles its own, and any payload that has already
/// been through [`attach_text_fallback`] has one — that value is returned
/// verbatim rather than re-rendered.
///
/// For normal (inline) payloads:
/// 1. Stdout content verbatim (empty stdout contributes nothing).
/// 2. Each stderr line prefixed with `stderr: ` followed by `\n`.
/// 3. A footer line — precedence: `cancelled` > `timeout` > `signal=N` > `exit=N`.
///
/// Empty stdout or stderr produce no extraneous separators.
///
/// Private by design. `dead_code` cannot report an orphaned `pub` item in a
/// library crate — such an item is externally reachable by definition, so the
/// lint has nothing to say about it. Privacy is what makes the loss of the last
/// caller a compiler warning rather than a silent one.
fn render_text(payload: &serde_json::Value) -> String {
    // Persisted-output path: the text_fallback field already contains the
    // envelope and footer assembled by invoke().
    if let Some(fallback) = payload.get("text_fallback").and_then(|v| v.as_str()) {
        return fallback.to_string();
    }

    // Background paths: format a compact summary for the model. Covers both the
    // auto-backgrounded promotion (which carries a `note`) and an explicit
    // `run_in_background: true` spawn (which does not).
    //
    // The condition must be checked before the exit-footer path below. A
    // backgrounded process has not exited, and none of `exit_status`, `signal`,
    // `timed_out` or `cancelled` is present in either payload — so falling
    // through would render a live process as the flat default `exit=0`.
    let backgrounded = payload
        .get("auto_backgrounded")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || payload.get("status").and_then(|v| v.as_str()) == Some("running");
    if backgrounded {
        let process_id = payload.get("process_id").and_then(|v| v.as_str()).unwrap_or("?");
        let output_path = payload.get("output_path").and_then(|v| v.as_str()).unwrap_or("?");
        let mut out = format!("process_id={process_id}\noutput_path={output_path}\nstatus=running\n");
        if let Some(note) = payload.get("note").and_then(|v| v.as_str()).filter(|n| !n.is_empty()) {
            out.push_str(note);
            out.push('\n');
        }
        return out;
    }

    let mut out = String::new();

    if let Some(stdout) = payload.get("stdout").and_then(|v| v.as_str()) {
        out.push_str(stdout);
    }

    if let Some(stderr) = payload.get("stderr").and_then(|v| v.as_str()) {
        for line in stderr.lines() {
            out.push_str("stderr: ");
            out.push_str(line);
            out.push('\n');
        }
    }

    let cancelled = payload
        .get("cancelled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let timed_out = payload
        .get("timed_out")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let signal = payload.get("signal").and_then(|v| v.as_i64()).map(|s| s as i32);
    let exit_status = payload
        .get("exit_status")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let is_error = payload
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    out.push_str(&exit_footer(is_error, cancelled, timed_out, signal, exit_status));
    out.push('\n');

    // Append the exit code note if the caller embedded it in the payload.
    if let Some(note) = payload.get("exit_code_note").and_then(|v| v.as_str()) {
        if !note.is_empty() {
            out.push_str(note);
            out.push('\n');
        }
    }

    out
}

/// Register [`BashTool`] into `registry` on the IO side.
pub fn register_bash(registry: &mut Registry) {
    registry.register_io(Arc::new(BashTool));
}
