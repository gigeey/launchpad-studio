use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use ao_engine_tools_core::RunnerContext;
use ao_protocol::error::AoError;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::task::JoinHandle;

/// Persistent BASH_ENV file read by every `bash -c` subprocess before the user's command runs.
///
/// Content (all lines are best-effort — failures are silenced):
/// 1. `set -o pipefail`              — propagate failures through pipelines.
/// 2. `shopt -s expand_aliases`      — enable alias expansion in non-interactive shells.
/// 3. `source <snapshot>`            — inject rc-derived functions, aliases, and PATH
///                                     captured once at startup by `shell_snapshot`.
///
/// Initialized once per process; `None` if the file cannot be created (all three
/// behaviours degrade gracefully to running without the corresponding feature).
static BASH_ENV_FILE: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    use std::io::Write;

    // Force snapshot initialization before writing the BASH_ENV content so the
    // source line can embed the snapshot path.
    let snapshot_path = super::shell_snapshot::SHELL_SNAPSHOT_FILE.as_ref();

    let mut f = match tempfile::Builder::new()
        .prefix("launchpad-bashenv-")
        .tempfile_in(std::env::temp_dir())
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                "BASH_ENV setup file creation failed: {e}; shell environment injection skipped"
            );
            return None;
        }
    };

    let mut content =
        String::from("set -o pipefail\nshopt -s expand_aliases 2>/dev/null || true\n");
    if let Some(snap) = snapshot_path {
        content.push_str(&format!(
            "[ -f \"{}\" ] && source \"{}\" 2>/dev/null || true\n",
            snap.display(),
            snap.display()
        ));
    }

    if let Err(e) = f.write_all(content.as_bytes()) {
        tracing::warn!("BASH_ENV setup file write failed: {e}; shell environment injection skipped");
        return None;
    }

    let (_, path) = match f.keep() {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(
                "BASH_ENV setup file persist failed: {e}; shell environment injection skipped"
            );
            return None;
        }
    };

    // SAFETY: atexit is POSIX-standard; BASH_ENV_FILE is already initialized when
    // the callback fires (atexit is only registered from within this initializer).
    unsafe { libc::atexit(cleanup_bash_env_file) };

    Some(path)
});

extern "C" fn cleanup_bash_env_file() {
    if let Some(p) = &*BASH_ENV_FILE {
        let _ = std::fs::remove_file(p);
    }
}

/// Build the environment for a bash subprocess.
///
/// Inherits the current process environment, strips all keys whose UTF-8
/// representation starts with `AO_`, `LAUNCHPAD_`, or `CLAUDE_` (case-sensitive
/// on Unix; case-insensitive on Windows), and injects `BASH_ENV` pointing at the
/// pipefail injection file (best-effort — skipped if file creation failed).
pub fn build_env() -> Vec<(OsString, OsString)> {
    let denied_prefixes: &[&str] = &["AO_", "LAUNCHPAD_", "CLAUDE_"];

    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| {
            if let Some(k_str) = k.to_str() {
                #[cfg(unix)]
                let denied = denied_prefixes.iter().any(|p| k_str.starts_with(p));
                #[cfg(not(unix))]
                let denied = denied_prefixes
                    .iter()
                    .any(|p| k_str.len() >= p.len() && k_str[..p.len()].eq_ignore_ascii_case(p));
                !denied
            } else {
                true // Non-UTF8 keys are not in the denylist.
            }
        })
        .collect();

    // Inject BASH_ENV (best-effort; skip if pipefail file unavailable).
    if let Some(bash_env_path) = &*BASH_ENV_FILE {
        env.retain(|(k, _)| k.to_str() != Some("BASH_ENV"));
        env.push((
            OsString::from("BASH_ENV"),
            bash_env_path.as_os_str().to_owned(),
        ));
    }

    // Neutralize editor-, pager-, and credential-prompt vars so that commands
    // spawning interactive helpers (git commit, git rebase -i, less, etc.) fail
    // fast instead of blocking on the null stdin.  Each key is removed from the
    // inherited env before being re-pushed, so an inherited value like EDITOR=vim
    // cannot survive.
    let mut set = |key: &str, val: &str| {
        env.retain(|(k, _)| k.to_str() != Some(key));
        env.push((OsString::from(key), OsString::from(val)));
    };
    set("GIT_EDITOR", "true");
    set("EDITOR", "true");
    set("VISUAL", "true");
    set("GIT_SEQUENCE_EDITOR", "true");
    set("GIT_PAGER", "cat");
    set("PAGER", "cat");
    set("GIT_TERMINAL_PROMPT", "0");

    env
}

