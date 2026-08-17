//! Unit tests for the `ao-normalizer` crate root.
//!
//! Declared from `lib.rs` as `#[cfg(test)] mod tests;` — `tests.rs` is the
//! same module as the inline `mod tests` block it replaces, so private items
//! of the crate root remain in scope here via `use super::*`.

use ao_protocol::agent::{CliProviderConfig, InputMode, OutputFormat};
use ao_protocol::event::AgentEventPayload;

use crate::generic::GenericNormalizer;
use crate::registry::NormalizerRegistry;
use crate::traits::OutputNormalizer;

#[test]
fn generic_normalizer_process_chunks_returns_text_deltas() {
    let mut normalizer = GenericNormalizer::new();

    let events1 = normalizer.process_chunk("hello ");
    assert_eq!(events1.len(), 1);
    assert!(matches!(&events1[0], AgentEventPayload::TextDelta { text } if text == "hello "));

    let events2 = normalizer.process_chunk("world ");
    assert_eq!(events2.len(), 1);
    assert!(matches!(&events2[0], AgentEventPayload::TextDelta { text } if text == "world "));

    let events3 = normalizer.process_chunk("!");
    assert_eq!(events3.len(), 1);
    assert!(matches!(&events3[0], AgentEventPayload::TextDelta { text } if text == "!"));
}

#[test]
fn generic_normalizer_finalize_returns_text_complete() {
    let mut normalizer = GenericNormalizer::new();
    normalizer.process_chunk("hello ");
    normalizer.process_chunk("world");
    normalizer.process_chunk("!");

    let events = normalizer.finalize(Some(0), "");
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], AgentEventPayload::TextComplete { text } if text == "hello world!")
    );
}

#[test]
fn generic_normalizer_finalize_with_stderr_produces_error() {
    let mut normalizer = GenericNormalizer::new();
    // Don't feed any chunks — empty accumulated

    let events = normalizer.finalize(Some(1), "something went wrong");
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], AgentEventPayload::Error { message, recoverable } if message == "something went wrong" && !recoverable)
    );
}

#[test]
fn generic_normalizer_finalize_with_content_and_stderr() {
    let mut normalizer = GenericNormalizer::new();
    normalizer.process_chunk("partial output");

    let events = normalizer.finalize(Some(1), "error occurred");
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], AgentEventPayload::TextComplete { text } if text == "partial output"));
    assert!(matches!(&events[1], AgentEventPayload::Error { message, .. } if message == "error occurred"));
}

#[test]
fn generic_normalizer_extract_session_id_returns_none() {
    let normalizer = GenericNormalizer::new();
    assert!(normalizer.extract_session_id().is_none());
}

#[test]
fn registry_unknown_command_returns_generic_normalizer() {
    let registry = NormalizerRegistry::new();
    let config = make_test_config();

    let mut normalizer = registry.create("unknown-tool", &config);

    // Verify it behaves like GenericNormalizer (returns TextDelta)
    let events = normalizer.process_chunk("test text");
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AgentEventPayload::TextDelta { text } if text == "test text"));
}

#[test]
fn registry_matches_command_name_from_path() {
    let registry = NormalizerRegistry::new();
    let config = make_test_config();

    // Full path should extract command name and fall back to generic
    let mut normalizer = registry.create("/usr/local/bin/some-tool", &config);
    let events = normalizer.process_chunk("output");
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AgentEventPayload::TextDelta { text } if text == "output"));
}

// --- ClaudeNormalizer tests ---

#[test]
fn claude_json_mode_feed_fixture_and_finalize() {
    let config = make_claude_config(OutputFormat::Json);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    let fixture = include_str!("../fixtures/claude_json_output.json");

    // In JSON mode, process_chunk returns empty vec (buffering)
    let events = normalizer.process_chunk(fixture);
    assert!(events.is_empty());

    // Finalize parses the JSON and extracts text
    let events = normalizer.finalize(Some(0), "");
    assert!(events.len() >= 1);

    // First event should be TextComplete with the result text
    assert!(
        matches!(&events[0], AgentEventPayload::TextComplete { text }
            if text == "Hello! I'm Claude, an AI assistant made by Anthropic.")
    );

    // Second event should be Usage. After the field-name fix the fixture
    // emits the real Anthropic field names; `cache_creation_input_tokens`
    // is non-zero on the result event to stand in for a first-turn write.
    // total = input + output + cache_read = 15 + 25 + 5.
    assert!(
        matches!(&events[1], AgentEventPayload::Usage {
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, total_tokens
        } if *input_tokens == 15
            && *output_tokens == 25
            && *cache_read_tokens == 5
            && *cache_creation_tokens == 2
            && *total_tokens == 45)
    );
}

#[test]
fn claude_json_mode_extracts_session_id() {
    let config = make_claude_config(OutputFormat::Json);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    let fixture = include_str!("../fixtures/claude_json_output.json");
    normalizer.process_chunk(fixture);
    normalizer.finalize(Some(0), "");

    assert_eq!(
        normalizer.extract_session_id(),
        Some("session-abc-123".to_string())
    );
}

#[test]
fn claude_stream_json_mode_processes_line_by_line() {
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    let fixture = include_str!("../fixtures/claude_stream_json_output.jsonl");

    // Feed fixture line by line (simulating streaming)
    let mut all_events = Vec::new();
    for line in fixture.lines() {
        if line.is_empty() {
            continue;
        }
        // Each line needs a newline to trigger processing
        let chunk = format!("{}\n", line);
        all_events.extend(normalizer.process_chunk(&chunk));
    }

    // Should have TextDelta events for content_block_delta lines
    let text_deltas: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::TextDelta { .. }))
        .collect();
    assert_eq!(text_deltas.len(), 3);

    // Verify text delta content
    assert!(matches!(&text_deltas[0], AgentEventPayload::TextDelta { text } if text == "Hello"));
    assert!(matches!(&text_deltas[1], AgentEventPayload::TextDelta { text } if text == "! I'm "));
    assert!(matches!(&text_deltas[2], AgentEventPayload::TextDelta { text } if text == "Claude."));

    // Should have Usage events from message_delta and result
    let usage_events: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::Usage { .. }))
        .collect();
    assert!(usage_events.len() >= 1);
}

#[test]
fn claude_stream_json_mode_finalize_returns_text_complete() {
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    let fixture = include_str!("../fixtures/claude_stream_json_output.jsonl");
    for line in fixture.lines() {
        if line.is_empty() {
            continue;
        }
        let chunk = format!("{}\n", line);
        normalizer.process_chunk(&chunk);
    }

    let events = normalizer.finalize(Some(0), "");
    // Should have TextComplete with accumulated text
    assert!(events.len() >= 1);
    assert!(
        matches!(&events[0], AgentEventPayload::TextComplete { text }
            if text == "Hello! I'm Claude.")
    );
}

#[test]
fn claude_stream_json_mode_extracts_session_id() {
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    let fixture = include_str!("../fixtures/claude_stream_json_output.jsonl");
    for line in fixture.lines() {
        if line.is_empty() {
            continue;
        }
        let chunk = format!("{}\n", line);
        normalizer.process_chunk(&chunk);
    }
    normalizer.finalize(Some(0), "");

    assert_eq!(
        normalizer.extract_session_id(),
        Some("session-abc-123".to_string())
    );
}

#[test]
fn registry_creates_claude_normalizer_for_claude_command() {
    let registry = NormalizerRegistry::new();
    let config = make_claude_config(OutputFormat::Json);

    let mut normalizer = registry.create("claude", &config);

    // Claude normalizer in JSON mode returns empty on process_chunk
    let events = normalizer.process_chunk(r#"{"result": "test"}"#);
    assert!(events.is_empty());

    // Finalize returns TextComplete
    let events = normalizer.finalize(Some(0), "");
    assert!(events.len() >= 1);
    assert!(matches!(&events[0], AgentEventPayload::TextComplete { text } if text == "test"));
}

#[test]
fn registry_creates_claude_normalizer_for_full_path() {
    let registry = NormalizerRegistry::new();
    let config = make_claude_config(OutputFormat::Json);

    // Full path should extract "claude" command name and match
    let mut normalizer = registry.create("/usr/local/bin/claude", &config);

    let events = normalizer.process_chunk(r#"{"result": "hello"}"#);
    assert!(events.is_empty()); // JSON mode buffers

    let events = normalizer.finalize(Some(0), "");
    assert!(matches!(&events[0], AgentEventPayload::TextComplete { text } if text == "hello"));
}

#[test]
fn claude_json_mode_content_array_fallback() {
    let config = make_claude_config(OutputFormat::Json);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    // Test the content[].text fallback path
    let json = r#"{"content": [{"type": "text", "text": "Hello from content array"}]}"#;
    normalizer.process_chunk(json);
    let events = normalizer.finalize(Some(0), "");
    assert!(
        matches!(&events[0], AgentEventPayload::TextComplete { text }
            if text == "Hello from content array")
    );
}

#[test]
fn claude_json_mode_message_content_fallback() {
    let config = make_claude_config(OutputFormat::Json);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    // Test the message.content[].text fallback path
    let json =
        r#"{"message": {"content": [{"type": "text", "text": "Hello from message.content"}]}}"#;
    normalizer.process_chunk(json);
    let events = normalizer.finalize(Some(0), "");
    assert!(
        matches!(&events[0], AgentEventPayload::TextComplete { text }
            if text == "Hello from message.content")
    );
}

#[test]
fn claude_stream_json_thinking_delta_emits_thinking_event() {
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    // Thinking delta event should emit ThinkingDelta
    let events = normalizer
        .process_chunk("{\"type\":\"thinking\",\"subtype\":\"delta\",\"text\":\"Let me think about this...\"}\n");
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], AgentEventPayload::ThinkingDelta { text } if text == "Let me think about this...")
    );

    // Thinking completed event should be a no-op
    let events = normalizer
        .process_chunk("{\"type\":\"thinking\",\"subtype\":\"completed\"}\n");
    assert!(events.is_empty());
}

