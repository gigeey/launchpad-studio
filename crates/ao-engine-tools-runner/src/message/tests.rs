use serde_json::json;

use super::{ContentBlock, Message};

// ---------------------------------------------------------------------------
// ContentBlock round-trips
// ---------------------------------------------------------------------------

#[test]
fn content_block_text_round_trips() {
    let block = ContentBlock::Text { text: "hello world".into() };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json, json!({"type": "text", "text": "hello world"}));
    let back: ContentBlock = serde_json::from_value(json).unwrap();
    assert_eq!(back, block);
}

#[test]
fn content_block_tool_use_round_trips() {
    let block = ContentBlock::ToolUse {
        id: "tu_abc123".into(),
        name: "read_file".into(),
        input: json!({"path": "/etc/hosts", "limit": 100}),
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "tool_use");
    assert_eq!(json["id"], "tu_abc123");
    assert_eq!(json["name"], "read_file");
    let back: ContentBlock = serde_json::from_value(json).unwrap();
    assert_eq!(back, block);
}

#[test]
fn content_block_tool_result_round_trips() {
    let block = ContentBlock::ToolResult {
        tool_use_id: "tu_abc123".into(),
        content: "file contents here".into(),
        is_error: false,
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "tool_result");
    assert_eq!(json["tool_use_id"], "tu_abc123");
    assert_eq!(json["is_error"], false);
    let back: ContentBlock = serde_json::from_value(json).unwrap();
    assert_eq!(back, block);
}

#[test]
fn content_block_tool_result_error_flag_round_trips() {
    let block = ContentBlock::ToolResult {
        tool_use_id: "tu_err".into(),
        content: "permission denied".into(),
        is_error: true,
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["is_error"], true);
    let back: ContentBlock = serde_json::from_value(json).unwrap();
    assert_eq!(back, block);
}

#[test]
fn content_block_thinking_with_text_and_signature_round_trips() {
    // The full shape: provider sent both reasoning text and a signature
    // (the typical `display = "summarized"` or `"raw"` case).
    let block = ContentBlock::Thinking {
        text: Some("Let me think about this step by step.".into()),
        signature: Some("sig_abc123==".into()),
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "thinking");
    assert_eq!(json["text"], "Let me think about this step by step.");
    assert_eq!(json["signature"], "sig_abc123==");
    let back: ContentBlock = serde_json::from_value(json).unwrap();
    assert_eq!(back, block);
}

#[test]
fn content_block_thinking_signature_only_round_trips() {
    // The `display = "omitted"` case: signature without text. Anthropic's
    // multi-turn replay rule still requires this block in the transcript
    // when the same turn emitted any tool_use.
    let block = ContentBlock::Thinking {
        text: None,
        signature: Some("sig_only==".into()),
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "thinking");
    let back: ContentBlock = serde_json::from_value(json).unwrap();
    assert_eq!(back, block);
}

#[test]
fn content_block_redacted_thinking_round_trips() {
    // The provider withheld the reasoning text and returned an opaque blob.
    // The block must survive serialisation byte-for-byte so it can be
    // replayed verbatim on the next tool-using turn.
    let block = ContentBlock::RedactedThinking {
        data: "EmwKAhgBEgy3va3pzix/LafPsn4aDFIT2Xlxh0L5L8rgVKIwR5r6".into(),
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "redacted_thinking");
    assert_eq!(
        json["data"],
        "EmwKAhgBEgy3va3pzix/LafPsn4aDFIT2Xlxh0L5L8rgVKIwR5r6"
    );
    let back: ContentBlock = serde_json::from_value(json).unwrap();
    assert_eq!(back, block);
}

// ---------------------------------------------------------------------------
// Message round-trips
// ---------------------------------------------------------------------------

#[test]
fn message_system_round_trips() {
    let msg = Message::System { content: "You are a helpful assistant.".into() };
    let json = serde_json::to_value(&msg).unwrap();
    assert_eq!(json["role"], "system");
    assert_eq!(json["content"], "You are a helpful assistant.");
    let back: Message = serde_json::from_value(json).unwrap();
    assert_eq!(back, msg);
}

#[test]
fn message_user_round_trips() {
    let msg = Message::User {
        content: vec![ContentBlock::Text { text: "What is the capital of France?".into() }],
    };
    let json = serde_json::to_value(&msg).unwrap();
    assert_eq!(json["role"], "user");
    assert_eq!(json["content"][0]["type"], "text");
    let back: Message = serde_json::from_value(json).unwrap();
    assert_eq!(back, msg);
}

#[test]
fn message_assistant_round_trips() {
    let msg = Message::Assistant {
        content: vec![ContentBlock::Text { text: "Paris is the capital of France.".into() }],
    };
    let json = serde_json::to_value(&msg).unwrap();
    assert_eq!(json["role"], "assistant");
    let back: Message = serde_json::from_value(json).unwrap();
    assert_eq!(back, msg);
}

