use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::kill_tree::kill_process_tree;
use crate::registry::{RunRecord, RunRegistry, RunStatus};
use crate::supervisor::{
    ManagedRun, ProcessSupervisor, RunExit, SpawnInput, TerminationReason,
};

/// Returns the user's full shell PATH, resolved once at first call.
/// Packaged macOS `.app` bundles inherit a minimal PATH (/usr/bin:/bin:/usr/sbin:/sbin),
/// so tools installed via Homebrew, npm, etc. are not found. This function runs the
/// user's default shell in login mode to capture the real PATH.
pub fn shell_path() -> &'static str {
    static SHELL_PATH: OnceLock<String> = OnceLock::new();
    SHELL_PATH.get_or_init(|| {
        #[cfg(unix)]
        {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());

            // Packaged macOS apps launched from Finder may not have HOME set,
            // which prevents the login shell from sourcing ~/.zprofile / ~/.zshrc.
            // Resolve HOME so the shell can find its rc files.
            let home = std::env::var("HOME").ok().or_else(|| {
                // Fallback: resolve HOME from the password database on macOS/Linux
                #[cfg(unix)]
                {
                    use std::ffi::CStr;
                    let uid = unsafe { libc::getuid() };
                    let pw = unsafe { libc::getpwuid(uid) };
                    if !pw.is_null() {
                        let dir = unsafe { CStr::from_ptr((*pw).pw_dir) };
                        return dir.to_str().ok().map(|s| s.to_string());
                    }
                }
                None
            });

            let mut cmd = std::process::Command::new(&shell);
            cmd.args(["-l", "-c", "echo $PATH"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());

            if let Some(ref h) = home {
                cmd.env("HOME", h);
            }

            let base_path = if let Ok(output) = cmd.output() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    path
                } else {
                    std::env::var("PATH").unwrap_or_default()
                }
            } else {
                std::env::var("PATH").unwrap_or_default()
            };

            // Always merge common tool directories that packaged macOS apps
            // miss — even when the login shell returned a PATH, it may not
            // include dirs added in .zshrc (login shell only sources .zprofile).
            if let Some(h) = home {
                let mut extras = vec![
                    format!("{}/.local/bin", h),
                    format!("{}/.cargo/bin", h),
                    "/usr/local/bin".to_string(),
                    "/opt/homebrew/bin".to_string(),
                ];

                // Resolve the active nvm node version directory.
                // nvm uses an alias file (~/.nvm/alias/default) rather than a
                // symlink at ~/.nvm/versions/node/default, so we read the alias
                // and find the matching installed version.
                let nvm_dir = format!("{}/.nvm", h);
                if let Ok(alias) = std::fs::read_to_string(format!("{}/alias/default", nvm_dir)) {
                    let prefix = alias.trim();
                    if !prefix.is_empty() {
                        let versions_dir = format!("{}/versions/node", nvm_dir);
                        if let Ok(entries) = std::fs::read_dir(&versions_dir) {
                            for entry in entries.flatten() {
                                let name = entry.file_name();
                                let name = name.to_string_lossy();
                                // Match e.g. "v22.21.1" against alias "22"
                                if name.starts_with('v')
                                    && name[1..].starts_with(prefix)
                                    && (name.len() == prefix.len() + 1
                                        || name.as_bytes().get(prefix.len() + 1) == Some(&b'.'))
                                {
                                    let bin = format!("{}/{}/bin", versions_dir, name);
                                    if std::path::Path::new(&bin).is_dir() {
                                        extras.push(bin);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                let mut parts: Vec<&str> = base_path.split(':').collect();
                for extra in &extras {
                    if !parts.contains(&extra.as_str()) {
                        parts.push(extra.as_str());
                    }
                }
                return parts.join(":");
            }

            return base_path;
        }
        // Non-unix fallback: return the current PATH (works fine in dev / non-bundled mode)
        #[cfg(not(unix))]
        { std::env::var("PATH").unwrap_or_default() }
    })
}

/// Default process supervisor that spawns real CLI processes.
pub struct DefaultProcessSupervisor {
    registry: Arc<RunRegistry>,
}

impl DefaultProcessSupervisor {
    pub fn new() -> Self {
        Self {
            registry: Arc::new(RunRegistry::new()),
        }
    }

    pub fn with_registry(registry: Arc<RunRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl ProcessSupervisor for DefaultProcessSupervisor {
    async fn spawn(&self, input: SpawnInput) -> Result<ManagedRun, ao_protocol::error::AoError> {
        let run_id = input.run_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let started_at = Utc::now();

        if input.argv.is_empty() {
            return Err(ao_protocol::error::AoError::Process(
                "argv must not be empty".to_string(),
            ));
        }

        let mut cmd = Command::new(&input.argv[0]);
        if input.argv.len() > 1 {
            cmd.args(&input.argv[1..]);
        }

        // Ensure the full user shell PATH is available (critical for packaged macOS apps)
        cmd.env("PATH", shell_path());

        if let Some(ref cwd) = input.cwd {
            cmd.current_dir(cwd);
        }

        if let Some(ref env_vars) = input.env {
            for (k, v) in env_vars {
                cmd.env(k, v);
            }
        }

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        if input.stdin_data.is_some() {
            cmd.stdin(std::process::Stdio::piped());
        } else {
            cmd.stdin(std::process::Stdio::null());
        }

        // On Unix, create a new session (process group) for clean tree termination.
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        info!(run_id = %run_id, argv = ?input.argv, "Spawning CLI process");
        let mut child = cmd.spawn().map_err(|e| {
            ao_protocol::error::AoError::Process(format!(
                "Failed to spawn '{}': {}",
                input.argv[0], e
            ))
        })?;

        let pid = child.id();

        // Write stdin data if provided, then close stdin.
        if let Some(stdin_data) = input.stdin_data {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(stdin_data.as_bytes()).await;
                let _ = stdin.shutdown().await;
            }
        }

        // Set up stdout/stderr streaming channels.
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = oneshot::channel::<TerminationReason>();

        // Idle-output watchdog state. The reader tasks stamp `last_activity_ms`
        // (millis since `activity_start`) on each successful read; the wait
        // coordinator polls it to detect a CLI that has stopped emitting output
        // without closing stdout (a real Claude CLI hang we hit in the field).
        let activity_start = Instant::now();
        let last_activity_ms = Arc::new(AtomicU64::new(0));

        // Spawn stdout reader task using chunked reads for low-latency streaming.
        let child_stdout = child.stdout.take();
        let last_activity_stdout = last_activity_ms.clone();
        let stdout_task = tokio::spawn(async move {
            if let Some(mut stdout) = child_stdout {
                let mut buf = [0u8; 4096];
                let mut leftover = Vec::new();
                loop {
                    match stdout.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            last_activity_stdout.store(
                                activity_start.elapsed().as_millis() as u64,
                                Ordering::Relaxed,
                            );
                            let data = if leftover.is_empty() {
                                &buf[..n]
                            } else {
                                leftover.extend_from_slice(&buf[..n]);
                                leftover.as_slice()
                            };

                            match std::str::from_utf8(data) {
                                Ok(text) => {
                                    if stdout_tx.send(text.to_owned()).is_err() {
                                        break;
                                    }
                                    leftover.clear();
                                }
                                Err(e) => {
                                    let valid_up_to = e.valid_up_to();
                                    if valid_up_to > 0 {
                                        let text = unsafe {
                                            std::str::from_utf8_unchecked(&data[..valid_up_to])
                                        };
                                        if stdout_tx.send(text.to_owned()).is_err() {
                                            break;
                                        }
                                    }
                                    let tail = data[valid_up_to..].to_vec();
                                    leftover = tail;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        // Spawn stderr reader task.
        let child_stderr = child.stderr.take();
        let last_activity_stderr = last_activity_ms.clone();
        let stderr_task = tokio::spawn(async move {
            if let Some(stderr) = child_stderr {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    last_activity_stderr.store(
                        activity_start.elapsed().as_millis() as u64,
                        Ordering::Relaxed,
                    );
                    if stderr_tx.send(line).is_err() {
                        break;
                    }
                }
            }
        });

        // Register in the run registry.
        self.registry.register(RunRecord {
            run_id: run_id.clone(),
            backend_id: input.backend_id.clone(),
            pid,
            started_at,
            scope_key: input.scope_key.clone(),
            status: RunStatus::Running,
        });

        let registry = self.registry.clone();
        let run_id_clone = run_id.clone();
        let timeout_ms = input.timeout_ms;
        let no_output_timeout_ms = input.no_output_timeout_ms;
        let last_activity_watchdog = last_activity_ms.clone();
        let tools_in_flight = input.tools_in_flight.clone();
        let form_suspended = input.form_suspended.clone();

        // Spawn the coordinator task that manages timeouts, cancellation, and process exit.
        let wait_handle = tokio::spawn(async move {
            let start = std::time::Instant::now();

            let exit = tokio::select! {
                // Branch 1: Cancellation requested
                reason = cancel_rx => {
                    let reason = reason.unwrap_or(TerminationReason::Cancelled);
                    debug!(run_id = %run_id_clone, "Run cancelled");
                    if let Some(p) = pid {
                        kill_process_tree(p, 2000).await;
                    }
                    RunExit {
                        reason,
                        exit_code: None,
                        duration_ms: start.elapsed().as_millis() as u64,
                        timed_out: false,
                        no_output_timed_out: false,
                    }
                }

                // Branch 2: Overall timeout — a deadline LOOP, not a one-shot
                // sleep, so time spent suspended on a synchronous user form
                // (`form_suspended > 0`) is excluded from the budget. Polls in
                // small slices and only advances `consumed_ms` toward
                // `timeout_ms` for slices where the run was NOT suspended; a
                // suspended slice is slept but not counted, which is
                // equivalent to pausing the deadline for the suspension's
                // duration and extending it by exactly that much once it
                // lifts.
                //
                // Deliberately keyed on `form_suspended`, NOT `tools_in_flight`
                // (see Branch 3 below) — a long `Bash` call or `Task` subagent
                // also holds `tools_in_flight > 0` but must keep consuming
                // this budget; only a genuine blocked-on-human suspension
                // pauses it.
                _ = async {
                    match timeout_ms {
                        Some(total_ms) => {
                            const POLL_MS: u64 = 500;
                            let mut consumed_ms: u64 = 0;
                            loop {
                                if consumed_ms >= total_ms {
                                    break;
                                }
                                let is_suspended = form_suspended
                                    .as_ref()
                                    .map(|c| c.load(Ordering::Relaxed) > 0)
                                    .unwrap_or(false);
                                if is_suspended {
                                    tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
                                    continue;
                                }
                                let remaining = total_ms - consumed_ms;
                                let slice = remaining.min(POLL_MS);
                                tokio::time::sleep(Duration::from_millis(slice)).await;
                                consumed_ms += slice;
                            }
                        }
                        None => {
                            // No timeout — wait forever (this future never completes)
                            std::future::pending::<()>().await;
                        }
                    }
                } => {
                    warn!(run_id = %run_id_clone, timeout_ms, "Run timed out");
                    if let Some(p) = pid {
                        kill_process_tree(p, 2000).await;
                    }
                    RunExit {
                        reason: TerminationReason::Timeout,
                        exit_code: None,
                        duration_ms: start.elapsed().as_millis() as u64,
                        timed_out: true,
                        no_output_timed_out: false,
                    }
                }

                // Branch 3: No-output (idle) timeout — CLI stopped emitting
                // bytes for `no_output_timeout_ms` while still holding stdout open.
                _ = async {
                    match no_output_timeout_ms {
                        Some(idle_ms) if idle_ms > 0 => loop {
                            let now_ms = activity_start.elapsed().as_millis() as u64;
                            let last = last_activity_watchdog.load(Ordering::Relaxed);
                            let elapsed = now_ms.saturating_sub(last);
                            if elapsed >= idle_ms {
                                // Pause the watchdog while tool calls are in
                                // flight. A subagent (Task tool) or long-running
                                // Bash keeps the parent CLI's stdout silent for
                                // minutes — that is not a hang. When the tool
                                // result eventually arrives via stdout, the
                                // reader stamps `last_activity` and the
                                // normalizer decrements the counter, so the
                                // watchdog gets a fresh idle window.
                                if tools_in_flight
                                    .as_ref()
                                    .map(|c| c.load(Ordering::Relaxed) > 0)
                                    .unwrap_or(false)
                                {
                                    tokio::time::sleep(Duration::from_millis(500)).await;
                                    continue;
                                }
                                break;
                            }
                            let remaining = idle_ms - elapsed;
                            tokio::time::sleep(Duration::from_millis(remaining.min(500))).await;
                        },
                        _ => std::future::pending::<()>().await,
                    }
                } => {
                    warn!(
                        run_id = %run_id_clone,
                        no_output_timeout_ms,
                        "Run exceeded no-output (idle) timeout"
                    );
                    if let Some(p) = pid {
                        kill_process_tree(p, 2000).await;
                    }
                    RunExit {
                        reason: TerminationReason::NoOutputTimeout,
                        exit_code: None,
                        duration_ms: start.elapsed().as_millis() as u64,
                        timed_out: false,
                        no_output_timed_out: true,
                    }
                }

                // Branch 4: Process exits naturally
                status = child.wait() => {
                    let code = match status {
                        Ok(s) => s.code(),
                        Err(e) => {
                            error!(run_id = %run_id_clone, error = %e, "Error waiting for process");
                            None
                        }
                    };
                    debug!(run_id = %run_id_clone, exit_code = ?code, "Process exited naturally");
                    RunExit {
                        reason: TerminationReason::Natural,
                        exit_code: code,
                        duration_ms: start.elapsed().as_millis() as u64,
                        timed_out: false,
                        no_output_timed_out: false,
                    }
                }
            };

            // Wait for stdout/stderr reader tasks to flush.
            let _ = stdout_task.await;
            let _ = stderr_task.await;

            // Update registry
            let status = match exit.reason {
                TerminationReason::Cancelled => RunStatus::Cancelled,
                _ => RunStatus::Completed,
            };
            registry.update_status(&run_id_clone, status);

            exit
        });

        Ok(ManagedRun {
            run_id,
            pid,
            started_at,
            stdout_rx,
            stderr_rx,
            wait_handle,
            cancel_tx,
        })
    }

    async fn cancel(&self, run_id: &str) -> Result<(), ao_protocol::error::AoError> {
        // Note: actual cancellation is done by the caller via cancel_tx on ManagedRun.
        // This method updates the registry status.
        self.registry.update_status(run_id, RunStatus::Cancelled);
        Ok(())
    }

    fn get_record(&self, run_id: &str) -> Option<RunRecord> {
        self.registry.get(run_id)
    }

    fn list_active(&self) -> Vec<RunRecord> {
        self.registry.list_active()
    }
}