/// Modern SSE-shaped thinking lifecycle (the format the claude CLI emits
/// today when invoked with `--thinking adaptive --thinking-display summarized`):
/// `content_block_start[type=thinking]` opens the block, one or more
/// `content_block_delta[type=thinking_delta]` carry progressive reasoning
/// text, and `content_block_stop` closes it. The normalizer must turn that
/// into the canonical `ThinkingStarted` → `ThinkingDelta`* → `ThinkingEnded`
/// triplet so the UI can light up its "Thinking…" pill on start and
/// collapse it on stop without ever touching provider-specific fields.
#[test]
fn claude_sse_thinking_block_emits_start_delta_end() {
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    let mut events = Vec::new();
    events.extend(normalizer.process_chunk(
        "{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n",
    ));
    events.extend(normalizer.process_chunk(
        "{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"first\"}}\n",
    ));
    events.extend(normalizer.process_chunk(
        "{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" second\"}}\n",
    ));
    events.extend(normalizer.process_chunk(
        "{\"type\":\"content_block_stop\",\"index\":0}\n",
    ));

    let kinds: Vec<&'static str> = events
        .iter()
        .map(|e| match e {
            AgentEventPayload::ThinkingStarted => "start",
            AgentEventPayload::ThinkingDelta { .. } => "delta",
            AgentEventPayload::ThinkingEnded { .. } => "end",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["start", "delta", "delta", "end"]);
    // Delta payloads carry the raw reasoning text verbatim.
    assert!(
        matches!(&events[1], AgentEventPayload::ThinkingDelta { text } if text == "first")
    );
    assert!(
        matches!(&events[2], AgentEventPayload::ThinkingDelta { text } if text == " second")
    );
    // Elapsed value is non-negative; we don't pin a specific number
    // because Instant::elapsed() depends on the host scheduler.
    assert!(
        matches!(&events[3], AgentEventPayload::ThinkingEnded { elapsed_ms: _ })
    );
}

/// `display = "omitted"` path: the model still engages its reasoning
/// channel and emits a `signature_delta` (the cryptographic proof), but
/// no `thinking_delta` events accompany it. The normalizer must still
/// produce `ThinkingStarted` / `ThinkingEnded` so the UI shows a
/// "Thinking…" pill — the absence of deltas is the load-bearing case
/// because that's what the CLI's default produces today.
#[test]
fn claude_sse_thinking_block_omitted_display_still_emits_start_end() {
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    let mut events = Vec::new();
    events.extend(normalizer.process_chunk(
        "{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n",
    ));
    // signature_delta is intentionally a no-op — the signature isn't
    // user-visible and the canonical event surface doesn't need it.
    events.extend(normalizer.process_chunk(
        "{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"abc\"}}\n",
    ));
    events.extend(normalizer.process_chunk(
        "{\"type\":\"content_block_stop\",\"index\":0}\n",
    ));

    // Exactly two canonical events: start + end. No delta.
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], AgentEventPayload::ThinkingStarted));
    assert!(matches!(&events[1], AgentEventPayload::ThinkingEnded { .. }));
}

#[test]
fn claude_stream_json_handles_partial_lines() {
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    // Send a partial line (no newline)
    let events = normalizer.process_chunk(r#"{"type":"content_block_delta","index":0,"delta":"#);
    assert!(events.is_empty()); // No complete line yet

    // Complete the line
    let events =
        normalizer.process_chunk(r#"{"type":"text_delta","text":"Hello"}}"#.to_string().as_str());
    assert!(events.is_empty()); // Still no newline

    // Send newline to flush
    let events = normalizer.process_chunk("\n");
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AgentEventPayload::TextDelta { text } if text == "Hello"));
}

#[test]
fn registry_creates_cursor_agent_normalizer_for_normalizer_field() {
    let registry = NormalizerRegistry::new();
    let mut config = make_test_config();
    config.normalizer = Some("cursor-agent".to_string());
    config.output_format = OutputFormat::StreamJson;

    // Should use CursorAgentNormalizer even though command is "test" (not registered)
    let mut normalizer = registry.create("test", &config);

    // CursorAgentNormalizer in StreamJson mode processes assistant events with full text
    let events = normalizer.process_chunk(
        "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"model\":\"claude-sonnet-4-20250514\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}},\"timestamp_ms\":1700000000000}\n",
    );
    assert_eq!(events.len(), 2); // TextDelta + Usage
    assert!(matches!(&events[0], AgentEventPayload::TextDelta { text } if text == "hi"));
}

#[test]
fn claude_code_stream_fixture_full_pipeline() {
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    let fixture = include_str!("../fixtures/claude_code_stream_output.jsonl");

    // Feed fixture line by line (simulating streaming)
    let mut all_events = Vec::new();
    for line in fixture.lines() {
        if line.is_empty() {
            continue;
        }
        let chunk = format!("{}\n", line);
        all_events.extend(normalizer.process_chunk(&chunk));
    }

    // Verify ThinkingDelta events are emitted for thinking deltas
    let thinking_deltas: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::ThinkingDelta { .. }))
        .collect();
    assert_eq!(thinking_deltas.len(), 2);
    assert!(
        matches!(&thinking_deltas[0], AgentEventPayload::ThinkingDelta { text } if text == "The user is asking me to explain")
    );
    assert!(
        matches!(&thinking_deltas[1], AgentEventPayload::ThinkingDelta { text } if text == " how Rust's ownership model works.")
    );

    // Verify TextDelta events are emitted for assistant content
    let text_deltas: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::TextDelta { .. }))
        .collect();
    assert_eq!(text_deltas.len(), 3);
    assert!(matches!(&text_deltas[0], AgentEventPayload::TextDelta { text } if text == "Rust's ownership"));
    assert!(matches!(&text_deltas[1], AgentEventPayload::TextDelta { text } if text == " model ensures memory safety"));
    assert!(matches!(&text_deltas[2], AgentEventPayload::TextDelta { text } if text == " without a garbage collector."));

    // Verify session_id is extracted (from the system init event)
    assert_eq!(
        normalizer.extract_session_id(),
        Some("session-code-456".to_string())
    );

    // Verify finalize produces TextComplete with accumulated text
    let final_events = normalizer.finalize(Some(0), "");
    assert!(final_events.len() >= 1);
    assert!(
        matches!(&final_events[0], AgentEventPayload::TextComplete { text }
            if text == "Rust's ownership model ensures memory safety without a garbage collector.")
    );
}

// --- No --include-partial-messages tests ---

#[test]
fn claude_stream_json_no_partial_messages_emits_text_from_result() {
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    let fixture = include_str!("../fixtures/claude_no_partial_messages_output.jsonl");

    // Feed fixture line by line
    let mut all_events = Vec::new();
    for line in fixture.lines() {
        if line.is_empty() {
            continue;
        }
        let chunk = format!("{}\n", line);
        all_events.extend(normalizer.process_chunk(&chunk));
    }

    // Without --include-partial-messages, no content_block_delta events arrive.
    // Text should come from the result event as a single TextDelta.
    let text_deltas: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::TextDelta { .. }))
        .collect();
    assert_eq!(text_deltas.len(), 1, "Expected exactly 1 TextDelta from result event");
    assert!(
        matches!(&text_deltas[0], AgentEventPayload::TextDelta { text }
            if text == "Here is the branch creation plan:\n\n1. Create branch `release/v1.0`\n2. Push to remote")
    );

    // Session ID should be extracted from system init
    assert_eq!(
        normalizer.extract_session_id(),
        Some("session-no-partial-789".to_string())
    );

    // Usage should be extracted from result
    let usage_events: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::Usage { .. }))
        .collect();
    assert_eq!(usage_events.len(), 1);

    // Finalize should produce TextComplete
    let final_events = normalizer.finalize(Some(0), "");
    assert!(final_events.len() >= 1);
    assert!(
        matches!(&final_events[0], AgentEventPayload::TextComplete { text }
            if text == "Here is the branch creation plan:\n\n1. Create branch `release/v1.0`\n2. Push to remote")
    );
}

