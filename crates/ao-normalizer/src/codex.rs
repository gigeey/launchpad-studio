use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ao_protocol::agent::{CliProviderConfig, OutputFormat};
use ao_protocol::event::AgentEventPayload;
use serde_json::Value;
use tracing::info;

use crate::traits::OutputNormalizer;

/// Normalizer for OpenAI Codex CLI (`codex exec`) output.
/// Handles both Text (plain passthrough) and StreamJsonl (JSONL events) output formats.
///
/// Codex JSONL format uses a different event structure than Claude/Cursor:
/// - `thread.started` with `thread_id` (session ID)
/// - `turn.started` / `turn.completed` lifecycle
/// - `item.started` / `item.completed` for reasoning, commands, and messages
/// - Text arrives all-at-once in `item.completed` (not incrementally streamed)
pub struct CodexNormalizer {
    output_format: OutputFormat,
    /// Accumulates text content for TextComplete on finalize.
    buffer: String,
    /// Buffers partial lines until a newline is received (StreamJsonl mode).
    line_buffer: String,
    session_id: Option<String>,
    /// Shared counter the supervisor watches to pause the idle-output watchdog
    /// while a tool call is in flight. Codex emits `item.started` /
    /// `item.completed` pairs for `command_execution` items; the CLI's stdout
    /// stays silent between those two events while the shell command actually
    /// runs, which is exactly the window the watchdog would otherwise mistake
    /// for a hang.
    tools_in_flight: Option<Arc<AtomicUsize>>,
}

impl CodexNormalizer {
    pub fn new(config: &CliProviderConfig) -> Self {
        Self {
            output_format: config.output_format.clone(),
            buffer: String::new(),
            line_buffer: String::new(),
            session_id: None,
            tools_in_flight: None,
        }
    }

    /// Process a single line of JSONL output from Codex.
    fn process_stream_line(&mut self, line: &str) -> Vec<AgentEventPayload> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return vec![];
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => return vec![],
        };

        let event_type = value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let mut events = Vec::new();

        match event_type {
            "thread.started" => {
                // Extract thread_id as session ID
                if let Some(thread_id) = value.get("thread_id").and_then(|v| v.as_str()) {
                    self.session_id = Some(thread_id.to_string());
                }
            }
            "item.completed" => {
                if let Some(item) = value.get("item") {
                    let item_type = item
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    match item_type {
                        "agent_message" => {
                            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                info!(target: "ao_normalizer", "[codex:text] {} chars: {}", text.len(), &text[..text.floor_char_boundary(200)]);
                                self.buffer.push_str(text);
                                events.push(AgentEventPayload::TextDelta {
                                    text: text.to_string(),
                                });
                            }
                        }
                        "reasoning" => {
                            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    events.push(AgentEventPayload::ThinkingDelta {
                                        text: text.to_string(),
                                    });
                                }
                            }
                        }
                        "command_execution" => {
                            let command = item
                                .get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let output = item
                                .get("aggregated_output")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            info!(target: "ao_normalizer", "[codex:tool_result] {}", command);
                            // Pair with the increment in `item.started`. Guard
                            // against underflow if a completed event ever
                            // arrives without a matching start (defensive — the
                            // codex JSONL stream is sequential in practice).
                            if let Some(counter) = &self.tools_in_flight {
                                let _ = counter.fetch_update(
                                    Ordering::Relaxed,
                                    Ordering::Relaxed,
                                    |v| if v == 0 { None } else { Some(v - 1) },
                                );
                            }
                            events.push(AgentEventPayload::ToolCallCompleted {
                                tool_name: command,
                                output,
                                // Codex JSONL items don't carry a stable
                                // call-correlation id today (see module doc).
                                tool_use_id: None,
                                is_error: false,
                            });
                        }
                        _ => {}
                    }
                }
            }
            "item.started" => {
                if let Some(item) = value.get("item") {
                    let item_type = item
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    if item_type == "command_execution" {
                        let command = item
                            .get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        // Hold the watchdog open across the silent shell-exec
                        // window — paired with the decrement in `item.completed`.
                        if let Some(counter) = &self.tools_in_flight {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                        events.push(AgentEventPayload::ToolCallStarted {
                            tool_name: command,
                            tool_input: None,
                            label: None,
                            tool_use_id: None,
                        });
                    }
                }
            }
            "turn.completed" => {
                // Codex emits `cached_input_tokens` (the codex binary's name
                // for cache hits) — map onto our canonical `cache_read_tokens`.
                // No cache-write surface in codex output today, so
                // `cache_creation_tokens` reports 0.
                if let Some(usage) = value.get("usage") {
                    let input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let output_tokens = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_read_tokens = usage
                        .get("cached_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    events.push(AgentEventPayload::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_creation_tokens: 0,
                        total_tokens: input_tokens + output_tokens + cache_read_tokens,
                    });
                }
            }
            "turn.failed" => {
                let message = value
                    .get("message")
                    .or_else(|| value.get("error"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Turn failed")
                    .to_string();
                events.push(AgentEventPayload::Error {
                    message,
                    recoverable: false,
                });
            }
            "error" => {
                let message = value
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown error")
                    .to_string();
                events.push(AgentEventPayload::Error {
                    message,
                    recoverable: false,
                });
            }
            // turn.started, item.started (non-command), and everything else → no-op
            _ => {}
        }

        events
    }
}

impl OutputNormalizer for CodexNormalizer {
    fn process_chunk(&mut self, chunk: &str) -> Vec<AgentEventPayload> {
        match self.output_format {
            OutputFormat::StreamJsonl => {
                // Append to line buffer, process complete lines
                self.line_buffer.push_str(chunk);
                let mut events = Vec::new();

                while let Some(newline_pos) = self.line_buffer.find('\n') {
                    let line: String = self.line_buffer.drain(..=newline_pos).collect();
                    events.extend(self.process_stream_line(&line));
                }

                events
            }
            _ => {
                // Text mode — pass-through like GenericNormalizer
                self.buffer.push_str(chunk);
                vec![AgentEventPayload::TextDelta {
                    text: chunk.to_string(),
                }]
            }
        }
    }

    fn finalize(&mut self, _exit_code: Option<i32>, stderr: &str) -> Vec<AgentEventPayload> {
        let mut events = Vec::new();

        match self.output_format {
            OutputFormat::StreamJsonl => {
                // Flush any remaining partial line
                if !self.line_buffer.is_empty() {
                    let remaining = std::mem::take(&mut self.line_buffer);
                    events.extend(self.process_stream_line(&remaining));
                }

                // Emit TextComplete with accumulated text
                if !self.buffer.is_empty() {
                    events.push(AgentEventPayload::TextComplete {
                        text: std::mem::take(&mut self.buffer),
                    });
                }
            }
            _ => {
                // Text mode fallback
                if !self.buffer.is_empty() {
                    events.push(AgentEventPayload::TextComplete {
                        text: std::mem::take(&mut self.buffer),
                    });
                }
            }
        }

        if !stderr.is_empty() {
            events.push(AgentEventPayload::Error {
                message: stderr.to_string(),
                recoverable: false,
            });
        }

        events
    }

    fn extract_session_id(&self) -> Option<String> {
        self.session_id.clone()
    }

    fn set_tools_in_flight_counter(&mut self, counter: Arc<AtomicUsize>) {
        self.tools_in_flight = Some(counter);
    }
}
