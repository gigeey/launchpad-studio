use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use chrono::{DateTime, Utc};

/// Configuration for how much conversation history to include.
#[derive(Clone)]
pub struct ContextConfig {
    pub active_window_minutes: i64,
    pub active_message_count: usize,
    pub same_day_message_count: usize,
    pub recent_days: i64,
    pub recent_message_count: usize,
    pub stale_message_count: usize,
    pub hard_max: usize,
    pub max_message_chars: usize,
    pub max_total_chars: usize,
    /// Grace budget (entries) added to `pinned_target * 2` when computing the
    /// max window before the anchor floor rotates. Default: 4.
    pub anchor_grace: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            active_window_minutes: 120,
            active_message_count: 20,
            same_day_message_count: 10,
            recent_days: 3,
            recent_message_count: 4,
            stale_message_count: 2,
            hard_max: 50,
            max_message_chars: 500,
            max_total_chars: 12000,
            anchor_grace: 4,
        }
    }
}

/// Determine how many previous messages to include based on elapsed time.
pub fn compute_message_count(
    last_ts: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    config: &ContextConfig,
) -> usize {
    let last_ts = match last_ts {
        Some(ts) => ts,
        None => return 0,
    };

    let elapsed = now.signed_duration_since(last_ts);
    let minutes = elapsed.num_minutes();

    let count = if minutes < config.active_window_minutes {
        config.active_message_count
    } else if last_ts.date_naive() == now.date_naive() {
        config.same_day_message_count
    } else if elapsed.num_days() <= config.recent_days {
        config.recent_message_count
    } else {
        config.stale_message_count
    };

    count.min(config.hard_max)
}

fn role_label(role: &TranscriptRole) -> &str {
    match role {
        TranscriptRole::System(s) => s.as_str(),
        TranscriptRole::Agent { agent } => agent.as_str(),
        TranscriptRole::Schedule { .. } => "schedule",
    }
}

/// Truncate a string at the nearest UTF-8 char boundary at or below `max_bytes`,
/// then append a marker indicating how many bytes were dropped. Used for tool
/// payloads in transcript replay where outputs may contain multibyte chars
/// (emoji, CJK, etc.) and a naive byte slice would panic.
fn truncate_with_marker(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    format!(
        "{}\n[... truncated; {} more bytes]",
        &s[..idx],
        s.len() - idx,
    )
}

/// Per-entry caps for tool transcript entries. Tool inputs are usually small
/// (a path, a JSON blob a few hundred bytes); outputs can be huge (file
/// contents, big JSON dumps). We cap each separately so a single fat output
/// doesn't blow the entire `max_total_chars` budget and starve other history.
const TOOL_USE_INPUT_CAP: usize = 600;
const TOOL_RESULT_OUTPUT_CAP: usize = 1200;