#[test]
fn claude_stream_json_no_partial_messages_with_assistant_event() {
    // Simulates when assistant event IS emitted but without content_block_delta streaming.
    // The assistant event has the full content in its message.content array.
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    // system init
    normalizer.process_chunk(
        r#"{"type":"system","subtype":"init","session_id":"sess-np-2","model":"claude-sonnet-4-20250514"}"#.to_owned().as_str(),
    );
    normalizer.process_chunk("\n");

    // assistant event with content (no prior content_block_delta)
    let events = normalizer.process_chunk(
        &format!("{}\n", r#"{"type":"assistant","message":{"id":"msg_np","type":"message","role":"assistant","content":[{"type":"text","text":"Branch created successfully."}],"model":"claude-sonnet-4-20250514","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}}"#),
    );

    // Buffer was empty, so assistant event should emit the text
    let text_deltas: Vec<&AgentEventPayload> = events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::TextDelta { .. }))
        .collect();
    assert_eq!(text_deltas.len(), 1);
    assert!(
        matches!(&text_deltas[0], AgentEventPayload::TextDelta { text }
            if text == "Branch created successfully.")
    );

    // Now if a result event comes with the same text, it should NOT duplicate
    let result_events = normalizer.process_chunk(
        &format!("{}\n", r#"{"type":"result","subtype":"success","result":"Branch created successfully.","usage":{"input_tokens":10,"output_tokens":5}}"#),
    );
    let result_text_deltas: Vec<&AgentEventPayload> = result_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::TextDelta { .. }))
        .collect();
    assert_eq!(result_text_deltas.len(), 0, "Result text should be deduped when assistant already emitted it");
}