/// Split a leading `cd <path> &&` (or `cd <path> ;`) from `command`.
///
/// Recognised forms (path may be unquoted, double-quoted, or single-quoted):
///   - `cd /path && rest`
///   - `cd "path with spaces" ; rest`
///   - `  cd 'path' && rest`   (leading whitespace is accepted)
///
/// **Limitation**: only a single leading `cd` is peeled. For `cd a && cd b && rest`,
/// the model is expected to compose `cd a/b && rest` before calling. Callers must
/// not rely on this function to chain multiple directory changes.
///
/// Returns `(Some(path), rest)` on match — path has surrounding quotes stripped.
/// Returns `(None, command)` on no match, leaving the original string untouched.
pub fn split_leading_cd(command: &str) -> (Option<&str>, &str) {
    let s = command.trim_start();

    // Must start with "cd" followed by whitespace, not e.g. "cds" or "cd".
    let after_cd = match s.strip_prefix("cd") {
        Some(r) => r,
        None => return (None, command),
    };
    if !after_cd.starts_with(|c: char| c.is_ascii_whitespace()) {
        return (None, command);
    }
    let after_ws = after_cd.trim_start();

    // Parse the path component.
    let (path, after_path) = if after_ws.starts_with('"') {
        let inner = &after_ws[1..];
        match inner.find('"') {
            Some(end) => (&inner[..end], &inner[end + 1..]),
            None => return (None, command),
        }
    } else if after_ws.starts_with('\'') {
        let inner = &after_ws[1..];
        match inner.find('\'') {
            Some(end) => (&inner[..end], &inner[end + 1..]),
            None => return (None, command),
        }
    } else {
        let end = after_ws
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(after_ws.len());
        (&after_ws[..end], &after_ws[end..])
    };

    if path.is_empty() {
        return (None, command);
    }

    // Separator: && or ;
    let trimmed = after_path.trim_start();
    let after_sep = if trimmed.starts_with("&&") {
        &trimmed[2..]
    } else if trimmed.starts_with(';') {
        &trimmed[1..]
    } else {
        return (None, command);
    };

    let rest = after_sep.trim_start();
    if rest.is_empty() {
        return (None, command);
    }

    (Some(path), rest)
}

/// Walk up from `cwd` to find the nearest ancestor that exists on disk.
///
/// Prevents spawn failures when the shell's recorded working directory has
/// been deleted between Bash calls. Falls back to `/` if no ancestor exists.
fn recover_cwd(cwd: PathBuf) -> PathBuf {
    if cwd.exists() {
        return cwd;
    }
    let mut cur = cwd.as_path();
    loop {
        match cur.parent() {
            Some(p) if p.exists() => return p.to_path_buf(),
            Some(p) => cur = p,
            None => return PathBuf::from("/"),
        }
    }
}

/// Build a shell script that runs `command` then captures the final working
/// directory into `$CWD_CAPTURE_FILE`.
///
/// Exit status is preserved via `__lp_ec` so the `pwd -P` write cannot
/// overwrite the user command's exit code. If the user command calls `exit`
/// directly the capture block will not run and cwd will not be updated for
/// that invocation.
fn cwd_capture_wrapper(command: &str) -> String {
    format!(
        "{command}\n__lp_ec=$?\npwd -P > \"$CWD_CAPTURE_FILE\" 2>/dev/null || true\nexit $__lp_ec"
    )
}

