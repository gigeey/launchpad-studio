//! Unit tests for conversation history storage.
//!
//! Declared from the parent module as `#[cfg(test)] mod tests;` — this is
//! the same module as the inline `mod tests` block it replaces, so private
//! items of the parent remain in scope here via `use super::*`.

use super::*;
use anchor::{AnchorKey, AnchorRotated, WindowAnchorRegistry};
use ao_persistence::{paths::DataRoot, PersistenceLayer};
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use chrono::Utc;
use std::sync::Arc;
use tempfile::TempDir;

async fn make_persistence(tmp: &TempDir) -> PersistenceLayer {
    PersistenceLayer::init_with_root(DataRoot::new(tmp.path()))
        .await
        .expect("persistence init")
}

fn make_entry(content: &str) -> TranscriptEntry {
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("user".to_string()),
        content: content.to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    }
}

async fn write_entries(
    persistence: &PersistenceLayer,
    agent_id: &str,
    entries: &[TranscriptEntry],
) {
    for entry in entries {
        persistence
            .transcripts
            .append(agent_id, entry)
            .await
            .expect("append");
    }
}

// --- Personal source tests ---

#[tokio::test]
async fn personal_persisted_true_excludes_last() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let entries = vec![make_entry("msg1"), make_entry("msg2"), make_entry("msg3")];
    write_entries(&p, "agent1", &entries).await;

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal {
                agent_id: "agent1".to_string(),
            },
            current_message_already_persisted: true,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    // Last entry ("msg3") excluded; msg1 and msg2 returned
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].content, "msg1");
    assert_eq!(result[1].content, "msg2");
}

#[tokio::test]
async fn personal_persisted_false_includes_all() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let entries = vec![make_entry("msg1"), make_entry("msg2")];
    write_entries(&p, "agent1", &entries).await;

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal {
                agent_id: "agent1".to_string(),
            },
            current_message_already_persisted: false,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    assert_eq!(result.len(), 2);
}

// --- TeamShared source tests ---

#[tokio::test]
async fn team_shared_persisted_true_excludes_last() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let key = "team_team1";
    let entries = vec![make_entry("team-msg1"), make_entry("team-msg2")];
    for entry in &entries {
        p.transcripts.append(key, entry).await.expect("append");
    }

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::TeamShared {
                team_id: "team1".to_string(),
            },
            current_message_already_persisted: true,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].content, "team-msg1");
}

#[tokio::test]
async fn team_shared_persisted_false_includes_all() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let key = "team_team1";
    let entries = vec![make_entry("team-msg1"), make_entry("team-msg2")];
    for entry in &entries {
        p.transcripts.append(key, entry).await.expect("append");
    }

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::TeamShared {
                team_id: "team1".to_string(),
            },
            current_message_already_persisted: false,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    assert_eq!(result.len(), 2);
}

// --- TeamPerAgent source tests ---

#[tokio::test]
async fn team_per_agent_persisted_true_excludes_last() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let key = "team_teamA_agentX";
    let entries = vec![make_entry("child-msg1"), make_entry("child-msg2")];
    for entry in &entries {
        p.transcripts.append(key, entry).await.expect("append");
    }

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::TeamPerAgent {
                team_id: "teamA".to_string(),
                agent_id: "agentX".to_string(),
            },
            current_message_already_persisted: true,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].content, "child-msg1");
}

#[tokio::test]
async fn team_per_agent_persisted_false_includes_all() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let key = "team_teamA_agentX";
    let entries = vec![make_entry("child-msg1"), make_entry("child-msg2")];
    for entry in &entries {
        p.transcripts.append(key, entry).await.expect("append");
    }

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::TeamPerAgent {
                team_id: "teamA".to_string(),
                agent_id: "agentX".to_string(),
            },
            current_message_already_persisted: false,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    assert_eq!(result.len(), 2);
}

// --- TasklistPath source tests ---

#[tokio::test]
async fn tasklist_path_persisted_false_includes_all() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let path = tmp.path().join("tasklist.jsonl");
    let entries = vec![make_entry("tl-msg1"), make_entry("tl-msg2")];
    for entry in &entries {
        p.transcripts
            .append_at(&path, entry)
            .await
            .expect("append_at");
    }

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::TasklistPath { path },
            current_message_already_persisted: false,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    assert_eq!(result.len(), 2);
}

#[tokio::test]
async fn tasklist_path_persisted_true_excludes_last() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let path = tmp.path().join("tasklist.jsonl");
    let entries = vec![make_entry("tl-msg1"), make_entry("tl-msg2")];
    for entry in &entries {
        p.transcripts
            .append_at(&path, entry)
            .await
            .expect("append_at");
    }

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::TasklistPath { path },
            current_message_already_persisted: true,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].content, "tl-msg1");
}

// --- Empty transcript tests ---

#[tokio::test]
async fn empty_transcript_personal_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal {
                agent_id: "nonexistent".to_string(),
            },
            current_message_already_persisted: true,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    assert!(result.is_empty());
}

#[tokio::test]
async fn empty_transcript_team_shared_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::TeamShared {
                team_id: "ghost-team".to_string(),
            },
            current_message_already_persisted: false,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    assert!(result.is_empty());
}

// --- Single-entry with persisted=true returns empty (only entry was tail-excluded) ---

#[tokio::test]
async fn single_entry_persisted_true_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    write_entries(&p, "agent1", &[make_entry("only-msg")]).await;

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal {
                agent_id: "agent1".to_string(),
            },
            current_message_already_persisted: true,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    assert!(result.is_empty());
}

// ── to_messages unit tests ──────────────────────────────────────────────

use ao_engine_tools_runner::message::{ContentBlock, Message};
use std::collections::HashMap;

fn make_user_msg(content: &str) -> TranscriptEntry {
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("user".to_string()),
        content: content.to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    }
}

fn make_schedule_msg(content: &str) -> TranscriptEntry {
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Schedule { task_id: "task-1".to_string() },
        content: content.to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    }
}

fn make_response(content: &str, turn_id: &str) -> TranscriptEntry {
    let mut m = HashMap::new();
    m.insert("turn_id".to_string(), serde_json::Value::String(turn_id.to_string()));
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Agent { agent: "agent".to_string() },
        content: content.to_string(),
        event_type: "response".to_string(),
        metadata: Some(m),
        hidden_from_user: false,
    }
}

fn make_tool_use_entry(id: &str, name: &str, input: serde_json::Value, turn_id: &str) -> TranscriptEntry {
    let mut m = HashMap::new();
    m.insert("tool_use_id".to_string(), serde_json::Value::String(id.to_string()));
    m.insert("tool_name".to_string(), serde_json::Value::String(name.to_string()));
    m.insert("input".to_string(), input);
    m.insert("turn_id".to_string(), serde_json::Value::String(turn_id.to_string()));
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Agent { agent: "agent".to_string() },
        content: String::new(),
        event_type: "tool_use".to_string(),
        metadata: Some(m),
        hidden_from_user: false,
    }
}

fn make_tool_result_entry(tool_use_id: &str, output: &str, is_error: bool, turn_id: &str) -> TranscriptEntry {
    let mut m = HashMap::new();
    m.insert("tool_use_id".to_string(), serde_json::Value::String(tool_use_id.to_string()));
    m.insert("output".to_string(), serde_json::Value::String(output.to_string()));
    m.insert("is_error".to_string(), serde_json::Value::Bool(is_error));
    m.insert("turn_id".to_string(), serde_json::Value::String(turn_id.to_string()));
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("tool".to_string()),
        content: String::new(),
        event_type: "tool_result".to_string(),
        metadata: Some(m),
        hidden_from_user: false,
    }
}