#[test]
fn claude_stream_json_no_partial_messages_multi_turn_with_tool_use() {
    // Simulates: text → tool_use → tool_result → text (second turn)
    // Without --include-partial-messages, each turn arrives as an `assistant` event.
    // The bug was that only the first turn's text was emitted; subsequent turns
    // were silently dropped because buffer was non-empty.
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    let mut all_events = Vec::new();

    // system init
    all_events.extend(normalizer.process_chunk(
        &format!("{}\n", r#"{"type":"system","subtype":"init","session_id":"sess-multi","model":"claude-sonnet-4-20250514"}"#),
    ));

    // First assistant turn: text only
    all_events.extend(normalizer.process_chunk(
        &format!("{}\n", r#"{"type":"assistant","message":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"Let me check git access now."}],"model":"claude-sonnet-4-20250514","stop_reason":"tool_use","usage":{"input_tokens":50,"output_tokens":20}}}"#),
    ));

    // Tool use block
    all_events.extend(normalizer.process_chunk(
        &format!("{}\n", r#"{"type":"content_block_start","content_block":{"type":"tool_use","id":"tool_1","name":"Bash"}}"#),
    ));
    all_events.extend(normalizer.process_chunk(
        &format!("{}\n", r#"{"type":"content_block_stop"}"#),
    ));

    // Tool result — Claude CLI feeds this back as a top-level `"user"`
    // event, never as a `content_block_start` (that shape never occurs
    // in real output — see `ClaudeNormalizer::process_event`'s `"user"`
    // arm).
    all_events.extend(normalizer.process_chunk(
        &format!("{}\n", r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":"git version 2.45.0","is_error":false}]}}"#),
    ));

    // The tool_result must resolve to the real tool name ("Bash"),
    // recovered via the id captured when the tool_use block opened —
    // not the opaque "tool_1" id.
    let tool_completions: Vec<(String, Option<String>)> = all_events
        .iter()
        .filter_map(|e| match e {
            AgentEventPayload::ToolCallCompleted { tool_name, output, .. } => {
                Some((tool_name.clone(), output.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(tool_completions.len(), 1, "expected exactly one ToolCallCompleted, got: {:?}", tool_completions);
    assert_eq!(tool_completions[0].0, "Bash");
    assert_eq!(tool_completions[0].1.as_deref(), Some("git version 2.45.0"));

    // Second assistant turn: text with the git version result
    all_events.extend(normalizer.process_chunk(
        &format!("{}\n", r#"{"type":"assistant","message":{"id":"msg_2","type":"message","role":"assistant","content":[{"type":"text","text":"Git is available. Version: 2.45.0"}],"model":"claude-sonnet-4-20250514","stop_reason":"end_turn","usage":{"input_tokens":80,"output_tokens":15}}}"#),
    ));

    // Result event (contains all text combined)
    all_events.extend(normalizer.process_chunk(
        &format!("{}\n", r#"{"type":"result","subtype":"success","result":"Let me check git access now.\n\nGit is available. Version: 2.45.0","usage":{"input_tokens":80,"output_tokens":35}}"#),
    ));

    // Collect text deltas — should have BOTH turns
    let text_deltas: Vec<String> = all_events
        .iter()
        .filter_map(|e| match e {
            AgentEventPayload::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(text_deltas.len(), 2, "Expected 2 TextDeltas (one per turn), got: {:?}", text_deltas);
    assert_eq!(text_deltas[0], "Let me check git access now.");
    assert_eq!(text_deltas[1], "Git is available. Version: 2.45.0");

    // Finalize should produce TextComplete with ALL accumulated text
    let final_events = normalizer.finalize(Some(0), "");
    let complete_text = final_events.iter().find_map(|e| match e {
        AgentEventPayload::TextComplete { text } => Some(text.clone()),
        _ => None,
    });
    assert!(complete_text.is_some());
    assert_eq!(
        complete_text.unwrap(),
        "Let me check git access now.Git is available. Version: 2.45.0"
    );
}

#[test]
fn claude_tools_in_flight_paired_around_tool_use() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    let counter = Arc::new(AtomicUsize::new(0));
    normalizer.set_tools_in_flight_counter(Arc::clone(&counter));

    // The tool_use content block opens — counter rises so the supervisor's
    // idle watchdog pauses across the silent MCP/tool-exec window that
    // follows.
    normalizer.process_chunk(
        "{\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"Bash\"}}\n",
    );
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // The tool_result for it arrives, as a top-level `"user"` event
    // (the real Claude CLI wire shape) — counter drops back.
    normalizer.process_chunk(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"tu_1\",\"content\":\"ok\",\"is_error\":false}]}}\n",
    );
    assert_eq!(counter.load(Ordering::Relaxed), 0);
}

/// Pins the actual bug fix: a `tool_result` arriving via the real
/// Claude CLI wire shape (top-level `"user"` event) must resolve to the
/// tool's real name via the id captured at `tool_use` block-open, not
/// the raw `tool_use_id`. This is what makes `ToolCallCompleted` fire at
/// all for CLI-mode agents — before this fix, `tool_result` was only
/// matched against a `content_block_start` shape that Claude CLI never
/// actually emits, so the event silently never fired (root cause of
/// `ArtifactWrite` — and every other tool — never resolving inline for
/// CLI-mode agents).
#[test]
fn claude_tool_result_resolves_real_name_via_user_event() {
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    normalizer.process_chunk(
        "{\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01Abc\",\"name\":\"ArtifactWrite\"}}\n",
    );
    normalizer.process_chunk("{\"type\":\"content_block_stop\"}\n");

    let events = normalizer.process_chunk(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_01Abc\",\"content\":\"{\\\"id\\\":\\\"artifact-1\\\"}\",\"is_error\":false}]}}\n",
    );

    let completed: Vec<&AgentEventPayload> = events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::ToolCallCompleted { .. }))
        .collect();
    assert_eq!(completed.len(), 1, "expected exactly one ToolCallCompleted, got: {:?}", events);
    match completed[0] {
        AgentEventPayload::ToolCallCompleted { tool_name, output, .. } => {
            assert_eq!(tool_name, "ArtifactWrite");
            assert_eq!(output.as_deref(), Some("{\"id\":\"artifact-1\"}"));
        }
        other => panic!("expected ToolCallCompleted, got {:?}", other),
    }
}

/// An unrecognized `tool_use_id` (e.g. id-capture bug, or a result for a
/// call made before the normalizer was constructed) must fall back to
/// `"unknown"` rather than panicking or silently dropping the event.
#[test]
fn claude_tool_result_unknown_id_falls_back_to_unknown_name() {
    let config = make_claude_config(OutputFormat::StreamJson);
    let mut normalizer = crate::claude::ClaudeNormalizer::new(&config);

    let events = normalizer.process_chunk(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_never_seen\",\"content\":\"ok\",\"is_error\":false}]}}\n",
    );

    let completed: Vec<&AgentEventPayload> = events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::ToolCallCompleted { .. }))
        .collect();
    assert_eq!(completed.len(), 1);
    match completed[0] {
        AgentEventPayload::ToolCallCompleted { tool_name, .. } => {
            assert_eq!(tool_name, "unknown");
        }
        other => panic!("expected ToolCallCompleted, got {:?}", other),
    }
}

// --- Normalizer registry override tests ---

#[test]
fn registry_explicit_normalizer_overrides_command_name() {
    let registry = NormalizerRegistry::new();
    let mut config = make_test_config();
    config.command = "agent".to_string();
    config.normalizer = Some("claude".to_string());
    config.output_format = OutputFormat::Json;

    // normalizer='claude' should override command='agent' and use ClaudeNormalizer
    let mut normalizer = registry.create("agent", &config);

    // ClaudeNormalizer in JSON mode buffers on process_chunk (returns empty)
    let events = normalizer.process_chunk(r#"{"result": "override works"}"#);
    assert!(events.is_empty());

    // Finalize returns TextComplete — confirms ClaudeNormalizer, not GenericNormalizer
    let events = normalizer.finalize(Some(0), "");
    assert!(events.len() >= 1);
    assert!(
        matches!(&events[0], AgentEventPayload::TextComplete { text } if text == "override works")
    );
}

#[test]
fn registry_cursor_agent_normalizer_overrides_unknown_command() {
    let registry = NormalizerRegistry::new();
    let mut config = make_test_config();
    config.normalizer = Some("cursor-agent".to_string());
    config.output_format = OutputFormat::StreamJson;

    // normalizer='cursor-agent' should override command='unknown'
    let mut normalizer = registry.create("unknown", &config);

    // CursorAgentNormalizer processes assistant events with full text in content[]
    let events = normalizer.process_chunk(
        "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"cursor works\"}],\"model\":\"claude-sonnet-4-20250514\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}},\"timestamp_ms\":1700000000000}\n",
    );
    assert_eq!(events.len(), 2); // TextDelta + Usage
    assert!(
        matches!(&events[0], AgentEventPayload::TextDelta { text } if text == "cursor works")
    );
}

#[test]
fn registry_no_normalizer_field_falls_back_to_command_name() {
    let registry = NormalizerRegistry::new();
    let mut config = make_claude_config(OutputFormat::Json);
    config.normalizer = None;

    // normalizer=None, command='claude' → should use ClaudeNormalizer via command name matching
    let mut normalizer = registry.create("claude", &config);

    // ClaudeNormalizer in JSON mode buffers on process_chunk
    let events = normalizer.process_chunk(r#"{"result": "backward compat"}"#);
    assert!(events.is_empty());

    let events = normalizer.finalize(Some(0), "");
    assert!(events.len() >= 1);
    assert!(
        matches!(&events[0], AgentEventPayload::TextComplete { text } if text == "backward compat")
    );
}

#[test]
fn registry_nonexistent_normalizer_falls_back_to_generic() {
    let registry = NormalizerRegistry::new();
    let mut config = make_test_config();
    config.normalizer = Some("nonexistent".to_string());

    // normalizer='nonexistent' not registered, command='unknown' not registered → GenericNormalizer
    let mut normalizer = registry.create("unknown", &config);

    // GenericNormalizer returns TextDelta immediately on process_chunk
    let events = normalizer.process_chunk("fallback works");
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], AgentEventPayload::TextDelta { text } if text == "fallback works")
    );
}

// --- CursorAgentNormalizer tests ---

#[test]
fn cursor_agent_stream_fixture_full_pipeline() {
    let config = make_cursor_agent_config();
    let mut normalizer = crate::cursor_agent::CursorAgentNormalizer::new(&config);

    let fixture = include_str!("../fixtures/test_cursor_agent_stream_output.jsonl");

    let mut all_events = Vec::new();
    for line in fixture.lines() {
        if line.is_empty() {
            continue;
        }
        let chunk = format!("{}\n", line);
        all_events.extend(normalizer.process_chunk(&chunk));
    }

    // Verify ThinkingDelta events
    let thinking_deltas: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::ThinkingDelta { .. }))
        .collect();
    assert_eq!(thinking_deltas.len(), 2);
    assert!(
        matches!(&thinking_deltas[0], AgentEventPayload::ThinkingDelta { text } if text == "The user wants to know about")
    );
    assert!(
        matches!(&thinking_deltas[1], AgentEventPayload::ThinkingDelta { text } if text == " Rust ownership semantics.")
    );

    // Verify only 1 TextDelta (not 2 — dedup skips the second assistant event)
    let text_deltas: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::TextDelta { .. }))
        .collect();
    assert_eq!(text_deltas.len(), 1);
    assert!(
        matches!(&text_deltas[0], AgentEventPayload::TextDelta { text }
            if text == "Rust uses ownership to manage memory safely without a garbage collector.")
    );

    // Verify session_id is extracted from system init
    assert_eq!(
        normalizer.extract_session_id(),
        Some("session-cursor-789".to_string())
    );

    // Verify finalize produces TextComplete
    let final_events = normalizer.finalize(Some(0), "");
    assert!(final_events.len() >= 1);
    assert!(
        matches!(&final_events[0], AgentEventPayload::TextComplete { text }
            if text == "Rust uses ownership to manage memory safely without a garbage collector.")
    );
}

#[test]
fn cursor_agent_dedup_skips_second_assistant_event() {
    let config = make_cursor_agent_config();
    let mut normalizer = crate::cursor_agent::CursorAgentNormalizer::new(&config);

    // First assistant event — should emit TextDelta
    let events1 = normalizer.process_chunk(
        "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello world\"}],\"model\":\"claude-sonnet-4-20250514\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}},\"timestamp_ms\":1700000000000}\n",
    );
    let text_deltas1: Vec<&AgentEventPayload> = events1
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::TextDelta { .. }))
        .collect();
    assert_eq!(text_deltas1.len(), 1);
    assert!(matches!(&text_deltas1[0], AgentEventPayload::TextDelta { text } if text == "hello world"));

    // Second assistant event (duplicate, no timestamp_ms) — should NOT emit TextDelta
    let events2 = normalizer.process_chunk(
        "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello world\"}],\"model\":\"claude-sonnet-4-20250514\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n",
    );
    let text_deltas2: Vec<&AgentEventPayload> = events2
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::TextDelta { .. }))
        .collect();
    assert_eq!(text_deltas2.len(), 0);
}

#[test]
fn cursor_agent_result_text_skipped_when_buffer_has_content() {
    let config = make_cursor_agent_config();
    let mut normalizer = crate::cursor_agent::CursorAgentNormalizer::new(&config);

    // Feed assistant event first (populates buffer)
    normalizer.process_chunk(
        "{\"type\":\"assistant\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"already seen\"}],\"model\":\"claude-sonnet-4-20250514\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}},\"timestamp_ms\":1700000000000}\n",
    );

    // Feed result event — text should be skipped since buffer is non-empty
    let events = normalizer.process_chunk(
        "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"sess-1\",\"result\":\"already seen\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}\n",
    );
    let text_deltas: Vec<&AgentEventPayload> = events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::TextDelta { .. }))
        .collect();
    assert_eq!(text_deltas.len(), 0);

    // But usage should still be extracted
    let usage_events: Vec<&AgentEventPayload> = events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::Usage { .. }))
        .collect();
    assert_eq!(usage_events.len(), 1);
}

#[test]
fn cursor_agent_ignores_user_message_echo() {
    let config = make_cursor_agent_config();
    let mut normalizer = crate::cursor_agent::CursorAgentNormalizer::new(&config);

    // User message echo should produce no events
    let events = normalizer.process_chunk(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
    );
    assert!(events.is_empty());
}

#[test]
fn cursor_agent_extracts_session_id_from_system_init() {
    let config = make_cursor_agent_config();
    let mut normalizer = crate::cursor_agent::CursorAgentNormalizer::new(&config);

    normalizer.process_chunk(
        "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-abc-123\",\"model\":\"claude-sonnet-4-20250514\"}\n",
    );

    assert_eq!(
        normalizer.extract_session_id(),
        Some("sess-abc-123".to_string())
    );
}

