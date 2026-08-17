//! JSONL telemetry writer with soft-cap rotation.
//!
//! [`JsonlTelemetryWriter`] implements [`TelemetryWriter`] by appending events
//! as JSON lines to a per-agent file. Events flow through a bounded async
//! channel (capacity 512) to a background tokio task that handles all file
//! I/O. If the channel is full the event is silently dropped — callers are
//! never blocked.
//!
//! When the file reaches 10 000 lines it is renamed `tool_usage.jsonl.1`
//! (overwriting any prior backup) and a fresh `tool_usage.jsonl` is created.
//! Atomicity on rotation is not guaranteed for v1: there is a brief window
//! where both files may be partially written if the process crashes mid-rename.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ao_engine_tools_core::{TelemetryWriter, ToolUsageEvent};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub use ao_engine_tools_core::NoopTelemetryWriter;

#[cfg(test)]
mod tests;

const CHANNEL_CAPACITY: usize = 512;
const ROTATION_LINE_THRESHOLD: usize = 10_000;

/// A [`TelemetryWriter`] that appends events as JSON lines to `path`, with
/// automatic soft-cap rotation at [`ROTATION_LINE_THRESHOLD`] lines.
///
/// Constructed by calling [`JsonlTelemetryWriter::new`] inside a tokio
/// runtime. Each instance owns one background task and one mpsc sender.
pub struct JsonlTelemetryWriter {
    tx: mpsc::Sender<ToolUsageEvent>,
    handle: JoinHandle<()>,
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
        JsonlTelemetryWriter { tx, handle }
    }

    /// Close the channel and wait for the background task to drain and exit.
    ///
    /// Use in tests to synchronize before asserting on file contents.
    pub async fn flush(self) {
        drop(self.tx);
        let _ = self.handle.await;
    }
}

impl TelemetryWriter for JsonlTelemetryWriter {
    fn emit(&self, event: ToolUsageEvent) {
        // Non-blocking: silently discard if the channel is full.
        let _ = self.tx.try_send(event);
    }
}

async fn background_writer(mut rx: mpsc::Receiver<ToolUsageEvent>, path: PathBuf) {
    // Seed from file on (re)start so a crash + restart doesn't reset the counter.
    // After that the counter is maintained in memory — O(1) per event.
    let mut line_count = count_lines_on_disk(&path).await;

    while let Some(event) = rx.recv().await {
        match write_event(&path, &event, &mut line_count).await {
            Ok(()) => {}
            Err(e) => tracing::warn!("tool_usage_log: failed to write event: {e}"),
        }
    }
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
