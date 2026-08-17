use ao_protocol::agent::{CliProviderConfig, OutputFormat};
use ao_protocol::event::AgentEventPayload;
use serde_json::Value;

use tracing::info;

use crate::helpers;
use crate::traits::OutputNormalizer;

/// Normalizer for Cursor Agent CLI output.
///
/// The cursor agent (`agent` command) uses a different streaming JSON format
/// than the Claude CLI:
/// - `assistant` events contain the FULL text in `content[]` (no `content_block_delta` events)
/// - `assistant` is emitted TWICE: once with `timestamp_ms` (the streaming event),
///   once without (the echo/duplicate)
///
/// Only assistant events with `timestamp_ms` are treated as streaming events and
/// have their text emitted. Events without `timestamp_ms` are the echo and are
/// skipped for text (usage/session_id are still extracted from both).
pub struct CursorAgentNormalizer {
    output_format: OutputFormat,
    buffer: String,
    line_buffer: String,
    session_id: Option<String>,
    session_id_fields: Vec<String>,
}

impl CursorAgentNormalizer {
    pub fn new(config: &CliProviderConfig) -> Self {
        Self {
            output_format: config.output_format.clone(),
            buffer: String::new(),
            line_buffer: String::new(),
            session_id: None,
            session_id_fields: config.session_id_fields.clone(),
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
        let mut events = Vec::new();

        match event_type {
            "system" => {
                if self.session_id.is_none() {
                    self.session_id = helpers::extract_session_id_from_value(&value, &self.session_id_fields);
                }
            }
            "user" => {
                // User message echo — ignore
            }
            "thinking" => {
                let subtype = value
                    .get("subtype")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if subtype == "delta" {
                    if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
                        events.push(AgentEventPayload::ThinkingDelta {
                            text: text.to_string(),
                        });
                    }
                }
            }
            "assistant" => {
                // Cursor agent emits assistant TWICE:
                //   - WITH timestamp_ms → the streaming event (emit text)
                //   - WITHOUT timestamp_ms → the echo/duplicate (skip text)
                let has_timestamp = value.get("timestamp_ms").is_some();
                if let Some(message) = value.get("message") {
                    if has_timestamp {
                        if let Some(text) = helpers::extract_content_texts(message.get("content")) {
                            info!(target: "ao_normalizer", "[cursor:text] {} chars: {}", text.len(), &text[..text.floor_char_boundary(200)]);
                            self.buffer.push_str(&text);
                            events.push(AgentEventPayload::TextDelta { text });
                        }
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
                if self.session_id.is_none() {
                    self.session_id = helpers::extract_session_id_from_value(&value, &self.session_id_fields);
                }
                if let Some(text) = helpers::collect_text(&value) {
                    // Only emit if we haven't already captured text from assistant events
                    if self.buffer.is_empty() {
                        self.buffer.push_str(&text);
                        events.push(AgentEventPayload::TextDelta { text });
                    }
                }
                if let Some(usage) = helpers::extract_usage(&value) {
                    events.push(usage);
                }
            }
            _ => {}
        }

        events
    }
}

impl OutputNormalizer for CursorAgentNormalizer {
    fn process_chunk(&mut self, chunk: &str) -> Vec<AgentEventPayload> {
        match self.output_format {
            OutputFormat::StreamJson | OutputFormat::StreamJsonl => {
                self.line_buffer.push_str(chunk);
                let mut events = Vec::new();

                while let Some(newline_pos) = self.line_buffer.find('\n') {
                    let line: String = self.line_buffer.drain(..=newline_pos).collect();
                    events.extend(self.process_stream_line(&line));
                }

                events
            }
            _ => {
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
            OutputFormat::StreamJson | OutputFormat::StreamJsonl => {
                // Flush any remaining partial line
                if !self.line_buffer.is_empty() {
                    let remaining = std::mem::take(&mut self.line_buffer);
                    events.extend(self.process_stream_line(&remaining));
                }

                if !self.buffer.is_empty() {
                    events.push(AgentEventPayload::TextComplete {
                        text: std::mem::take(&mut self.buffer),
                    });
                }
            }
            _ => {
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