fn make_cursor_agent_config() -> CliProviderConfig {
    CliProviderConfig {
        command: "agent".to_string(),
        args: vec![],
        normalizer: Some("cursor-agent".to_string()),
        output_format: OutputFormat::StreamJson,
        input_mode: InputMode::Arg,
        model_arg: None,
        model_aliases: std::collections::HashMap::new(),
        system_prompt_arg: None,
        session_arg: None,
        resume_args: vec![],
        session_id_fields: vec![],
        clear_env: false,
        no_output_timeout_ms: 30000,
        file_capabilities: None,
    }
}

fn make_test_config() -> CliProviderConfig {
    CliProviderConfig {
        command: "test".to_string(),
        args: vec![],
        normalizer: None,
        output_format: OutputFormat::Text,
        input_mode: InputMode::Arg,
        model_arg: None,
        model_aliases: std::collections::HashMap::new(),
        system_prompt_arg: None,
        session_arg: None,
        resume_args: vec![],
        session_id_fields: vec![],
        clear_env: false,
        no_output_timeout_ms: 30000,
        file_capabilities: None,
    }
}

fn make_claude_config(output_format: OutputFormat) -> CliProviderConfig {
    CliProviderConfig {
        command: "claude".to_string(),
        args: vec![],
        normalizer: None,
        output_format,
        input_mode: InputMode::Arg,
        model_arg: None,
        model_aliases: std::collections::HashMap::new(),
        system_prompt_arg: None,
        session_arg: None,
        resume_args: vec![],
        session_id_fields: vec![],
        clear_env: false,
        no_output_timeout_ms: 30000,
        file_capabilities: None,
    }
}

fn make_codex_config(output_format: OutputFormat) -> CliProviderConfig {
    CliProviderConfig {
        command: "codex".to_string(),
        args: vec!["exec".to_string(), "--json".to_string()],
        normalizer: Some("codex".to_string()),
        output_format,
        input_mode: InputMode::Arg,
        model_arg: None,
        model_aliases: std::collections::HashMap::new(),
        system_prompt_arg: None,
        session_arg: None,
        resume_args: vec![],
        session_id_fields: vec!["thread_id".to_string()],
        clear_env: false,
        no_output_timeout_ms: 60000,
        file_capabilities: None,
    }
}

// --- CodexNormalizer tests ---

#[test]
fn codex_jsonl_fixture_full_pipeline() {
    let config = make_codex_config(OutputFormat::StreamJsonl);
    let mut normalizer = crate::codex::CodexNormalizer::new(&config);

    let fixture = include_str!("../fixtures/codex_exec_json_output.jsonl");

    let mut all_events = Vec::new();
    for line in fixture.lines() {
        if line.is_empty() {
            continue;
        }
        let chunk = format!("{}\n", line);
        all_events.extend(normalizer.process_chunk(&chunk));
    }

    // Verify ThinkingDelta from reasoning item
    let thinking_deltas: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::ThinkingDelta { .. }))
        .collect();
    assert_eq!(thinking_deltas.len(), 1);
    assert!(
        matches!(&thinking_deltas[0], AgentEventPayload::ThinkingDelta { text }
            if text == "Let me analyze this codebase.")
    );

    // Verify ToolCallStarted from command_execution item.started
    let tool_starts: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::ToolCallStarted { .. }))
        .collect();
    assert_eq!(tool_starts.len(), 1);
    assert!(
        matches!(&tool_starts[0], AgentEventPayload::ToolCallStarted { tool_name, .. }
            if tool_name == "bash -lc 'ls src/'")
    );

    // Verify ToolCallCompleted from command_execution item.completed
    let tool_completes: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::ToolCallCompleted { .. }))
        .collect();
    assert_eq!(tool_completes.len(), 1);
    assert!(
        matches!(&tool_completes[0], AgentEventPayload::ToolCallCompleted { tool_name, output, .. }
            if tool_name == "bash -lc 'ls src/'" && output.as_deref() == Some("main.rs\nlib.rs\n"))
    );

    // Verify TextDelta from agent_message (one event, all-at-once)
    let text_deltas: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::TextDelta { .. }))
        .collect();
    assert_eq!(text_deltas.len(), 1);
    assert!(
        matches!(&text_deltas[0], AgentEventPayload::TextDelta { text }
            if text == "The src directory contains main.rs and lib.rs.")
    );

    // Verify Usage from turn.completed (cached_input_tokens → cache_read_tokens).
    // Codex has no cache-write surface, so cache_creation_tokens = 0.
    // total = input + output + cache_read = 1200 + 45 + 200.
    let usage_events: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::Usage { .. }))
        .collect();
    assert_eq!(usage_events.len(), 1);
    assert!(
        matches!(&usage_events[0], AgentEventPayload::Usage {
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, total_tokens
        } if *input_tokens == 1200
            && *output_tokens == 45
            && *cache_read_tokens == 200
            && *cache_creation_tokens == 0
            && *total_tokens == 1445)
    );

    // Verify session_id extracted from thread.started
    assert_eq!(
        normalizer.extract_session_id(),
        Some("67e55044-10b1-426f-9247-bb680e5fe0c8".to_string())
    );

    // Verify finalize produces TextComplete
    let final_events = normalizer.finalize(Some(0), "");
    assert!(final_events.len() >= 1);
    assert!(
        matches!(&final_events[0], AgentEventPayload::TextComplete { text }
            if text == "The src directory contains main.rs and lib.rs.")
    );
}

#[test]
fn codex_text_mode_passthrough() {
    let config = make_codex_config(OutputFormat::Text);
    let mut normalizer = crate::codex::CodexNormalizer::new(&config);

    // Text mode should behave like GenericNormalizer
    let events1 = normalizer.process_chunk("hello ");
    assert_eq!(events1.len(), 1);
    assert!(matches!(&events1[0], AgentEventPayload::TextDelta { text } if text == "hello "));

    let events2 = normalizer.process_chunk("world");
    assert_eq!(events2.len(), 1);
    assert!(matches!(&events2[0], AgentEventPayload::TextDelta { text } if text == "world"));

    let final_events = normalizer.finalize(Some(0), "");
    assert_eq!(final_events.len(), 1);
    assert!(
        matches!(&final_events[0], AgentEventPayload::TextComplete { text }
            if text == "hello world")
    );

    // No session ID in text mode
    assert!(normalizer.extract_session_id().is_none());
}

#[test]
fn codex_extracts_thread_id_as_session_id() {
    let config = make_codex_config(OutputFormat::StreamJsonl);
    let mut normalizer = crate::codex::CodexNormalizer::new(&config);

    normalizer.process_chunk(
        "{\"type\":\"thread.started\",\"thread_id\":\"abc-123-def\"}\n",
    );

    assert_eq!(
        normalizer.extract_session_id(),
        Some("abc-123-def".to_string())
    );
}

#[test]
fn codex_turn_failed_emits_error() {
    let config = make_codex_config(OutputFormat::StreamJsonl);
    let mut normalizer = crate::codex::CodexNormalizer::new(&config);

    let events = normalizer.process_chunk(
        "{\"type\":\"turn.failed\",\"message\":\"Rate limit exceeded\"}\n",
    );
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], AgentEventPayload::Error { message, recoverable }
            if message == "Rate limit exceeded" && !recoverable)
    );
}

#[test]
fn codex_reasoning_emits_thinking_delta() {
    let config = make_codex_config(OutputFormat::StreamJsonl);
    let mut normalizer = crate::codex::CodexNormalizer::new(&config);

    // Empty reasoning text should be skipped
    let events_empty = normalizer.process_chunk(
        "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"reasoning\",\"text\":\"\"}}\n",
    );
    assert!(events_empty.is_empty());

    // Non-empty reasoning text should emit ThinkingDelta
    let events = normalizer.process_chunk(
        "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_1\",\"type\":\"reasoning\",\"text\":\"Thinking about the problem...\"}}\n",
    );
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], AgentEventPayload::ThinkingDelta { text }
            if text == "Thinking about the problem...")
    );
}