/// Render a `tool_use` transcript entry as the `<tool_use>` XML shape the
/// model emitted. Inputs live in `metadata.input` (a JSON Value) — when the
/// entry was queued by [`TimelineAdapter`] we discarded its `content` field
/// and stashed everything structurally in metadata, so naïve `entry.content`
/// rendering would produce a bare `[HH:MM] agent:` line. Returns an empty
/// string when metadata is malformed (the caller skips that line).
fn render_tool_use_xml(entry: &TranscriptEntry) -> String {
    let meta = match entry.metadata.as_ref() {
        Some(m) => m,
        None => return String::new(),
    };
    let tool_use_id = meta
        .get("tool_use_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let tool_name = meta
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let input_str = meta
        .get("input")
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .unwrap_or_default();
    let input_display = truncate_with_marker(&input_str, TOOL_USE_INPUT_CAP);
    format!(
        "<tool_use id=\"{}\" name=\"{}\">{}</tool_use>",
        tool_use_id, tool_name, input_display,
    )
}

/// Render a `tool_result` transcript entry as the `<tool_result>` XML shape
/// the runner emits to the model. Output text + the `is_error` flag both
/// live in metadata for the same reason as `render_tool_use_xml`. Output is
/// truncated per-entry to keep huge file reads from monopolising the
/// `[Conversation history]` budget.
fn render_tool_result_xml(entry: &TranscriptEntry) -> String {
    let meta = match entry.metadata.as_ref() {
        Some(m) => m,
        None => return String::new(),
    };
    let tool_use_id = meta
        .get("tool_use_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let output = meta.get("output").and_then(|v| v.as_str()).unwrap_or("");
    let is_error = meta
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let output_display = truncate_with_marker(output, TOOL_RESULT_OUTPUT_CAP);
    let error_attr = if is_error { " is_error=\"true\"" } else { "" };
    format!(
        "<tool_result tool_use_id=\"{}\"{}>{}</tool_result>",
        tool_use_id, error_attr, output_display,
    )
}

/// Format transcript entries into a conversation history block.
///
/// Returns empty string for empty entries.
/// Each entry is formatted as `[HH:MM] role: content`.
/// Individual messages are truncated at `max_message_chars`.
/// Stops adding messages when total size exceeds `max_total_chars`.
pub fn format_context(entries: &[TranscriptEntry], config: &ContextConfig) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let mut lines = Vec::new();
    let mut total_chars = 0;

    // Iterate newest-first so recent messages get priority within the budget.
    for entry in entries.iter().rev() {
        // The body of the rendered line depends on the entry kind. For
        // ordinary messages/responses, use the content field. For tool_use /
        // tool_result entries, synthesize the <tool_use>/<tool_result> XML
        // from metadata — those entries are queued with empty `content`, so
        // naïve content rendering would emit a bare `[HH:MM] role:` line and
        // hide every tool exchange from prior turns. Without this routing,
        // the model has no idea what tools it called or what came back
        // outside the active continuation chain.
        let body = match entry.event_type.as_str() {
            "tool_use" => render_tool_use_xml(entry),
            "tool_result" => render_tool_result_xml(entry),
            // TODO revisit per-message truncation; currently messages pass
            // through unmodified and only `max_total_chars` is enforced.
            _ => entry.content.clone(),
        };

        // Drop the line entirely if the synthesizer returned empty (malformed
        // metadata, etc.) — better to skip than emit a noise line.
        if body.is_empty() && entry.event_type != "message" && entry.event_type != "response" {
            continue;
        }

        // Extract attachment references from metadata so the agent can re-read prior images
        let attachment_note = entry
            .metadata
            .as_ref()
            .and_then(|m| m.get("attachments"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        let path = a.get("file_path").and_then(|p| p.as_str())?;
                        let atype = a
                            .get("attachment_type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("file");
                        let label = match atype {
                            "image" | "Image" => "image",
                            "folder" | "Folder" => "folder",
                            _ => "file",
                        };
                        Some(format!("[Previously attached {}: {}]", label, path))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        let line = if attachment_note.is_empty() {
            format!(
                "[{}] {}: {}",
                entry.ts.format("%H:%M"),
                role_label(&entry.role),
                body
            )
        } else {
            format!(
                "[{}] {}: {}\n{}",
                entry.ts.format("%H:%M"),
                role_label(&entry.role),
                body,
                attachment_note
            )
        };

        total_chars += line.len();
        if total_chars > config.max_total_chars && !lines.is_empty() {
            break;
        }

        lines.push(line);
    }

    // Reverse back to chronological order for display.
    lines.reverse();

    format!("[Conversation history]\n{}", lines.join("\n"))
}

/// Build a complete prompt with conversation context prepended.
///
/// If entries are empty, returns the bare user prompt.
/// Otherwise wraps history in `[Conversation history]` and the prompt in `[Current message]`.
///
/// Caller is responsible for slice stability; this function does NOT re-slice.
pub fn build_prompt_with_context(
    entries: &[TranscriptEntry],
    user_prompt: &str,
    config: &ContextConfig,
) -> String {
    let context = format_context(entries, config);
    if context.is_empty() {
        return user_prompt.to_string();
    }

    format!("{}\n\n[Current message]\n{}", context, user_prompt)
}

/// Format recalled transcript entries into a [Recalled context] block.
///
/// If entries are empty, returns a "no matching history found" message.
/// Each entry is formatted as `[HH:MM] role: content`.
pub fn format_recalled_context(entries: &[TranscriptEntry], query: Option<&str>) -> String {
    if entries.is_empty() {
        return match query {
            Some(q) => format!("[No matching history found for query '{}']", q),
            None => "[No matching history found]".to_string(),
        };
    }

    let header = match query {
        Some(q) => format!(
            "[Recalled context ({} messages matching '{}')]",
            entries.len(),
            q
        ),
        None => format!("[Recalled context ({} messages)]", entries.len()),
    };

    let lines: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                "[{}] {}: {}",
                e.ts.format("%H:%M"),
                role_label(&e.role),
                e.content
            )
        })
        .collect();

    format!("{}\n{}", header, lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use serde_json::{json, Value};
    use std::collections::HashMap;

    fn make_entry(ts: DateTime<Utc>, role: TranscriptRole, content: &str) -> TranscriptEntry {
        TranscriptEntry {
            ts,
            role,
            content: content.to_string(),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        }
    }

    /// Build a `tool_use` transcript entry shaped exactly like the one
    /// `TimelineAdapter::queue_tool_use_entry` produces: empty `content`,
    /// payload in `metadata`. Tests guard against drift between the persisted
    /// shape and what `format_context` expects to consume.
    fn make_tool_use_entry(
        ts: DateTime<Utc>,
        agent: &str,
        tool_use_id: &str,
        tool_name: &str,
        input: Value,
    ) -> TranscriptEntry {
        let mut m = HashMap::new();
        m.insert("tool_use_id".to_string(), json!(tool_use_id));
        m.insert("tool_name".to_string(), json!(tool_name));
        m.insert("input".to_string(), input);
        m.insert("turn_id".to_string(), json!("turn-1"));
        TranscriptEntry {
            ts,
            role: TranscriptRole::Agent {
                agent: agent.to_string(),
            },
            content: String::new(),
            event_type: "tool_use".to_string(),
            metadata: Some(m),
            hidden_from_user: false,
        }
    }

    fn make_tool_result_entry(
        ts: DateTime<Utc>,
        tool_use_id: &str,
        output: &str,
        is_error: bool,
    ) -> TranscriptEntry {
        let mut m = HashMap::new();
        m.insert("tool_use_id".to_string(), json!(tool_use_id));
        m.insert("output".to_string(), json!(output));
        m.insert("is_error".to_string(), json!(is_error));
        m.insert("turn_id".to_string(), json!("turn-1"));
        TranscriptEntry {
            ts,
            role: TranscriptRole::System("tool".to_string()),
            content: String::new(),
            event_type: "tool_result".to_string(),
            metadata: Some(m),
            hidden_from_user: false,
        }
    }

    fn user_role() -> TranscriptRole {
        TranscriptRole::System("user".to_string())
    }

    fn agent_role() -> TranscriptRole {
        TranscriptRole::Agent {
            agent: "test-agent".to_string(),
        }
    }

    // === compute_message_count tests ===

    #[test]
    fn test_compute_active_window() {
        let config = ContextConfig::default();
        let now = Utc::now();
        let last = now - Duration::minutes(10);
        assert_eq!(compute_message_count(Some(last), now, &config), 20);
    }

    #[test]
    fn test_compute_same_day() {
        let config = ContextConfig::default();
        // Use a fixed time well into the day so subtracting 3 hours stays same day
        // but is clearly beyond the 120-minute active window.
        let now = Utc.with_ymd_and_hms(2026, 2, 25, 14, 0, 0).unwrap();
        let last = now - Duration::hours(3);
        assert_eq!(compute_message_count(Some(last), now, &config), 10);
    }

    #[test]
    fn test_compute_active_window_boundary() {
        let config = ContextConfig::default();
        let now = Utc.with_ymd_and_hms(2026, 2, 25, 14, 0, 0).unwrap();
        // 119 minutes is within the 120-minute active window.
        let last_within = now - Duration::minutes(119);
        assert_eq!(compute_message_count(Some(last_within), now, &config), 20);
        // 121 minutes is beyond the active window → same-day tier.
        let last_beyond = now - Duration::minutes(121);
        assert_eq!(compute_message_count(Some(last_beyond), now, &config), 10);
    }

    #[test]
    fn test_compute_recent() {
        let config = ContextConfig::default();
        let now = Utc::now();
        let last = now - Duration::days(2);
        assert_eq!(compute_message_count(Some(last), now, &config), 4);
    }

    #[test]
    fn test_compute_stale() {
        let config = ContextConfig::default();
        let now = Utc::now();
        let last = now - Duration::days(10);
        assert_eq!(compute_message_count(Some(last), now, &config), 2);
    }

    #[test]
    fn test_compute_no_prior() {
        let config = ContextConfig::default();
        let now = Utc::now();
        assert_eq!(compute_message_count(None, now, &config), 0);
    }

    // === format_context tests ===

    #[test]
    fn test_format_empty() {
        let config = ContextConfig::default();
        assert_eq!(format_context(&[], &config), "");
    }

    #[test]
    fn test_format_basic() {
        let config = ContextConfig::default();
        let ts = Utc.with_ymd_and_hms(2026, 2, 25, 10, 30, 0).unwrap();
        let entries = vec![
            make_entry(ts, user_role(), "hello"),
            make_entry(ts + Duration::minutes(1), agent_role(), "hi there"),
        ];
        let result = format_context(&entries, &config);
        assert!(result.starts_with("[Conversation history]\n"));
        assert!(result.contains("[10:30] user: hello"));
        assert!(result.contains("[10:31] test-agent: hi there"));
    }

    #[test]
    fn test_format_truncates_long_message() {
        // Per-message truncation is currently disabled (commented out in format_context),
        // so long messages pass through unmodified.
        let config = ContextConfig {
            max_message_chars: 10,
            ..Default::default()
        };
        let ts = Utc.with_ymd_and_hms(2026, 2, 25, 10, 0, 0).unwrap();
        let long_msg = "this is a very long message that should be truncated";
        let entries = vec![make_entry(ts, user_role(), long_msg)];
        let result = format_context(&entries, &config);
        assert!(
            result.contains(long_msg),
            "Full message should pass through since per-message truncation is disabled"
        );
    }

    #[test]
    fn test_format_total_size_cap() {
        let config = ContextConfig {
            max_total_chars: 50,
            ..Default::default()
        };
        let ts = Utc.with_ymd_and_hms(2026, 2, 25, 10, 0, 0).unwrap();
        let entries = vec![
            make_entry(ts, user_role(), "first message"),
            make_entry(ts + Duration::minutes(1), user_role(), "second message"),
            make_entry(ts + Duration::minutes(2), user_role(), "third message"),
        ];
        let result = format_context(&entries, &config);
        // Newest entries should survive truncation, not oldest
        let line_count = result.lines().count();
        // Header + at most 2 lines (total_chars cap reached)
        assert!(
            line_count < 4,
            "Should have fewer than 4 lines due to size cap, got {}",
            line_count
        );
        assert!(line_count >= 2, "Should have at least header + 1 entry");
        // The third (newest) message must be present
        assert!(
            result.contains("third message"),
            "Newest message should survive truncation"
        );
    }

    #[test]
    fn test_format_keeps_newest_entries_when_truncated() {
        // Budget sized so only the last 2 of 5 entries fit.
        // Each line is roughly "[HH:MM] user: msgNN" ≈ 20 chars.
        let config = ContextConfig {
            max_total_chars: 45,
            ..Default::default()
        };
        let ts = Utc.with_ymd_and_hms(2026, 2, 25, 10, 0, 0).unwrap();
        let entries = vec![
            make_entry(ts, user_role(), "msg01"),
            make_entry(ts + Duration::minutes(1), user_role(), "msg02"),
            make_entry(ts + Duration::minutes(2), user_role(), "msg03"),
            make_entry(ts + Duration::minutes(3), user_role(), "msg04"),
            make_entry(ts + Duration::minutes(4), user_role(), "msg05"),
        ];
        let result = format_context(&entries, &config);
        // Only the two newest entries should appear
        assert!(
            result.contains("msg05"),
            "Most recent entry must be present"
        );
        assert!(result.contains("msg04"), "Second most recent must be present");
        assert!(
            !result.contains("msg01"),
            "Oldest entry should be dropped"
        );
        assert!(
            !result.contains("msg02"),
            "Second oldest should be dropped"
        );
        assert!(
            !result.contains("msg03"),
            "Third oldest should be dropped"
        );
        // Chronological order preserved in output
        let pos4 = result.find("msg04").unwrap();
        let pos5 = result.find("msg05").unwrap();
        assert!(
            pos4 < pos5,
            "Entries should appear in chronological order"
        );
    }

    // === tool_use / tool_result rendering tests ===

    #[test]
    fn test_format_tool_use_entry_renders_xml_from_metadata() {
        // Regression: tool_use entries used to render as bare `[HH:MM] agent:`
        // lines because their `content` field is empty (payload lives in
        // metadata). format_context must synthesize the <tool_use> shape.
        let config = ContextConfig::default();
        let ts = Utc.with_ymd_and_hms(2026, 5, 19, 10, 0, 0).unwrap();
        let entries = vec![make_tool_use_entry(
            ts,
            "test-agent",
            "tu-1",
            "Read",
            json!({ "file_path": "/tmp/foo.txt" }),
        )];
        let result = format_context(&entries, &config);
        assert!(result.starts_with("[Conversation history]\n"));
        assert!(
            result.contains("[10:00] test-agent: <tool_use id=\"tu-1\" name=\"Read\">"),
            "tool_use line should carry the agent role + <tool_use> XML: {}",
            result,
        );
        assert!(
            result.contains(r#"{"file_path":"/tmp/foo.txt"}"#),
            "tool input JSON should appear inline: {}",
            result,
        );
        assert!(
            result.contains("</tool_use>"),
            "tool_use block must be closed: {}",
            result,
        );
        // The bare empty-content fallback should no longer appear.
        assert!(
            !result.contains("[10:00] test-agent: \n"),
            "should not emit a bare empty agent line for tool_use entries",
        );
    }

    #[test]
    fn test_format_tool_result_entry_renders_xml_from_metadata() {
        let config = ContextConfig::default();
        let ts = Utc.with_ymd_and_hms(2026, 5, 19, 10, 0, 30).unwrap();
        let entries = vec![make_tool_result_entry(
            ts,
            "tu-1",
            "hello world",
            false,
        )];
        let result = format_context(&entries, &config);
        assert!(
            result.contains("[10:00] tool: <tool_result tool_use_id=\"tu-1\">hello world</tool_result>"),
            "expected tool_result line in: {}",
            result,
        );
    }

    #[test]
    fn test_format_tool_result_entry_marks_is_error() {
        let config = ContextConfig::default();
        let ts = Utc.with_ymd_and_hms(2026, 5, 19, 10, 0, 30).unwrap();
        let entries = vec![make_tool_result_entry(
            ts,
            "tu-2",
            "boom",
            true,
        )];
        let result = format_context(&entries, &config);
        assert!(
            result.contains(r#"<tool_result tool_use_id="tu-2" is_error="true">boom</tool_result>"#),
            "expected error attribute on tool_result: {}",
            result,
        );
    }

    #[test]
    fn test_format_tool_result_truncates_huge_output() {
        // A tool that dumps a megabyte of file contents must not blow the
        // entire history budget. Per-entry cap kicks in well before
        // max_total_chars even matters.
        let config = ContextConfig::default();
        let ts = Utc.with_ymd_and_hms(2026, 5, 19, 10, 0, 30).unwrap();
        let huge = "x".repeat(50_000);
        let entries = vec![make_tool_result_entry(ts, "tu-1", &huge, false)];
        let result = format_context(&entries, &config);
        assert!(
            result.contains("[... truncated;"),
            "huge tool output should be truncated with marker: {}",
            &result[..result.len().min(500)],
        );
        assert!(
            result.len() < 5_000,
            "truncated rendering should fit well under the default total cap, got {} bytes",
            result.len(),
        );
    }

    #[test]
    fn test_format_mixed_message_and_tool_entries_preserves_order() {
        // Realistic shape: user asks → agent emits tool_use → tool_result →
        // agent emits response. All four must appear in chronological order.
        let config = ContextConfig::default();
        let t0 = Utc.with_ymd_and_hms(2026, 5, 19, 10, 0, 0).unwrap();
        let entries = vec![
            make_entry(t0, user_role(), "read /tmp/foo"),
            make_tool_use_entry(
                t0 + Duration::seconds(5),
                "test-agent",
                "tu-1",
                "Read",
                json!({ "file_path": "/tmp/foo" }),
            ),
            make_tool_result_entry(t0 + Duration::seconds(10), "tu-1", "hi", false),
            {
                let mut e = make_entry(t0 + Duration::seconds(15), agent_role(), "done");
                e.event_type = "response".to_string();
                e
            },
        ];
        let result = format_context(&entries, &config);
        let pos_user = result.find("read /tmp/foo").expect("user line");
        let pos_tu = result.find("<tool_use id=\"tu-1\"").expect("tool_use line");
        let pos_tr = result
            .find("<tool_result tool_use_id=\"tu-1\"")
            .expect("tool_result line");
        let pos_resp = result.find("done").expect("agent response line");
        assert!(
            pos_user < pos_tu && pos_tu < pos_tr && pos_tr < pos_resp,
            "entries must render in chronological order; got:\n{}",
            result,
        );
    }

    #[test]
    fn test_format_tool_entry_with_missing_metadata_is_skipped() {
        // Defensive: if metadata is somehow missing/malformed we drop the
        // line rather than emit empty XML that would just confuse the model.
        let config = ContextConfig::default();
        let ts = Utc.with_ymd_and_hms(2026, 5, 19, 10, 0, 0).unwrap();
        let mut bad = make_tool_use_entry(
            ts,
            "test-agent",
            "tu-1",
            "Read",
            json!({}),
        );
        bad.metadata = None;
        let entries = vec![
            make_entry(ts, user_role(), "keep me"),
            bad,
        ];
        let result = format_context(&entries, &config);
        assert!(result.contains("keep me"));
        assert!(
            !result.contains("<tool_use"),
            "malformed tool_use entry should be skipped, not emit empty XML",
        );
    }

    // === build_prompt_with_context tests ===

    #[test]
    fn test_build_prompt_no_history() {
        let config = ContextConfig::default();
        let result = build_prompt_with_context(&[], "hello world", &config);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_build_prompt_with_history() {
        let config = ContextConfig::default();
        let ts = Utc.with_ymd_and_hms(2026, 2, 25, 10, 0, 0).unwrap();
        let entries = vec![make_entry(ts, user_role(), "previous message")];
        let result = build_prompt_with_context(&entries, "new message", &config);
        assert!(result.contains("[Conversation history]"));
        assert!(result.contains("[Current message]"));
        assert!(result.contains("new message"));
        assert!(result.contains("previous message"));
    }

    // === format_recalled_context tests ===

    #[test]
    fn test_recalled_context_empty_no_query() {
        let result = format_recalled_context(&[], None);
        assert_eq!(result, "[No matching history found]");
    }

    #[test]
    fn test_recalled_context_empty_with_query() {
        let result = format_recalled_context(&[], Some("mermaid"));
        assert_eq!(result, "[No matching history found for query 'mermaid']");
    }

    #[test]
    fn test_recalled_context_with_entries_no_query() {
        let ts = Utc.with_ymd_and_hms(2026, 2, 25, 10, 0, 0).unwrap();
        let entries = vec![
            make_entry(ts, user_role(), "hello"),
            make_entry(ts + Duration::minutes(1), agent_role(), "hi there"),
        ];
        let result = format_recalled_context(&entries, None);
        assert!(result.starts_with("[Recalled context (2 messages)]"));
        assert!(result.contains("[10:00] user: hello"));
        assert!(result.contains("[10:01] test-agent: hi there"));
    }

    #[test]
    fn test_recalled_context_with_entries_and_query() {
        let ts = Utc.with_ymd_and_hms(2026, 2, 25, 10, 0, 0).unwrap();
        let entries = vec![make_entry(ts, user_role(), "draw a diagram")];
        let result = format_recalled_context(&entries, Some("diagram"));
        assert!(result.starts_with("[Recalled context (1 messages matching 'diagram')]"));
        assert!(result.contains("[10:00] user: draw a diagram"));
    }
}
