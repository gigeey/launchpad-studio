use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::handle::{BackgroundAgentId, RunnerEvent};

/// Metadata describing a background agent's spawn context.
///
/// Passed to every [`SidechainPersister::persist_event`] call so each
/// persisted entry carries the information needed for UI sidechain rendering.
pub struct SidechainEventMeta {
    /// The id assigned to the child agent.
    pub background_agent_id: BackgroundAgentId,
    /// The `agent_id` of the parent that spawned this child.
    pub parent_agent_id: String,
    /// The subagent type name (e.g. "Explore").
    pub subagent_type: String,
    /// When the child was spawned.
    pub spawned_at: DateTime<Utc>,
}

/// Receives every [`RunnerEvent`] the child emits and persists it so the UI
/// can render the sidechain as a collapsible card under the parent's Task
/// tool call.
///
/// The production implementation (`FileSidechainPersister` in
/// `ao-engine-tools-runner`) writes JSONL entries to the per-agent transcript
/// path rooted under `LAUNCHPAD_STUDIO_DATA_DIR`. Tests supply a no-op or a
/// temp-dir-backed implementation.
#[async_trait]
pub trait SidechainPersister: Send + Sync {
    /// Persist a single child event alongside its spawn-context metadata.
    async fn persist_event(&self, meta: &SidechainEventMeta, event: &RunnerEvent);
}

/// A [`SidechainPersister`] that discards every event — the default when no
/// persistence is configured.
pub struct NoopSidechainPersister;

#[async_trait]
impl SidechainPersister for NoopSidechainPersister {
    async fn persist_event(&self, _meta: &SidechainEventMeta, _event: &RunnerEvent) {}
}