/// Read the directory captured by [`cwd_capture_wrapper`] and, if it differs
/// from `start_cwd` (after symlink resolution), write it into `ctx.cwd`.
fn apply_cwd_from_capture(
    ctx: &RunnerContext,
    start_cwd: &std::path::Path,
    capture_path: &std::path::Path,
) {
    let Ok(raw) = std::fs::read_to_string(capture_path) else {
        return;
    };
    let new_cwd = PathBuf::from(raw.trim());
    if !new_cwd.is_dir() {
        return;
    }
    let canonical_start =
        std::fs::canonicalize(start_cwd).unwrap_or_else(|_| start_cwd.to_path_buf());
    if new_cwd != canonical_start {
        *ctx.cwd.write().unwrap() = new_cwd;
    }
}

/// Outcome of a completed bash subprocess.
pub struct ExecutionOutcome {
    /// Raw (untruncated) stdout when `needs_persistence` is true; otherwise
    /// middle-truncated to fit within `COMBINED_LIMIT`.
    pub stdout: Vec<u8>,
    /// Raw (untruncated) stderr when `needs_persistence` is true; otherwise
    /// middle-truncated to fit within `COMBINED_LIMIT`.
    pub stderr: Vec<u8>,
    /// Process exit code. 0 when terminated by a signal on Unix.
    pub exit_status: i32,
    /// Signal number that killed the process; `None` on normal exit or non-unix.
    pub signal: Option<i32>,
    /// True when the timeout deadline fired and initiated process termination.
    pub timed_out: bool,
    /// True when `ctx.cancel` fired and initiated process termination.
    /// When both timeout and cancel fire simultaneously, cancelled takes precedence.
    pub cancelled: bool,
    /// True when the raw combined output exceeds `PERSISTENCE_THRESHOLD`.
    /// When set, `stdout` and `stderr` carry the full untruncated bytes so the
    /// caller can write them to disk and return a compact envelope instead.
    pub needs_persistence: bool,
}

/// Resolve the shell binary. Prefers bash from `$SHELL` so that BASH_ENV
/// (pipefail injection) is reliably read; falls back to `/bin/bash`.
/// Non-bash values of `$SHELL` (e.g. `/bin/zsh`) are ignored because zsh
/// does not read BASH_ENV for `shell -c` invocations.
fn resolve_shell() -> PathBuf {
    if let Ok(shell) = std::env::var("SHELL") {
        if shell.contains("bash") {
            return PathBuf::from(shell);
        }
    }
    PathBuf::from("/bin/bash")
}

/// Send `sig` to the child's process group (Unix only).
///
/// We spawn subprocesses in a dedicated process group (`process_group(0)`), so
/// negating the PID delivers the signal to ALL members of the group — including
/// grandchildren (e.g. `sleep` spawned by bash). Without this, killing bash while
/// a child holds the stdout pipe open causes the output-pump task to stall.
#[cfg(unix)]
fn signal_child(child: &tokio::process::Child, sig: libc::c_int) {
    if let Some(pid) = child.id() {
        // SAFETY: kill() is async-signal-safe; negative PID targets the process group.
        unsafe { libc::kill(-(pid as libc::pid_t), sig) };
    }
}