#[test]
fn to_messages_user_message_maps_to_user() {
    let msgs = to_messages(&[make_user_msg("hello")], None, None);
    assert_eq!(msgs.len(), 1);
    assert_eq!(
        msgs[0],
        Message::User { content: vec![ContentBlock::Text { text: "hello".to_string() }] }
    );
}

#[test]
fn to_messages_schedule_message_maps_to_user() {
    let msgs = to_messages(&[make_schedule_msg("scheduled")], None, None);
    assert_eq!(msgs.len(), 1);
    assert_eq!(
        msgs[0],
        Message::User { content: vec![ContentBlock::Text { text: "scheduled".to_string() }] }
    );
}

#[test]
fn to_messages_response_maps_to_assistant_text() {
    let msgs = to_messages(&[make_response("hello from assistant", "t1")], None, None);
    assert_eq!(msgs.len(), 1);
    assert_eq!(
        msgs[0],
        Message::Assistant {
            content: vec![ContentBlock::Text { text: "hello from assistant".to_string() }],
        }
    );
}

#[test]
fn to_messages_tool_use_maps_to_assistant_tool_use() {
    let input = serde_json::json!({"path": "/tmp/foo"});
    // Paired with its tool_result so the orphan-filter doesn't drop it.
    let msgs = to_messages(&[
        make_tool_use_entry("tu-1", "Read", input.clone(), "t1"),
        make_tool_result_entry("tu-1", "file body", false, "t1"),
    ], None, None);
    assert_eq!(msgs.len(), 2);
    assert_eq!(
        msgs[0],
        Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "tu-1".to_string(),
                name: "Read".to_string(),
                input,
            }],
        }
    );
}

#[test]
fn to_messages_tool_result_maps_to_tool_result() {
    // Paired with its tool_use so the orphan-filter doesn't drop it.
    let msgs = to_messages(&[
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "t1"),
        make_tool_result_entry("tu-1", "file content", false, "t1"),
    ], None, None);
    assert_eq!(msgs.len(), 2);
    assert_eq!(
        msgs[1],
        Message::ToolResult {
            tool_use_id: "tu-1".to_string(),
            content: vec![ContentBlock::Text { text: "file content".to_string() }],
            is_error: false,
        }
    );
}

#[test]
fn to_messages_same_turn_id_coalesces_into_single_assistant() {
    let input = serde_json::json!({"k": "v"});
    let entries = vec![
        make_response("thinking", "turn-abc"),
        make_tool_use_entry("tu-1", "Bash", input.clone(), "turn-abc"),
        // Pair the tool_use so it survives the orphan filter.
        make_tool_result_entry("tu-1", "ok", false, "turn-abc"),
    ];
    let msgs = to_messages(&entries, None, None);
    assert_eq!(msgs.len(), 2);
    assert_eq!(
        msgs[0],
        Message::Assistant {
            content: vec![
                ContentBlock::Text { text: "thinking".to_string() },
                ContentBlock::ToolUse { id: "tu-1".to_string(), name: "Bash".to_string(), input },
            ],
        }
    );
}

#[test]
fn to_messages_missing_turn_id_produces_separate_assistant_messages() {
    let entries = vec![
        TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::Agent { agent: "a".to_string() },
            content: "first".to_string(),
            event_type: "response".to_string(),
            metadata: None,
            hidden_from_user: false,
        },
        TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::Agent { agent: "a".to_string() },
            content: "second".to_string(),
            event_type: "response".to_string(),
            metadata: None,
            hidden_from_user: false,
        },
    ];
    let msgs = to_messages(&entries, None, None);
    assert_eq!(msgs.len(), 2, "missing turn_id must not coalesce");
}

#[test]
fn to_messages_mismatched_turn_id_produces_separate_assistant_messages() {
    let entries = vec![
        make_response("turn1 text", "turn-1"),
        make_tool_use_entry("tu-2", "Read", serde_json::json!({}), "turn-2"),
        // Pair tu-2 so it survives the orphan filter.
        make_tool_result_entry("tu-2", "ok", false, "turn-2"),
    ];
    let msgs = to_messages(&entries, None, None);
    assert_eq!(msgs.len(), 3, "different turn_ids must not coalesce");
}

#[test]
fn to_messages_unknown_event_type_is_dropped() {
    let entries = vec![
        make_user_msg("hello"),
        TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::System("user".to_string()),
            content: "ignored".to_string(),
            event_type: "unknown_custom_event".to_string(),
            metadata: None,
            hidden_from_user: false,
        },
        make_response("hi", "t1"),
    ];
    let msgs = to_messages(&entries, None, None);
    assert_eq!(msgs.len(), 2, "unknown events must be dropped; expected user+assistant");
}

// ── Pair-preservation regression tests ─────────────────────────────────
//
// Bug context: a live agent transcript hit
//   `messages.0.content.0: tool_use_ids were found in tool_result blocks`
// after a multi-tool turn pushed the entries-per-window count past the
// 20-entry `active_message_count`. The window happened to start on a
// `tool_result` whose paired `tool_use` was exactly one entry outside.

/// Slicer must walk start leftward when the window begins on a `tool_result`,
/// so the paired `tool_use` stays in scope and the resulting messages array
/// satisfies the Anthropic pairing constraint.
#[tokio::test]
async fn slicer_expands_window_past_leading_tool_result() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;

    // Transcript: 4 entries where the natural window of size 2 would begin
    // on the tool_result at index 2. With expansion, start walks to index 1
    // (the matching tool_use).
    let entries = vec![
        make_response("preamble", "turn-a"),
        make_tool_use_entry("tu-pair", "Read", serde_json::json!({"p": "/x"}), "turn-a"),
        make_tool_result_entry("tu-pair", "data", false, "turn-a"),
        make_response("postscript", "turn-a"),
    ];
    for e in &entries {
        p.transcripts.append("agent-slicer", e).await.expect("append");
    }

    // Force a small window. We use a 1-minute active window so that 1
    // minute of staleness drops us into `same_day_message_count = 2`.
    let now = entries.last().unwrap().ts + chrono::Duration::minutes(5);
    let config = ContextConfig {
        active_window_minutes: 1,
        same_day_message_count: 2,
        ..ContextConfig::default()
    };

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal {
                agent_id: "agent-slicer".to_string(),
            },
            current_message_already_persisted: false,
            now,
            config,
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    assert!(
        result.iter().any(|e| e.event_type == "tool_use"),
        "expansion must keep the paired tool_use in the slice (got events: {:?})",
        result.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
    assert!(
        result[0].event_type != "tool_result",
        "slice must not begin on an orphan tool_result"
    );
}

/// Translator pre-pass must drop a `tool_result` block whose `tool_use_id`
/// has no matching `tool_use` earlier in the slice (left-edge orphan, e.g.
/// when the slicer expansion could not recover the pair).
#[test]
fn to_messages_drops_orphan_tool_result() {
    let entries = vec![
        make_tool_result_entry("orphan-tu", "stale output", false, "old-turn"),
        make_response("response after", "new-turn"),
    ];
    let msgs = to_messages(&entries, None, None);
    // Orphan tool_result dropped → only the response survives.
    assert_eq!(msgs.len(), 1);
    assert_eq!(
        msgs[0],
        Message::Assistant {
            content: vec![ContentBlock::Text { text: "response after".to_string() }],
        }
    );
}

/// Translator pre-pass must drop a `tool_use` block whose `tool_use_id`
/// has no matching `tool_result` later in the slice (right-edge orphan,
/// e.g. when the runner crashed between dispatch and result persistence).
#[test]
fn to_messages_drops_orphan_tool_use() {
    let entries = vec![
        make_response("preamble", "turn-1"),
        make_tool_use_entry("never-resolved", "Bash", serde_json::json!({}), "turn-1"),
        // No matching tool_result.
        make_user_msg("follow-up"),
    ];
    let msgs = to_messages(&entries, None, None);
    // Orphan tool_use dropped → preamble Assistant message + User follow-up.
    assert_eq!(msgs.len(), 2);
    assert_eq!(
        msgs[0],
        Message::Assistant {
            content: vec![ContentBlock::Text { text: "preamble".to_string() }],
        }
    );
    assert_eq!(
        msgs[1],
        Message::User {
            content: vec![ContentBlock::Text { text: "follow-up".to_string() }],
        }
    );
}

