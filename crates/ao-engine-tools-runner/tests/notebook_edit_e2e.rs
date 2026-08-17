//! End-to-end integration test for NotebookEdit through the full runner stack.
//!
//! Exercises Read → NotebookEdit insert → Read round-trip via Registry,
//! RunnerContext, bounded executor, and ReadFileState propagation to prove
//! the tool is correctly wired from the agent's perspective.

use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::{DenialTracker, NoopDenialTracker, PermissionMode, Registry, RunnerContext, SessionKind};
use ao_engine_tools_io::register_all;
use ao_engine_tools_runner::hooks::config::RunnerSettings;
use ao_engine_tools_runner::prompt_bridge::{StubBridge, UserPromptBridge};
use ao_engine_tools_runner::message::{ContentBlock, Message};
use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};
use ao_engine_tools_runner::query_loop::{run_session, RunnerConfig, SessionOutcome};
use serde_json::{json, Value};
use tokio::time::timeout;

fn collect_tool_results(outcome: &SessionOutcome) -> Vec<Value> {
    outcome
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { tool_use_id, content, is_error } => {
                let content_str = content.iter().find_map(|b| {
                    if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None }
                }).unwrap_or("");
                // Parse as JSON for structured payloads; fall back to string.
                let content_val: Value = serde_json::from_str(content_str)
                    .unwrap_or_else(|_| Value::String(content_str.to_string()));
                Some(json!({
                    "tool_use_id": tool_use_id,
                    "content": content_val,
                    "is_error": is_error,
                }))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn read_then_insert_then_read_round_trips_through_runner() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let nb_path = tempdir.path().join("test.ipynb");

    // Seed a two-cell markdown notebook with ids 'a' and 'b'.
    let notebook_json = r#"{
 "nbformat": 4,
 "nbformat_minor": 5,
 "metadata": {},
 "cells": [
  {
   "cell_type": "markdown",
   "id": "a",
   "metadata": {},
   "source": "cell a content"
  },
  {
   "cell_type": "markdown",
   "id": "b",
   "metadata": {},
   "source": "cell b content"
  }
 ]
}"#;
    tokio::fs::write(&nb_path, notebook_json)
        .await
        .expect("write seed notebook");

    let nb_str = nb_path.to_str().unwrap().to_string();

    let mut registry = Registry::new();
    register_all(&mut registry);

    let runner_ctx = RunnerContext::new("session-nb-e2e", "agent-nb")
        .expect("ctx")
        .with_registry(Arc::new(registry));
    let ctx_clone = runner_ctx.clone();

    let script = vec![
        // Turn 1: Read — populates ReadFileState so the subsequent NotebookEdit
        // can pass the read-before-write gate.
        vec![
            CompletionEvent::ToolUse {
                id: "r1".into(),
                name: "Read".into(),
                input: json!({"file_path": nb_str}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 2: NotebookEdit insert before cell 'b'.
        vec![
            CompletionEvent::ToolUse {
                id: "ne1".into(),
                name: "NotebookEdit".into(),
                input: json!({
                    "notebook_path": nb_str,
                    "cell_id": "b",
                    "edit_mode": "insert",
                    "new_source": "inserted body",
                    "cell_type": "code"
                }),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 3: Re-read to observe the inserted cell.
        vec![
            CompletionEvent::ToolUse {
                id: "r2".into(),
                name: "Read".into(),
                input: json!({"file_path": nb_str}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 4: Final text turn (no tool uses → session exits).
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];

    let provider = Arc::new(MockProviderClient::new(script));
    let config = RunnerConfig {
        provider,
        bridge: Arc::new(StubBridge) as Arc<dyn UserPromptBridge>,
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings: RunnerSettings::default(),
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    };

    let outcome = timeout(
        Duration::from_secs(10),
        run_session(Vec::new(), runner_ctx, config),
    )
    .await
    .expect("session did not finish within 10 s")
    .expect("session ok");

    assert!(!outcome.cancelled);
    assert_eq!(outcome.turns, 4, "3 tool turns + 1 final text turn");

    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 3, "r1 + ne1 + r2");

    // ── r1: Read returned Text and populated read_file_state ──────────────────

    assert_eq!(results[0]["tool_use_id"], "r1");
    assert_eq!(results[0]["is_error"], false);
    let r1_content = results[0]["content"].as_str().expect("r1 content string");
    // cat-n output must contain the first cell's source.
    assert!(
        r1_content.contains("cell a content"),
        "r1 should contain 'cell a content', got: {r1_content}"
    );
    assert!(
        ctx_clone.read_file_state.get(&nb_path).is_some(),
        "read_file_state must contain the notebook path after the first Read"
    );

    // ── ne1: NotebookEdit insert succeeded ────────────────────────────────────

    assert_eq!(results[1]["tool_use_id"], "ne1");
    assert_eq!(results[1]["is_error"], false);
    let ne1_msg = results[1]["content"].as_str().expect("ne1 content string");
    assert!(
        ne1_msg.contains("Cell inserted before b"),
        "insert confirmation must mention 'Cell inserted before b', got: {ne1_msg}"
    );
    assert!(
        ne1_msg.contains(nb_str.as_str()),
        "insert confirmation must contain the notebook path, got: {ne1_msg}"
    );

    // ── r2: Re-read — the dedup stub must not mask the edit, and the
    //    structured notebook view must show the inserted cell in order ────────

    assert_eq!(results[2]["tool_use_id"], "r2");
    assert_eq!(results[2]["is_error"], false);
    let r2_content = results[2]["content"].as_str().expect("r2 content string");

    // The second Read runs after a NotebookEdit wrote the file. The edit leaves
    // an unsurfaced ReadFileState snapshot, so the dedup guard must fall through
    // and return real content rather than the "File unchanged" reminder.
    assert!(
        !r2_content.contains("File unchanged"),
        "re-read must surface post-edit content, not the dedup stub; got: {r2_content}"
    );

    // .ipynb reads return the structured cell-by-cell view (not raw JSON). The
    // inserted code cell must appear between the two original markdown cells.
    assert!(
        r2_content.contains("--- Cell 3 [markdown] ---"),
        "re-read must show 3 cells after insert; got: {r2_content}"
    );
    let pos_a = r2_content
        .find("cell a content")
        .expect("original cell a must be present in the re-read");
    let pos_inserted = r2_content
        .find("inserted body")
        .expect("inserted cell source must be present in the re-read");
    let pos_b = r2_content
        .find("cell b content")
        .expect("original cell b must be present in the re-read");
    assert!(
        pos_a < pos_inserted && pos_inserted < pos_b,
        "inserted cell must sit between cell a and cell b; got: {r2_content}"
    );

    // The structured view is a rendering; verify the NotebookEdit write itself
    // by parsing the on-disk notebook JSON directly.
    let on_disk = tokio::fs::read_to_string(&nb_path)
        .await
        .expect("read notebook from disk");
    let nb_value: Value =
        serde_json::from_str(&on_disk).expect("on-disk notebook must be valid JSON");

    let cells = nb_value["cells"].as_array().expect("cells must be a JSON array");
    assert_eq!(cells.len(), 3, "notebook must have 3 cells after insert");

    // cells[1] is the newly inserted code cell.
    assert_eq!(
        cells[1]["source"], "inserted body",
        "cells[1].source must be 'inserted body'"
    );
    assert_eq!(
        cells[1]["cell_type"], "code",
        "cells[1].cell_type must be 'code'"
    );
    assert_eq!(
        cells[1]["outputs"],
        json!([]),
        "cells[1].outputs must be an empty array"
    );

    // cells[2] is the original 'b' cell, now shifted to index 2.
    assert_eq!(
        cells[2]["id"], "b",
        "cells[2].id must be 'b' (original cell b shifted right by insert)"
    );
}
