use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ao_protocol::agent::{CliProviderConfig, OutputFormat};
use ao_protocol::event::AgentEventPayload;
use serde_json::Value;
use tracing::{debug, info, trace};

use crate::helpers;
use crate::traits::OutputNormalizer;

/// Normalizer for Claude CLI output.
/// Handles both JSON (buffered) and StreamJson (line-by-line) output formats.
pub struct ClaudeNormalizer {
    output_format: OutputFormat,
    /// For JSON mode: accumulates the entire JSON blob.
    /// For Stream mode: accumulates text content for TextComplete on finalize.
    buffer: String,
    /// For Stream mode: buffers partial lines until a newline is received.
    line_buffer: String,
    session_id: Option<String>,
    session_id_fields: Vec<String>,
    /// Tracks the name of the current tool_use block whose input is being streamed.
    pending_tool_name: Option<String>,
    /// The current tool_use block's real id (Claude's `toolu_...`), captured
    /// alongside `pending_tool_name` so the completed `ToolCallStarted` (fired
    /// once its accumulated input parses in `content_block_stop`) carries the
    /// same id as the block's earlier no-input announcement.
    pending_tool_use_id: Option<String>,
    /// Accumulates `input_json_delta` fragments for the current tool_use block.
    pending_tool_input_json: String,
    /// Maps a tool_use block's `id` to its `name`, so a later `tool_result`
    /// (which only carries the id) can be reported under the right tool
    /// name. Populated when a `tool_use` content block opens; entries are
    /// never evicted (ids are unique per turn and the map's lifetime is one
    /// process run) — see `process_event`'s `"user"` arm.
    tool_names_by_id: HashMap<String, String>,
    /// Whether text deltas were received for the current assistant turn.
    /// Used to dedup: if deltas were streamed, the subsequent `assistant` event
    /// carries the same text and should be skipped. Resets on each new turn.
    has_text_deltas_for_turn: bool,
    /// Shared counter the supervisor watches to pause the idle-output watchdog
    /// while tool calls (especially subagents) are in flight. Incremented on
    /// each `tool_use` block start, decremented on each matching `tool_result`.
    tools_in_flight: Option<Arc<AtomicUsize>>,
    /// Wall-clock instant the active thinking content block was opened. Set
    /// on `content_block_start[type=thinking]`, consumed and cleared on the
    /// matching `content_block_stop` to populate `ThinkingEnded.elapsed_ms`.
    /// `None` outside of a thinking block — the presence of `Some` is also
    /// how `content_block_stop` decides whether to emit a thinking-end event
    /// vs treat the close as a text/tool block close.
    thinking_started_at: Option<std::time::Instant>,
}

impl ClaudeNormalizer {
    pub fn new(config: &CliProviderConfig) -> Self {
        Self {
            output_format: config.output_format.clone(),
            buffer: String::new(),
            line_buffer: String::new(),
            session_id: None,
            session_id_fields: config.session_id_fields.clone(),
            pending_tool_name: None,
            pending_tool_use_id: None,
            pending_tool_input_json: String::new(),
            tool_names_by_id: HashMap::new(),
            has_text_deltas_for_turn: false,
            tools_in_flight: None,
            thinking_started_at: None,
        }
    }

    /// Process a single line of streaming JSON output.
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

        debug!("[normalizer] stream event type={:?}, line_len={}", event_type, trimmed.len());

