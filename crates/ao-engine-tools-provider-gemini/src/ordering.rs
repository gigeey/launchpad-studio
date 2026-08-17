//! Per-turn positional ordering tracker for Gemini parallel tool calls.
//!
//! Gemini's `parts[]` array carries no stable per-call IDs. This module
//! synthesises IDs in the format `gemini-call-{turn_index}-{part_index}` and
//! provides parsers so the denormalizer can emit `functionResponse` parts in
//! the original parts-array order regardless of executor completion order.
//!
//! The synthesised id is an internal pairing handle — the runner sees it
//! opaquely; only this provider crate generates and consumes it.

use std::collections::HashMap;

/// Records each `functionCall` part's turn position and original function name.
///
/// Call [`record`][Self::record] for every `functionCall` part seen in the
/// translator; call [`parts_for_turn`][Self::parts_for_turn] in the
/// denormalizer to recover the original order and names when building the
/// next-turn `functionResponse` parts.
#[derive(Debug, Default)]
pub struct ToolCallOrderTracker {
    calls: HashMap<(usize, usize), String>,
}

impl ToolCallOrderTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a function call and return its synthesised tool-use id.
    ///
    /// `part_index` is the absolute zero-based index of this part within the
    /// full accumulated `parts[]` array for the turn (text parts counted too).
    pub fn record(&mut self, turn_index: usize, part_index: usize, name: &str) -> String {
        self.calls.insert((turn_index, part_index), name.to_owned());
        format!("gemini-call-{turn_index}-{part_index}")
    }

    /// Look up the original function name for a recorded call.
    pub fn lookup_name(&self, turn_index: usize, part_index: usize) -> Option<&str> {
        self.calls.get(&(turn_index, part_index)).map(|s| s.as_str())
    }

    /// Return all recorded `(part_index, name)` pairs for `turn_index`, sorted
    /// by `part_index`. Used by the denormalizer to detect missing results and
    /// emit `functionResponse` parts in original parts-array order.
    pub fn parts_for_turn(&self, turn_index: usize) -> Vec<(usize, &str)> {
        let mut v: Vec<(usize, &str)> = self
            .calls
            .iter()
            .filter(|((t, _), _)| *t == turn_index)
            .map(|((_, p), n)| (*p, n.as_str()))
            .collect();
        v.sort_by_key(|(p, _)| *p);
        v
    }
}

/// Parse a synthesised tool-use id into `(turn_index, part_index)`.
///
/// Returns `None` for ids that do not match the `gemini-call-{n}-{m}` format.
pub fn parse_tool_use_id(id: &str) -> Option<(usize, usize)> {
    let rest = id.strip_prefix("gemini-call-")?;
    let (turn_str, part_str) = rest.split_once('-')?;
    Some((turn_str.parse().ok()?, part_str.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // (a) record + parse round-trip
    #[test]
    fn record_and_parse_round_trip() {
        let mut tracker = ToolCallOrderTracker::new();
        let id = tracker.record(0, 2, "Read");
        assert_eq!(id, "gemini-call-0-2");
        assert_eq!(parse_tool_use_id(&id), Some((0, 2)));
        assert_eq!(tracker.lookup_name(0, 2), Some("Read"));
    }

    #[test]
    fn record_multiple_calls_per_turn() {
        let mut tracker = ToolCallOrderTracker::new();
        let id0 = tracker.record(0, 0, "Read");
        let id1 = tracker.record(0, 1, "Bash");
        assert_eq!(id0, "gemini-call-0-0");
        assert_eq!(id1, "gemini-call-0-1");

        let parts = tracker.parts_for_turn(0);
        assert_eq!(parts, vec![(0, "Read"), (1, "Bash")]);
    }

    #[test]
    fn parts_for_turn_returns_only_matching_turn() {
        let mut tracker = ToolCallOrderTracker::new();
        tracker.record(0, 0, "Read");
        tracker.record(1, 0, "Bash");

        let turn0 = tracker.parts_for_turn(0);
        assert_eq!(turn0, vec![(0, "Read")]);

        let turn1 = tracker.parts_for_turn(1);
        assert_eq!(turn1, vec![(0, "Bash")]);
    }

    // (b) parse on malformed id returns None
    #[test]
    fn parse_malformed_id_returns_none() {
        assert_eq!(parse_tool_use_id("some-other-id"), None);
        assert_eq!(parse_tool_use_id("gemini-call-abc-def"), None);
        assert_eq!(parse_tool_use_id("gemini-call-0"), None);
        assert_eq!(parse_tool_use_id("gemini-call-"), None);
        assert_eq!(parse_tool_use_id(""), None);
    }

    #[test]
    fn parse_valid_ids() {
        assert_eq!(parse_tool_use_id("gemini-call-0-0"), Some((0, 0)));
        assert_eq!(parse_tool_use_id("gemini-call-3-12"), Some((3, 12)));
        assert_eq!(parse_tool_use_id("gemini-call-0-2"), Some((0, 2)));
    }
}