#[test]
fn codex_tools_in_flight_paired_around_command_execution() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let config = make_codex_config(OutputFormat::StreamJsonl);
    let mut normalizer = crate::codex::CodexNormalizer::new(&config);

    let counter = Arc::new(AtomicUsize::new(0));
    normalizer.set_tools_in_flight_counter(Arc::clone(&counter));

    // Baseline: nothing in flight.
    assert_eq!(counter.load(Ordering::Relaxed), 0);

    // item.started for command_execution → counter ticks up. The watchdog
    // pauses past this point even if codex's stdout goes silent while the
    // shell command runs.
    normalizer.process_chunk(
        "{\"type\":\"item.started\",\"item\":{\"id\":\"item_1\",\"type\":\"command_execution\",\"command\":\"bash -lc 'sleep 30'\",\"aggregated_output\":\"\",\"status\":\"in_progress\"}}\n",
    );
    assert_eq!(counter.load(Ordering::Relaxed), 1);

    // item.completed for the same command → counter drops back to 0.
    normalizer.process_chunk(
        "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_1\",\"type\":\"command_execution\",\"command\":\"bash -lc 'sleep 30'\",\"aggregated_output\":\"done\",\"exit_code\":0,\"status\":\"completed\"}}\n",
    );
    assert_eq!(counter.load(Ordering::Relaxed), 0);
}

#[test]
fn codex_tools_in_flight_does_not_underflow_on_unpaired_completed() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let config = make_codex_config(OutputFormat::StreamJsonl);
    let mut normalizer = crate::codex::CodexNormalizer::new(&config);

    let counter = Arc::new(AtomicUsize::new(0));
    normalizer.set_tools_in_flight_counter(Arc::clone(&counter));

    // A completed event with no preceding start (e.g. fixture replay,
    // crash-recovery) must NOT wrap the usize underneath us.
    normalizer.process_chunk(
        "{\"type\":\"item.completed\",\"item\":{\"id\":\"orphan\",\"type\":\"command_execution\",\"command\":\"x\",\"aggregated_output\":\"\",\"exit_code\":0,\"status\":\"completed\"}}\n",
    );
    assert_eq!(counter.load(Ordering::Relaxed), 0);
}

#[test]
fn codex_tools_in_flight_unwired_is_a_no_op() {
    // Mirrors the production path where the engine forgets to wire the
    // counter — the normalizer must not panic.
    let config = make_codex_config(OutputFormat::StreamJsonl);
    let mut normalizer = crate::codex::CodexNormalizer::new(&config);
    normalizer.process_chunk(
        "{\"type\":\"item.started\",\"item\":{\"id\":\"x\",\"type\":\"command_execution\",\"command\":\"y\",\"aggregated_output\":\"\",\"status\":\"in_progress\"}}\n",
    );
    normalizer.process_chunk(
        "{\"type\":\"item.completed\",\"item\":{\"id\":\"x\",\"type\":\"command_execution\",\"command\":\"y\",\"aggregated_output\":\"\",\"exit_code\":0,\"status\":\"completed\"}}\n",
    );
}

#[test]
fn registry_creates_codex_normalizer() {
    let registry = NormalizerRegistry::new();
    let config = make_codex_config(OutputFormat::StreamJsonl);

    // Should use CodexNormalizer via normalizer="codex" field
    let mut normalizer = registry.create("codex", &config);

    // CodexNormalizer in StreamJsonl mode processes thread.started
    let events = normalizer.process_chunk(
        "{\"type\":\"thread.started\",\"thread_id\":\"test-thread-id\"}\n",
    );
    assert!(events.is_empty()); // thread.started emits no payload events

    // Process an agent_message
    let events = normalizer.process_chunk(
        "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"codex works\"}}\n",
    );
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AgentEventPayload::TextDelta { text } if text == "codex works"));

    // Verify session_id was extracted
    assert_eq!(
        normalizer.extract_session_id(),
        Some("test-thread-id".to_string())
    );
}

// --- AgyNormalizer tests ---

/// Sample blob for the defensive JSON fallback path in `finalize` — not
/// what the real `agy` binary emits today (that's plain text, covered by
/// `agy_text_mode_finalize_emits_text_complete_for_plain_output` below),
/// but kept in case a future `agy` release adds a JSON output mode.
const AGY_SAMPLE_RESULT: &str = r#"{"conversation_id":"46568e4d-4a4b-4286-a966-622b50e6c0f2","status":"SUCCESS","response":"Yes, I received your message!","duration_seconds":1.755547,"num_turns":1,"usage":{"input_tokens":9787,"output_tokens":119,"thinking_tokens":70,"cache_read_tokens":8140,"total_tokens":9906}}"#;

fn make_agy_config(output_format: OutputFormat) -> CliProviderConfig {
    CliProviderConfig {
        command: "agy".to_string(),
        args: vec![
            "--output-format".to_string(),
            "stream-json".to_string(),
        ],
        normalizer: Some("agy".to_string()),
        output_format,
        input_mode: InputMode::Arg,
        model_arg: Some("--model".to_string()),
        model_aliases: std::collections::HashMap::new(),
        system_prompt_arg: None,
        session_arg: None,
        resume_args: vec![],
        session_id_fields: vec!["conversation_id".to_string()],
        clear_env: false,
        no_output_timeout_ms: 30000,
        file_capabilities: None,
    }
}

// --- AgyNormalizer NDJSON (stream-json) tests ---

const AGY_NDJSON_INIT: &str = r#"{"event":"init","conversation_id":"agy-conv-1","init":{"cwd":"/tmp","tools":["read_file"],"permission_mode":"request-review"}}"#;

const AGY_NDJSON_STEP_AGENT_RESPONSE: &str = r#"{"event":"step_update","step_update":{"conversation_id":"agy-conv-1","step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"Hello from agy","duration_seconds":0.5,"usage":{"input_tokens":10,"output_tokens":2,"thinking_tokens":1,"cache_read_tokens":0,"total_tokens":12}}}"#;

const AGY_NDJSON_STEP_CHECKPOINT: &str = r#"{"event":"step_update","step_update":{"conversation_id":"agy-conv-1","step_index":0,"state":"DONE","step_type":"checkpoint","duration_seconds":0.1}}"#;

const AGY_NDJSON_STEP_USER_INPUT: &str = r#"{"event":"step_update","step_update":{"conversation_id":"agy-conv-1","step_index":0,"state":"DONE","step_type":"user_input","duration_seconds":0.0}}"#;

const AGY_NDJSON_STEP_UNKNOWN: &str = r#"{"event":"step_update","step_update":{"conversation_id":"agy-conv-1","step_index":2,"state":"DONE","step_type":"some_future_type","text_delta":"should not appear"}}"#;

const AGY_NDJSON_RESULT_SUCCESS: &str = r#"{"event":"result","result":{"conversation_id":"agy-conv-1","status":"SUCCESS","response":"Hello from agy","duration_seconds":1.2,"num_turns":1,"usage":{"input_tokens":100,"output_tokens":20,"thinking_tokens":5,"cache_read_tokens":40,"total_tokens":120}}}"#;

const AGY_NDJSON_RESULT_FAILURE: &str = r#"{"event":"result","result":{"conversation_id":"agy-conv-1","status":"ERROR","response":"something broke","duration_seconds":0.3,"num_turns":1,"usage":{"input_tokens":5,"output_tokens":0,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":5}}}"#;

const AGY_NDJSON_UNKNOWN_TOP_LEVEL_EVENT: &str = r#"{"event":"some_future_event","payload":{"foo":"bar"}}"#;

#[test]
fn agy_stream_json_agent_response_step_emits_text_delta() {
    let config = make_agy_config(OutputFormat::StreamJson);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    let events = normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_STEP_AGENT_RESPONSE));
    assert_eq!(events.len(), 1, "expected exactly one TextDelta");
    assert!(matches!(
        &events[0],
        AgentEventPayload::TextDelta { text } if text == "Hello from agy"
    ));
}

#[test]
fn agy_stream_json_non_agent_response_step_emits_no_text() {
    let config = make_agy_config(OutputFormat::StreamJson);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    for line in [
        AGY_NDJSON_STEP_CHECKPOINT,
        AGY_NDJSON_STEP_USER_INPUT,
        AGY_NDJSON_STEP_UNKNOWN,
    ] {
        let events = normalizer.process_chunk(&format!("{}\n", line));
        assert!(
            events.is_empty(),
            "step_type other than agent_response must emit no events, got {:?} for line {}",
            events,
            line
        );
    }
}