/// Grace period between SIGTERM and SIGKILL when terminating a subprocess.
///
/// Shared by every termination path — foreground timeout, foreground cancel,
/// and BashKill on a background command — so a process that installs a SIGTERM
/// handler gets the same window to clean up regardless of who stopped it.
pub(crate) const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// Send SIGTERM, wait up to `grace`, then send SIGKILL if still alive.
/// On non-unix targets, `Child::kill()` is used immediately (no grace period —
/// Windows has no graceful-termination primitive in std::process).
///
/// Signals go to the whole process group via [`signal_child`], so a shell that
/// spawned the real work as a grandchild does not leave it orphaned.
pub(crate) async fn terminate_child(child: &mut tokio::process::Child, grace: Duration) {
    #[cfg(unix)]
    {
        signal_child(child, libc::SIGTERM);
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(_) => {}
            Err(_grace_elapsed) => {
                signal_child(child, libc::SIGKILL);
                let _ = child.wait().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = grace; // no grace on Windows
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

/// Middle-truncate `bytes` to at most `max` bytes, inserting a marker at the cut.
///
/// If `bytes.len() <= max`, returns a copy verbatim. Otherwise splits into a
/// head and tail of roughly equal size with a `\n[output truncated: N bytes elided]\n`
/// marker sandwiched between them. Two passes are used to resolve the circular
/// dependency between the marker length and the head/tail sizes.
pub fn middle_truncate(bytes: &[u8], max: usize) -> Vec<u8> {
    if bytes.len() <= max {
        return bytes.to_vec();
    }

    // Pass 1: approximate N to get a stable marker length.
    let approx_n = bytes.len().saturating_sub(max);
    let m1 = format!("\n[output truncated: {} bytes elided]\n", approx_n).into_bytes();
    let head1 = (max / 2).saturating_sub(m1.len() / 2);
    let tail1 = max.saturating_sub(head1).saturating_sub(m1.len());
    let actual_n = bytes.len().saturating_sub(head1).saturating_sub(tail1);

    // Pass 2: compute head/tail with the exact N.
    let marker = format!("\n[output truncated: {} bytes elided]\n", actual_n).into_bytes();
    let head_len = (max / 2).saturating_sub(marker.len() / 2);
    let tail_len = max.saturating_sub(head_len).saturating_sub(marker.len());

    let mut out = Vec::with_capacity(head_len + marker.len() + tail_len);
    out.extend_from_slice(&bytes[..head_len]);
    out.extend_from_slice(&marker);
    out.extend_from_slice(&bytes[bytes.len() - tail_len..]);
    out
}

/// Combined output budget for middle-truncation (30 KB).
/// Outputs above this limit but below `PERSISTENCE_THRESHOLD` are truncated inline.
const COMBINED_LIMIT: usize = 30 * 1024;

/// Combined output size at which disk persistence is triggered instead of inline delivery.
/// Above this limit the full untruncated bytes are returned in `ExecutionOutcome` so the
/// caller can write them to disk and return a compact envelope. Chosen to sit comfortably
/// above `COMBINED_LIMIT` so the middle-truncation path still handles moderately-large
/// outputs while very large outputs avoid flooding the model context window.
pub const PERSISTENCE_THRESHOLD: usize = 100_000;

/// Compute per-stream truncation budget (proportional, min 1024 bytes).
///
/// Formula: COMBINED_LIMIT * own_len / (stdout_len + stderr_len + 1)
/// The +1 avoids division by zero and biases toward retaining content when
/// both streams are empty.
fn stream_budget(own_len: usize, other_len: usize) -> usize {
    let total = own_len + other_len + 1;
    ((COMBINED_LIMIT as f64 * own_len as f64 / total as f64) as usize).max(1024)
}

/// Spawn `command` in a fresh shell subprocess, enforce `timeout_ms`, honour
/// `ctx.cancel`, capture stdout and stderr, apply proportional middle-truncation
/// if combined output exceeds 30 KB, and return the outcome.
///
/// After the command exits, captures the shell's final working directory via
/// `pwd -P` readback and updates `ctx.cwd` when it changed. If `ctx.cwd`
/// no longer exists on disk before spawning, recovers to the nearest
/// existing ancestor so the spawn does not fail with ENOENT.
pub async fn run(
    command: &str,
    ctx: &RunnerContext,
    timeout_ms: u64,
) -> Result<ExecutionOutcome, AoError> {
    let shell = resolve_shell();

    // Recover deleted working directory before spawning.
    let start_cwd = {
        let raw = ctx.cwd.read().unwrap().clone();
        let recovered = recover_cwd(raw.clone());
        if recovered != raw {
            *ctx.cwd.write().unwrap() = recovered.clone();
        }
        recovered
    };
    let cancel = ctx.cancel.clone();

    // Per-invocation tempfile for cwd capture (best-effort; skipped on failure).
    let capture_file = tempfile::Builder::new().prefix("lp-cwd-").tempfile().ok();
    let (run_cmd, env) = {
        let mut e = build_env();
        let s = if let Some(p) = capture_file.as_ref().and_then(|f| f.path().to_str()) {
            e.push((OsString::from("CWD_CAPTURE_FILE"), OsString::from(p)));
            cwd_capture_wrapper(command)
        } else {
            command.to_string()
        };
        (s, e)
    };

    let mut cmd = Command::new(&shell);
    cmd.args(["-c", &run_cmd])
        .current_dir(&start_cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_clear()
        .envs(env);
    // Place the child in its own process group so signals reach all descendants.
    // Without this, grandchildren (e.g. `sleep` spawned by bash) keep stdout pipes
    // open after the shell is killed, blocking the output-pump tasks indefinitely.
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| AoError::Process(format!("failed to spawn shell: {e}")))?;

    let mut stdout_handle = child.stdout.take().expect("stdout is piped");
    let mut stderr_handle = child.stderr.take().expect("stderr is piped");

    // Pump both streams concurrently so a full stdout pipe doesn't block stderr.
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stdout_handle.read_to_end(&mut buf).await?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stderr_handle.read_to_end(&mut buf).await?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    });

    let mut timed_out = false;
    let mut cancelled = false;
    let grace = TERMINATE_GRACE;

    // Three-way race: natural exit | timeout | cancellation token.
    // When both timeout and cancel fire simultaneously, cancelled takes precedence
    // because the cancel arm is checked first by tokio::select!'s fair scheduling.
    let final_status = tokio::select! {
        result = child.wait() => {
            result.map_err(|e| AoError::Process(format!("wait failed: {e}")))?
        }
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
            timed_out = true;
            terminate_child(&mut child, grace).await;
            child.wait().await.map_err(|e| AoError::Process(format!("wait failed: {e}")))?
        }
        _ = cancel.cancelled() => {
            // cancelled takes precedence over timed_out in the outcome flag.
            cancelled = true;
            terminate_child(&mut child, grace).await;
            child.wait().await.map_err(|e| AoError::Process(format!("wait failed: {e}")))?
        }
    };

    let stdout = stdout_task
        .await
        .map_err(|e| AoError::Internal(e.to_string()))?
        .map_err(AoError::Io)?;
    let stderr = stderr_task
        .await
        .map_err(|e| AoError::Internal(e.to_string()))?
        .map_err(AoError::Io)?;

    let stdout_len = stdout.len();
    let stderr_len = stderr.len();
    let combined_len = stdout_len + stderr_len;

    // When combined output exceeds the persistence threshold, return the full
    // untruncated bytes and let the caller write them to disk. Skip truncation
    // entirely so the file on disk captures the complete output.
    let needs_persistence = combined_len > PERSISTENCE_THRESHOLD;

    // Apply proportional middle-truncation for outputs that exceed the inline
    // budget but fall below the persistence threshold.
    let (stdout, stderr) = if needs_persistence {
        (stdout, stderr)
    } else if combined_len > COMBINED_LIMIT {
        let stdout_bud = stream_budget(stdout_len, stderr_len);
        let stderr_bud = stream_budget(stderr_len, stdout_len);
        (
            middle_truncate(&stdout, stdout_bud),
            middle_truncate(&stderr, stderr_bud),
        )
    } else {
        (stdout, stderr)
    };

    #[cfg(unix)]
    let (exit_status, signal) = {
        use std::os::unix::process::ExitStatusExt;
        let sig = final_status.signal();
        let code = if sig.is_some() {
            0
        } else {
            final_status.code().unwrap_or(0)
        };
        (code, sig)
    };

    #[cfg(not(unix))]
    let (exit_status, signal) = (final_status.code().unwrap_or(0), None::<i32>);

    // Update ctx.cwd from the captured directory if the command changed location.
    if let Some(ref f) = capture_file {
        apply_cwd_from_capture(ctx, &start_cwd, f.path());
    }

    Ok(ExecutionOutcome {
        stdout,
        stderr,
        exit_status,
        signal,
        timed_out,
        cancelled,
        needs_persistence,
    })
}

