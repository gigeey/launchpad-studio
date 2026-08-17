//! In-use lock for a workspace's data root.
//!
//! The dual-worktree dev setup isolates two `ao-server` processes by port
//! (`AO_PORT`) and, once workspace switching is live, by which data root
//! each activated — but nothing stops a user from activating the SAME data
//! root in two processes at once, which would put two servers writing
//! through one on-disk store. This module is that guard: a small marker
//! file, written into the data root itself, recording which process
//! currently considers it "mine".
//!
//! Lifecycle:
//! - Written once at server startup ([`acquire_startup_lock`]), against
//!   whatever root this process resolved. Best-effort: a write failure
//!   (read-only fs, permissions) is logged and does not fail startup — see
//!   that function's doc for why.
//! - Removed on graceful shutdown ([`release_lock`]). A hard kill (SIGKILL,
//!   power loss) leaves the file behind; that's fine, see "stale" below.
//! - [`require_not_locked`] is the read side, called by
//!   `POST /workspaces/{id}/activate` against the TARGET root being
//!   activated (never this process's own root). A lock naming a pid that
//!   is no longer alive is STALE and is never treated as a conflict — a
//!   crashed process must not permanently brick a workspace.
//!
//! The lock only takes effect for processes started AFTER it exists: it is
//! written once at startup, so activating a workspace does not retroactively
//! protect it until whichever process is holding it open restarts against
//! the lock code in this file.

use std::path::Path;

use serde::{Deserialize, Serialize};

use ao_persistence::paths::DataRoot;
use ao_protocol::error::AoError;

use crate::error::AppError;

/// On-disk shape of `.workspace.lock`. `port` is best-effort context (for a
/// human reading the file, or a richer error message later) — liveness is
/// determined solely by `pid`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockInfo {
    pid: u32,
    port: Option<u16>,
}

/// Write the lock file for `root`, recording this process's pid and (if
/// known) the port it's listening on. Called once at server startup,
/// unconditionally overwriting whatever was there — see the module doc for
/// why an existing lock at this point is never a conflict this function
/// itself needs to resolve (that's [`require_not_locked`]'s job, and it's
/// only ever checked against a DIFFERENT process's activation request).
///
/// Best-effort by design: a write failure only logs a warning. Refusing to
/// boot because a lock file couldn't be written would trade a rare failure
/// mode (two processes sharing one root) for a much more common and worse
/// one (the server won't start on a read-only or permission-restricted
/// filesystem).
pub async fn acquire_startup_lock(root: &Path, port: Option<u16>) {
    let lock = LockInfo {
        pid: std::process::id(),
        port,
    };
    let json = match serde_json::to_string_pretty(&lock) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!("failed to serialize workspace lock: {e}");
            return;
        }
    };

    let path = DataRoot::new(root).workspace_lock_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::warn!(
                path = %path.display(),
                "failed to create data root directory for workspace lock; continuing without the in-use guard: {e}"
            );
            return;
        }
    }
    if let Err(e) = tokio::fs::write(&path, json).await {
        tracing::warn!(
            path = %path.display(),
            "failed to write workspace lock; continuing without the in-use guard: {e}"
        );
    }
}

/// Remove the lock file for `root`, if present. Called on graceful
/// shutdown. Best-effort: a removal failure is logged, not propagated —
/// shutdown must not hang or error out over housekeeping.
pub async fn release_lock(root: &Path) {
    let path = DataRoot::new(root).workspace_lock_path();
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                "failed to remove workspace lock on shutdown: {e}"
            );
        }
    }
}

/// Read and parse `root`'s lock file, if any. `None` covers "no lock file",
/// "unreadable", and "corrupt JSON" alike — all three mean there is nothing
/// to enforce. Unlike [`ao_protocol::workspaces::load_registry`], this file
/// is disposable process-liveness metadata, not user data, so a corrupt
/// copy is simply ignored rather than surfaced as an error.
async fn read_lock(root: &Path) -> Option<LockInfo> {
    let path = DataRoot::new(root).workspace_lock_path();
    let contents = tokio::fs::read_to_string(&path).await.ok()?;
    serde_json::from_str(&contents).ok()
}

