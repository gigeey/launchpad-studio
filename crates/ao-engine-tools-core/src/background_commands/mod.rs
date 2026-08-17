//! Background shell command subsystem — registry, handles, and bounded buffers
//! for the Bash tool's `run_in_background` execution path.
//!
//! # Modules
//!
//! - [`id`] — [`BackgroundCommandId`]: short human-friendly ids (`bash_1`, `bash_2`, …).
//! - [`handle`] — [`BackgroundCommandHandle`], [`BackgroundCommandStatus`],
//!   [`BoundedOutputBuffer`]: the per-command state held by the registry.
//! - [`registry`] — [`BackgroundCommandRegistry`]: cap-enforced Arc-based lookup.
//!
//! # Design notes
//!
//! The registry is stored as `Arc<BackgroundCommandRegistry>` on `RunnerContext`
//! so every tool invocation in a session reaches the same instance — the Bash
//! tool inserts on spawn and BashStatus/BashKill read and mutate.
//! Child contexts receive a fresh independent registry (not shared with the
//! parent), matching the pattern used by `background_agents`.
//!
//! A context is only session-scoped if whoever built it made it so. The native
//! runner holds one `RunnerContext` for a whole run and gets this for free; the
//! MCP HTTP route builds a fresh context per JSON-RPC call and must bind the
//! session's registry explicitly via `RunnerContext::with_background_commands`
//! (see `McpAgentSession::background_commands`). Omitting that bind hands every
//! request an empty registry, so an id returned by Bash cannot be found by a
//! later BashStatus/BashKill and the process leaks with no way to stop it.

pub mod handle;
pub mod id;
pub mod registry;

pub use handle::{
    BackgroundCommandHandle, BackgroundCommandStatus, BoundedOutputBuffer, OUTPUT_BUFFER_CAP,
};
pub use id::BackgroundCommandId;
pub use registry::{BackgroundCommandRegistry, BackgroundCommandRegistryError};
