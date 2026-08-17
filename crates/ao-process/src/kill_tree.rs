use tracing::{debug, warn};

/// Check whether a process with the given PID is still alive.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    // Signal 0 doesn't send a signal but checks if the process exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Kill a process tree by sending SIGTERM to the process group,
/// waiting a grace period, then sending SIGKILL if still alive.
#[cfg(unix)]
pub async fn kill_process_tree(pid: u32, grace_ms: u64) {
    let pgid = -(pid as libc::pid_t);

    debug!(pid, "Sending SIGTERM to process group");
    unsafe {
        libc::kill(pgid, libc::SIGTERM);
    }

    // Wait for the grace period
    tokio::time::sleep(std::time::Duration::from_millis(grace_ms)).await;

    // If still alive, escalate to SIGKILL
    if is_process_alive(pid) {
        warn!(pid, "Process still alive after grace period, sending SIGKILL");
        unsafe {
            libc::kill(pgid, libc::SIGKILL);
        }
    } else {
        debug!(pid, "Process terminated gracefully after SIGTERM");
    }
}