        self.process_event(event_type, &value)
    }

    /// Process a parsed JSON event by type. Used both for top-level events
    /// and for unwrapped `stream_event` inner events.
    fn process_event(&mut self, event_type: &str, value: &Value) -> Vec<AgentEventPayload> {
        let mut events = Vec::new();

        match event_type {
            "stream_event" => {
                // Unwrap the inner event and process it recursively.
                // Format: {"type":"stream_event","event":{"type":"content_block_delta",...}}
                if let Some(inner) = value.get("event") {
                    let inner_type = inner
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    debug!("[normalizer] unwrapped stream_event -> inner type={:?}", inner_type);
                    events.extend(self.process_event(inner_type, inner));
                }
            }
            "system" => {
                // Claude CLI init event — extract session_id
                if self.session_id.is_none() {
                    self.session_id = helpers::extract_session_id_from_value(value, &self.session_id_fields);
                }
            }
            "user" => {
                // Claude CLI feeds tool results back to the model as a
                // top-level `{"type":"user","message":{"content":[...]}}`
                // event — this is the ONLY place a completed tool result
                // actually appears in the stream. Each `content` item with
                // `"type":"tool_result"` carries the executed tool's
                // `tool_use_id` (never its name), so the name is recovered
                // from `tool_names_by_id`, populated when the matching
                // `tool_use` block opened.
                if let Some(items) = value
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for item in items {
                        if item.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                            continue;
                        }
                        let tool_use_id = item
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let tool_name = self
                            .tool_names_by_id
                            .get(tool_use_id)
                            .cloned()
                            .unwrap_or_else(|| {
                                tracing::warn!(
                                    target: "ao_normalizer",
                                    "[tool_result] unknown tool_use_id={:?}, falling back to \"unknown\"",
                                    tool_use_id,
                                );
                                "unknown".to_string()
                            });
                        // `content` is either a plain string (the common
                        // case) or an array of content blocks (e.g. a
                        // multi-block result) — handle both shapes.
                        let output = match item.get("content") {
                            Some(Value::String(s)) => Some(s.clone()),
                            Some(Value::Array(_)) => helpers::extract_content_texts(item.get("content")),
                            _ => None,
                        };
                        let is_error = item.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        info!(
                            target: "ao_normalizer",
                            "[tool_result] {} output_len={}",
                            tool_name,
                            output.as_ref().map(|o| o.len()).unwrap_or(0)
                        );
                        if let Some(counter) = &self.tools_in_flight {
                            // Guard against underflow if increment/decrement
                            // ever get out of sync (e.g. a tool_result with no
                            // prior tool_use block).
                            let _ = counter.fetch_update(
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                                |v| if v == 0 { None } else { Some(v - 1) },
                            );
                        }
                        events.push(AgentEventPayload::ToolCallCompleted {
                            tool_name,
                            output,
                            tool_use_id: if tool_use_id.is_empty() {
                                None
                            } else {
                                Some(tool_use_id.to_string())
                            },
                            is_error,
                        });
                    }
                }
            }
            "content_block_delta" => {
                if let Some(delta) = value.get("delta") {
                    let delta_type = delta
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    // Diagnostic: log every delta variant we see so we can tell
                    // whether the upstream API is producing thinking_delta /
                    // signature_delta events at all. The TRACE level is on by
                    // default for this crate; flip to debug if it's too noisy.
                    trace!(
                        target: "ao_normalizer",
                        "[content_block_delta] inner type={:?} text_len={:?} partial_json_len={:?} thinking_len={:?} has_signature={}",
                        delta_type,
                        delta.get("text").and_then(|v| v.as_str()).map(str::len),
                        delta.get("partial_json").and_then(|v| v.as_str()).map(str::len),
                        delta.get("thinking").and_then(|v| v.as_str()).map(str::len),
                        delta.get("signature").is_some(),
                    );
                    if delta_type == "text_delta" {
                        if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                            self.has_text_deltas_for_turn = true;
                            self.buffer.push_str(text);
                            info!(target: "ao_normalizer", "[text_delta] {} chars: {}", text.len(), &text[..text.floor_char_boundary(200)]);
                            events.push(AgentEventPayload::TextDelta {
                                text: text.to_string(),
                            });
                        }
                    } else if delta_type == "input_json_delta" {
                        // Accumulate partial tool input JSON
                        if let Some(partial) = delta.get("partial_json").and_then(|v| v.as_str()) {
                            self.pending_tool_input_json.push_str(partial);
                        }
                    } else if delta_type == "thinking_delta" {
                        // Progressive reasoning text. Anthropic chunks these
                        // at multi-character boundaries (5-100+ chars per
                        // chunk on a typical reasoning prompt), not
                        // character-by-character like `text_delta`. Pass each
                        // chunk through verbatim — the frontend buffers and
                        // re-renders.
                        if let Some(text) = delta.get("thinking").and_then(|v| v.as_str()) {
                            // Make sure consumers that subscribe only to
                            // delta-shaped streams (e.g. providers that don't
                            // emit a start event) still mount a pill. If we
                            // already saw a start, this is a no-op fanout —
                            // the frontend dedups via the active flag.
                            if self.thinking_started_at.is_none() {
                                self.thinking_started_at = Some(std::time::Instant::now());
                                events.push(AgentEventPayload::ThinkingStarted);
                            }
                            events.push(AgentEventPayload::ThinkingDelta {
                                text: text.to_string(),
                            });
                            debug!(target: "ao_normalizer", "[thinking_delta] {} chars", text.len());
                        }
                    } else if delta_type == "signature_delta" {
                        // signature_delta carries the cryptographic signature
                        // that proves the model engaged its reasoning channel
                        // for this turn. There's no user-visible payload —
                        // the signature is consumed by the provider's
                        // multi-turn context handoff, not surfaced in the UI.
                        // We don't emit anything for it, but we log so the
                        // "thinking happened even though no deltas arrived"
                        // case is visible in traces.
                        trace!(target: "ao_normalizer", "[signature_delta] received");
                    } else {
                        // Catch anything new (e.g. summary_text_delta) so we
                        // don't silently drop a future variant.
                        trace!(
                            target: "ao_normalizer",
                            "[content_block_delta] unrecognized inner type={:?} delta={}",
                            delta_type,
                            delta,
                        );
                    }
                }
            }
            "content_block_stop" => {
                trace!(target: "ao_normalizer", "[content_block_stop]");
                // If we were accumulating tool input, emit an updated ToolCallStarted
                let tool_use_id = self.pending_tool_use_id.take();
                if let Some(tool_name) = self.pending_tool_name.take() {
                    if !self.pending_tool_input_json.is_empty() {
                        let input_json = std::mem::take(&mut self.pending_tool_input_json);
                        if let Ok(parsed) = serde_json::from_str::<Value>(&input_json) {
                            info!(target: "ao_normalizer", "[tool_call_complete] {} input={}", tool_name, &input_json[..input_json.floor_char_boundary(500)]);
                            events.push(AgentEventPayload::ToolCallStarted {
                                tool_name,
                                tool_input: Some(parsed),
                                label: None,
                                tool_use_id,
                            });
                        }
                    }
                }
                // If a thinking block was open, this stop closes it. Emit the
                // matching end event with the elapsed wall-clock so the UI
                // can label "Thought for Ns" when the bubble collapses.
                if let Some(started_at) = self.thinking_started_at.take() {
                    let elapsed_ms = started_at.elapsed().as_millis() as u64;
                    info!(target: "ao_normalizer", "[thinking_end] elapsed_ms={}", elapsed_ms);
                    events.push(AgentEventPayload::ThinkingEnded { elapsed_ms });
                }
            }
            "content_block_start" => {
                if let Some(content_block) = value.get("content_block") {
                    let block_type = content_block
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    // Diagnostic: surface every block-open so the thinking
                    // block lifecycle (open -> [deltas] -> close) is visible
                    // even when no deltas arrive between open and close.
                    trace!(
                        target: "ao_normalizer",
                        "[content_block_start] block_type={:?}",
                        block_type,
                    );
                    if block_type == "thinking" {
                        // Mark the thinking block as open and emit the
                        // canonical start event. We always emit here even if
                        // `display = "omitted"` will suppress all subsequent
                        // deltas — the UI relies on this event to mount its
                        // "Thinking…" indicator and (later) collapse it once
                        // the matching `content_block_stop` arrives.
                        if self.thinking_started_at.is_none() {
                            self.thinking_started_at = Some(std::time::Instant::now());
                            info!(target: "ao_normalizer", "[thinking_start]");
                            events.push(AgentEventPayload::ThinkingStarted);
                        }
                    } else if block_type == "tool_use" {
                        let tool_name = content_block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        info!(target: "ao_normalizer", "[tool_call_start] {}", tool_name);
                        // Remember id -> name so the `tool_result` that comes
                        // back later (as a top-level `"user"` event — see
                        // below) can be reported under the real tool name
                        // instead of its opaque id.
                        let block_id = content_block.get("id").and_then(|v| v.as_str()).map(str::to_string);
                        if let Some(id) = &block_id {
                            self.tool_names_by_id.insert(id.clone(), tool_name.clone());
                        }
                        if let Some(counter) = &self.tools_in_flight {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                        // Emit immediately with no input (input arrives via input_json_delta)
                        events.push(AgentEventPayload::ToolCallStarted {
                            tool_name: tool_name.clone(),
                            tool_input: None,
                            label: None,
                            tool_use_id: block_id.clone(),
                        });
                        // Start accumulating input deltas
                        self.pending_tool_name = Some(tool_name);
                        self.pending_tool_use_id = block_id;
                        self.pending_tool_input_json.clear();
                    }
                    // NOTE: `tool_result` blocks never arrive as a
                    // `content_block_start` in real Claude CLI output — tool
                    // results are injected by the CLI as a top-level
                    // `"user"` event (see the `"user"` arm below), never
                    // streamed as an assistant content block. An arm used to
                    // live here matching `block_type == "tool_result"`; it
                    // was dead code that (incorrectly) read the tool's NAME
                    // from the `tool_use_id` field, and since the arm never
                    // actually fired, `ToolCallCompleted` never reached the
                    // event bus for CLI-mode runs — the root cause of tool
                    // results (e.g. `ArtifactWrite`) never resolving inline.
                }
            }
            "assistant" => {
                // Claude CLI stream-json format: {"type":"assistant","message":{...}}
                // When --include-partial-messages is used, text is already streamed via
                // content_block_delta events. Skip duplicate text emission if deltas
                // were received for this turn.
                if let Some(message) = value.get("message") {
                    if let Some(text) = helpers::extract_content_texts(message.get("content")) {
                        if !self.has_text_deltas_for_turn {
                            // No deltas were received for this turn — emit text from the assistant event
                            self.buffer.push_str(&text);
                            events.push(AgentEventPayload::TextDelta {
                                text,
                            });
                        }
                        // Reset for the next turn
                        self.has_text_deltas_for_turn = false;
                    }
                    if self.session_id.is_none() {
                        self.session_id = helpers::extract_session_id_from_value(message, &self.session_id_fields);
                    }
                    if let Some(usage) = helpers::extract_usage(message) {
                        events.push(usage);
                    }
                }
            }
            "result" => {
                // Final result event — extract text, session_id, and usage
                if self.session_id.is_none() {
                    self.session_id = helpers::extract_session_id_from_value(value, &self.session_id_fields);
                }
                if let Some(text) = helpers::collect_text(value) {
                    // Only emit if no text was captured from delta or assistant events.
                    // Unlike the assistant handler, result text spans ALL turns so we
                    // use buffer.is_empty() as the fallback gate.
                    if self.buffer.is_empty() {
                        self.buffer.push_str(&text);
                        events.push(AgentEventPayload::TextDelta {
                            text,
                        });
                    }
                }
                if let Some(usage) = helpers::extract_usage(value) {
                    events.push(usage);
                }
            }
            "thinking" => {
                let subtype = value
                    .get("subtype")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if subtype == "delta" {
                    if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
                        info!(target: "ao_normalizer", "[thinking] {} chars: {}", text.len(), &text[..text.floor_char_boundary(200)]);
                        events.push(AgentEventPayload::ThinkingDelta {
                            text: text.to_string(),
                        });
                    }
                }
                // "completed" subtype is a no-op
            }
            "message_delta" => {
                // May contain usage info
                if let Some(usage) = helpers::extract_usage(value) {
                    if let AgentEventPayload::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_creation_tokens,
                        total_tokens,
                    } = &usage
                    {
                        info!(
                            target: "ao_normalizer",
                            "[usage] input={} output={} cache_read={} cache_creation={} total={}",
                            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, total_tokens
                        );
                    }
                    events.push(usage);
                }
            }
            _ => {}
        }

        events
    }
}

