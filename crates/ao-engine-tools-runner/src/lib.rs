//! In-process query loop, permission system, and hook runner for the
//! native engine.
//!
//! This crate is the runtime that drives the IO tool catalog (provided
//! by `ao-engine-tools-io`) and the engine tool catalog through a typed
//! pipeline: validation → pre-hook → permission gate → tool invocation
//! → post-hook → result encoding. The pipeline is exercised end-to-end
//! by [`query_loop::run_session`] against a [`provider::ProviderClient`]
//! trait so the engine can target any model backend without coupling
//! the runner to a specific request/response shape.
//!
//! # Modules
//!
//! - [`validation`] — JSON-schema + tool-level input validation.
//! - [`partition`] — group concurrency-safe tool calls into batches,
//!   preserving original order across batch boundaries.
//! - [`executor`] — bounded concurrent execution of a batch with
//!   cancellation propagation.
//! - [`hooks`] — `settings.json` loader and bash subprocess runner for
//!   pre/post tool hooks.
//! - [`permissions`] — rule grammar parser and decision combinator
//!   that fuses tool-declared decisions, hook outcomes, and the
//!   user-prompt bridge into a final allow / deny verdict.
//! - [`prompt_bridge`] — trait surface for asking the user about
//!   ambiguous tool calls, plus an in-memory denial counter that fences
//!   repeated `Ask` outcomes from async subagents.
//! - [`provider`] — minimal `ProviderClient` seam for talking to a
//!   model backend; concrete provider implementations land in a later
//!   phase.
//! - [`query_loop`] — top-level `run_session` entry point that ties
//!   the pipeline together.
//!
//! # Canonical message boundary
//!
//! The [`message`] module defines the canonical [`Message`] and [`ContentBlock`]
//! types — the post-Loop-A boundary type that every provider crate normalises
//! against. Provider-specific wire formats (Anthropic `content` arrays, OpenAI
//! `messages`, Gemini `contents`) stay inside their respective provider crates;
//! only canonical [`Message`]s cross the runner boundary.
//!
//! # Status
//!
//! Skeleton only at this point. Each module documents the work that
//! lands in its owning story.

pub mod background_agents;
pub mod executor;
pub mod hooks;
pub mod mcp;
pub mod message;
pub mod partition;
pub mod permissions;
pub mod promotion_judge;
pub mod prompt_bridge;
pub mod provider;
pub mod query_loop;
pub mod reflection;
pub mod thread_summary;
pub mod tool_usage_log;
pub mod validation;
pub mod verification;

#[cfg(test)]
pub(crate) mod test_env;

pub use message::{ContentBlock, Message, MessageNormalizer, NormalizerError};

/// Register the full IO and engine tool catalogs into `registry`.
///
/// Intended for bin-only callers (e.g. `ao-engine-tools-cli`) that need a
/// complete tool catalog without depending on the io and engine crates
/// separately. Library consumers that require a partial or custom catalog
/// should call `ao_engine_tools_io::register_all` and
/// `ao_engine_tools_engine::register_all` directly.
///
/// Note: autonomous-only tools (e.g. `Sleep`) are NOT included here. Call
/// [`register_autonomous_tools`] after this function when building a registry
/// for a background or scheduled-agent session.
pub fn register_all(registry: &mut ao_engine_tools_core::Registry) {
    ao_engine_tools_io::register_all(registry);
    ao_engine_tools_engine::register_all(registry);
}

/// Register tools that belong exclusively to autonomous sessions (background
/// agents, scheduled-task runs, tasklist workers). Call this in addition to
/// [`register_all`] when the session kind is [`ao_engine_tools_core::SessionKind::Autonomous`].
pub fn register_autonomous_tools(registry: &mut ao_engine_tools_core::Registry) {
    ao_engine_tools_engine::register_autonomous_tools(registry);
}