#[test]
fn message_tool_result_round_trips() {
    let msg = Message::ToolResult {
        tool_use_id: "tu_xyz".into(),
        content: vec![ContentBlock::Text { text: "42".into() }],
        is_error: false,
    };
    let json = serde_json::to_value(&msg).unwrap();
    assert_eq!(json["role"], "tool_result");
    assert_eq!(json["tool_use_id"], "tu_xyz");
    assert_eq!(json["is_error"], false);
    let back: Message = serde_json::from_value(json).unwrap();
    assert_eq!(back, msg);
}

// ---------------------------------------------------------------------------
// Multi-block and linkage tests
// ---------------------------------------------------------------------------

#[test]
fn assistant_with_text_and_tool_use_blocks_round_trips() {
    let msg = Message::Assistant {
        content: vec![
            ContentBlock::Text { text: "I'll look that up for you.".into() },
            ContentBlock::ToolUse {
                id: "tu_1".into(),
                name: "search".into(),
                input: json!({"query": "capital of France"}),
            },
        ],
    };
    let json = serde_json::to_value(&msg).unwrap();
    assert_eq!(json["role"], "assistant");
    let content = json["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "tool_use");
    assert_eq!(content[1]["id"], "tu_1");
    let back: Message = serde_json::from_value(json).unwrap();
    assert_eq!(back, msg);
}

#[test]
fn user_with_mixed_text_and_tool_result_blocks_round_trips() {
    let msg = Message::User {
        content: vec![
            ContentBlock::Text { text: "Here is the result:".into() },
            ContentBlock::ToolResult {
                tool_use_id: "tu_1".into(),
                content: "Paris".into(),
                is_error: false,
            },
        ],
    };
    let json = serde_json::to_value(&msg).unwrap();
    assert_eq!(json["role"], "user");
    let content = json["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "tool_result");
    assert_eq!(content[1]["tool_use_id"], "tu_1");
    let back: Message = serde_json::from_value(json).unwrap();
    assert_eq!(back, msg);
}

#[test]
fn assistant_tool_use_and_tool_result_linkage_round_trips() {
    let tool_id = "tu_link_test";

    // Assistant turn emits a ToolUse block
    let assistant_msg = Message::Assistant {
        content: vec![ContentBlock::ToolUse {
            id: tool_id.into(),
            name: "read_file".into(),
            input: json!({"path": "/tmp/data.txt"}),
        }],
    };

    // Runner responds with a ToolResult message referencing the same id
    let result_msg = Message::ToolResult {
        tool_use_id: tool_id.into(),
        content: vec![ContentBlock::Text { text: "file data here".into() }],
        is_error: false,
    };

    let assistant_json = serde_json::to_value(&assistant_msg).unwrap();
    let result_json = serde_json::to_value(&result_msg).unwrap();

    // The id in the ToolUse block matches the tool_use_id in the ToolResult message
    assert_eq!(
        assistant_json["content"][0]["id"],
        result_json["tool_use_id"],
        "tool_use_id linkage broken across round-trip"
    );

    let back_assistant: Message = serde_json::from_value(assistant_json).unwrap();
    let back_result: Message = serde_json::from_value(result_json).unwrap();
    assert_eq!(back_assistant, assistant_msg);
    assert_eq!(back_result, result_msg);
}

// ---------------------------------------------------------------------------
// strip_closed_turn_reasoning
// ---------------------------------------------------------------------------

use super::strip_closed_turn_reasoning;

fn thinking() -> ContentBlock {
    ContentBlock::Thinking { text: Some("reason".into()), signature: Some("sig==".into()) }
}

fn redacted() -> ContentBlock {
    ContentBlock::RedactedThinking { data: "blob==".into() }
}

fn text(t: &str) -> ContentBlock {
    ContentBlock::Text { text: t.into() }
}

#[test]
fn strip_removes_reasoning_from_closed_text_only_turn() {
    // The exact shape that triggered the 400: a text-only assistant turn
    // (reconstructed with its thinking block) followed by a fresh user turn.
    let mut messages = vec![
        Message::User { content: vec![text("hi")] },
        Message::Assistant { content: vec![thinking(), text("hello")] },
        Message::User { content: vec![text("again")] },
    ];
    strip_closed_turn_reasoning(&mut messages);

    assert_eq!(
        messages[1],
        Message::Assistant { content: vec![text("hello")] },
        "closed assistant turn must lose its thinking block"
    );
}

#[test]
fn strip_keeps_reasoning_on_active_tool_cycle_turn() {
    // Assistant turn that emitted tool_use and is answered by a tool_result on
    // the next message — this is the active cycle, reasoning must survive.
    let mut messages = vec![
        Message::Assistant {
            content: vec![
                thinking(),
                redacted(),
                ContentBlock::ToolUse { id: "c1".into(), name: "Echo".into(), input: json!({}) },
            ],
        },
        Message::ToolResult {
            tool_use_id: "c1".into(),
            content: vec![text("ok")],
            is_error: false,
        },
        Message::Assistant { content: vec![thinking(), text("done")] },
    ];
    strip_closed_turn_reasoning(&mut messages);

    // Cycle turn keeps all three blocks in order.
    assert_eq!(
        messages[0],
        Message::Assistant {
            content: vec![
                thinking(),
                redacted(),
                ContentBlock::ToolUse { id: "c1".into(), name: "Echo".into(), input: json!({}) },
            ],
        },
        "active-cycle assistant turn must preserve reasoning verbatim"
    );
    // The trailing closed turn is stripped.
    assert_eq!(messages[2], Message::Assistant { content: vec![text("done")] });
}

#[test]
fn strip_removes_reasoning_from_final_assistant_turn() {
    // A transcript ending on an assistant turn (no following message) is closed.
    let mut messages = vec![
        Message::User { content: vec![text("hi")] },
        Message::Assistant { content: vec![redacted(), text("bye")] },
    ];
    strip_closed_turn_reasoning(&mut messages);
    assert_eq!(messages[1], Message::Assistant { content: vec![text("bye")] });
}

#[test]
fn strip_drops_reasoning_only_turn_that_becomes_empty() {
    // A reasoning-only assistant turn (no text, no tool_use) would serialise to
    // content:[] after the strip — drop it entirely.
    let mut messages = vec![
        Message::User { content: vec![text("hi")] },
        Message::Assistant { content: vec![thinking()] },
        Message::User { content: vec![text("again")] },
    ];
    strip_closed_turn_reasoning(&mut messages);
    assert_eq!(messages.len(), 2, "empty assistant turn must be removed");
    assert!(matches!(messages[0], Message::User { .. }));
    assert!(matches!(messages[1], Message::User { .. }));
}

#[test]
fn strip_is_noop_when_no_reasoning_present() {
    let mut messages = vec![
        Message::User { content: vec![text("hi")] },
        Message::Assistant { content: vec![text("hello")] },
    ];
    let before = messages.clone();
    strip_closed_turn_reasoning(&mut messages);
    assert_eq!(messages, before);
}

// ---------------------------------------------------------------------------
// truncate_to_context_budget
// ---------------------------------------------------------------------------

use super::truncate_to_context_budget;

#[test]
fn truncate_returns_input_unchanged_when_budget_is_none() {
    let messages = vec![
        Message::User { content: vec![text("hi")] },
        Message::Assistant { content: vec![text("hello")] },
    ];
    let out = truncate_to_context_budget(&messages, None);
    assert_eq!(out, messages);
}

#[test]
fn truncate_keeps_everything_when_under_budget() {
    let messages = vec![
        Message::User { content: vec![text("hi")] },
        Message::Assistant { content: vec![text("hello")] },
    ];
    let out = truncate_to_context_budget(&messages, Some(10_000));
    assert_eq!(out, messages);
}

#[test]
fn truncate_drops_oldest_message_first_under_a_tight_budget() {
    let long = "x".repeat(400);
    let messages = vec![
        Message::User { content: vec![text(&long)] },
        Message::Assistant { content: vec![text(&long)] },
        Message::User { content: vec![text("most recent")] },
    ];
    let out = truncate_to_context_budget(&messages, Some(20));
    assert!(out.len() < messages.len(), "tight budget must drop at least one message");
    assert_eq!(
        out.last(),
        messages.last(),
        "the most recent message must always survive"
    );
}

#[test]
fn truncate_never_produces_an_empty_result_even_when_the_last_message_alone_exceeds_budget() {
    let long = "x".repeat(4000);
    let messages = vec![Message::User { content: vec![text(&long)] }];
    // Budget far smaller than even one message's estimated cost.
    let out = truncate_to_context_budget(&messages, Some(1));
    assert_eq!(out.len(), 1, "the sole message must be kept regardless of budget");
}

#[test]
fn truncate_keeps_tool_use_and_its_tool_result_together_as_one_atomic_group() {
    // An Assistant tool_use message must never be dropped without its
    // immediately-following ToolResult(s) — splitting them produces a
    // request Anthropic's API rejects (an orphaned tool_result).
    let long = "x".repeat(400);
    let messages = vec![
        Message::User { content: vec![text(&long)] },
        Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "Echo".into(),
                input: json!({}),
            }],
        },
        Message::ToolResult {
            tool_use_id: "c1".into(),
            content: vec![text("ok")],
            is_error: false,
        },
        Message::User { content: vec![text("most recent")] },
    ];
    // Budget tight enough to force dropping the oldest group, but generous
    // enough that if the tool_use/tool_result pair were split (kept
    // separately) this would still fit — the assertion below checks they
    // survive or are dropped as one unit, not that they're dropped at all.
    let out = truncate_to_context_budget(&messages, Some(15));
    let has_tool_use = out.iter().any(|m| matches!(m, Message::Assistant { content } if content.iter().any(|b| matches!(b, ContentBlock::ToolUse { .. }))));
    let has_tool_result = out.iter().any(|m| matches!(m, Message::ToolResult { .. }));
    assert_eq!(
        has_tool_use, has_tool_result,
        "tool_use and its tool_result must be kept or dropped together, never split"
    );
}