#[test]
fn agy_stream_json_full_sequence_emits_text_once_and_maps_usage() {
    let config = make_agy_config(OutputFormat::StreamJson);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    let mut all_events = Vec::new();
    for line in [AGY_NDJSON_INIT, AGY_NDJSON_STEP_AGENT_RESPONSE, AGY_NDJSON_RESULT_SUCCESS] {
        all_events.extend(normalizer.process_chunk(&format!("{}\n", line)));
    }

    // Assistant text must appear exactly once from streaming (the result
    // event's `response` duplicates the same text and must be suppressed).
    let text_deltas: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::TextDelta { .. }))
        .collect();
    assert_eq!(text_deltas.len(), 1, "expected exactly one TextDelta across the whole run");
    assert!(matches!(
        &text_deltas[0],
        AgentEventPayload::TextDelta { text } if text == "Hello from agy"
    ));

    // Usage must come from the result event only, mapping all 5 internal
    // fields (cache_creation_tokens always 0; thinking_tokens dropped).
    let usage_events: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::Usage { .. }))
        .collect();
    assert_eq!(usage_events.len(), 1, "expected exactly one Usage event, from result only");
    assert!(matches!(
        &usage_events[0],
        AgentEventPayload::Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 40,
            cache_creation_tokens: 0,
            total_tokens: 120,
        }
    ));

    // Finalize seals the streamed text with one consolidating TextComplete
    // — not a second copy of result.response.
    let final_events = normalizer.finalize(Some(0), "");
    assert_eq!(final_events.len(), 1);
    assert!(matches!(
        &final_events[0],
        AgentEventPayload::TextComplete { text } if text == "Hello from agy"
    ));
}

#[test]
fn agy_stream_json_non_success_result_emits_error() {
    let config = make_agy_config(OutputFormat::StreamJson);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    let events = normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_RESULT_FAILURE));

    let errors: Vec<&AgentEventPayload> = events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::Error { .. }))
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(matches!(
        errors[0],
        AgentEventPayload::Error { message, recoverable: false } if message == "something broke"
    ));

    // No TextDelta should be emitted for a failed run's response text.
    assert!(!events.iter().any(|e| matches!(e, AgentEventPayload::TextDelta { .. })));
}

#[test]
fn agy_stream_json_tolerates_unknown_step_type_and_unknown_top_level_event() {
    let config = make_agy_config(OutputFormat::StreamJson);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    // Unknown step_type inside a known event — no panic, no events.
    let events = normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_STEP_UNKNOWN));
    assert!(events.is_empty());

    // Entirely unknown top-level event — no panic, no events.
    let events = normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_UNKNOWN_TOP_LEVEL_EVENT));
    assert!(events.is_empty());

    // Normalizer must still be usable afterwards (didn't get poisoned).
    let events = normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_STEP_AGENT_RESPONSE));
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AgentEventPayload::TextDelta { .. }));
}

#[test]
fn agy_stream_json_captures_conversation_id_as_session_id() {
    let config = make_agy_config(OutputFormat::StreamJson);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    assert_eq!(normalizer.extract_session_id(), None);

    normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_INIT));
    assert_eq!(normalizer.extract_session_id(), Some("agy-conv-1".to_string()));
}

#[test]
fn agy_stream_json_handles_partial_lines_across_chunks() {
    let config = make_agy_config(OutputFormat::StreamJson);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    let full_line = format!("{}\n", AGY_NDJSON_STEP_AGENT_RESPONSE);
    let (first, second) = full_line.split_at(30);

    assert!(normalizer.process_chunk(first).is_empty(), "no complete line yet");
    let events = normalizer.process_chunk(second);
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AgentEventPayload::TextDelta { text } if text == "Hello from agy"));
}

#[test]
fn parse_agy_result_maps_all_usage_fields() {
    let value: serde_json::Value = serde_json::from_str(AGY_SAMPLE_RESULT).expect("valid JSON");
    let parsed = crate::agy::parse_agy_result(&value).expect("response field present");

    assert_eq!(parsed.response, "Yes, I received your message!");
    assert_eq!(parsed.status, "SUCCESS");

    let usage = parsed.usage.expect("usage object present");
    assert_eq!(usage.input_tokens, 9787);
    assert_eq!(usage.output_tokens, 119);
    assert_eq!(usage.thinking_tokens, 70);
    assert_eq!(usage.cache_read_tokens, 8140);
    assert_eq!(usage.total_tokens, 9906);
}

#[test]
fn agy_json_mode_buffers_until_finalize() {
    let config = make_agy_config(OutputFormat::Json);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    // Feed the sample in two chunks — process_chunk must not emit
    // anything until finalize, since agy has no streaming mode.
    let (first, second) = AGY_SAMPLE_RESULT.split_at(40);
    assert!(normalizer.process_chunk(first).is_empty());
    assert!(normalizer.process_chunk(second).is_empty());

    let events = normalizer.finalize(Some(0), "");
    assert_eq!(events.len(), 2, "expected a TextComplete and a Usage event");
    assert!(matches!(
        &events[0],
        AgentEventPayload::TextComplete { text } if text == "Yes, I received your message!"
    ));
    assert!(matches!(
        &events[1],
        AgentEventPayload::Usage {
            input_tokens: 9787,
            output_tokens: 119,
            cache_read_tokens: 8140,
            cache_creation_tokens: 0,
            total_tokens: 9906,
        }
    ));
}

#[test]
fn agy_json_mode_extracts_session_id() {
    let config = make_agy_config(OutputFormat::Json);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    normalizer.process_chunk(AGY_SAMPLE_RESULT);
    normalizer.finalize(Some(0), "");

    assert_eq!(
        normalizer.extract_session_id(),
        Some("46568e4d-4a4b-4286-a966-622b50e6c0f2".to_string())
    );
}

#[test]
fn agy_json_mode_non_success_status_emits_error() {
    let config = make_agy_config(OutputFormat::Json);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    let blob = r#"{"conversation_id":"x","status":"ERROR","response":"something went wrong","usage":{"input_tokens":1,"output_tokens":0,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":1}}"#;
    normalizer.process_chunk(blob);
    let events = normalizer.finalize(Some(1), "");

    assert!(matches!(
        &events[0],
        AgentEventPayload::Error { message, recoverable: false } if message == "something went wrong"
    ));
}

#[test]
fn agy_text_mode_finalize_emits_text_complete_for_plain_output() {
    let config = make_agy_config(OutputFormat::Text);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    // Real `agy --print` output is plain text, not JSON — process_chunk
    // should stream it as TextDelta while buffering it.
    let events = normalizer.process_chunk("Yes, I received your message!");
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        AgentEventPayload::TextDelta { text } if text == "Yes, I received your message!"
    ));

    let events = normalizer.finalize(Some(0), "");
    assert_eq!(
        events.len(),
        1,
        "expected exactly one authoritative TextComplete, no duplicate"
    );
    assert!(matches!(
        &events[0],
        AgentEventPayload::TextComplete { text } if text == "Yes, I received your message!"
    ));
}

#[test]
fn registry_creates_agy_normalizer_for_agy_command() {
    let registry = NormalizerRegistry::new();
    let config = make_agy_config(OutputFormat::Json);

    let mut normalizer = registry.create("agy", &config);

    let events = normalizer.process_chunk(AGY_SAMPLE_RESULT);
    assert!(events.is_empty(), "JSON mode buffers until finalize");

    let events = normalizer.finalize(Some(0), "");
    assert!(matches!(
        &events[0],
        AgentEventPayload::TextComplete { text } if text == "Yes, I received your message!"
    ));
}

// --- AgyNormalizer tool-step tests ---
// Fixtures below are the real captured `agy` events for `step_type ==
// "tool"`: a stable `step_index` (not an API-native id) correlates a
// call's ACTIVE announcement with its later DONE/ERROR outcome.

const AGY_NDJSON_TOOL_ACTIVE_NO_PARAMS: &str = r#"{"event":"step_update","step_update":{"conversation_id":"agy-conv-1","step_index":3,"state":"ACTIVE","step_type":"tool","tool_name":"list_permissions","tool_info":{"name":"list_permissions"}}}"#;

const AGY_NDJSON_TOOL_DONE_WITH_OUTPUT: &str = r#"{"event":"step_update","step_update":{"conversation_id":"agy-conv-1","step_index":3,"state":"DONE","step_type":"tool","tool_name":"list_permissions","duration_seconds":0.11,"tool_info":{"name":"list_permissions","output":"read_file: allow\nwrite_file: deny"}}}"#;

const AGY_NDJSON_TOOL_ACTIVE_WITH_PARAMS: &str = r#"{"event":"step_update","step_update":{"conversation_id":"agy-conv-1","step_index":6,"state":"ACTIVE","step_type":"tool","tool_name":"list_dir","tool_info":{"name":"list_dir","parameters":{"DirectoryPath":"/some/path"}}}}"#;

const AGY_NDJSON_TOOL_DONE_WITHOUT_OUTPUT: &str = r#"{"event":"step_update","step_update":{"conversation_id":"agy-conv-1","step_index":6,"state":"DONE","step_type":"tool","tool_name":"list_dir","duration_seconds":0.10,"tool_info":{"name":"list_dir","parameters":{"DirectoryPath":"/some/path"}}}}"#;

