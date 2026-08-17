//! Live event sink for the dogfood CLI binary.
//!
//! Receives `SessionEvent`s from the query loop in real time and prints
//! them to stdout. The format contract is one bracketed prefix per
//! event-shape transition:
//!
//!   * `[assistant] <text>` — emitted on the FIRST `AssistantText` chunk
//!     of an assistant block; subsequent chunks of the same block are
//!     written raw (no prefix) so multi-chunk turns render as a single
//!     prose paragraph instead of one prefix per chunk.
//!   * `[tool_use] <name> <input-json>` — emitted per tool-use block.
//!   * `[tool_result] <tool_use_id> <body>` (or `[tool_error] ...`) —
//!     emitted per tool-result the runner hands back. Bodies longer
//!     than `BODY_TRUNCATE_BYTES` are clipped to keep the terminal usable
//!     when a tool returns a multi-megabyte payload.
//!
//! Stdout is flushed after every event so SIGINT during a streamed text
//! block leaves a clean line on the user's terminal.

use std::io::{self, Write};
use std::sync::Mutex;

use ao_engine_tools_core::ToolOutput;
use ao_engine_tools_runner::query_loop::{SessionEvent, SessionEventSink};

/// Cap on how many bytes of a tool-result body are written to stdout
/// before the trailing `… <truncated>` marker. Picked to fit a typical
/// 80-col terminal with ~30 lines of output — enough to spot-check what
/// a tool returned without flooding the dogfood loop.
const BODY_TRUNCATE_BYTES: usize = 2_000;

/// Stdout sink for the dogfood CLI. Tracks whether we are mid-text-run
/// so non-text events can insert a newline first when the previous
/// chunk did not end with one.
pub struct StdoutSink {
    state: Mutex<RenderState>,
}

#[derive(Default)]
struct RenderState {
    /// True when the most recent event was an `AssistantText` chunk and
    /// the cumulative output for that block has not yet ended on `\n`.
    /// Drives the prefix-vs-no-prefix decision and the trailing-newline
    /// fixup for non-text events.
    in_text_run: bool,
}

impl StdoutSink {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RenderState::default()),
        }
    }

    /// Print a trailing newline if the last event left us in mid-line.
    /// Called by the REPL after `run_session` returns so the next `> `
    /// prompt does not paste onto the tail of the assistant's last
    /// chunk.
    pub fn finish_turn(&self) {
        let mut state = self.state.lock().unwrap();
        if state.in_text_run {
            println!();
            io::stdout().flush().ok();
            state.in_text_run = false;
        }
    }
}