/// Coalesced-turn invariant: when a turn has a Text block + a tool_use
/// that gets filtered as orphan, the Assistant message must still emit
/// with the Text block (not an empty `content: []`).
#[test]
fn to_messages_orphan_tool_use_does_not_empty_assistant_message() {
    let entries = vec![
        make_response("hello", "turn-x"),
        make_tool_use_entry("orphan", "Bash", serde_json::json!({}), "turn-x"),
    ];
    let msgs = to_messages(&entries, None, None);
    assert_eq!(msgs.len(), 1);
    match &msgs[0] {
        Message::Assistant { content } => {
            assert_eq!(content.len(), 1);
            assert!(matches!(content[0], ContentBlock::Text { .. }));
        }
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

/// Build a `thinking` transcript entry using the TB-4 `block_json` format.
fn make_thinking_entry(
    text: &str,
    signature: &str,
    model: Option<&str>,
    key_fingerprint: Option<&str>,
    turn_id: &str,
) -> TranscriptEntry {
    let mut m = HashMap::new();
    m.insert("turn_id".to_string(), serde_json::Value::String(turn_id.to_string()));
    let block = ContentBlock::Thinking {
        text: if text.is_empty() { None } else { Some(text.to_string()) },
        signature: if signature.is_empty() { None } else { Some(signature.to_string()) },
    };
    m.insert("block_json".to_string(), serde_json::to_value(&block).unwrap());
    if let Some(model) = model {
        m.insert("model".to_string(), serde_json::Value::String(model.to_string()));
    }
    if let Some(fp) = key_fingerprint {
        m.insert("key_fingerprint".to_string(), serde_json::Value::String(fp.to_string()));
    }
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Agent { agent: "agent".to_string() },
        content: String::new(),
        event_type: "thinking".to_string(),
        metadata: Some(m),
        hidden_from_user: false,
    }
}

/// Build a `thinking` transcript entry using the legacy split-field format
/// (pre-TB-4). Used to exercise the backward-compatible read path.
fn make_thinking_entry_legacy(
    text: &str,
    signature: &str,
    model: Option<&str>,
    key_fingerprint: Option<&str>,
    turn_id: &str,
) -> TranscriptEntry {
    let mut m = HashMap::new();
    m.insert("turn_id".to_string(), serde_json::Value::String(turn_id.to_string()));
    m.insert("thinking_text".to_string(), serde_json::Value::String(text.to_string()));
    m.insert("signature".to_string(), serde_json::Value::String(signature.to_string()));
    if let Some(model) = model {
        m.insert("model".to_string(), serde_json::Value::String(model.to_string()));
    }
    if let Some(fp) = key_fingerprint {
        m.insert("key_fingerprint".to_string(), serde_json::Value::String(fp.to_string()));
    }
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Agent { agent: "agent".to_string() },
        content: String::new(),
        event_type: "thinking".to_string(),
        metadata: Some(m),
        hidden_from_user: false,
    }
}

