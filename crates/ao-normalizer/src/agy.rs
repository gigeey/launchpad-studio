use ao_protocol::agent::{CliProviderConfig, OutputFormat};
use ao_protocol::event::AgentEventPayload;
use serde_json::Value;

use crate::helpers;
use crate::traits::OutputNormalizer;

/// Normalizer for the Google Antigravity CLI (`agy`).
///
/// `agy` supports two output shapes, selected by the `output_format` on the
/// provider config:
///
/// - `StreamJson`/`StreamJsonl` (the default template as of v1.1.7): agy is
///   invoked with `--output-format stream-json` and emits one JSON object per
///   line (NDJSON) — an `init` event carrying the conversation id, zero or
///   more `step_update` events (`step_type == "agent_response"` carries
///   user-visible assistant text in `text_delta`; `step_type == "tool"`
///   carries a tool call's ACTIVE/DONE/ERROR lifecycle — see
///   [`process_tool_step`](AgyNormalizer::process_tool_step)), and a terminal
///   `result` event carrying the full response text again plus the
///   authoritative token usage. See
///   [`process_stream_line`](AgyNormalizer::process_stream_line) for the full
///   event dispatch.
/// - `Text` (legacy/back-compat): `agy --print` emits plain text to stdout
///   with no event structure at all. `process_chunk` forwards each chunk as
///   a `TextDelta` while buffering it, and `finalize` emits one authoritative
///   `TextComplete` for the buffered text.
///
/// The `Json` (whole-blob) parsing path in `finalize` (see
/// [`AgyResult`]/[`AgyUsage`]) is kept as a defensive fallback in case a
/// future `agy` release ever emits the same NDJSON `result` shape as a single
/// buffered blob instead of streaming it — it is not the primary path for any
/// `agy` release shipped today.
pub struct AgyNormalizer {
    output_format: OutputFormat,
    /// For `Json` mode: accumulates the entire JSON blob.
    /// For `Text` mode: accumulates raw text for one final `TextComplete`.
    /// For `StreamJson`/`StreamJsonl` mode: accumulates the assistant text
    /// already emitted via `TextDelta` (from `agent_response` step_updates,
    /// or the `result.response` fallback — see `process_stream_line`), so
    /// `finalize` can seal it with one consolidating `TextComplete` without
    /// re-adding content that was already streamed.
    buffer: String,
    /// For `StreamJson`/`StreamJsonl` mode: buffers a partial line until a
    /// terminating `\n` is received.
    line_buffer: String,
    session_id: Option<String>,
    session_id_fields: Vec<String>,
}

impl AgyNormalizer {
    pub fn new(config: &CliProviderConfig) -> Self {
        Self {
            output_format: config.output_format.clone(),
            buffer: String::new(),
            line_buffer: String::new(),
            session_id: None,
            session_id_fields: config.session_id_fields.clone(),
        }
    }