impl Default for StdoutSink {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionEventSink for StdoutSink {
    fn emit(&self, event: SessionEvent) {
        let mut state = self.state.lock().unwrap();
        let mut stdout = io::stdout().lock();

        match event {
            SessionEvent::AssistantText(text) => {
                if !state.in_text_run {
                    let _ = write!(stdout, "[assistant] ");
                }
                let _ = write!(stdout, "{text}");
                state.in_text_run = !text.ends_with('\n');
            }
            SessionEvent::ToolUse { id: _, name, input } => {
                if state.in_text_run {
                    let _ = writeln!(stdout);
                    state.in_text_run = false;
                }
                let _ = writeln!(stdout, "[tool_use] {name} {input}");
            }
            SessionEvent::ToolResult { tool_use_id, output } => {
                if state.in_text_run {
                    let _ = writeln!(stdout);
                    state.in_text_run = false;
                }
                let (prefix, body) = match output {
                    ToolOutput::Text(s) => ("[tool_result]", s),
                    ToolOutput::Structured(v) => {
                        ("[tool_result]", ToolOutput::structured_to_text(&v))
                    }
                    ToolOutput::Error { message, .. } => ("[tool_error]", message),
                    // Multimodal blocks render as their textual summary (binary
                    // payloads are described, not dumped to the terminal).
                    ToolOutput::Blocks(blocks) => {
                        ("[tool_result]", ToolOutput::Blocks(blocks).as_text())
                    }
                };
                let truncated = clip_body(&body);
                let _ = writeln!(stdout, "{prefix} {tool_use_id} {truncated}");
            }
            SessionEvent::Usage(_) => {
                // Usage events are informational; no terminal output in v1.
            }
            SessionEvent::ThinkingStart => {
                if state.in_text_run {
                    let _ = writeln!(stdout);
                    state.in_text_run = false;
                }
                let _ = writeln!(stdout, "[thinking…]");
            }
            SessionEvent::ThinkingDelta { text } => {
                // Reasoning text streams alongside the assistant turn. Keep
                // it compact in the CLI: indent each chunk under the
                // [thinking…] header so it visually attaches to the start
                // marker without competing with subsequent `[assistant]`
                // output.
                if !text.is_empty() {
                    let _ = write!(stdout, "{text}");
                }
            }
            SessionEvent::ThinkingEnd { elapsed_ms } => {
                let _ = writeln!(stdout, "\n[thought for {elapsed_ms}ms]");
                state.in_text_run = false;
            }
            SessionEvent::HiddenUserMessage { content } => {
                if state.in_text_run {
                    let _ = writeln!(stdout);
                    state.in_text_run = false;
                }
                // Mirror the GUI's coalesce hint — the user explicitly
                // wants to see when a synthesized user-role injection
                // (currently: inline skill bodies) is feeding back into
                // the next turn.
                let truncated = clip_body(&content);
                let _ = writeln!(stdout, "[hidden_user] {truncated}");
            }
            SessionEvent::ThinkingBlock { .. } | SessionEvent::RedactedThinkingBlock { .. } => {
                // Reasoning blocks are persistence-only; no terminal output.
            }
            SessionEvent::FormPosted { form_id, .. } => {
                if state.in_text_run {
                    let _ = writeln!(stdout);
                    state.in_text_run = false;
                }
                let _ = writeln!(stdout, "[form_posted] {form_id}");
            }
        }
        let _ = stdout.flush();
    }
}

/// Trim `body` to at most `BODY_TRUNCATE_BYTES`, appending a trailing
/// truncation marker if the body was clipped. Operates on byte
/// boundaries that respect UTF-8 character starts so the marker never
/// lands inside a multi-byte codepoint.
fn clip_body(body: &str) -> String {
    if body.len() <= BODY_TRUNCATE_BYTES {
        return body.to_string();
    }
    // Walk back to the previous char boundary so we don't slice mid-codepoint.
    let mut end = BODY_TRUNCATE_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… <truncated>", &body[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clip_body_short_passes_through() {
        assert_eq!(clip_body("hello"), "hello");
    }

    #[test]
    fn clip_body_long_truncates_with_marker() {
        let long = "a".repeat(BODY_TRUNCATE_BYTES + 50);
        let out = clip_body(&long);
        assert!(out.ends_with("… <truncated>"));
        assert!(out.len() < long.len());
    }

    #[test]
    fn clip_body_respects_char_boundary() {
        // 1 byte "a" repeated up to the cap, then a 4-byte codepoint
        // straddling the cap boundary. The clip must walk back, never
        // produce invalid UTF-8.
        let mut s = "a".repeat(BODY_TRUNCATE_BYTES - 2);
        s.push('🦀'); // 4 bytes — straddles the cap
        let out = clip_body(&s);
        // Round-trip through String::from_utf8 → impossible to assert, but
        // the call already returns a String, so a panic in slicing would
        // have surfaced. Just confirm the marker is present.
        assert!(out.ends_with("… <truncated>"));
    }

    #[test]
    fn sink_in_text_run_resets_on_tool_use() {
        let sink = StdoutSink::new();
        // Manually drive state — emit() writes to global stdout, which
        // we don't capture here, but the state transitions are
        // observable through finish_turn's no-op behavior.
        sink.emit(SessionEvent::AssistantText("partial".into())); // no trailing newline
        assert!(sink.state.lock().unwrap().in_text_run);
        sink.emit(SessionEvent::ToolUse {
            id: "id_1".into(),
            name: "Glob".into(),
            input: json!({"pattern": "**/*.rs"}),
        });
        assert!(!sink.state.lock().unwrap().in_text_run);
    }

    #[test]
    fn sink_text_ending_in_newline_does_not_set_in_text_run() {
        let sink = StdoutSink::new();
        sink.emit(SessionEvent::AssistantText("done.\n".into()));
        assert!(!sink.state.lock().unwrap().in_text_run);
    }
}
