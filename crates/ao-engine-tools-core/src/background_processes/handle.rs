use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Opaque stable identifier for a live background subprocess.
///
/// Wraps a UUID v4 string so the identifier is globally unique, URL-safe,
/// and survives round-trips through JSON.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BackgroundProcessId(String);

impl BackgroundProcessId {
    /// Generate a fresh random id.
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Return the underlying string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for BackgroundProcessId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for BackgroundProcessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for BackgroundProcessId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        uuid::Uuid::parse_str(s)
            .map(|u| Self(u.to_string()))
            .map_err(|e| format!("invalid BackgroundProcessId '{s}': {e}"))
    }
}

impl From<String> for BackgroundProcessId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// A live handle to an in-flight background subprocess.
///
/// Held by [`BackgroundProcessRegistry`](super::registry::BackgroundProcessRegistry)
/// for the lifetime of the subprocess.
///
/// # Stdout / stderr buffering
///
/// Background tasks pump the child's piped streams and append completed output
/// to `stdout` and `stderr`. The buffers grow unbounded in this MVP — a
/// follow-up will add bounded ring-buffer overflow. Do not rely on these
/// buffers for anything latency-sensitive while the child is still running.
///
/// # Cancellation
///
/// Dropping the handle does NOT cancel the subprocess. Cancellation requires
/// an explicit BashKill tool (not yet built — tracked deferral). The `cancel`
/// token is reserved for future use.
///
/// # ctx.cancel propagation
///
/// Background subprocesses do NOT honour the parent `ctx.cancel` token. Once
/// registered, a background process is owned by the registry and survives the
/// parent tool call. This is intentional for fire-and-forget workloads.
pub struct BackgroundProcessHandle {
    pub id: BackgroundProcessId,
    /// The child process. Wrapped in a tokio Mutex so callers can `.kill().await`
    /// from async contexts (e.g. test cleanup or a future BashKill tool).
    pub child: tokio::sync::Mutex<tokio::process::Child>,
    /// Accumulated stdout bytes. Appended-to by the stdout pump task at stream end.
    pub stdout: Mutex<Vec<u8>>,
    /// Accumulated stderr bytes. Appended-to by the stderr pump task at stream end.
    pub stderr: Mutex<Vec<u8>>,
    /// Wall-clock time the subprocess was registered.
    pub started_at: SystemTime,
    /// The command string (post cd-lifting) passed to the shell.
    pub command: String,
    /// Cancellation token reserved for a future BashKill tool.
    pub cancel: CancellationToken,
}

impl fmt::Debug for BackgroundProcessHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackgroundProcessHandle")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("started_at", &self.started_at)
            .finish_non_exhaustive()
    }
}
