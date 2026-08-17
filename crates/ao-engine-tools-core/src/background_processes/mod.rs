//! Background subprocess subsystem — types and registry for live background
//! shell processes spawned by the Bash tool's `run_in_background` mode.
//!
//! # Modules
//!
//! - [`handle`] — [`BackgroundProcessId`] and [`BackgroundProcessHandle`] types.
//! - [`registry`] — [`BackgroundProcessRegistry`] with capacity enforcement
//!   and Arc-based lookup.
//!
//! # Design notes
//!
//! This module is modelled after the sibling `background_agents/` module but
//! targets `tokio::process::Child` handles rather than subagent runners.
//!
//! Polling and stopping background processes are **explicitly out of scope** for
//! this module. A follow-up loop will introduce `BashStatus`/`BashKill` tools
//! or polymorphic `Task*` coverage.

pub mod handle;
pub mod registry;

#[cfg(test)]
mod tests;

pub use handle::{BackgroundProcessHandle, BackgroundProcessId};
pub use registry::{BackgroundProcessRegistry, RegistryError};
