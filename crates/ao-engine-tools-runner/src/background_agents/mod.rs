//! Background agent subsystem — re-exported from `ao-engine-tools-core`,
//! plus the production [`SessionChildRunner`] that wraps [`run_session`] and
//! [`FileSidechainPersister`] that persists child events to JSONL transcripts.
//!
//! Types and the registry live in `ao_engine_tools_core::background_agents`
//! so that `RunnerContext` (also in core) can hold an
//! `Arc<BackgroundAgentRegistry>` without a circular dependency.

pub mod file_sidechain_persister;
pub mod session_child_runner;

pub use ao_engine_tools_core::background_agents::{
    BackgroundAgentHandle, BackgroundAgentId, BackgroundAgentRegistry, BackgroundAgentSnapshot,
    ChildRunner, NoopSidechainPersister, RegistryError, RunnerEvent, SidechainEventMeta,
    SidechainPersister, SpawnerError, SubagentSpawner, TaskFinalReport, TaskFinalStatus,
};
pub use file_sidechain_persister::FileSidechainPersister;
pub use session_child_runner::SessionChildRunner;
