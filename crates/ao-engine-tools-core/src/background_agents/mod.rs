//! Background agent subsystem — types and registry for live in-flight
//! subagent runners.
//!
//! # Modules
//!
//! - [`handle`] — [`BackgroundAgentId`], [`BackgroundAgentHandle`],
//!   [`RunnerEvent`], and [`TaskFinalReport`] types.
//! - [`registry`] — [`BackgroundAgentRegistry`] with cap enforcement,
//!   snapshot lookup, and cascade cancellation.
//! - [`definition`] — [`SubagentDefinition`] and [`ModelId`] types.
//! - [`subagent_registry`] — [`SubagentRegistry`] static catalog of known
//!   subagent definitions (distinct from the live-handle registry). No
//!   built-in definitions ship with the engine; entries are populated by
//!   feature code (e.g. skill fork-mode constructs its own
//!   [`SubagentDefinition`] and spawns it directly, bypassing the registry).
//! - [`spawner`] — [`SubagentSpawner`] with depth, name-recursion, and
//!   concurrency guards; [`SpawnerError`] with distinct variants.
//! - [`cancel`] — [`cancel_delegation`] and [`CancelOutcome`], the single
//!   not-found/already-cancelled/cancelled cancel primitive shared by the
//!   `DelegateStop` engine tool and the delegate-cancel HTTP route.
//!
//! # Child registry filter mechanism
//!
//! `SubagentSpawner::build_child_context` uses [`Registry::filter_for`] to
//! produce the child's filtered tool registry. `filter_for` constructs a
//! fresh `Registry` containing only `Arc`-cloned references to the tools
//! named in `SubagentDefinition::allowed_tools`. The result is wholly
//! independent of the parent registry — no wrapper type or proxy is involved.
//! Tool names that are absent from the parent registry are silently skipped.

pub mod cancel;
pub mod child_runner;
pub mod definition;
pub mod handle;
pub mod registry;
pub mod sidechain_persister;
pub mod spawner;
pub mod subagent_registry;

pub use cancel::{cancel_delegation, CancelOutcome};
pub use child_runner::ChildRunner;
pub use definition::{ModelId, SubagentDefinition, ALL_TOOLS_WILDCARD};
pub use handle::{
    BackgroundAgentHandle, BackgroundAgentId, RunnerEvent, TaskFinalReport, TaskFinalStatus,
};
pub use registry::{BackgroundAgentRegistry, BackgroundAgentSnapshot, RegistryError};
pub use sidechain_persister::{NoopSidechainPersister, SidechainEventMeta, SidechainPersister};
pub use spawner::{effective_delegate_depth_cap, effective_depth_cap, SpawnerError, SubagentSpawner, DEFAULT_DEPTH_CAP, DELEGATE_DEPTH_CAP};
pub use subagent_registry::{SubagentRegistry, UnknownSubagentType};
