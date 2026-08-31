use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

use super::handle::{BackgroundAgentId, RunnerEvent};

/// Errors returned by [`SidechainPersister::persist_event`].
///
/// Mirrors the `thiserror`-per-concern convention already used in this
/// module (see [`super::spawner::SpawnerError`], [`super::registry::RegistryError`]):
/// a local enum with one variant per distinct failure mode, each carrying the
/// path and underlying cause so callers can log or surface a precise message
/// instead of a bare `io::Error`.
#[derive(Debug, Error)]
pub enum SidechainPersistError {
    /// `create_dir_all` failed for the transcript's parent directory.
    #[error("failed to create transcript directory {path:?}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The transcript entry failed to serialize to JSON.
    #[error("failed to serialize sidechain event: {source}")]
    Serialize {
        #[source]
        source: serde_json::Error,
    },

    /// The transcript file could not be opened (created/appended).
    #[error("failed to open transcript file {path:?}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The transcript file was opened but the entry could not be written.
    ///
    /// This is the branch that produces an existing-but-empty transcript
    /// file: `open` succeeded (so the file was created), but `write_all`
    /// failed before any bytes landed.
    #[error("failed to write to transcript file {path:?}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

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
    ///
    /// Returns `Err` when the event did not actually land on disk (directory
    /// creation, serialization, open, or write failure) so callers that
    /// depend on the write having happened — e.g. the spawn-marker write in
    /// `SubagentSpawner::spawn_named_async_core`, which hands the caller a
    /// background-agent id implying its transcript exists — can detect and
    /// react to the failure instead of silently reporting success.
    async fn persist_event(
        &self,
        meta: &SidechainEventMeta,
        event: &RunnerEvent,
    ) -> Result<(), SidechainPersistError>;
}

/// A [`SidechainPersister`] that discards every event — the default when no
/// persistence is configured.
pub struct NoopSidechainPersister;

#[async_trait]
impl SidechainPersister for NoopSidechainPersister {
    async fn persist_event(
        &self,
        _meta: &SidechainEventMeta,
        _event: &RunnerEvent,
    ) -> Result<(), SidechainPersistError> {
        Ok(())
    }
}
