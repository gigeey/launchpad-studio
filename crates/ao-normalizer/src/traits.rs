use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use ao_protocol::event::AgentEventPayload;

/// Trait for normalizing CLI process output into unified AgentEventPayload events.
///
/// Implementations handle different output formats (text, JSON, streaming JSON, etc.)
/// and translate them into the common event format.
pub trait OutputNormalizer: Send {
    /// Process a chunk of stdout output in streaming mode.
    /// Returns zero or more event payloads extracted from the chunk.
    fn process_chunk(&mut self, chunk: &str) -> Vec<AgentEventPayload>;

    /// Finalize processing after the process exits.
    /// Flushes any buffered content and handles stderr/exit code.
    fn finalize(&mut self, exit_code: Option<i32>, stderr: &str) -> Vec<AgentEventPayload>;

    /// Extract session ID from the process output, if available.
    /// Used for session resume support.
    fn extract_session_id(&self) -> Option<String>;

    /// Install a shared counter the normalizer should increment when a tool
    /// call starts and decrement when it completes. The supervisor reads this
    /// counter to pause its idle-output watchdog during long-running tool
    /// calls (especially subagents, which keep the parent CLI's stdout silent
    /// for minutes at a time).
    ///
    /// Implementations:
    /// - `ClaudeNormalizer` increments on `content_block_start[tool_use]` and
    ///   decrements on `tool_result` blocks.
    /// - `CodexNormalizer` increments on `item.started[command_execution]` and
    ///   decrements on the matching `item.completed`.
    /// - `CursorAgentNormalizer` and `GenericNormalizer` do not expose tool
    ///   boundaries in their stream, so they inherit the no-op default. If a
    ///   future CLI under one of those normalizers grows tool semantics, wire
    ///   the counter through to match the pattern above — otherwise sync tools
    ///   that take longer than `no_output_timeout_ms` will spuriously trigger
    ///   the idle watchdog.
    fn set_tools_in_flight_counter(&mut self, _counter: Arc<AtomicUsize>) {}
}