/// A reasoning block authored by the same model and key survives reconstruction.
#[test]
fn to_messages_keeps_thinking_block_when_model_matches() {
    let entries = vec![
        make_thinking_entry("reasoning", "sig-abc", Some("model-1"), Some("fp-A"), "turn-1"),
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "turn-1"),
        make_tool_result_entry("tu-1", "body", false, "turn-1"),
    ];
    let msgs = to_messages(&entries, Some("model-1"), Some("fp-A"));
    assert_eq!(msgs.len(), 2);
    match &msgs[0] {
        Message::Assistant { content } => assert!(
            matches!(content.first(), Some(ContentBlock::Thinking { .. })),
            "thinking block must lead the assistant turn, got {:?}",
            content
        ),
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

/// A reasoning block authored by a *different* model is dropped — replaying
/// its model-bound signature is a hard 400. The tool_use/tool_result pair
/// from the same turn is untouched.
#[test]
fn to_messages_drops_thinking_block_on_model_mismatch() {
    let entries = vec![
        make_thinking_entry("reasoning", "sig-abc", Some("model-1"), Some("fp-A"), "turn-1"),
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "turn-1"),
        make_tool_result_entry("tu-1", "body", false, "turn-1"),
    ];
    let msgs = to_messages(&entries, Some("model-2"), Some("fp-A"));
    assert_eq!(msgs.len(), 2);
    match &msgs[0] {
        Message::Assistant { content } => assert!(
            !content.iter().any(|b| matches!(b, ContentBlock::Thinking { .. })),
            "cross-model thinking block must be dropped, got {:?}",
            content
        ),
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

/// An untagged (legacy) reasoning block cannot be attributed to a model or key,
/// so it is dropped rather than risk replaying a stale signature.
#[test]
fn to_messages_drops_untagged_thinking_block() {
    let entries = vec![
        make_thinking_entry("reasoning", "sig-abc", None, None, "turn-1"),
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "turn-1"),
        make_tool_result_entry("tu-1", "body", false, "turn-1"),
    ];
    let msgs = to_messages(&entries, Some("model-1"), Some("fp-A"));
    match &msgs[0] {
        Message::Assistant { content } => assert!(
            !content.iter().any(|b| matches!(b, ContentBlock::Thinking { .. })),
            "untagged thinking block must be dropped, got {:?}",
            content
        ),
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

/// When the resuming model is unknown we cannot prove a match, so even a
/// tagged block is dropped — safety over reasoning continuity.
#[test]
fn to_messages_drops_thinking_block_when_current_model_unknown() {
    let entries = vec![
        make_thinking_entry("reasoning", "sig-abc", Some("model-1"), Some("fp-A"), "turn-1"),
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "turn-1"),
        make_tool_result_entry("tu-1", "body", false, "turn-1"),
    ];
    let msgs = to_messages(&entries, None, Some("fp-A"));
    match &msgs[0] {
        Message::Assistant { content } => assert!(
            !content.iter().any(|b| matches!(b, ContentBlock::Thinking { .. })),
            "thinking block must be dropped when current model is unknown, got {:?}",
            content
        ),
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

/// A block authored under key A must be dropped when replayed under key B,
/// even when the model is the same — Anthropic signatures are bound to both.
#[test]
fn to_messages_drops_thinking_block_on_key_fingerprint_mismatch() {
    let entries = vec![
        make_thinking_entry("reasoning", "sig-abc", Some("model-1"), Some("fp-A"), "turn-1"),
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "turn-1"),
        make_tool_result_entry("tu-1", "body", false, "turn-1"),
    ];
    // Same model, different key fingerprint.
    let msgs = to_messages(&entries, Some("model-1"), Some("fp-B"));
    assert_eq!(msgs.len(), 2);
    match &msgs[0] {
        Message::Assistant { content } => assert!(
            !content.iter().any(|b| matches!(b, ContentBlock::Thinking { .. })),
            "thinking block authored under key A must be dropped when replayed under key B, got {:?}",
            content
        ),
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

/// A block with no key_fingerprint tag (legacy, authored before TB-1) is
/// dropped when a current fingerprint is known — cannot verify provenance.
#[test]
fn to_messages_drops_thinking_block_with_no_key_fingerprint_tag() {
    // Block has a model tag but no key_fingerprint tag (legacy transcript).
    let entries = vec![
        make_thinking_entry("reasoning", "sig-abc", Some("model-1"), None, "turn-1"),
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "turn-1"),
        make_tool_result_entry("tu-1", "body", false, "turn-1"),
    ];
    let msgs = to_messages(&entries, Some("model-1"), Some("fp-A"));
    match &msgs[0] {
        Message::Assistant { content } => assert!(
            !content.iter().any(|b| matches!(b, ContentBlock::Thinking { .. })),
            "unfingerprinted block must be dropped when key fingerprint is known, got {:?}",
            content
        ),
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

// ── TB-4: literal-block storage and legacy fallback ──────────────────────

/// New-format (TB-4) `block_json` entries round-trip through `to_messages`
/// with the signature preserved byte-identically. This verifies the primary
/// write+read path introduced by TB-4.
#[test]
fn to_messages_thinking_block_literal_block_json_round_trips() {
    let original_sig = "Lit/Era+lB==";
    let entries = vec![
        make_thinking_entry("literal reasoning", original_sig, Some("m1"), Some("fp-1"), "t1"),
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "t1"),
        make_tool_result_entry("tu-1", "body", false, "t1"),
    ];
    let msgs = to_messages(&entries, Some("m1"), Some("fp-1"));
    assert_eq!(msgs.len(), 2);
    match &msgs[0] {
        Message::Assistant { content } => match content.first() {
            Some(ContentBlock::Thinking { text, signature }) => {
                assert_eq!(text.as_deref(), Some("literal reasoning"));
                assert_eq!(
                    signature.as_deref(),
                    Some(original_sig),
                    "signature must be byte-identical through block_json round-trip"
                );
            }
            other => panic!("expected Thinking block first, got {:?}", other),
        },
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

/// Legacy split-field entries (pre-TB-4: `thinking_text` + `signature` in
/// metadata, no `block_json`) are reconstructed correctly via the fallback
/// path. This ensures existing persisted transcripts remain readable.
#[test]
fn to_messages_thinking_block_legacy_split_fields_fall_back_correctly() {
    let original_sig = "Legacy/Sig==";
    let entries = vec![
        make_thinking_entry_legacy("legacy reasoning", original_sig, Some("m1"), Some("fp-1"), "t1"),
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "t1"),
        make_tool_result_entry("tu-1", "body", false, "t1"),
    ];
    let msgs = to_messages(&entries, Some("m1"), Some("fp-1"));
    assert_eq!(msgs.len(), 2);
    match &msgs[0] {
        Message::Assistant { content } => match content.first() {
            Some(ContentBlock::Thinking { text, signature }) => {
                assert_eq!(text.as_deref(), Some("legacy reasoning"), "fallback path must preserve text");
                assert_eq!(
                    signature.as_deref(),
                    Some(original_sig),
                    "fallback path must preserve signature byte-identically"
                );
            }
            other => panic!("expected Thinking block, got {:?}", other),
        },
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

/// A `block_json` field that cannot be deserialized as `ContentBlock` is
/// skipped with a warning rather than panicking.
#[test]
fn to_messages_thinking_block_malformed_block_json_is_skipped() {
    let mut m = HashMap::new();
    m.insert("turn_id".to_string(), serde_json::Value::String("t1".to_string()));
    m.insert("block_json".to_string(), serde_json::json!({"type": "unknown_type", "x": 1}));
    m.insert("model".to_string(), serde_json::Value::String("m1".to_string()));
    m.insert("key_fingerprint".to_string(), serde_json::Value::String("fp-1".to_string()));
    let thinking_entry = TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Agent { agent: "agent".to_string() },
        content: String::new(),
        event_type: "thinking".to_string(),
        metadata: Some(m),
        hidden_from_user: false,
    };
    let entries = vec![
        thinking_entry,
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "t1"),
        make_tool_result_entry("tu-1", "body", false, "t1"),
    ];
    let msgs = to_messages(&entries, Some("m1"), Some("fp-1"));
    // Malformed block_json skipped → only the tool_use/result pair survives.
    match &msgs[0] {
        Message::Assistant { content } => assert!(
            !content.iter().any(|b| matches!(b, ContentBlock::Thinking { .. })),
            "malformed block_json must be skipped, not included"
        ),
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

// ── Default-model resolve-and-match tests (TB-3) ───────────────────────
//
// When an agent profile has `model = None`, the engine resolves None →
// provider.default_model() before calling to_messages. The tests below
// verify the behaviour of to_messages once that resolution has happened —
// i.e. `current_model` carries the concrete resolved string, not None.

/// A reasoning block authored under the provider's resolved default model X
/// must be kept when the resuming run also resolves to model X. This is the
/// happy path for default-model agents persisting and replaying reasoning.
#[test]
fn to_messages_default_model_resolved_keeps_block_on_same_model() {
    let resolved = "claude-opus-4-7";
    let entries = vec![
        make_thinking_entry("reasoning", "sig-xyz", Some(resolved), Some("fp-A"), "turn-1"),
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "turn-1"),
        make_tool_result_entry("tu-1", "body", false, "turn-1"),
    ];
    // Simulate: engine resolved agent.model=None → provider.default_model()="claude-opus-4-7"
    let msgs = to_messages(&entries, Some(resolved), Some("fp-A"));
    assert_eq!(msgs.len(), 2);
    match &msgs[0] {
        Message::Assistant { content } => assert!(
            matches!(content.first(), Some(ContentBlock::Thinking { .. })),
            "default-model reasoning block must be kept when resolved model matches, got {:?}",
            content
        ),
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

/// When the provider's default model changed between the authoring run and
/// the resume run, the resolved models differ and the block must be dropped.
#[test]
fn to_messages_default_model_resolved_drops_block_on_different_model() {
    let entries = vec![
        make_thinking_entry("reasoning", "sig-xyz", Some("claude-opus-4-7"), Some("fp-A"), "turn-1"),
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "turn-1"),
        make_tool_result_entry("tu-1", "body", false, "turn-1"),
    ];
    // Simulate: provider's default changed between author run and resume run.
    let msgs = to_messages(&entries, Some("claude-opus-4-8"), Some("fp-A"));
    assert_eq!(msgs.len(), 2);
    match &msgs[0] {
        Message::Assistant { content } => assert!(
            !content.iter().any(|b| matches!(b, ContentBlock::Thinking { .. })),
            "reasoning block must be dropped when resolved model differs, got {:?}",
            content
        ),
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

/// TB-2: the `signature` field is preserved byte-for-byte through the full
/// persist → reconstruct path. A block that survives model+key validation
/// must carry an *identical* signature string — not merely non-empty, but
/// character-for-character equal to the value stored in the transcript entry.
/// The base64-special chars (/, +, =) in the fixture catch encoding transforms
/// that would truncate or rewrite individual bytes.
#[test]
fn to_messages_thinking_block_signature_bytes_identical_after_reconstruct() {
    let original_sig = "EqRsT7uV8w/Xy+ZaB==";
    let entries = vec![
        make_thinking_entry("some reasoning", original_sig, Some("model-1"), Some("fp-A"), "turn-1"),
        // Active tool-use cycle keeps the block alive past strip_closed_turn_reasoning.
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "turn-1"),
        make_tool_result_entry("tu-1", "body", false, "turn-1"),
    ];
    let msgs = to_messages(&entries, Some("model-1"), Some("fp-A"));
    assert_eq!(msgs.len(), 2);
    match &msgs[0] {
        Message::Assistant { content } => {
            let block = content.first().expect("assistant turn must contain a block");
            match block {
                ContentBlock::Thinking { signature, .. } => {
                    assert_eq!(
                        signature.as_deref(),
                        Some(original_sig),
                        "signature must be byte-for-byte identical after reconstruct; got {:?}",
                        signature
                    );
                }
                other => panic!("expected Thinking block, got {:?}", other),
            }
        }
        other => panic!("expected Assistant message, got {:?}", other),
    }
}

// ── Window anchor integration tests ────────────────────────────────────

fn make_fresh_registry() -> Arc<WindowAnchorRegistry> {
    Arc::new(WindowAnchorRegistry::new())
}

/// First call with a registry pins a Fresh anchor and returns `target` entries.
#[tokio::test]
async fn first_call_pins_anchor_and_returns_target_slice() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let registry = make_fresh_registry();

    let entries: Vec<_> = (0..25).map(|i| make_entry(&format!("msg-{}", i))).collect();
    write_entries(&p, "agent-anchor", &entries).await;

    let now = entries.last().unwrap().ts + chrono::Duration::seconds(5);
    let (result, signal) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-anchor".to_string() },
            current_message_already_persisted: false,
            now,
            config: ContextConfig::default(), // active_message_count = 20
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;

    assert_eq!(result.len(), 20, "first call should return target (20) entries");
    assert_eq!(signal, Some(AnchorRotated::Fresh), "first pin must emit Fresh");
    let key = AnchorKey::Personal("agent-anchor".to_string());
    assert!(registry.get(&key).is_some(), "anchor must be stored in registry");
}

/// Second call (within grace) returns the same floor index — CACHE HIT, no signal.
#[tokio::test]
async fn second_call_within_grace_returns_same_floor() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let registry = make_fresh_registry();

    let entries: Vec<_> = (0..25).map(|i| make_entry(&format!("msg-{}", i))).collect();
    write_entries(&p, "agent-cache", &entries).await;

    let now = entries.last().unwrap().ts + chrono::Duration::seconds(5);
    let (result1, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-cache".to_string() },
            current_message_already_persisted: false,
            now,
            config: ContextConfig::default(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;

    // Append one new entry and call again.
    let new_entry = make_entry("new-msg");
    p.transcripts.append("agent-cache", &new_entry).await.expect("append");

    let (result2, signal2) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-cache".to_string() },
            current_message_already_persisted: false,
            now: new_entry.ts + chrono::Duration::seconds(1),
            config: ContextConfig::default(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;

    assert!(signal2.is_none(), "second call within grace must be a cache hit (None signal)");
    // Verify byte-prefix stability by comparing content strings (TranscriptEntry: no PartialEq).
    let prefix_contents: Vec<&str> = result2[..result1.len()].iter().map(|e| e.content.as_str()).collect();
    let result1_contents: Vec<&str> = result1.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(prefix_contents, result1_contents, "byte-prefix must be identical across turns");
}

/// After appending enough entries to exceed max_window, the floor rotates.
#[tokio::test]
async fn growth_past_max_window_rotates_floor() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let registry = make_fresh_registry();

    let config = ContextConfig {
        active_message_count: 5,
        anchor_grace: 2, // max_window = 5*2+2 = 12
        ..ContextConfig::default()
    };

    let entries: Vec<_> = (0..10).map(|i| make_entry(&format!("msg-{}", i))).collect();
    write_entries(&p, "agent-grow", &entries).await;

    let now = entries.last().unwrap().ts + chrono::Duration::seconds(1);
    let (_, sig1) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-grow".to_string() },
            current_message_already_persisted: false,
            now,
            config: config.clone(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert_eq!(sig1, Some(AnchorRotated::Fresh));

    // Add 13 more entries to push past max_window (12).
    for i in 10..23_i32 {
        let e = make_entry(&format!("extra-{}", i));
        p.transcripts.append("agent-grow", &e).await.expect("append");
    }

    let (_, sig2) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-grow".to_string() },
            current_message_already_persisted: false,
            now: Utc::now() + chrono::Duration::seconds(2),
            config: config.clone(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert_eq!(sig2, Some(AnchorRotated::Rotated), "must rotate when past max_window");

    // Third call: post-rotation cache hit.
    let (_, sig3) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-grow".to_string() },
            current_message_already_persisted: false,
            now: Utc::now() + chrono::Duration::seconds(3),
            config,
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert!(sig3.is_none(), "post-rotation call must be a cache hit");
}

/// Recording stub for [`ReflectionTriggerSubscriber`] — captures every
/// trigger it receives so trigger tests can assert on reason/agent_id/count.
struct RecordingReflectionSubscriber {
    seen: std::sync::Mutex<Vec<ReflectionTrigger>>,
}

impl RecordingReflectionSubscriber {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn snapshot(&self) -> Vec<ReflectionTrigger> {
        self.seen.lock().unwrap().clone()
    }
}

impl ReflectionTriggerSubscriber for RecordingReflectionSubscriber {
    fn on_reflection_trigger(&self, trigger: ReflectionTrigger) {
        self.seen.lock().unwrap().push(trigger);
    }
}

/// The sharpest cue: a `select` call that rotates the anchor floor must
/// fire exactly one `AnchorRotated` reflection trigger, and a call that
/// only pins (`Fresh`) or hits the cache must fire none — mirrors
/// [`growth_past_max_window_rotates_floor`] with a subscriber attached.
#[tokio::test]
async fn anchor_rotation_fires_reflection_trigger() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let registry = make_fresh_registry();
    let subscriber = RecordingReflectionSubscriber::new();

    let config = ContextConfig {
        active_message_count: 5,
        anchor_grace: 2, // max_window = 5*2+2 = 12
        ..ContextConfig::default()
    };

    let entries: Vec<_> = (0..10).map(|i| make_entry(&format!("msg-{}", i))).collect();
    write_entries(&p, "agent-reflect", &entries).await;

    let now = entries.last().unwrap().ts + chrono::Duration::seconds(1);
    let (_, sig1) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-reflect".to_string() },
            current_message_already_persisted: false,
            now,
            config: config.clone(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: Some(subscriber.clone()),
        },
    )
    .await;
    assert_eq!(sig1, Some(AnchorRotated::Fresh));
    assert!(
        subscriber.snapshot().is_empty(),
        "the first pin (Fresh) is not a rotation and must not fire a trigger"
    );

    // Add 13 more entries to push past max_window (12) and force a rotation.
    for i in 10..23_i32 {
        let e = make_entry(&format!("extra-{}", i));
        p.transcripts.append("agent-reflect", &e).await.expect("append");
    }

    let (_, sig2) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-reflect".to_string() },
            current_message_already_persisted: false,
            now: Utc::now() + chrono::Duration::seconds(2),
            config: config.clone(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: Some(subscriber.clone()),
        },
    )
    .await;
    assert_eq!(sig2, Some(AnchorRotated::Rotated));

    let seen = subscriber.snapshot();
    assert_eq!(seen.len(), 1, "rotation must fire exactly one trigger");
    assert_eq!(seen[0].reason, ReflectionTriggerReason::AnchorRotated);
    assert_eq!(seen[0].agent_id, "agent-reflect");
    assert_eq!(
        seen[0].transcript_path,
        p.data_root.agent_transcript_path("agent-reflect").to_string_lossy()
    );

    // Third call: post-rotation cache hit — no further trigger.
    let (_, sig3) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-reflect".to_string() },
            current_message_already_persisted: false,
            now: Utc::now() + chrono::Duration::seconds(3),
            config,
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: Some(subscriber.clone()),
        },
    )
    .await;
    assert!(sig3.is_none(), "post-rotation call must be a cache hit");
    assert_eq!(
        subscriber.snapshot().len(),
        1,
        "a cache-hit call must not fire an additional trigger"
    );
}

/// The secondary cue: a thread whose last message is already older than
/// `active_window_minutes` fires an `IdleTimeout` trigger the next time
/// it's dispatched, independent of whether the anchor also rotates.
#[tokio::test]
async fn idle_past_active_window_fires_reflection_trigger() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let subscriber = RecordingReflectionSubscriber::new();

    let config = ContextConfig {
        active_window_minutes: 120,
        ..ContextConfig::default()
    };

    let entries = vec![make_entry("only message")];
    write_entries(&p, "agent-idle", &entries).await;

    // Still within the active window — must not fire IdleTimeout.
    let now_within = entries[0].ts + chrono::Duration::minutes(119);
    let (_, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-idle".to_string() },
            current_message_already_persisted: false,
            now: now_within,
            config: config.clone(),
            anchor_registry: None,
            reflection_subscriber: Some(subscriber.clone()),
        },
    )
    .await;
    assert!(
        subscriber.snapshot().is_empty(),
        "dispatch within the active window must not fire IdleTimeout"
    );

    // Past the active window — must fire IdleTimeout.
    let now_past = entries[0].ts + chrono::Duration::minutes(121);
    let (_, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-idle".to_string() },
            current_message_already_persisted: false,
            now: now_past,
            config,
            anchor_registry: None,
            reflection_subscriber: Some(subscriber.clone()),
        },
    )
    .await;
    let seen = subscriber.snapshot();
    assert_eq!(seen.len(), 1, "idle-past-window dispatch must fire exactly one trigger");
    assert_eq!(seen[0].reason, ReflectionTriggerReason::IdleTimeout);
    assert_eq!(seen[0].agent_id, "agent-idle");
}

/// Team/tasklist/project history sources aren't backed by a `Thread` row
/// and its `distilled_through_ts` watermark, so the trigger deliberately skips
/// them rather than emit a trigger with a made-up identity.
#[tokio::test]
async fn non_thread_sources_never_fire_reflection_triggers() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let subscriber = RecordingReflectionSubscriber::new();

    let entries = vec![make_entry("m1"), make_entry("m2")];
    for e in &entries {
        p.transcripts.append("team_t1", e).await.expect("append");
    }

    // Idle-past-window `now` so IdleTimeout WOULD fire if this source were eligible.
    let now = entries.last().unwrap().ts + chrono::Duration::minutes(200);
    let (_, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::TeamShared { team_id: "t1".to_string() },
            current_message_already_persisted: false,
            now,
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: Some(subscriber.clone()),
        },
    )
    .await;
    assert!(
        subscriber.snapshot().is_empty(),
        "TeamShared is out of the trigger's scope and must never fire"
    );
}

/// A `compute_message_count` drop (time decay) does NOT force rotation when the
/// anchor window is still within `pinned_target * 2 + grace`.
#[tokio::test]
async fn time_decay_target_drop_does_not_force_rotation_within_grace() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let registry = make_fresh_registry();

    let config = ContextConfig {
        active_window_minutes: 1,
        active_message_count: 10,
        same_day_message_count: 4,
        anchor_grace: 4, // pinned_target=10; max_window = 10*2+4 = 24
        ..ContextConfig::default()
    };

    let base_ts = Utc::now() - chrono::Duration::minutes(2);
    let entries: Vec<TranscriptEntry> = (0..15_i64).map(|i| TranscriptEntry {
        ts: base_ts + chrono::Duration::seconds(i),
        role: TranscriptRole::System("user".to_string()),
        content: format!("old-msg-{}", i),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    }).collect();
    write_entries(&p, "agent-decay", &entries).await;

    // First call: within active window → target=10, anchor pinned at index 5.
    let now_active = base_ts + chrono::Duration::seconds(30);
    let (_, sig1) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-decay".to_string() },
            current_message_already_persisted: false,
            now: now_active,
            config: config.clone(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert_eq!(sig1, Some(AnchorRotated::Fresh));
    let key = AnchorKey::Personal("agent-decay".to_string());
    assert_eq!(registry.get(&key).unwrap().pinned_target, 10);

    // Second call: 2 min later → target drops to 4 (same_day). But
    // pinned_target(10)*2+4=24, and slice.len()-floor_idx=15-5=10 ≤ 24 → CACHE HIT.
    let now_stale = entries.last().unwrap().ts + chrono::Duration::minutes(2);
    let (_, sig2) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-decay".to_string() },
            current_message_already_persisted: false,
            now: now_stale,
            config,
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert!(sig2.is_none(), "time decay within grace must not force rotation; got {:?}", sig2);
}

/// Pair-preservation walk runs AFTER the anchor lookup decides the floor index.
#[tokio::test]
async fn pair_preservation_walk_runs_after_anchor_lookup() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let registry = make_fresh_registry();

    let entries = vec![
        make_response("preamble", "turn-a"),
        make_tool_use_entry("tu-1", "Read", serde_json::json!({}), "turn-a"),
        make_tool_result_entry("tu-1", "data", false, "turn-a"),
        make_response("postscript", "turn-b"),
    ];
    for e in &entries {
        p.transcripts.append("agent-pair", e).await.expect("append");
    }

    // Force target=2 so the naive start would land on tool_result at index 2.
    let now = entries.last().unwrap().ts + chrono::Duration::minutes(5);
    let config = ContextConfig {
        active_window_minutes: 1,
        same_day_message_count: 2,
        ..ContextConfig::default()
    };

    let (result, signal) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-pair".to_string() },
            current_message_already_persisted: false,
            now,
            config,
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;

    assert!(
        result.iter().any(|e| e.event_type == "tool_use"),
        "pair-preservation walk must keep the paired tool_use in slice"
    );
    assert_ne!(result[0].event_type, "tool_result", "slice must not begin on an orphan tool_result");
    assert_eq!(signal, Some(AnchorRotated::Fresh));
}