    /// Process a single line of `agy --output-format stream-json` NDJSON
    /// output. Dispatches on the top-level `"event"` field:
    ///
    /// - `"init"`: captures the session id from the top-level
    ///   `conversation_id` field (sibling to `"event"`/`"init"`).
    /// - `"step_update"`: `step_update.step_type == "agent_response"` carries
    ///   assistant text (`step_update.text_delta`), emitted as a `TextDelta`
    ///   and appended to `buffer`. `step_type == "tool"` carries a tool
    ///   call's ACTIVE/DONE/ERROR lifecycle, dispatched to
    ///   [`process_tool_step`](Self::process_tool_step). Every other
    ///   `step_type` (`user_input`, `checkpoint`, `unknown`, or any value not
    ///   yet defined by agy) emits nothing — this is intentionally permissive
    ///   so a future agy release adding a new step_type degrades to "no
    ///   events" rather than an error.
    /// - `"result"`: the terminal event. Captures the session id from
    ///   `result.conversation_id` (nested here, unlike `init`'s top-level
    ///   field). agy re-sends the full assistant text as `result.response` —
    ///   emitting it unconditionally would double the message on top of the
    ///   already-streamed `agent_response` deltas, so it's only used as a
    ///   fallback when nothing was streamed (`buffer` still empty), exactly
    ///   like `ClaudeNormalizer`'s `result` handling. `result.usage` is the
    ///   run's authoritative total and is the *only* source of `Usage`
    ///   events for agy — `step_update.usage` (per-step) is never forwarded,
    ///   which would double-count against this total.
    /// - Any other/unknown top-level `"event"` value, or a line that isn't
    ///   valid JSON: silently skipped. Never returns an error.
    fn process_stream_line(&mut self, line: &str) -> Vec<AgentEventPayload> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return vec![];
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => return vec![],
        };

        let event = value.get("event").and_then(|v| v.as_str()).unwrap_or("");
        let mut events = Vec::new();

        match event {
            "init" => {
                if self.session_id.is_none() {
                    self.session_id =
                        helpers::extract_session_id_from_value(&value, &self.session_id_fields);
                }
            }
            "step_update" => {
                if let Some(step_update) = value.get("step_update") {
                    let step_type = step_update
                        .get("step_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if step_type == "agent_response" {
                        if let Some(text) = step_update.get("text_delta").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                self.buffer.push_str(text);
                                events.push(AgentEventPayload::TextDelta {
                                    text: text.to_string(),
                                });
                            }
                        }
                    } else if step_type == "tool" {
                        events.extend(self.process_tool_step(step_update));
                    }
                    // Every other step_type (user_input, checkpoint, unknown,
                    // or anything not yet defined) carries no user-visible
                    // text — intentionally emit nothing.
                }
            }
            "result" => {
                if let Some(result) = value.get("result") {
                    if self.session_id.is_none() {
                        self.session_id = helpers::extract_session_id_from_value(
                            result,
                            &self.session_id_fields,
                        );
                    }

                    if let Some(parsed) = parse_agy_result(result) {
                        if parsed.status == "SUCCESS" {
                            // Anti-double-render: only fall back to the full
                            // `response` text if nothing was streamed via
                            // agent_response deltas above.
                            if self.buffer.is_empty() && !parsed.response.is_empty() {
                                self.buffer.push_str(&parsed.response);
                                events.push(AgentEventPayload::TextDelta {
                                    text: parsed.response.clone(),
                                });
                            }
                        } else {
                            let message = if parsed.response.is_empty() {
                                parsed.status.clone()
                            } else {
                                parsed.response.clone()
                            };
                            events.push(AgentEventPayload::Error {
                                message,
                                recoverable: false,
                            });
                        }

                        // Authoritative total for the whole run — the only
                        // Usage event agy's NDJSON path emits.
                        if let Some(usage) = parsed.usage {
                            events.push(AgentEventPayload::Usage {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                                cache_read_tokens: usage.cache_read_tokens,
                                cache_creation_tokens: 0,
                                total_tokens: usage.total_tokens,
                            });
                        }
                    }
                }
            }
            _ => {
                // Unknown top-level event — tolerate and skip, never error.
            }
        }

        events
    }

    /// Handle one `step_update` whose `step_type == "tool"`. A tool call's
    /// `ACTIVE` (started), `DONE` (completed), and `ERROR` (failed) states
    /// share a single `step_index`, which is stable across that call's whole
    /// lifecycle — used here as a synthetic correlation id since agy has no
    /// API-native tool-call id (unlike Claude's `toolu_...` block id). Unlike
    /// `ClaudeNormalizer`, no id -> name map needs to be stashed across calls:
    /// `tool_name` is present at the top level of `step_update` on every
    /// state, so it's read fresh from each event.
    fn process_tool_step(&mut self, step_update: &Value) -> Vec<AgentEventPayload> {
        let mut events = Vec::new();

        let step_index = match step_update.get("step_index").and_then(Value::as_u64) {
            Some(i) => i,
            None => return events,
        };
        let tool_call_id = format!("agy-tool-{step_index}");
        let tool_name = step_update
            .get("tool_name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let state = step_update.get("state").and_then(|v| v.as_str()).unwrap_or("");
        let tool_info = step_update.get("tool_info");

        match state {
            "ACTIVE" => {
                let tool_input = tool_info.and_then(|info| info.get("parameters")).cloned();
                events.push(AgentEventPayload::ToolCallStarted {
                    tool_name,
                    tool_input,
                    label: None,
                    tool_use_id: Some(tool_call_id),
                });
            }
            "DONE" => {
                // `tool_info.output` is absent for tools whose DONE event
                // only echoes back the input `parameters` with no result
                // body (a known agy limitation) — that's not an error, just
                // an empty output.
                let output = tool_info
                    .and_then(|info| info.get("output"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                events.push(AgentEventPayload::ToolCallCompleted {
                    tool_name,
                    output,
                    tool_use_id: Some(tool_call_id),
                    is_error: false,
                });
            }
            "ERROR" => {
                let output = tool_info
                    .and_then(|info| info.get("error"))
                    .and_then(|err| err.get("message"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                events.push(AgentEventPayload::ToolCallCompleted {
                    tool_name,
                    output,
                    tool_use_id: Some(tool_call_id),
                    is_error: true,
                });
            }
            _ => {
                // Unknown/future state value — tolerate and skip.
            }
        }

        events
    }
}

/// One `usage` object from an `agy` JSON result blob — either the whole-blob
/// `Json` mode's terminal object, or the `result.usage` object nested inside
/// an NDJSON `result` event.
///
/// Kept separate from [`crate::helpers::extract_usage`]: agy names its cache
/// field `cache_read_tokens` (not Anthropic's `cache_read_input_tokens`) and
/// reports its own precomputed `total_tokens` which, per the confirmed
/// sample, equals `input_tokens + output_tokens` rather than the
/// input+output+cache_read sum the shared helper derives. Routing agy's usage
/// through that helper would silently produce the wrong total, so it gets its
/// own small extractor instead.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgyUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Reasoning-token count. Parsed but not currently forwarded onto
    /// `AgentEventPayload::Usage` — see the doc comment where it's dropped in
    /// `AgyNormalizer::finalize`/`process_stream_line` for why.
    pub thinking_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
}

/// Parsed shape of one `agy` JSON terminal result object — either the
/// whole-blob `Json` mode's top-level object, or the `result` field nested
/// inside an NDJSON `result` event.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgyResult {
    pub response: String,
    pub status: String,
    pub usage: Option<AgyUsage>,
}

/// Parse a single `agy` JSON result object (whole-blob `Json` mode, or the
/// inner `result` object of an NDJSON `result` event).
///
/// Returns `None` only when `response` is missing or not a string — every
/// other field degrades to a default rather than failing the whole parse,
/// since `status`/`usage` are each independently useful even if the other is
/// absent or malformed.
pub(crate) fn parse_agy_result(value: &Value) -> Option<AgyResult> {
    let response = value.get("response").and_then(|v| v.as_str())?.to_string();
    let status = value
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let usage = value.get("usage").map(|usage| AgyUsage {
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        thinking_tokens: usage.get("thinking_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_read_tokens: usage.get("cache_read_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        total_tokens: usage.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
    });
    Some(AgyResult { response, status, usage })
}

impl OutputNormalizer for AgyNormalizer {
    fn process_chunk(&mut self, chunk: &str) -> Vec<AgentEventPayload> {
        match self.output_format {
            OutputFormat::Json => {
                // Buffer everything, parse on finalize — the whole-blob mode
                // never streams.
                self.buffer.push_str(chunk);
                vec![]
            }
            OutputFormat::StreamJson | OutputFormat::StreamJsonl => {
                // Append to the line buffer and drain complete ('\n'-terminated)
                // lines; a trailing partial line is held until the next chunk
                // (or flushed in `finalize`).
                self.line_buffer.push_str(chunk);
                let mut events = Vec::new();
                while let Some(newline_pos) = self.line_buffer.find('\n') {
                    let line: String = self.line_buffer.drain(..=newline_pos).collect();
                    events.extend(self.process_stream_line(&line));
                }
                events
            }
            _ => {
                // Text mode (and defensive fallback for any other format) —
                // the real `agy --print` binary emits plain text.
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
            OutputFormat::Json => {
                let buffer = std::mem::take(&mut self.buffer);
                match serde_json::from_str::<Value>(&buffer) {
                    Ok(value) => {
                        if self.session_id.is_none() {
                            self.session_id = helpers::extract_session_id_from_value(
                                &value,
                                &self.session_id_fields,
                            );
                        }

                        if let Some(parsed) = parse_agy_result(&value) {
                            if parsed.status.eq_ignore_ascii_case("success") {
                                events.push(AgentEventPayload::TextComplete {
                                    text: parsed.response,
                                });
                            } else {
                                let message = if parsed.response.is_empty() {
                                    format!("agy run ended with status {:?}", parsed.status)
                                } else {
                                    parsed.response
                                };
                                events.push(AgentEventPayload::Error {
                                    message,
                                    recoverable: false,
                                });
                            }

                            // v1 limitation: `thinking_tokens` has no field on the
                            // shared Usage event (only input/output/cache_read/
                            // cache_creation/total) — dropped rather than invented
                            // a field, same as the NDJSON path.
                            if let Some(usage) = parsed.usage {
                                events.push(AgentEventPayload::Usage {
                                    input_tokens: usage.input_tokens,
                                    output_tokens: usage.output_tokens,
                                    cache_read_tokens: usage.cache_read_tokens,
                                    cache_creation_tokens: 0,
                                    total_tokens: usage.total_tokens,
                                });
                            }
                        }
                    }
                    Err(_) => {
                        // Not a JSON blob after all — degrade to plain text.
                        if !buffer.is_empty() {
                            events.push(AgentEventPayload::TextComplete { text: buffer });
                        }
                    }
                }
            }
            OutputFormat::StreamJson | OutputFormat::StreamJsonl => {
                // Flush any remaining partial line in the line buffer.
                if !self.line_buffer.is_empty() {
                    let remaining = std::mem::take(&mut self.line_buffer);
                    events.extend(self.process_stream_line(&remaining));
                }

                // Seal the streamed text with one consolidating TextComplete —
                // the content was already delivered live via TextDelta, this
                // just marks the message done (mirrors ClaudeNormalizer).
                if !self.buffer.is_empty() {
                    events.push(AgentEventPayload::TextComplete {
                        text: std::mem::take(&mut self.buffer),
                    });
                }
            }
            _ => {
                // Text mode fallback.
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
}