impl OutputNormalizer for ClaudeNormalizer {
    fn process_chunk(&mut self, chunk: &str) -> Vec<AgentEventPayload> {
        match self.output_format {
            OutputFormat::Json => {
                // Buffer everything, parse on finalize
                self.buffer.push_str(chunk);
                vec![]
            }
            OutputFormat::StreamJson | OutputFormat::StreamJsonl => {
                // Append to line buffer, process complete lines
                self.line_buffer.push_str(chunk);
                let mut events = Vec::new();

                debug!(
                    "[normalizer] process_chunk: {} bytes, line_buffer now {} bytes, has_newline={}",
                    chunk.len(),
                    self.line_buffer.len(),
                    self.line_buffer.contains('\n'),
                );

                // Process all complete lines (terminated by newline)
                while let Some(newline_pos) = self.line_buffer.find('\n') {
                    let line: String = self.line_buffer.drain(..=newline_pos).collect();
                    let line_events = self.process_stream_line(&line);
                    debug!(
                        "[normalizer] processed line ({} bytes) -> {} events",
                        line.len(),
                        line_events.len(),
                    );
                    events.extend(line_events);
                }

                debug!("[normalizer] chunk produced {} total events, leftover {} bytes", events.len(), self.line_buffer.len());
                events
            }
            _ => {
                // Text mode — fall through to generic-like behavior
                self.buffer.push_str(chunk);
                vec![AgentEventPayload::TextDelta {
                    text: chunk.to_string(),
                }]
            }
        }
    }

    fn finalize(&mut self, _exit_code: Option<i32>, stderr: &str) -> Vec<AgentEventPayload> {
        info!(
            target: "ao_normalizer",
            "[finalize] exit_code={:?} stderr_len={} buffer_len={}",
            _exit_code, stderr.len(), self.buffer.len()
        );
        let mut events = Vec::new();

        match self.output_format {
            OutputFormat::Json => {
                // Parse the complete JSON buffer
                let buffer = std::mem::take(&mut self.buffer);
                if let Ok(value) = serde_json::from_str::<Value>(&buffer) {
                    if self.session_id.is_none() {
                        self.session_id = helpers::extract_session_id_from_value(&value, &self.session_id_fields);
                    }

                    if let Some(text) = helpers::collect_text(&value) {
                        events.push(AgentEventPayload::TextComplete { text });
                    }

                    if let Some(usage) = helpers::extract_usage(&value) {
                        events.push(usage);
                    }
                }
            }
            OutputFormat::StreamJson | OutputFormat::StreamJsonl => {
                // Flush any remaining partial line in the line buffer
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
