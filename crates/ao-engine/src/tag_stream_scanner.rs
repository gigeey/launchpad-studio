//! Streaming tag scanner used by AgentRunner to strip recognized action tags
//! from the outbound TextDelta stream and emit lifecycle events at tag
//! boundaries.
//!
//! The scanner is a four-state machine over the delta stream (see `State`):
//! `Normal` (passing text through), `InHeader` (buffering a possible opening
//! tag), `InBody` (inside a recognized tag, suppressing its content), and
//! `InBodyHeader` (buffering a possible closing tag). Text is only withheld
//! from the caller while a partial tag is buffered, so a sequence that turns
//! out not to be a tag is emitted intact rather than swallowed.
//!
//! Which tags are recognized, and what each maps to, is decided by
//! [`resolve_tag`] — that function is the registry.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use uuid::Uuid;

/// Extracts key="value" attribute pairs from a tag's attribute string.
static ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(\w+)="([^"]*)""#).expect("attr regex should compile")
});

/// Lifecycle event produced by the scanner as recognized tags stream in.
#[derive(Debug, Clone, PartialEq)]
pub enum ScannerEvent {
    ActionStarted {
        action_id: String,
        kind: String,
        summary: String,
    },
    ActionCompleted {
        action_id: String,
    },
}

#[derive(Debug)]
enum State {
    /// Default: bytes pass through to display.
    Normal,
    /// After a `<` in Normal context: buffering header chars until we reach
    /// `>` to decide if this is a recognized tag.
    InHeader { buf: String },
    /// Inside the body of a recognized open tag. Bytes are suppressed until
    /// the matching `</name>` close tag.
    InBody { name: String, action_id: String },
    /// Inside a body and seen a `<`: buffering until `>` to check for the
    /// matching close tag. Bytes still suppressed.
    InBodyHeader {
        name: String,
        action_id: String,
        buf: String,
    },
}

pub struct TagStreamScanner {
    state: State,
    /// Currently-open recognized tags, in open order. Used so `drain()` can
    /// emit `ActionCompleted` for any entries still open at end-of-stream.
    open_stack: Vec<(String, String)>,
}

impl Default for TagStreamScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl TagStreamScanner {
    pub fn new() -> Self {
        Self {
            state: State::Normal,
            open_stack: Vec::new(),
        }
    }

    /// Feed a chunk of streamed text. Returns the stripped display text and
    /// any lifecycle events that fired while processing the chunk.
    pub fn feed(&mut self, delta: &str) -> (String, Vec<ScannerEvent>) {
        let mut out = String::new();
        let mut events = Vec::new();
        for ch in delta.chars() {
            self.step(ch, &mut out, &mut events);
        }
        (out, events)
    }

    /// Flush any pending state at end-of-stream. A partial non-tag buffer in
    /// `Normal` context flushes as literal text. Any still-open recognized
    /// tags emit `ActionCompleted` so the UI doesn't render orphan chips.
    pub fn drain(&mut self) -> (String, Vec<ScannerEvent>) {
        let mut out = String::new();
        let mut events = Vec::new();

        match std::mem::replace(&mut self.state, State::Normal) {
            State::Normal => {}
            State::InHeader { buf } => {
                out.push_str(&buf);
            }
            State::InBody { .. } | State::InBodyHeader { .. } => {
                // Body content was intentionally suppressed; nothing to flush.
            }
        }
        while let Some((_, action_id)) = self.open_stack.pop() {
            events.push(ScannerEvent::ActionCompleted { action_id });
        }
        (out, events)
    }

    fn step(&mut self, ch: char, out: &mut String, events: &mut Vec<ScannerEvent>) {
        let state = std::mem::replace(&mut self.state, State::Normal);
        let new_state = match state {
            State::Normal => {
                if ch == '<' {
                    State::InHeader {
                        buf: "<".to_string(),
                    }
                } else {
                    out.push(ch);
                    State::Normal
                }
            }
            State::InHeader { mut buf } => {
                // Right after `<`, bail fast if the next char can't start a
                // tag name or close marker. This keeps stray '<' in prose
                // (e.g. `a < b`) from swallowing arbitrary downstream text.
                if buf.len() == 1
                    && ch != '/'
                    && !ch.is_ascii_alphanumeric()
                    && ch != '_'
                {
                    out.push_str(&buf);
                    out.push(ch);
                    State::Normal
                } else if ch == '<' {
                    // Previous buffer wasn't a tag (no '>' seen). Flush and
                    // restart with the new '<'.
                    out.push_str(&buf);
                    State::InHeader {
                        buf: "<".to_string(),
                    }
                } else if ch == '>' {
                    buf.push('>');
                    self.handle_resolved_header(&buf, out, events)
                } else {
                    buf.push(ch);
                    State::InHeader { buf }
                }
            }
            State::InBody { name, action_id } => {
                if ch == '<' {
                    State::InBodyHeader {
                        name,
                        action_id,
                        buf: "<".to_string(),
                    }
                } else {
                    State::InBody { name, action_id }
                }
            }
            State::InBodyHeader {
                name,
                action_id,
                mut buf,
            } => {
                if ch == '<' {
                    // Another '<' while buffering; reset the buffer and keep suppressing.
                    State::InBodyHeader {
                        name,
                        action_id,
                        buf: "<".to_string(),
                    }
                } else if ch == '>' {
                    buf.push('>');
                    if is_matching_close_tag(&buf, &name) {
                        events.push(ScannerEvent::ActionCompleted {
                            action_id: action_id.clone(),
                        });
                        pop_stack_by_action_id(&mut self.open_stack, &action_id);
                        State::Normal
                    } else {
                        // Some other tag-shaped content inside the body; keep suppressing.
                        State::InBody { name, action_id }
                    }
                } else {
                    buf.push(ch);
                    State::InBodyHeader {
                        name,
                        action_id,
                        buf,
                    }
                }
            }
        };
        self.state = new_state;
    }

