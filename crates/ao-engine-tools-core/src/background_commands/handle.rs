use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;

use super::id::BackgroundCommandId;

/// Byte cap for the in-memory output ring buffer: 1 MiB.
///
/// When combined stdout+stderr output for a background command exceeds this
/// limit the oldest bytes are dropped from the front of the buffer so a
/// chatty process cannot exhaust heap. Disk output is unaffected — the log
/// file grows without bound until the child exits.
pub const OUTPUT_BUFFER_CAP: usize = 1024 * 1024;

/// Bounded in-memory ring buffer for combined background command output.
///
/// Retains the most-recent `capacity` bytes. When new output would push the
/// total past the cap, oldest bytes are dropped from the front and
/// `dropped_bytes` is incremented so callers can detect overflow without
/// inspecting raw byte counts.
pub struct BoundedOutputBuffer {
    data: Vec<u8>,
    capacity: usize,
    /// Total bytes dropped from the front since this buffer was created.
    pub dropped_bytes: u64,
}

impl BoundedOutputBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: Vec::new(),
            capacity,
            dropped_bytes: 0,
        }
    }

    /// Append `chunk`, dropping the oldest bytes from the front if needed.
    pub fn append(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        // If the chunk alone exceeds capacity, keep only the tail of the chunk.
        let chunk = if chunk.len() > self.capacity {
            let drop = chunk.len() - self.capacity;
            self.dropped_bytes += drop as u64;
            &chunk[drop..]
        } else {
            chunk
        };
        // Drop bytes from the front to make room.
        let total = self.data.len() + chunk.len();
        if total > self.capacity {
            let to_drop = (total - self.capacity).min(self.data.len());
            self.dropped_bytes += to_drop as u64;
            self.data.drain(..to_drop);
        }
        self.data.extend_from_slice(chunk);
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Lifecycle state of a background shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundCommandStatus {
    Running,
    Exited { code: i32 },
    Killed,
    Failed { reason: String },
}

/// A live handle for a registered background shell command.
///
/// Created by the Bash tool's background spawn path and held in
/// [`BackgroundCommandRegistry`](super::registry::BackgroundCommandRegistry)
/// for the session lifetime. The drain task (spawned at registration time)
/// updates `status` and `output_buffer` concurrently as the child produces
/// output and eventually exits.
///
/// # Output routing
///
/// All combined stdout+stderr bytes are streamed in arrival order to both:
/// - `output_path` — the on-disk log file; persists until the session ends
///   or the file is explicitly removed. The model can read it with the Read tool.
/// - `output_buffer` — a bounded in-memory ring buffer capped at
///   `OUTPUT_BUFFER_CAP` bytes. The BashStatus tool returns a snippet of this
///   buffer for quick inspection without a file read.
///
/// # Termination
///
/// `cancel` and `terminated` are a request/acknowledge pair travelling in
/// opposite directions. BashKill fires `cancel`; the drain task races it
/// against the output pumps and, when it wins, terminates the child's whole
/// process group (SIGTERM, grace period, SIGKILL). The drain task fires
/// `terminated` once the child has been reaped and `status` has reached its
/// terminal value — on natural exit as well as on kill — which is what lets
/// BashKill report a confirmed kill rather than a signal it never saw land.
pub struct BackgroundCommandHandle {
    pub id: BackgroundCommandId,
    pub command: String,
    pub started_at: SystemTime,
    /// Absolute path to the file receiving all stdout+stderr output.
    pub output_path: PathBuf,
    /// Current lifecycle status, updated by the drain task on exit.
    pub status: Mutex<BackgroundCommandStatus>,
    /// Bounded ring buffer of recent combined output.
    pub output_buffer: Mutex<BoundedOutputBuffer>,
    /// Fired by BashKill to request termination; watched by the drain task.
    pub cancel: CancellationToken,
    /// Fired by the drain task once the child has been reaped and `status`
    /// holds its terminal value. Awaited by BashKill so a reported kill means
    /// the OS process is actually gone, not merely that a token was signalled.
    pub terminated: CancellationToken,
}

impl fmt::Debug for BackgroundCommandHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackgroundCommandHandle")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("started_at", &self.started_at)
            .field("output_path", &self.output_path)
            .finish_non_exhaustive()
    }
}