/// Outcome of [`run_foreground`]: either the command finished or it was promoted
/// to background because it exceeded the auto-background time threshold.
pub enum ForegroundOutcome {
    /// Command finished (naturally, timed out, or cancelled).
    Done(ExecutionOutcome),
    /// Command was still running at the auto-background threshold.
    /// The caller should register the process with the background registry using
    /// the pre-threshold output so no bytes are lost.
    Backgrounded {
        /// Bytes read from stdout before the threshold fired.
        pre_stdout: Vec<u8>,
        /// Bytes read from stderr before the threshold fired.
        pre_stderr: Vec<u8>,
        /// The live child process — ownership passes to the background drain task.
        child: tokio::process::Child,
        /// Child stdout pipe (partially consumed) — passes to the drain task.
        stdout: tokio::process::ChildStdout,
        /// Child stderr pipe (partially consumed) — passes to the drain task.
        stderr: tokio::process::ChildStderr,
    },
}

/// Run `command` in foreground mode with optional auto-background promotion.
///
/// When `auto_bg_ms` is `Some(n)` and `n < timeout_ms`, a command still running
/// after `n` ms is promoted to background and returns
/// [`ForegroundOutcome::Backgrounded`]. The caller hands the child process to
/// [`crate::bash::background::register_running_process`].
///
/// Output accumulated before the threshold is included so the handoff is
/// lossless — pre-threshold bytes will be written to disk and seeded into the
/// in-memory buffer by the drain task.
///
/// When `auto_bg_ms` is `None` or `>= timeout_ms`, behaviour matches [`run`].
pub async fn run_foreground(
    command: &str,
    ctx: &RunnerContext,
    timeout_ms: u64,
    auto_bg_ms: Option<u64>,
) -> Result<ForegroundOutcome, AoError> {
    // Recover deleted working directory before spawning.
    let start_cwd = {
        let raw = ctx.cwd.read().unwrap().clone();
        let recovered = recover_cwd(raw.clone());
        if recovered != raw {
            *ctx.cwd.write().unwrap() = recovered.clone();
        }
        recovered
    };

    // Per-invocation tempfile for cwd capture (best-effort; skipped on failure).
    let capture_file = tempfile::Builder::new().prefix("lp-cwd-").tempfile().ok();
    let (run_cmd, env) = {
        let mut e = build_env();
        let s = if let Some(p) = capture_file.as_ref().and_then(|f| f.path().to_str()) {
            e.push((OsString::from("CWD_CAPTURE_FILE"), OsString::from(p)));
            cwd_capture_wrapper(command)
        } else {
            command.to_string()
        };
        (s, e)
    };

    let shell = resolve_shell();
    let mut spawn_cmd = Command::new(&shell);
    spawn_cmd
        .args(["-c", &run_cmd])
        .current_dir(&start_cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_clear()
        .envs(env);
    #[cfg(unix)]
    spawn_cmd.process_group(0);
    let mut child = spawn_cmd
        .spawn()
        .map_err(|e| AoError::Process(format!("failed to spawn shell: {e}")))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");

    let cancel = ctx.cancel.clone();
    let mut timed_out = false;
    let mut cancelled = false;
    let grace = TERMINATE_GRACE;

    let mut pre_stdout: Vec<u8> = Vec::new();
    let mut pre_stderr: Vec<u8> = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    // Per-read staging buffers — separate so both arms can coexist in select!.
    let mut out_buf = vec![0u8; 8192];
    let mut err_buf = vec![0u8; 8192];

    // Auto-background is only armed when the threshold is shorter than the timeout.
    let do_auto_bg = auto_bg_ms.map(|ms| ms < timeout_ms).unwrap_or(false);
    // Use Duration::MAX as sentinel when not armed (the select arm is guarded anyway).
    let auto_bg_dur = auto_bg_ms
        .map(Duration::from_millis)
        .unwrap_or(Duration::MAX);

    let timeout_deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
    let auto_bg_deadline = tokio::time::sleep(auto_bg_dur);
    tokio::pin!(timeout_deadline);
    tokio::pin!(auto_bg_deadline);

    let phase = 'select: loop {
        tokio::select! {
            result = stdout.read(&mut out_buf), if !stdout_eof => {
                match result {
                    Ok(0) | Err(_) => {
                        stdout_eof = true;
                        if stderr_eof { break 'select "completed"; }
                    }
                    Ok(n) => { pre_stdout.extend_from_slice(&out_buf[..n]); }
                }
            }
            result = stderr.read(&mut err_buf), if !stderr_eof => {
                match result {
                    Ok(0) | Err(_) => {
                        stderr_eof = true;
                        if stdout_eof { break 'select "completed"; }
                    }
                    Ok(n) => { pre_stderr.extend_from_slice(&err_buf[..n]); }
                }
            }
            _ = &mut auto_bg_deadline, if do_auto_bg => {
                // If both pipes are already at EOF the child has finished; promote
                // to "completed" rather than incorrectly returning Backgrounded.
                if stdout_eof && stderr_eof {
                    break 'select "completed";
                }
                break 'select "auto_bg";
            }
            _ = &mut timeout_deadline => {
                timed_out = true;
                break 'select "terminated";
            }
            _ = cancel.cancelled() => {
                cancelled = true;
                break 'select "terminated";
            }
        }
    };

    if phase == "auto_bg" {
        return Ok(ForegroundOutcome::Backgrounded {
            pre_stdout,
            pre_stderr,
            child,
            stdout,
            stderr,
        });
    }

    if phase == "terminated" {
        terminate_child(&mut child, grace).await;
        // Drain whatever remains in the pipe buffers after the kill so the
        // model sees partial output rather than nothing.
        let _ = stdout.read_to_end(&mut pre_stdout).await;
        let _ = stderr.read_to_end(&mut pre_stderr).await;
    }
    // "completed": both pipes at EOF; fall through to reap the child.

    let final_status = child
        .wait()
        .await
        .map_err(|e| AoError::Process(format!("wait failed: {e}")))?;

    // Update cwd from capture for foreground-completed paths.
    // For the Backgrounded path, cwd is not read back (process still running).
    if let Some(ref f) = capture_file {
        apply_cwd_from_capture(ctx, &start_cwd, f.path());
    }

    Ok(ForegroundOutcome::Done(apply_truncation(
        pre_stdout,
        pre_stderr,
        final_status,
        timed_out,
        cancelled,
    )))
}

/// Apply proportional middle-truncation and produce an [`ExecutionOutcome`].
fn apply_truncation(
    stdout_raw: Vec<u8>,
    stderr_raw: Vec<u8>,
    status: std::process::ExitStatus,
    timed_out: bool,
    cancelled: bool,
) -> ExecutionOutcome {
    let stdout_len = stdout_raw.len();
    let stderr_len = stderr_raw.len();
    let combined_len = stdout_len + stderr_len;
    let needs_persistence = combined_len > PERSISTENCE_THRESHOLD;

    let (stdout, stderr) = if needs_persistence {
        (stdout_raw, stderr_raw)
    } else if combined_len > COMBINED_LIMIT {
        let sb = stream_budget(stdout_len, stderr_len);
        let eb = stream_budget(stderr_len, stdout_len);
        (middle_truncate(&stdout_raw, sb), middle_truncate(&stderr_raw, eb))
    } else {
        (stdout_raw, stderr_raw)
    };

    #[cfg(unix)]
    let (exit_status, signal) = {
        use std::os::unix::process::ExitStatusExt;
        let sig = status.signal();
        let code = if sig.is_some() { 0 } else { status.code().unwrap_or(0) };
        (code, sig)
    };
    #[cfg(not(unix))]
    let (exit_status, signal) = (status.code().unwrap_or(0), None::<i32>);

    ExecutionOutcome {
        stdout,
        stderr,
        exit_status,
        signal,
        timed_out,
        cancelled,
        needs_persistence,
    }
}

/// Return value from [`run_background`]: the live child process plus two tasks
/// that pump its stdout and stderr streams into `Vec<u8>` buffers.
pub struct BackgroundSpawn {
    /// The child process (stdio pipes already taken; do NOT call `.take()` again).
    pub child: tokio::process::Child,
    /// Task that drains stdout into a `Vec<u8>`. Completes when the stream closes.
    pub stdout_task: JoinHandle<Vec<u8>>,
    /// Task that drains stderr into a `Vec<u8>`. Completes when the stream closes.
    pub stderr_task: JoinHandle<Vec<u8>>,
}

/// Return value from [`run_background_raw`]: child process with raw pipe handles.
///
/// Unlike [`BackgroundSpawn`], no pump tasks have been spawned. The caller is
/// responsible for draining `stdout` and `stderr` (e.g., routing to disk and an
/// in-memory buffer) and for waiting on `child`.
pub struct BackgroundSpawnRaw {
    /// The spawned child (stdout/stderr already taken via `.take()`).
    pub child: tokio::process::Child,
    /// Child's stdout pipe, ready for async reading.
    pub stdout: tokio::process::ChildStdout,
    /// Child's stderr pipe, ready for async reading.
    pub stderr: tokio::process::ChildStderr,
}

/// Spawn `command` in a fresh shell subprocess for background execution.
///
/// Unlike [`run`], this function returns immediately after spawning — it does
/// NOT wait for the child to exit. The caller is responsible for wrapping the
/// returned [`BackgroundSpawn`] in a [`BackgroundProcessHandle`] and inserting
/// it into `ctx.background_processes`.
///
/// Timeout, cancellation, and output truncation are NOT applied to background
/// processes in this MVP. Stdout/stderr buffers grow unbounded until the child
/// exits and the pump tasks complete.
pub fn run_background(command: &str, ctx: &RunnerContext) -> Result<BackgroundSpawn, AoError> {
    let shell = resolve_shell();
    let cwd = {
        let raw = ctx.cwd.read().unwrap().clone();
        let recovered = recover_cwd(raw.clone());
        if recovered != raw {
            *ctx.cwd.write().unwrap() = recovered.clone();
        }
        recovered
    };

    let mut cmd = Command::new(&shell);
    cmd.args(["-c", command])
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_clear()
        .envs(build_env());
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| AoError::Process(format!("failed to spawn shell (background): {e}")))?;

    let mut stdout_handle = child.stdout.take().expect("stdout is piped");
    let mut stderr_handle = child.stderr.take().expect("stderr is piped");

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stdout_handle.read_to_end(&mut buf).await.ok();
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stderr_handle.read_to_end(&mut buf).await.ok();
        buf
    });

    Ok(BackgroundSpawn {
        child,
        stdout_task,
        stderr_task,
    })
}

/// Spawn `command` for background execution and return the raw child with its
/// pipe handles, without pre-spawning any pump tasks.
///
/// The caller drives all output routing (disk, in-memory buffer) and is
/// responsible for calling `child.wait()` to reap the process. This is the
/// low-level primitive used by the `BackgroundCommandRegistry` path; prefer
/// this over [`run_background`] when you need control over output routing.
pub fn run_background_raw(command: &str, ctx: &RunnerContext) -> Result<BackgroundSpawnRaw, AoError> {
    let shell = resolve_shell();
    let cwd = {
        let raw = ctx.cwd.read().unwrap().clone();
        let recovered = recover_cwd(raw.clone());
        if recovered != raw {
            *ctx.cwd.write().unwrap() = recovered.clone();
        }
        recovered
    };

    let mut cmd = Command::new(&shell);
    cmd.args(["-c", command])
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_clear()
        .envs(build_env());
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| AoError::Process(format!("failed to spawn shell (background): {e}")))?;

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    Ok(BackgroundSpawnRaw { child, stdout, stderr })
}