/// `FloorMarker` computed from a re-read entry is identical to the one computed
/// at pin time — verifying that disk round-trips don't corrupt the marker.
#[tokio::test]
async fn floor_marker_survives_re_read() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let registry = make_fresh_registry();

    let entries: Vec<_> = (0..5).map(|i| make_entry(&format!("msg-{}", i))).collect();
    write_entries(&p, "agent-persist", &entries).await;

    let now = entries.last().unwrap().ts + chrono::Duration::seconds(5);
    let (_, sig1) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-persist".to_string() },
            current_message_already_persisted: false,
            now,
            config: ContextConfig::default(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert_eq!(sig1, Some(AnchorRotated::Fresh));

    // Re-read (no new entries) — should be a cache hit on the re-read marker.
    let (_, sig2) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-persist".to_string() },
            current_message_already_persisted: false,
            now: now + chrono::Duration::seconds(1),
            config: ContextConfig::default(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert!(sig2.is_none(), "re-read must hit the stored marker (no rotation)");
}

/// The rotation signal is `Some(Fresh)` on first pin, `Some(Rotated)` on rotation,
/// and `None` on cache hit (verified explicitly at the call site).
#[tokio::test]
async fn rotation_emits_anchor_rotated_signal_at_call_site() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let registry = make_fresh_registry();

    let config = ContextConfig {
        active_message_count: 3,
        anchor_grace: 0, // max_window = 3*2+0 = 6
        ..ContextConfig::default()
    };

    let entries: Vec<_> = (0..5).map(|i| make_entry(&format!("m{}", i))).collect();
    write_entries(&p, "agent-signal", &entries).await;

    let now = entries.last().unwrap().ts + chrono::Duration::seconds(1);
    let (_, s1) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-signal".to_string() },
            current_message_already_persisted: false,
            now,
            config: config.clone(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert_eq!(s1, Some(AnchorRotated::Fresh), "first call must emit Fresh");

    // Cache hit — no new entries.
    let (_, s2) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-signal".to_string() },
            current_message_already_persisted: false,
            now: now + chrono::Duration::seconds(1),
            config: config.clone(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert!(s2.is_none(), "cache hit must emit None");

    // Push past max_window.
    for i in 5..12_i32 {
        let e = make_entry(&format!("extra-{}", i));
        p.transcripts.append("agent-signal", &e).await.expect("append");
    }
    let (_, s3) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal { agent_id: "agent-signal".to_string() },
            current_message_already_persisted: false,
            now: Utc::now() + chrono::Duration::seconds(2),
            config,
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert_eq!(s3, Some(AnchorRotated::Rotated), "rotation must emit Rotated");
}

/// Personal, TeamShared, TeamPerAgent, and TasklistPath each get independent
/// anchors in the same registry — no scope leakage.
#[tokio::test]
async fn scope_keys_do_not_leak() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let registry = make_fresh_registry();

    // Write entries for all four scope types.
    let personal: Vec<_> = (0..5).map(|i| make_entry(&format!("p-{}", i))).collect();
    write_entries(&p, "scope-agent", &personal).await;

    let team_shared: Vec<_> = (0..5).map(|i| make_entry(&format!("ts-{}", i))).collect();
    for e in &team_shared { p.transcripts.append("team_scope-team", e).await.unwrap(); }

    let team_per_agent: Vec<_> = (0..5).map(|i| make_entry(&format!("tpa-{}", i))).collect();
    for e in &team_per_agent { p.transcripts.append("team_scope-team_scope-agent", e).await.unwrap(); }

    let tl_path = tmp.path().join("tasklist.jsonl");
    let tasklist: Vec<_> = (0..5).map(|i| make_entry(&format!("tl-{}", i))).collect();
    for e in &tasklist { p.transcripts.append_at(&tl_path, e).await.unwrap(); }

    let now = Utc::now() + chrono::Duration::seconds(1);
    let config = ContextConfig::default();

    macro_rules! sel {
        ($src:expr) => {
            select(
                &p,
                HistorySelectInput {
                    source: $src,
                    current_message_already_persisted: false,
                    now,
                    config: config.clone(),
                    anchor_registry: Some(Arc::clone(&registry)),
                    reflection_subscriber: None,
                },
            )
            .await
        };
    }

    // All four variants must get a Fresh anchor independently.
    let (_, sp) = sel!(HistorySource::Personal { agent_id: "scope-agent".to_string() });
    assert_eq!(sp, Some(AnchorRotated::Fresh), "Personal: expected Fresh");

    let (_, sts) = sel!(HistorySource::TeamShared { team_id: "scope-team".to_string() });
    assert_eq!(sts, Some(AnchorRotated::Fresh), "TeamShared: expected Fresh");

    let (_, stpa) = sel!(HistorySource::TeamPerAgent {
        team_id: "scope-team".to_string(),
        agent_id: "scope-agent".to_string(),
    });
    assert_eq!(stpa, Some(AnchorRotated::Fresh), "TeamPerAgent: expected Fresh");

    let (_, stl) = sel!(HistorySource::TasklistPath { path: tl_path.clone() });
    assert_eq!(stl, Some(AnchorRotated::Fresh), "TasklistPath: expected Fresh");

    // Second call for Personal must be a cache hit (others' anchors unaffected).
    let (_, sp2) = sel!(HistorySource::Personal { agent_id: "scope-agent".to_string() });
    assert!(sp2.is_none(), "Personal: expected cache hit on second call");
}

/// End-to-end regression: rebuild the exact shape that caused the
/// 2026-05-11 failure. A multi-tool turn followed by
/// later responses pushes entries past `active_message_count`; the slice
/// would naively begin on a `tool_result`. After the fix, the resulting
/// `Vec<Message>` must contain no `Message::ToolResult` at index 0.
#[tokio::test]
async fn regression_multi_tool_turn_does_not_leave_orphan_tool_result_at_messages_zero() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;

    // Build a transcript with enough entries that, after tail-exclusion,
    // a 20-entry window starts on a tool_result. We pad with 5 turns of
    // (msg, response) = 10 entries, then a 4-entry tool-using turn, then
    // 7 more single-entry turns, then the tail user msg (excluded).
    let mut entries: Vec<TranscriptEntry> = Vec::new();
    for i in 0..5 {
        entries.push(make_user_msg(&format!("pad-user-{}", i)));
        entries.push(make_response(&format!("pad-resp-{}", i), &format!("pad-turn-{}", i)));
    }
    // Index 10: tool_use_A
    // Index 11: tool_result_A
    // Index 12: tool_use_B  ← will be just outside the 20-entry window
    // Index 13: tool_result_B  ← would be slice[0] without expansion
    entries.push(make_tool_use_entry("tu-A", "WorkflowActionSkipPhase", serde_json::json!({}), "tt"));
    entries.push(make_tool_result_entry("tu-A", "skipped", false, "tt"));
    entries.push(make_tool_use_entry("tu-B", "WorkflowActionStart", serde_json::json!({}), "tt"));
    entries.push(make_tool_result_entry("tu-B", "started", false, "tt"));
    // Index 14..30: 17 more entries to push window-start past the tool turn
    for i in 0..16 {
        entries.push(make_response(&format!("trailing-{}", i), &format!("trailing-turn-{}", i)));
    }
    // Index 30: the user message that triggered the failed dispatch
    entries.push(make_user_msg("Are you able to recall?"));

    for e in &entries {
        p.transcripts.append("agent-regression", e).await.expect("append");
    }

    let last_ts = entries.last().unwrap().ts;
    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal {
                agent_id: "agent-regression".to_string(),
            },
            current_message_already_persisted: true,
            // Within `active_window_minutes` → n = 20.
            now: last_ts + chrono::Duration::seconds(10),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    let messages = to_messages(&result, None, None);

    // The bug: messages[0] was Message::ToolResult, which Anthropic rejects.
    match messages.first() {
        None => panic!("expected non-empty messages"),
        Some(Message::ToolResult { .. }) => {
            panic!("regression: messages[0] is an orphan ToolResult")
        }
        _ => {}
    }

    // Every ToolResult in the output must have a preceding ToolUse with
    // the same id in an earlier Assistant message.
    let mut seen_tool_use_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in &messages {
        match m {
            Message::Assistant { content } => {
                for block in content {
                    if let ContentBlock::ToolUse { id, .. } = block {
                        seen_tool_use_ids.insert(id.clone());
                    }
                }
            }
            Message::ToolResult { tool_use_id, .. } => {
                assert!(
                    seen_tool_use_ids.contains(tool_use_id),
                    "orphan ToolResult for {} (no preceding ToolUse)",
                    tool_use_id
                );
            }
            _ => {}
        }
    }
}

