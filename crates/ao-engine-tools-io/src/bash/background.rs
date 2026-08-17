//! Background-mode Bash spawning, registry integration, and output routing.
//!
//! # Output routing
//!
//! When `run_in_background` is set, the child's combined stdout+stderr is
//! streamed concurrently to:
//! - A disk file under `{data_root}/bash-background/{id}.log` — persists for
//!   the session; the model can read it at any time with the Read tool.
//! - A bounded in-memory ring buffer (cap: `OUTPUT_BUFFER_CAP` bytes) held on
//!   the `BackgroundCommandHandle`. Older bytes are dropped when the cap is
//!   hit so a chatty process cannot exhaust heap.
//!
//! # Cancellation
//!
//! Background subprocesses do NOT honour `ctx.cancel` propagation. Once
//! registered, a process is owned by the registry and survives the parent
//! tool call. BashKill signals the handle's `cancel` token; the drain task
//! below watches it and terminates the child's process group.
//!
//! # Registry sharing
//!
//! The `BackgroundCommandRegistry` is stored as `Arc<BackgroundCommandRegistry>`
//! on `RunnerContext.background_commands`. Every tool invocation that shares a
//! `RunnerContext` — or is bound to the same registry Arc — reaches the same
//! instance without a process-wide singleton. The native runner holds one
//! context per run; the MCP HTTP route builds one per JSON-RPC call and binds
//! the session's registry explicitly (see `McpAgentSession::background_commands`).
//! Child contexts receive a fresh independent registry (matches the
//! `background_agents` pattern).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use ao_engine_tools_core::{
    BackgroundCommandHandle, BackgroundCommandId, BackgroundCommandStatus, BoundedOutputBuffer,
    RunnerContext, OUTPUT_BUFFER_CAP,
};
use ao_protocol::{data_root::resolve_data_root, error::AoError};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use super::execute;

/// How long the drain task waits for the output pumps to finish after a kill.
///
/// Killing the process group closes the pipes, so the pumps normally return
/// within microseconds and this budget is never reached. It exists as a
/// backstop for a process that escaped the group (e.g. by calling `setsid`)
/// and still holds the write end open: losing the tail of a log is a better
/// outcome than a drain task that never completes and never reports a status.
const POST_KILL_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Spawn `command` as a background subprocess, register the handle in
/// `ctx.background_commands`, route output to disk and the in-memory buffer,
/// and return the assigned [`BackgroundCommandId`] together with the disk path.
///
/// Returns immediately — the child continues running after this function
/// returns. The drain task (spawned internally) updates the handle's status
/// and buffers concurrently.
pub async fn spawn_and_register(
    command: &str,
    ctx: &RunnerContext,
) -> Result<(BackgroundCommandId, PathBuf), AoError> {
    let execute::BackgroundSpawnRaw {
        child,
        stdout,
        stderr,
    } = execute::run_background_raw(command, ctx)?;

    let id = BackgroundCommandId::new();

    // Prepare the disk output directory and file path.
    let data_root = resolve_data_root()?;
    let output_dir = data_root.join("bash-background");
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(AoError::Io)?;
    let output_path = output_dir.join(format!("{id}.log"));

    let handle = Arc::new(BackgroundCommandHandle {
        id: id.clone(),
        command: command.to_string(),
        started_at: SystemTime::now(),
        output_path: output_path.clone(),
        status: Mutex::new(BackgroundCommandStatus::Running),
        output_buffer: Mutex::new(BoundedOutputBuffer::new(OUTPUT_BUFFER_CAP)),
        cancel: CancellationToken::new(),
        terminated: CancellationToken::new(),
    });

    // Spawn the drain task that owns the child for its lifetime.
    // The task monitors handle.cancel so BashKill can signal termination.
    let handle_for_drain = handle.clone();
    let path_for_drain = output_path.clone();
    tokio::spawn(async move {
        drain(child, stdout, stderr, path_for_drain, handle_for_drain).await;
    });

    ctx.background_commands
        .insert(handle)
        .await
        .map_err(|e| AoError::ValidationError(format!("background command registry: {e}")))?;

    Ok((id, output_path))
}

