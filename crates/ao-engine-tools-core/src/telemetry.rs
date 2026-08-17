use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Kind of telemetry event emitted for tool usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Selected,
    Invoked,
}

/// A single tool usage event emitted by the runner or ToolSearch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUsageEvent {
    pub agent_id: String,
    pub session_id: String,
    pub tool_name: String,
    pub kind: EventKind,
    pub ts: DateTime<Utc>,
    pub metadata: serde_json::Value,
}

/// Sink for tool usage telemetry events.
///
/// Implementations must never block the caller. The concrete
/// `JsonlTelemetryWriter` uses a bounded async channel and silently drops
/// events when the channel is full.
pub trait TelemetryWriter: Send + Sync {
    fn emit(&self, event: ToolUsageEvent);
    /// Returns `true` only for the built-in no-op sink.
    ///
    /// Session startup uses this to decide whether to install a real
    /// `JsonlTelemetryWriter`. Callers that supply a custom writer (e.g.
    /// test spies) should keep the default `false` so their writer is not
    /// replaced.
    fn is_noop(&self) -> bool {
        false
    }
}

/// No-op [`TelemetryWriter`] that silently discards every event.
///
/// Used as the default in [`RunnerContext`] and in tests that do not need
/// to inspect telemetry output.
pub struct NoopTelemetryWriter;

impl TelemetryWriter for NoopTelemetryWriter {
    fn emit(&self, _event: ToolUsageEvent) {}
    fn is_noop(&self) -> bool {
        true
    }
}