// ── Per-thread history selection ────────────────────────────────────────
//
// These tests pin the runtime contract for `HistorySource::PersonalThread`:
// each thread reads from its own JSONL transcript, a branch thread's
// `history_floor_ts` keeps pre-branch entries out of the live window, and
// resolving via the back-compat `Personal` variant continues to read the
// agent-keyed transcript byte-for-byte.

/// Fresh threads start with an empty transcript file — `select` must
/// return zero entries regardless of how busy the agent's default thread
/// is. This pins that fresh threads are truly forked from their parent
/// agent's chat history rather than sharing it.
#[tokio::test]
async fn personal_thread_fresh_returns_empty_initially() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;

    // Pile on entries to the agent's default file; the fresh thread
    // must not see any of them.
    let default_entries = vec![
        make_entry("default-1"),
        make_entry("default-2"),
        make_entry("default-3"),
    ];
    write_entries(&p, "agent-x", &default_entries).await;

    let fresh_path = p.data_root.thread_transcript_path("fresh-thread-1");
    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::PersonalThread {
                agent_id: "agent-x".to_string(),
                transcript_path: fresh_path,
                branch_source_path: None,
                history_floor_ts: None,
            },
            current_message_already_persisted: false,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    assert!(
        result.is_empty(),
        "fresh thread must start with no live entries; got {} entries",
        result.len()
    );
}