/// Check whether `pid` names a live process. Unix-only, matching
/// `ao_process::kill_tree` (the only existing pid-liveness check in this
/// codebase) — on any other target, [`require_not_locked`] treats every
/// lock as stale rather than blocking on a platform where liveness can't
/// actually be verified, favoring "never permanently brick a workspace"
/// over enforcing a guard that would otherwise be unconditionally wrong.
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    ao_process::kill_tree::is_process_alive(pid)
}

#[cfg(not(unix))]
fn is_process_alive(_pid: u32) -> bool {
    false
}

/// `POST /workspaces/{id}/activate` guard: reject activation if `root` (the
/// TARGET workspace being activated, not this process's own root) is
/// currently locked by a different, still-live process.
///
/// A lock naming this same process's pid, or naming a pid that is no
/// longer alive (stale — e.g. left behind by a hard kill), is not a
/// conflict and is silently treated as unlocked.
pub async fn require_not_locked(root: &Path) -> Result<(), AppError> {
    let Some(lock) = read_lock(root).await else {
        return Ok(());
    };
    if lock.pid == std::process::id() {
        return Ok(());
    }
    if is_process_alive(lock.pid) {
        return Err(AppError(AoError::Conflict(format!(
            "That workspace is in use by another running Launchpad Studio (pid {}). Quit it first.",
            lock.pid
        ))));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn require_not_locked_allows_missing_lock() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(require_not_locked(tmp.path()).await.is_ok());
    }

    #[tokio::test]
    async fn require_not_locked_allows_own_pid() {
        let tmp = tempfile::tempdir().unwrap();
        acquire_startup_lock(tmp.path(), Some(3101)).await;
        assert!(require_not_locked(tmp.path()).await.is_ok());
    }

    #[tokio::test]
    async fn acquire_and_release_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = DataRoot::new(tmp.path()).workspace_lock_path();

        acquire_startup_lock(tmp.path(), Some(3101)).await;
        assert!(tokio::fs::try_exists(&lock_path).await.unwrap());

        release_lock(tmp.path()).await;
        assert!(!tokio::fs::try_exists(&lock_path).await.unwrap());
    }

    #[tokio::test]
    async fn release_lock_on_missing_file_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        // No lock was ever acquired here; must not error or panic.
        release_lock(tmp.path()).await;
    }

    /// Live-pid case: a lock naming a still-running (different) process must
    /// be refused with the exact contract message, including the pid.
    #[cfg(unix)]
    #[tokio::test]
    async fn require_not_locked_rejects_live_other_pid_with_exact_message() {
        let tmp = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("failed to spawn helper process for the liveness test");
        let pid = child.id();

        let lock = LockInfo {
            pid,
            port: Some(3101),
        };
        tokio::fs::write(
            DataRoot::new(tmp.path()).workspace_lock_path(),
            serde_json::to_string(&lock).unwrap(),
        )
        .await
        .unwrap();

        let err = require_not_locked(tmp.path()).await.unwrap_err();
        match err.0 {
            AoError::Conflict(msg) => assert_eq!(
                msg,
                format!(
                    "That workspace is in use by another running Launchpad Studio (pid {}). Quit it first.",
                    pid
                )
            ),
            other => panic!("expected AoError::Conflict, got {other:?}"),
        }

        let _ = child.kill();
        let _ = child.wait();
    }

    /// Stale-pid case: a lock naming a pid that has already exited (and been
    /// reaped) must be treated as unlocked, not refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn require_not_locked_allows_stale_dead_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("failed to spawn short-lived helper process for the staleness test");
        let pid = child.id();
        child.wait().expect("failed to reap helper process");

        let lock = LockInfo { pid, port: None };
        tokio::fs::write(
            DataRoot::new(tmp.path()).workspace_lock_path(),
            serde_json::to_string(&lock).unwrap(),
        )
        .await
        .unwrap();

        assert!(require_not_locked(tmp.path()).await.is_ok());
    }
}