    fn handle_resolved_header(
        &mut self,
        raw: &str,
        out: &mut String,
        events: &mut Vec<ScannerEvent>,
    ) -> State {
        // `raw` looks like "<...>".
        let inner = &raw[1..raw.len() - 1];
        if inner.starts_with('/') {
            // Stray close tag in Normal context — emit literally.
            out.push_str(raw);
            return State::Normal;
        }

        let (inner, self_closing) = if let Some(stripped) = inner.strip_suffix('/') {
            (stripped.trim_end(), true)
        } else {
            (inner, false)
        };

        let mut parts = inner.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("").to_string();
        if name.is_empty() {
            out.push_str(raw);
            return State::Normal;
        }
        let attrs_str = parts.next().unwrap_or("").trim();
        let attrs = parse_attrs(attrs_str);

        match resolve_tag(&name, &attrs) {
            Some((kind, summary)) => {
                let action_id = Uuid::new_v4().to_string();
                events.push(ScannerEvent::ActionStarted {
                    action_id: action_id.clone(),
                    kind,
                    summary,
                });
                // Self-closing recognized tags defer ActionCompleted to
                // drain() so the chip stays painted for the lifetime of the
                // turn. Emitting Started/Completed microseconds apart lands
                // both events in the same React tick and the chip never
                // appears.
                self.open_stack.push((name.clone(), action_id.clone()));
                if self_closing {
                    State::Normal
                } else {
                    State::InBody { name, action_id }
                }
            }
            None => {
                out.push_str(raw);
                State::Normal
            }
        }
    }
}

fn pop_stack_by_action_id(stack: &mut Vec<(String, String)>, action_id: &str) {
    if let Some(pos) = stack.iter().rposition(|(_, a)| a == action_id) {
        stack.remove(pos);
    }
}

fn is_matching_close_tag(raw: &str, name: &str) -> bool {
    let inner = match raw.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
        Some(s) => s,
        None => return false,
    };
    let inner = inner.trim();
    match inner.strip_prefix('/') {
        Some(rest) => rest.trim() == name,
        None => false,
    }
}

fn parse_attrs(s: &str) -> HashMap<String, String> {
    ATTR_RE
        .captures_iter(s)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

/// Resolves a tag name (plus parsed attrs) to the `(kind, summary)` the
/// frontend chip should display. Returns `None` for unrecognized tags.
fn resolve_tag(name: &str, attrs: &HashMap<String, String>) -> Option<(String, String)> {
    match name {
        "task" => {
            let action = attrs.get("action").map(String::as_str).unwrap_or("");
            let (kind, summary) = match action {
                "complete" => ("task_complete", "Completing task…"),
                "fail" => ("task_fail", "Failing task…"),
                "request_clarification" => {
                    ("task_request_clarification", "Requesting clarification…")
                }
                _ => ("task", "Updating task…"),
            };
            Some((kind.into(), summary.into()))
        }
        "tasklist" => {
            let action = attrs.get("action").map(String::as_str).unwrap_or("");
            let (kind, summary) = match action {
                "create" => ("tasklist_create", "Creating tasklist…"),
                _ => ("tasklist", "Managing tasklist…"),
            };
            Some((kind.into(), summary.into()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrecognized_tag_is_passed_through() {
        let mut s = TagStreamScanner::new();
        let (text, events) = s.feed("before <unknown attr=\"x\">body</unknown> after");
        assert_eq!(text, "before <unknown attr=\"x\">body</unknown> after");
        assert!(events.is_empty());
    }

    #[test]
    fn stray_lt_that_is_not_a_tag_passes_through() {
        let mut s = TagStreamScanner::new();
        let (text, events) = s.feed("if a < b then c > d");
        assert_eq!(text, "if a < b then c > d");
        assert!(events.is_empty());
    }

    #[test]
    fn partial_non_tag_header_drains_as_literal() {
        let mut s = TagStreamScanner::new();
        let (text, events) = s.feed("tail <maybe_a_tag");
        assert_eq!(text, "tail ");
        assert!(events.is_empty());
        let (drained_text, drained_events) = s.drain();
        assert_eq!(drained_text, "<maybe_a_tag");
        assert!(drained_events.is_empty());
    }

}