/// A branch thread filters out pre-branch entries from the live window
/// because `history_floor_ts` is set to the branch point. Entries with
/// `ts < floor` belong to the source thread and stay reachable only via
/// `RecallHistory`. This pins the "set floor" half of the contract.
#[tokio::test]
async fn personal_thread_branch_floor_excludes_pre_branch_entries() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;

    // Pre-branch entries in the SOURCE thread (the agent's default file).
    let base_ts = Utc::now() - chrono::Duration::seconds(120);
    let pre_branch: Vec<TranscriptEntry> = (0..3)
        .map(|i| TranscriptEntry {
            ts: base_ts + chrono::Duration::seconds(i),
            role: TranscriptRole::System("user".to_string()),
            content: format!("pre-{}", i),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        })
        .collect();
    write_entries(&p, "agent-branch", &pre_branch).await;

    // Branch off the latest pre-branch entry. The branch's own transcript
    // file is created next to the agent's, starting with two post-branch
    // entries that we want the live window to see.
    let branch_at = pre_branch.last().unwrap().ts;
    let branch_path = p.data_root.thread_transcript_path("branch-thread-1");
    let post_branch: Vec<TranscriptEntry> = (0..2)
        .map(|i| TranscriptEntry {
            ts: branch_at + chrono::Duration::seconds(10 + i),
            role: TranscriptRole::System("user".to_string()),
            content: format!("post-{}", i),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        })
        .collect();
    for e in &post_branch {
        p.transcripts.append_at(&branch_path, e).await.unwrap();
    }
    // Also seed an entry in the branch transcript that is below the floor,
    // simulating an accidental rewind: `select` must still drop it.
    let stray_below_floor = TranscriptEntry {
        ts: branch_at - chrono::Duration::seconds(1),
        role: TranscriptRole::System("user".to_string()),
        content: "stray-below-floor".to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    };
    p.transcripts
        .append_at(&branch_path, &stray_below_floor)
        .await
        .unwrap();

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::PersonalThread {
                agent_id: "agent-branch".to_string(),
                transcript_path: branch_path,
                branch_source_path: None,
                history_floor_ts: Some(branch_at),
            },
            current_message_already_persisted: false,
            now: branch_at + chrono::Duration::seconds(60),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    let contents: Vec<&str> = result.iter().map(|e| e.content.as_str()).collect();
    assert!(
        !contents.iter().any(|c| c.starts_with("pre-")),
        "pre-branch entries must not appear in the live window; got {:?}",
        contents
    );
    assert!(
        !contents.iter().any(|c| *c == "stray-below-floor"),
        "entries below the floor must be dropped; got {:?}",
        contents
    );
    assert_eq!(
        contents,
        vec!["post-0", "post-1"],
        "live window should be the branch's own post-floor entries"
    );
}