/// Register a process that was started in foreground mode but exceeded the
/// auto-background threshold.
///
/// Unlike [`spawn_and_register`] (which spawns a fresh child), this function
/// takes an already-running child along with its pipe handles. Output collected
/// before the handoff (`pre_stdout`, `pre_stderr`) is pre-written to the disk
/// file and pre-seeded into the in-memory buffer so no bytes are lost during
/// the transition.
///
/// The caller receives the assigned [`BackgroundCommandId`] and disk log path
/// immediately; the drain task runs concurrently in the background.
pub async fn register_running_process(
    command: &str,
    child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    pre_stdout: Vec<u8>,
    pre_stderr: Vec<u8>,
    ctx: &RunnerContext,
) -> Result<(BackgroundCommandId, PathBuf), AoError> {
    let id = BackgroundCommandId::new();
    let data_root = resolve_data_root()?;
    let output_dir = data_root.join("bash-background");
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(AoError::Io)?;
    let output_path = output_dir.join(format!("{id}.log"));

    let handle = Arc::new(BackgroundCommandHandle {
        id: id.clone(),
        command: command.to_string(),
        started_at: SystemTime::now(),
        output_path: output_path.clone(),
        status: Mutex::new(BackgroundCommandStatus::Running),
        output_buffer: Mutex::new(BoundedOutputBuffer::new(OUTPUT_BUFFER_CAP)),
        cancel: CancellationToken::new(),
        terminated: CancellationToken::new(),
    });

    // Pre-seed the in-memory buffer with pre-threshold output so BashStatus
    // returns content immediately even before the drain task has run.
    {
        let mut buf = handle.output_buffer.lock().unwrap();
        buf.append(&pre_stdout);
        buf.append(&pre_stderr);
    }

    // Spawn drain task: writes the prelude to disk first, then continues from pipes.
    let handle_for_drain = handle.clone();
    let path_for_drain = output_path.clone();
    tokio::spawn(async move {
        drain_with_prelude(
            child,
            stdout,
            stderr,
            pre_stdout,
            pre_stderr,
            path_for_drain,
            handle_for_drain,
        )
        .await;
    });

    ctx.background_commands
        .insert(handle)
        .await
        .map_err(|e| AoError::ValidationError(format!("background command registry: {e}")))?;

    Ok((id, output_path))
}

/// Drain a process that was auto-backgrounded after partial foreground execution.
///
/// Writes the pre-threshold bytes (`pre_stdout`, `pre_stderr`) to the disk file
/// first, then continues reading from the live pipes until EOF, routing all
/// subsequent output to both the disk file and the in-memory buffer.
async fn drain_with_prelude(
    mut child: tokio::process::Child,
    mut stdout: tokio::process::ChildStdout,
    mut stderr: tokio::process::ChildStderr,
    pre_stdout: Vec<u8>,
    pre_stderr: Vec<u8>,
    output_path: PathBuf,
    handle: Arc<BackgroundCommandHandle>,
) {
    use tokio::io::AsyncReadExt;

    let file = match tokio::fs::File::create(&output_path).await {
        Ok(f) => f,
        Err(e) => {
            *handle.status.lock().unwrap() = BackgroundCommandStatus::Failed {
                reason: format!("cannot create output file: {e}"),
            };
            return;
        }
    };
    let shared_file = Arc::new(tokio::sync::Mutex::new(file));

    // Write pre-threshold bytes to disk (buffer already seeded in register_running_process).
    {
        let mut f = shared_file.lock().await;
        let _ = f.write_all(&pre_stdout).await;
        let _ = f.write_all(&pre_stderr).await;
    }

    // Continue draining post-threshold pipe output to disk + buffer.
    let stdout_file = shared_file.clone();
    let stdout_handle = handle.clone();
    let stdout_pump = tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    let mut f = stdout_file.lock().await;
                    let _ = f.write_all(chunk).await;
                    drop(f);
                    stdout_handle.output_buffer.lock().unwrap().append(chunk);
                }
            }
        }
    });

    let stderr_file = shared_file.clone();
    let stderr_handle = handle.clone();
    let stderr_pump = tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    let mut f = stderr_file.lock().await;
                    let _ = f.write_all(chunk).await;
                    drop(f);
                    stderr_handle.output_buffer.lock().unwrap().append(chunk);
                }
            }
        }
    });

    let killed = race_pumps_against_kill(stdout_pump, stderr_pump, &mut child, &handle).await;

    {
        let mut f = shared_file.lock().await;
        let _ = f.flush().await;
    }

    finish(child, killed, handle).await;
}

