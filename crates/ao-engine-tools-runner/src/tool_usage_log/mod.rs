//! JSONL telemetry writer with soft-cap rotation.
//!
//! [`JsonlTelemetryWriter`] implements [`TelemetryWriter`] by appending events
//! as JSON lines to a per-agent file. Events flow through a bounded async
//! channel (capacity 512) to a background tokio task that handles all file
//! I/O. If the channel is full the event is dropped — callers are never
//! blocked, but the drop is counted and surfaced via [`FlushReport`] so it
//! is no longer silent.
//!
//! When the file reaches 10 000 lines it is renamed `tool_usage.jsonl.1`
//! (overwriting any prior backup) and a fresh `tool_usage.jsonl` is created.
//! Atomicity on rotation is not guaranteed for v1: there is a brief window
//! where both files may be partially written if the process crashes mid-rename.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ao_engine_tools_core::{TelemetryWriter, ToolUsageEvent};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub use ao_engine_tools_core::NoopTelemetryWriter;

#[cfg(test)]
mod tests;

const CHANNEL_CAPACITY: usize = 512;
const ROTATION_LINE_THRESHOLD: usize = 10_000;

/// Outcome of draining a [`JsonlTelemetryWriter`] via [`JsonlTelemetryWriter::flush`].
///
/// All-zero-and-not-panicked ([`FlushReport::is_clean`]) means every event
/// emitted before `flush()` was called made it to disk. Any other value
/// means events were lost, and the field(s) that are non-zero/true name the
/// mechanism.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FlushReport {
    /// Number of events the background task successfully wrote to disk.
    pub written: u64,
    /// Non-zero when `emit()`'s `try_send` found the channel full — the
    /// caller-side event never entered the channel at all.
    pub dropped_on_emit: u64,
    /// Non-zero when the background task's `write_event` call returned an
    /// error (e.g. filesystem failure) and the event was logged and dropped.
    pub write_failures: u64,
    /// True when the background task panicked and its `JoinHandle` returned
    /// an `Err`, so any events still in flight when it died were never
    /// written and are not reflected in the other counters.
    pub writer_panicked: bool,
}

impl FlushReport {
    /// Returns true only if no events were dropped on emit, no writes
    /// failed, and the background task did not panic.
    pub fn is_clean(&self) -> bool {
        self.dropped_on_emit == 0 && self.write_failures == 0 && !self.writer_panicked
    }
}

/// A [`TelemetryWriter`] that appends events as JSON lines to `path`, with
/// automatic soft-cap rotation at [`ROTATION_LINE_THRESHOLD`] lines.
///
/// Constructed by calling [`JsonlTelemetryWriter::new`] inside a tokio
/// runtime. Each instance owns one background task and one mpsc sender.
pub struct JsonlTelemetryWriter {
    tx: mpsc::Sender<ToolUsageEvent>,
    handle: JoinHandle<(u64, u64)>,
    dropped_on_emit: Arc<AtomicU64>,
}

impl JsonlTelemetryWriter {
    /// Create a new writer targeting `path` with the default channel capacity.
    ///
    /// Spawns a background tokio task to drain the event channel and write
    /// to the file. The caller must be inside a tokio runtime.
    pub fn new(path: PathBuf) -> Self {
        Self::new_with_capacity(path, CHANNEL_CAPACITY)
    }

    /// Create a new writer with an explicit channel capacity.
    ///
    /// Useful in tests that need to send more events than the default 512-slot
    /// buffer can hold without dropping — all events queued before `flush()` is
    /// called will be written as long as they fit in the channel.
    pub fn new_with_capacity(path: PathBuf, capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        let handle = tokio::spawn(background_writer(rx, path));
        JsonlTelemetryWriter {
            tx,
            handle,
            dropped_on_emit: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Close the channel and wait for the background task to drain and exit.
    ///
    /// Use in tests to synchronize before asserting on file contents. Returns
    /// a [`FlushReport`] describing whether any events were lost and, if so,
    /// by which mechanism — do not discard it silently.
    pub async fn flush(self) -> FlushReport {
        drop(self.tx);
        let dropped_on_emit = self.dropped_on_emit.load(Ordering::Relaxed);
        match self.handle.await {
            Ok((written, write_failures)) => FlushReport {
                written,
                dropped_on_emit,
                write_failures,
                writer_panicked: false,
            },
            Err(_join_err) => FlushReport {
                written: 0,
                dropped_on_emit,
                write_failures: 0,
                writer_panicked: true,
            },
        }
    }
}

impl TelemetryWriter for JsonlTelemetryWriter {
    fn emit(&self, event: ToolUsageEvent) {
        // Non-blocking: still never blocks, still never panics — but now
        // counts drops instead of discarding them invisibly.
        if self.tx.try_send(event).is_err() {
            self.dropped_on_emit.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn background_writer(mut rx: mpsc::Receiver<ToolUsageEvent>, path: PathBuf) -> (u64, u64) {
    // Seed from file on (re)start so a crash + restart doesn't reset the counter.
    // After that the counter is maintained in memory — O(1) per event.
    let mut line_count = count_lines_on_disk(&path).await;
    let mut written: u64 = 0;
    let mut write_failures: u64 = 0;

    while let Some(event) = rx.recv().await {
        match write_event(&path, &event, &mut line_count).await {
            Ok(()) => written += 1,
            Err(e) => {
                write_failures += 1;
                tracing::warn!("tool_usage_log: failed to write event: {e}");
            }
        }
    }

    (written, write_failures)
}

async fn write_event(
    path: &Path,
    event: &ToolUsageEvent,
    line_count: &mut usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    if *line_count >= ROTATION_LINE_THRESHOLD {
        let mut rotated: OsString = path.as_os_str().to_owned();
        rotated.push(".1");
        tokio::fs::rename(path, PathBuf::from(rotated)).await?;
        *line_count = 0;
    }

    let json = serde_json::to_string(event)?;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(format!("{json}\n").as_bytes()).await?;
    *line_count += 1;

    Ok(())
}

/// Count newlines in the file on disk. Used only at task startup to seed the
/// in-memory counter; subsequent writes maintain it without re-reading.
async fn count_lines_on_disk(path: &Path) -> usize {
    match tokio::fs::read(path).await {
        Ok(bytes) => bytes.iter().filter(|&&b| b == b'\n').count(),
        Err(_) => 0,
    }
}