/// Branching MUST be a true fork at the file level — appending to the
/// branch's transcript leaves the source thread's bytes untouched. This
/// closes the loop on "branching off message N must NOT mutate the source
/// thread" by exercising the same JSONL writers the runner uses.
#[tokio::test]
async fn personal_thread_branch_writes_do_not_mutate_source_transcript() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;

    // Seed the source (default) thread.
    let source_entries = vec![make_entry("src-1"), make_entry("src-2")];
    write_entries(&p, "agent-source", &source_entries).await;

    let source_path = p.data_root.agent_transcript_path("agent-source");
    let bytes_before = tokio::fs::read(&source_path).await.unwrap();

    // Create a branch transcript and append several entries.
    let branch_path = p.data_root.thread_transcript_path("branch-fork");
    for i in 0..5 {
        let entry = TranscriptEntry {
            ts: Utc::now() + chrono::Duration::seconds(i),
            role: TranscriptRole::System("user".to_string()),
            content: format!("branch-{}", i),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        };
        p.transcripts.append_at(&branch_path, &entry).await.unwrap();
    }

    let bytes_after = tokio::fs::read(&source_path).await.unwrap();
    assert_eq!(
        bytes_before, bytes_after,
        "source transcript bytes must be byte-for-byte unchanged after \
             branch writes — branching is a true fork, not a copy-on-write"
    );

    // And the live window for the source thread still sees exactly its
    // original entries (no leakage from the branch file).
    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal {
                agent_id: "agent-source".to_string(),
            },
            current_message_already_persisted: false,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;
    let contents: Vec<&str> = result.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(contents, vec!["src-1", "src-2"]);
}

/// Resolving "the default thread" via the back-compat `Personal` variant
/// is byte-equivalent to the pre-thread single-thread read path: same
/// file, same entries, same order. This is the migration-compat guarantee
/// for callers that never pass a `thread_id`.
#[tokio::test]
async fn personal_default_thread_path_preserved_for_back_compat_callers() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;

    let entries = vec![make_entry("a"), make_entry("b"), make_entry("c")];
    write_entries(&p, "agent-bc", &entries).await;

    let (result, _) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::Personal {
                agent_id: "agent-bc".to_string(),
            },
            current_message_already_persisted: false,
            now: Utc::now(),
            config: ContextConfig::default(),
            anchor_registry: None,
            reflection_subscriber: None,
        },
    )
    .await;

    let contents: Vec<&str> = result.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(contents, vec!["a", "b", "c"]);
}

/// Anchor keys for two threads of the same agent must not collide — each
/// thread carries its own `WindowAnchor` so a turn in thread A cannot
/// rotate thread B's pinned floor.
#[tokio::test]
async fn personal_thread_anchor_keys_do_not_collide_across_threads() {
    let tmp = TempDir::new().unwrap();
    let p = make_persistence(&tmp).await;
    let registry = make_fresh_registry();

    let path_a = p.data_root.thread_transcript_path("thread-A");
    let path_b = p.data_root.thread_transcript_path("thread-B");
    for i in 0..3 {
        let e = make_entry(&format!("a-{}", i));
        p.transcripts.append_at(&path_a, &e).await.unwrap();
    }
    for i in 0..3 {
        let e = make_entry(&format!("b-{}", i));
        p.transcripts.append_at(&path_b, &e).await.unwrap();
    }

    let now = Utc::now() + chrono::Duration::seconds(1);

    let (_, sig_a) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::PersonalThread {
                agent_id: "agent-multi".to_string(),
                transcript_path: path_a.clone(),
                branch_source_path: None,
                history_floor_ts: None,
            },
            current_message_already_persisted: false,
            now,
            config: ContextConfig::default(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert_eq!(sig_a, Some(anchor::AnchorRotated::Fresh));

    let (_, sig_b) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::PersonalThread {
                agent_id: "agent-multi".to_string(),
                transcript_path: path_b.clone(),
                branch_source_path: None,
                history_floor_ts: None,
            },
            current_message_already_persisted: false,
            now,
            config: ContextConfig::default(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert_eq!(
        sig_b,
        Some(anchor::AnchorRotated::Fresh),
        "second thread must also pin its own Fresh anchor — not inherit \
             thread A's anchor"
    );

    // And A's anchor still hits on a re-read after B was pinned.
    let (_, sig_a2) = select(
        &p,
        HistorySelectInput {
            source: HistorySource::PersonalThread {
                agent_id: "agent-multi".to_string(),
                transcript_path: path_a,
                branch_source_path: None,
                history_floor_ts: None,
            },
            current_message_already_persisted: false,
            now: now + chrono::Duration::seconds(1),
            config: ContextConfig::default(),
            anchor_registry: Some(Arc::clone(&registry)),
            reflection_subscriber: None,
        },
    )
    .await;
    assert!(
        sig_a2.is_none(),
        "thread A's anchor must survive thread B's pin (cache hit, no \
             rotation)"
    );
}