const AGY_NDJSON_TOOL_ACTIVE_BEFORE_ERROR: &str = r#"{"event":"step_update","step_update":{"conversation_id":"agy-conv-1","step_index":8,"state":"ACTIVE","step_type":"tool","tool_name":"list_dir","tool_info":{"name":"list_dir","parameters":{"DirectoryPath":"/x"}}}}"#;

const AGY_NDJSON_TOOL_ERROR: &str = r#"{"event":"step_update","step_update":{"conversation_id":"agy-conv-1","step_index":8,"state":"ERROR","step_type":"tool","tool_name":"list_dir","duration_seconds":0.11,"tool_info":{"name":"list_dir","parameters":{"DirectoryPath":"/x"},"error":{"type":"TOOL_ERROR","message":"User denied permission for read_file(/x)."}}}}"#;

#[test]
fn agy_stream_json_tool_active_done_with_output_pair_correlates_ids() {
    let config = make_agy_config(OutputFormat::StreamJson);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    let started = normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_TOOL_ACTIVE_NO_PARAMS));
    assert_eq!(started.len(), 1);
    let started_id = match &started[0] {
        AgentEventPayload::ToolCallStarted { tool_name, tool_input, tool_use_id, .. } => {
            assert_eq!(tool_name, "list_permissions");
            assert!(tool_input.is_none(), "no-arg tool must carry no input, got {:?}", tool_input);
            tool_use_id.clone().expect("tool_use_id must be set")
        }
        other => panic!("expected ToolCallStarted, got {:?}", other),
    };

    let completed = normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_TOOL_DONE_WITH_OUTPUT));
    assert_eq!(completed.len(), 1);
    match &completed[0] {
        AgentEventPayload::ToolCallCompleted { tool_name, output, tool_use_id, is_error } => {
            assert_eq!(tool_name, "list_permissions");
            assert_eq!(output.as_deref(), Some("read_file: allow\nwrite_file: deny"));
            assert_eq!(tool_use_id.as_ref(), Some(&started_id), "completed id must match started id");
            assert!(!is_error);
        }
        other => panic!("expected ToolCallCompleted, got {:?}", other),
    }
}

#[test]
fn agy_stream_json_tool_done_without_output_emits_empty_output_not_error() {
    let config = make_agy_config(OutputFormat::StreamJson);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    let started = normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_TOOL_ACTIVE_WITH_PARAMS));
    assert_eq!(started.len(), 1);
    let (started_id, tool_input) = match &started[0] {
        AgentEventPayload::ToolCallStarted { tool_input, tool_use_id, .. } => {
            (tool_use_id.clone().expect("tool_use_id must be set"), tool_input.clone())
        }
        other => panic!("expected ToolCallStarted, got {:?}", other),
    };
    assert_eq!(tool_input, Some(serde_json::json!({"DirectoryPath": "/some/path"})));

    let completed = normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_TOOL_DONE_WITHOUT_OUTPUT));
    assert_eq!(completed.len(), 1);
    match &completed[0] {
        AgentEventPayload::ToolCallCompleted { output, tool_use_id, is_error, .. } => {
            assert!(output.is_none(), "missing tool_info.output must render as empty, not error");
            assert_eq!(tool_use_id.as_ref(), Some(&started_id));
            assert!(!is_error, "missing output alone must not be treated as an error");
        }
        other => panic!("expected ToolCallCompleted, got {:?}", other),
    }
}

#[test]
fn agy_stream_json_tool_error_emits_completed_with_error_and_does_not_abort_stream() {
    let config = make_agy_config(OutputFormat::StreamJson);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    let started = normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_TOOL_ACTIVE_BEFORE_ERROR));
    let started_id = match &started[0] {
        AgentEventPayload::ToolCallStarted { tool_use_id, .. } => {
            tool_use_id.clone().expect("tool_use_id must be set")
        }
        other => panic!("expected ToolCallStarted, got {:?}", other),
    };

    let errored = normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_TOOL_ERROR));
    assert_eq!(errored.len(), 1);
    match &errored[0] {
        AgentEventPayload::ToolCallCompleted { tool_name, output, tool_use_id, is_error } => {
            assert_eq!(tool_name, "list_dir");
            assert_eq!(output.as_deref(), Some("User denied permission for read_file(/x)."));
            assert_eq!(tool_use_id.as_ref(), Some(&started_id));
            assert!(is_error);
        }
        other => panic!("expected ToolCallCompleted, got {:?}", other),
    }

    // The stream must keep working after a tool ERROR — a tool failure is
    // not a fatal normalizer error (only a non-SUCCESS `result.status` is).
    let events = normalizer.process_chunk(&format!("{}\n", AGY_NDJSON_STEP_AGENT_RESPONSE));
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AgentEventPayload::TextDelta { .. }));
}

#[test]
fn agy_stream_json_end_to_end_with_tools_usage_sole_source_and_correlated_ids() {
    let config = make_agy_config(OutputFormat::StreamJson);
    let mut normalizer = crate::agy::AgyNormalizer::new(&config);

    // agent_response step carrying only per-step usage, no text_delta —
    // occurs in tool-heavy runs and must emit nothing.
    const AGENT_RESPONSE_USAGE_ONLY: &str = r#"{"event":"step_update","step_update":{"conversation_id":"agy-conv-1","step_index":1,"state":"DONE","step_type":"agent_response","duration_seconds":0.5,"usage":{"input_tokens":10,"output_tokens":2,"thinking_tokens":1,"cache_read_tokens":0,"total_tokens":12}}}"#;
    const RESULT_SUCCESS_EMPTY_RESPONSE: &str = r#"{"event":"result","result":{"conversation_id":"agy-conv-1","status":"SUCCESS","response":"","duration_seconds":2.0,"num_turns":1,"usage":{"input_tokens":500,"output_tokens":50,"thinking_tokens":10,"cache_read_tokens":100,"total_tokens":550}}}"#;

    let mut all_events = Vec::new();
    for line in [
        AGY_NDJSON_INIT,
        AGY_NDJSON_STEP_USER_INPUT,
        AGY_NDJSON_STEP_UNKNOWN,
        AGENT_RESPONSE_USAGE_ONLY,
        AGY_NDJSON_TOOL_ACTIVE_NO_PARAMS,
        AGY_NDJSON_TOOL_DONE_WITH_OUTPUT,
        AGY_NDJSON_STEP_CHECKPOINT,
        AGY_NDJSON_TOOL_ACTIVE_BEFORE_ERROR,
        AGY_NDJSON_TOOL_ERROR,
        RESULT_SUCCESS_EMPTY_RESPONSE,
    ] {
        all_events.extend(normalizer.process_chunk(&format!("{}\n", line)));
    }

    // Exactly one Usage event, sourced solely from result.usage — the
    // per-step usage on the agent_response step must not be summed in.
    let usage_events: Vec<&AgentEventPayload> = all_events
        .iter()
        .filter(|e| matches!(e, AgentEventPayload::Usage { .. }))
        .collect();
    assert_eq!(usage_events.len(), 1, "expected exactly one Usage event, got {:?}", usage_events);
    assert!(matches!(
        usage_events[0],
        AgentEventPayload::Usage {
            input_tokens: 500,
            output_tokens: 50,
            cache_read_tokens: 100,
            cache_creation_tokens: 0,
            total_tokens: 550,
        }
    ));

    // No fatal Error — a tool ERROR is not a stream-level error, and
    // result.status is SUCCESS.
    assert!(!all_events.iter().any(|e| matches!(e, AgentEventPayload::Error { .. })));

    // Both tool calls correlate start <-> completion via the same id.
    let started_ids: Vec<String> = all_events
        .iter()
        .filter_map(|e| match e {
            AgentEventPayload::ToolCallStarted { tool_use_id, .. } => tool_use_id.clone(),
            _ => None,
        })
        .collect();
    let completed: Vec<(String, bool)> = all_events
        .iter()
        .filter_map(|e| match e {
            AgentEventPayload::ToolCallCompleted { tool_use_id, is_error, .. } => {
                Some((tool_use_id.clone().expect("id set"), *is_error))
            }
            _ => None,
        })
        .collect();
    assert_eq!(started_ids.len(), 2);
    assert_eq!(completed.len(), 2);
    assert_eq!(started_ids[0], completed[0].0);
    assert!(!completed[0].1, "list_permissions call must complete without error");
    assert_eq!(started_ids[1], completed[1].0);
    assert!(completed[1].1, "list_dir call must complete with error");

    // finalize completes cleanly: no leftover buffered text (empty
    // result.response, nothing streamed via agent_response deltas) and no
    // stray Error.
    let final_events = normalizer.finalize(Some(0), "");
    assert!(
        final_events.is_empty(),
        "expected a clean finalize with nothing buffered, got {:?}",
        final_events
    );
}