/// Race the output pumps against BashKill's cancel token, terminating the
/// child's whole process group if the token wins. Returns whether a kill was
/// performed.
///
/// The group — not just the direct child — is signalled because background
/// commands are spawned with `process_group(0)` (see
/// `execute::run_background_raw`). The direct child is the shell, so for a
/// command like `sh -c 'cargo test …'` the real work is a grandchild that
/// signalling the shell alone would orphan. `execute::terminate_child` sends
/// SIGTERM to the group, waits out the grace period, then SIGKILLs.
async fn race_pumps_against_kill(
    stdout_pump: tokio::task::JoinHandle<()>,
    stderr_pump: tokio::task::JoinHandle<()>,
    child: &mut tokio::process::Child,
    handle: &Arc<BackgroundCommandHandle>,
) -> bool {
    let pumps = async {
        let _ = tokio::join!(stdout_pump, stderr_pump);
    };
    tokio::pin!(pumps);

    let killed = tokio::select! {
        _ = &mut pumps => false,
        _ = handle.cancel.cancelled() => {
            execute::terminate_child(child, execute::TERMINATE_GRACE).await;
            true
        }
    };

    // On the kill path the pumps were still mid-read when the select resolved.
    // The process group is gone by now, so the pipes are closed and the pumps
    // return immediately; awaiting them means bytes already sitting in the
    // kernel buffer reach the log instead of being dropped on the floor.
    if killed {
        let _ = tokio::time::timeout(POST_KILL_DRAIN_GRACE, pumps).await;
    }

    killed
}

/// Reap the child and record the handle's terminal status, then signal
/// `handle.terminated` so BashKill can confirm the process is really gone.
async fn finish(
    mut child: tokio::process::Child,
    killed: bool,
    handle: Arc<BackgroundCommandHandle>,
) {
    let code = match child.wait().await {
        Ok(status) => status.code().unwrap_or(0),
        Err(_) => 0,
    };

    // The drain task is the only writer of a terminal status, so `Killed` here
    // means a kill this task actually performed. `code` is deliberately unused
    // on that path: a process destroyed by a signal has no exit code
    // (`ExitStatus::code()` is `None`), and the `unwrap_or(0)` above would
    // otherwise report a clean exit for a process we just SIGKILLed.
    {
        let mut st = handle.status.lock().unwrap();
        if matches!(*st, BackgroundCommandStatus::Running) {
            *st = if killed {
                BackgroundCommandStatus::Killed
            } else {
                BackgroundCommandStatus::Exited { code }
            };
        }
    }

    // Must be last: BashKill wakes on this token and immediately reads
    // `status`, so the status write above has to be visible first.
    handle.terminated.cancel();
}

/// Drain stdout and stderr from the child to the disk log and in-memory buffer,
/// then wait for the child to exit and update the handle's status.
///
/// Runs for the entire lifetime of the background process. Monitors
/// `handle.cancel` so the BashKill tool can signal termination while this
/// task is still draining — when the token fires, the child's process group
/// receives SIGTERM, then SIGKILL after a grace period, and status
/// transitions to `Killed`.
async fn drain(
    mut child: tokio::process::Child,
    mut stdout: tokio::process::ChildStdout,
    mut stderr: tokio::process::ChildStderr,
    output_path: PathBuf,
    handle: Arc<BackgroundCommandHandle>,
) {
    use tokio::io::AsyncReadExt;

    // Open the output file before spawning pump tasks.
    let file = match tokio::fs::File::create(&output_path).await {
        Ok(f) => f,
        Err(e) => {
            *handle.status.lock().unwrap() = BackgroundCommandStatus::Failed {
                reason: format!("cannot create output file: {e}"),
            };
            return;
        }
    };

    // Shared file writer — both pump tasks write through this async mutex.
    let shared_file = Arc::new(tokio::sync::Mutex::new(file));

    let stdout_file = shared_file.clone();
    let stdout_handle = handle.clone();
    let stdout_pump = tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    let mut f = stdout_file.lock().await;
                    let _ = f.write_all(chunk).await;
                    drop(f);
                    stdout_handle.output_buffer.lock().unwrap().append(chunk);
                }
            }
        }
    });

    let stderr_file = shared_file.clone();
    let stderr_handle = handle.clone();
    let stderr_pump = tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    let mut f = stderr_file.lock().await;
                    let _ = f.write_all(chunk).await;
                    drop(f);
                    stderr_handle.output_buffer.lock().unwrap().append(chunk);
                }
            }
        }
    });

    let killed = race_pumps_against_kill(stdout_pump, stderr_pump, &mut child, &handle).await;

    // Flush the output file after output streams are settled.
    {
        let mut f = shared_file.lock().await;
        let _ = f.flush().await;
    }

    finish(child, killed, handle).await;
}
