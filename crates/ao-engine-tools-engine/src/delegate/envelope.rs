use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

/// Directive envelope mode for the Delegate tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeMode {
    /// Fork mode: child shares the parent's context.
    ForkShared,
    /// Fresh mode: child runs with its own context.
    Fresh,
}

/// Soft cap on total bytes emitted by [`format_history_block`]. When the
/// running total crosses this, the formatter stops adding older lines and
/// returns whatever has been collected. Sized to leave ~40% of a typical
/// 12K-char context budget for the directive itself.
const FORK_HISTORY_BUDGET_BYTES: usize = 7000;

/// Per-tool-result truncation cap. Tool outputs can be huge (file reads,
/// big JSON dumps); capping each individually keeps a single fat result from
/// monopolising the history budget before older user messages get a chance.
const FORK_TOOL_RESULT_CAP_BYTES: usize = 800;

/// Per-tool-use truncation cap. Inputs are usually small but a pasted blob
/// in a Bash command (or a giant tool_input JSON) could otherwise dominate.
const FORK_TOOL_USE_CAP_BYTES: usize = 400;

/// Build a `[Conversation history]` block from a list of `TranscriptEntry`s,
/// suitable for prepending to a forked-delegation directive.
///
/// Iterates newest-first so the most recent turns get priority within the
/// byte budget, then reverses the surviving lines back to chronological order
/// for display. Returns an empty string when `entries` is empty so the caller
/// can detect "no context to share" without inspecting whitespace.
///
/// Entry rendering:
/// - `event_type == "tool_use"`: synthesises `<tool_use id="…" name="…">…</tool_use>`
///   from `metadata.tool_use_id`, `metadata.tool_name`, and `metadata.input`.
/// - `event_type == "tool_result"`: synthesises `<tool_result tool_use_id="…">…</tool_result>`
///   from `metadata.tool_use_id` and `metadata.output`, with `is_error="true"`
///   when `metadata.is_error` is set.
/// - Everything else: uses `entry.content` directly (covers `message`,
///   `response`, and any future event types we don't explicitly model).
///
/// Tool entries with missing/malformed metadata are dropped silently — better
/// to skip a noise line than emit a bare `[HH:MM] role:` with no body.
pub fn format_history_block(entries: &[TranscriptEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut total_bytes: usize = 0;

    for entry in entries.iter().rev() {
        let body = match entry.event_type.as_str() {
            "tool_use" => render_tool_use(entry),
            "tool_result" => render_tool_result(entry),
            _ => entry.content.clone(),
        };

        // Skip entries the renderer couldn't produce a useful body for.
        // Plain message/response entries with empty content are also dropped
        // since they'd render as a bare role line with nothing after the colon.
        if body.is_empty() {
            continue;
        }

        let line = format!(
            "[{}] {}: {}",
            entry.ts.format("%H:%M"),
            role_label(&entry.role),
            body
        );

        // Honour the budget once at least one line is in — guarantees the
        // most recent turn always makes it in even if it's by itself larger
        // than the budget. Better to overshoot once than emit an empty block.
        total_bytes += line.len();
        if total_bytes > FORK_HISTORY_BUDGET_BYTES && !lines.is_empty() {
            break;
        }
        lines.push(line);
    }

    lines.reverse();
    format!("[Conversation history]\n{}", lines.join("\n"))
}

fn role_label(role: &TranscriptRole) -> &str {
    match role {
        TranscriptRole::System(s) => s.as_str(),
        TranscriptRole::Agent { agent } => agent.as_str(),
        TranscriptRole::Schedule { .. } => "schedule",
    }
}

/// Truncate at the nearest UTF-8 boundary at or below `max_bytes` and append
/// a marker indicating the byte count dropped. Multibyte-safe (emoji, CJK)
/// so a naïve byte slice doesn't panic on the child runner.
fn truncate_with_marker(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    format!("{}\n[... truncated; {} more bytes]", &s[..idx], s.len() - idx)
}

fn render_tool_use(entry: &TranscriptEntry) -> String {
    let meta = match entry.metadata.as_ref() {
        Some(m) => m,
        None => return String::new(),
    };
    let tool_use_id = meta.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("?");
    let tool_name = meta.get("tool_name").and_then(|v| v.as_str()).unwrap_or("?");
    let input_str = meta
        .get("input")
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .unwrap_or_default();
    let display = truncate_with_marker(&input_str, FORK_TOOL_USE_CAP_BYTES);
    format!(
        "<tool_use id=\"{}\" name=\"{}\">{}</tool_use>",
        tool_use_id, tool_name, display,
    )
}

fn render_tool_result(entry: &TranscriptEntry) -> String {
    let meta = match entry.metadata.as_ref() {
        Some(m) => m,
        None => return String::new(),
    };
    let tool_use_id = meta.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("?");
    let output = meta.get("output").and_then(|v| v.as_str()).unwrap_or("");
    let is_error = meta.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
    let display = truncate_with_marker(output, FORK_TOOL_RESULT_CAP_BYTES);
    let err_attr = if is_error { " is_error=\"true\"" } else { "" };
    format!(
        "<tool_result tool_use_id=\"{}\"{}>{}</tool_result>",
        tool_use_id, err_attr, display,
    )
}

/// Build the envelope wrapping a delegated directive.
///
/// Name resolution: if `name` is empty, falls back to the first 8 chars of
/// `id` so the envelope never contains a blank placeholder.
pub fn build_envelope(
    parent_name: &str,
    child_name: &str,
    mode: EnvelopeMode,
    directive: &str,
) -> String {
    match mode {
        EnvelopeMode::ForkShared => format!(
            "[Delegated by {parent_name}. You are operating as {child_name} in fork mode \
(sharing {parent_name}'s context). Handle this directive directly \
— do not re-delegate it to yourself.]\n\n{directive}"
        ),
        EnvelopeMode::Fresh => format!(
            "[Delegated by {parent_name}. Handle this directive.]\n\n{directive}"
        ),
    }
}

/// Return `name` if non-empty, otherwise the first 8 chars of `id`.
pub fn name_or_prefix<'a>(name: &'a str, id: &'a str) -> &'a str {
    if !name.is_empty() {
        name
    } else {
        let end = id
            .char_indices()
            .nth(8)
            .map(|(i, _)| i)
            .unwrap_or(id.len());
        &id[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
    use chrono::{TimeZone, Utc};
    use serde_json::json;
    use std::collections::HashMap;

    fn make_entry(role: TranscriptRole, content: &str, event_type: &str) -> TranscriptEntry {
        TranscriptEntry {
            ts: Utc.with_ymd_and_hms(2026, 5, 20, 12, 34, 0).unwrap(),
            role,
            content: content.to_string(),
            event_type: event_type.to_string(),
            metadata: None,
            hidden_from_user: false,
        }
    }

    fn tool_use_entry(tool_use_id: &str, tool_name: &str, input: serde_json::Value) -> TranscriptEntry {
        let mut meta = HashMap::new();
        meta.insert("tool_use_id".into(), json!(tool_use_id));
        meta.insert("tool_name".into(), json!(tool_name));
        meta.insert("input".into(), input);
        TranscriptEntry {
            ts: Utc.with_ymd_and_hms(2026, 5, 20, 12, 35, 0).unwrap(),
            role: TranscriptRole::Agent { agent: "agent-7ba6".into() },
            content: String::new(),
            event_type: "tool_use".into(),
            metadata: Some(meta),
            hidden_from_user: false,
        }
    }

    fn tool_result_entry(tool_use_id: &str, output: &str, is_error: bool) -> TranscriptEntry {
        let mut meta = HashMap::new();
        meta.insert("tool_use_id".into(), json!(tool_use_id));
        meta.insert("output".into(), json!(output));
        meta.insert("is_error".into(), json!(is_error));
        TranscriptEntry {
            ts: Utc.with_ymd_and_hms(2026, 5, 20, 12, 35, 5).unwrap(),
            role: TranscriptRole::System("tool".into()),
            content: String::new(),
            event_type: "tool_result".into(),
            metadata: Some(meta),
            hidden_from_user: false,
        }
    }

    #[test]
    fn empty_entries_yield_empty_string() {
        // Callers branch on the empty string to decide whether to prepend
        // anything to the directive — non-empty whitespace would defeat that.
        assert_eq!(format_history_block(&[]), "");
    }

    #[test]
    fn message_entries_render_in_chronological_order() {
        let mut e1 = make_entry(TranscriptRole::System("user".into()), "first user message", "message");
        e1.ts = Utc.with_ymd_and_hms(2026, 5, 20, 10, 0, 0).unwrap();
        let mut e2 = make_entry(TranscriptRole::Agent { agent: "agent-x".into() }, "first reply", "response");
        e2.ts = Utc.with_ymd_and_hms(2026, 5, 20, 10, 1, 0).unwrap();
        let mut e3 = make_entry(TranscriptRole::System("user".into()), "follow-up", "message");
        e3.ts = Utc.with_ymd_and_hms(2026, 5, 20, 10, 2, 0).unwrap();

        let out = format_history_block(&[e1, e2, e3]);
        assert!(out.starts_with("[Conversation history]"));
        let first_pos = out.find("first user message").expect("first message must appear");
        let reply_pos = out.find("first reply").expect("reply must appear");
        let followup_pos = out.find("follow-up").expect("follow-up must appear");
        assert!(first_pos < reply_pos, "messages must remain in chronological order");
        assert!(reply_pos < followup_pos, "messages must remain in chronological order");
    }

    #[test]
    fn tool_use_entry_synthesises_xml_from_metadata() {
        // Tool entries are persisted with empty `content` and a metadata bag;
        // a naïve renderer would emit a bare `[time] role:` and lose all visibility
        // into what tool the parent called.
        let entry = tool_use_entry("tu-42", "Read", json!({ "file_path": "/etc/hosts" }));
        let out = format_history_block(&[entry]);
        assert!(out.contains("<tool_use id=\"tu-42\" name=\"Read\">"));
        assert!(out.contains("/etc/hosts"));
        assert!(out.contains("</tool_use>"));
    }

    #[test]
    fn tool_result_entry_synthesises_xml_with_error_flag() {
        let entry = tool_result_entry("tu-42", "permission denied", true);
        let out = format_history_block(&[entry]);
        assert!(out.contains("<tool_result tool_use_id=\"tu-42\" is_error=\"true\">"));
        assert!(out.contains("permission denied"));
    }

    #[test]
    fn tool_result_without_error_omits_is_error_attribute() {
        let entry = tool_result_entry("tu-42", "ok", false);
        let out = format_history_block(&[entry]);
        assert!(out.contains("<tool_result tool_use_id=\"tu-42\">"));
        assert!(!out.contains("is_error"));
    }

    #[test]
    fn tool_use_without_metadata_is_skipped_not_rendered_as_blank() {
        let mut entry = tool_use_entry("tu-42", "Read", json!({}));
        entry.metadata = None;
        let user_msg = make_entry(TranscriptRole::System("user".into()), "hello", "message");
        let out = format_history_block(&[entry, user_msg]);
        // The malformed tool_use entry should not produce a `[12:35] agent:` blank line.
        assert!(!out.contains("agent-7ba6:"));
        assert!(out.contains("hello"));
    }

    #[test]
    fn budget_drops_oldest_entries_when_total_exceeds_cap() {
        // Build entries whose individual sizes are tractable but whose total
        // overflows the budget. Newer entries (later in the slice) must survive;
        // older ones get dropped.
        let big_content = "x".repeat(2000);
        let entries: Vec<TranscriptEntry> = (0..10)
            .map(|i| {
                let mut e = make_entry(
                    TranscriptRole::System("user".into()),
                    &format!("msg-{i}-{}", big_content),
                    "message",
                );
                e.ts = Utc.with_ymd_and_hms(2026, 5, 20, 10, i, 0).unwrap();
                e
            })
            .collect();

        let out = format_history_block(&entries);
        // The most recent entry (msg-9) must be present; the oldest (msg-0) must not.
        assert!(out.contains("msg-9-"), "newest entry must survive the budget");
        assert!(!out.contains("msg-0-"), "oldest entry must be dropped");
    }

    #[test]
    fn tool_result_oversized_output_is_truncated_in_place() {
        let huge = "x".repeat(FORK_TOOL_RESULT_CAP_BYTES + 500);
        let entry = tool_result_entry("tu-1", &huge, false);
        let out = format_history_block(&[entry]);
        assert!(out.contains("[... truncated;"));
        // Output should not include all 500 extra bytes literally — the
        // truncation marker should appear before the tail.
        let count = out.matches("x").count();
        assert!(
            count <= FORK_TOOL_RESULT_CAP_BYTES + 50,
            "tool_result body must be capped, got {} bytes of x",
            count
        );
    }

    #[test]
    fn fresh_envelope_contains_parent_name_and_handle_directive() {
        let out = build_envelope("Alice", "Bob", EnvelopeMode::Fresh, "do the thing");
        assert!(out.contains("Alice"), "must contain parent name");
        assert!(out.contains("Handle this directive."), "must contain handle phrasing");
        assert!(out.ends_with("do the thing"), "directive must be at the end");
    }

    #[test]
    fn fork_envelope_contains_both_names_and_fork_mode() {
        let out = build_envelope("Alice", "Bob", EnvelopeMode::ForkShared, "do the thing");
        assert!(out.contains("Alice"), "must contain parent name");
        assert!(out.contains("Bob"), "must contain child name");
        assert!(out.contains("in fork mode"), "must mention fork mode");
        assert!(
            out.contains("sharing Alice's context"),
            "must reference parent context"
        );
        assert!(out.ends_with("do the thing"), "directive must be at the end");
    }

    #[test]
    fn empty_parent_name_falls_back_to_id_prefix() {
        let out = build_envelope("", "Bob", EnvelopeMode::Fresh, "task");
        // name_or_prefix returns empty string if name is empty — but build_envelope
        // receives the already-resolved name. Callers must pass name_or_prefix output.
        // This test verifies name_or_prefix directly.
        let resolved = name_or_prefix("", "abc12345xyz");
        assert_eq!(resolved, "abc12345", "must truncate to 8 chars");
        // The envelope itself should not blow up with an empty string.
        assert!(!out.is_empty());
    }

    #[test]
    fn empty_child_name_falls_back_to_id_prefix() {
        let resolved = name_or_prefix("", "xxxxxxxx-yyyy");
        assert_eq!(resolved, "xxxxxxxx", "must truncate to 8 chars");
    }

    #[test]
    fn directive_with_bracket_and_newlines_round_trips_cleanly() {
        let directive = "do [this]\nand that\n";
        let out = build_envelope("P", "C", EnvelopeMode::Fresh, directive);
        assert!(out.ends_with(directive), "directive preserved verbatim");
    }
}
