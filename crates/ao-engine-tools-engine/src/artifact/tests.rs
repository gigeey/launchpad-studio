use std::sync::Arc;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_persistence::{artifact_store::ArtifactStore, paths::DataRoot};
use ao_protocol::artifact::IntentSource;
use serde_json::json;

use super::{resolve_intent_note, ArtifactWrite, INTENT_NOTE_FALLBACK_MAX_CHARS};

fn make_store(tmp: &tempfile::TempDir) -> Arc<ArtifactStore> {
    Arc::new(ArtifactStore::new(DataRoot::new(tmp.path())))
}

fn make_ctx(store: Arc<ArtifactStore>) -> RunnerContext {
    let cwd = std::env::temp_dir();
    RunnerContext::new_with_cwd("session-1", "agent-1", cwd).with_artifact_store(store)
}

fn as_structured(out: ToolOutput) -> serde_json::Value {
    match out {
        ToolOutput::Structured(v) => v,
        other => panic!("expected structured output, got {:?}", other),
    }
}

fn as_error(out: ToolOutput) -> (String, bool) {
    match out {
        ToolOutput::Error { message, recoverable } => (message, recoverable),
        other => panic!("expected error output, got {:?}", other),
    }
}

#[tokio::test]
async fn invalid_renderer_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = make_ctx(make_store(&tmp));

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "Bad renderer",
                "renderer": "timeline",
                "payload": {}
            }),
            &ctx,
        )
        .await
        .unwrap();

    let (message, recoverable) = as_error(out);
    assert!(message.contains("Invalid renderer"), "got: {message}");
    assert!(!recoverable);
}

#[tokio::test]
async fn object_payload_for_html_renderer_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = make_ctx(make_store(&tmp));

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "Bad pairing",
                "renderer": "html",
                "payload": { "not": "a string" }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let (message, recoverable) = as_error(out);
    assert!(message.contains("must be an HTML string"), "got: {message}");
    assert!(!recoverable);
}

#[tokio::test]
async fn string_payload_for_table_renderer_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = make_ctx(make_store(&tmp));

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "Bad pairing",
                "renderer": "table",
                "payload": "not an object"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let (message, recoverable) = as_error(out);
    assert!(message.contains("must be a JSON object"), "got: {message}");
    assert!(!recoverable);
}

#[tokio::test]
async fn table_columns_as_key_label_objects_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = make_ctx(make_store(&tmp));

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "Bad columns",
                "renderer": "table",
                "payload": {
                    "columns": [{"key": "task", "label": "Task"}, {"key": "owner", "label": "Owner"}],
                    "rows": [{"task": "Write docs", "owner": "Alex"}]
                }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let (message, recoverable) = as_error(out);
    assert!(message.contains("plain strings"), "got: {message}");
    assert!(message.contains("key, label"), "got: {message}");
    assert!(!recoverable);
}

#[tokio::test]
async fn table_missing_columns_or_rows_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = make_ctx(make_store(&tmp));

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "Missing rows",
                "renderer": "table",
                "payload": { "columns": ["Task", "Owner"] }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let (message, recoverable) = as_error(out);
    assert!(message.contains("'columns' array"), "got: {message}");
    assert!(message.contains("'rows' array"), "got: {message}");
    assert!(!recoverable);
}

#[tokio::test]
async fn table_well_formed_payload_is_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = make_ctx(make_store(&tmp));

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "Good table",
                "renderer": "table",
                "payload": {
                    "columns": ["Task", "Owner"],
                    "rows": [["Write docs", "Alex"], ["Fix bug", "Sam"]]
                }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let value = as_structured(out);
    assert_eq!(value["renderer"], json!("table"));
}

#[tokio::test]
async fn chart_series_without_values_array_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = make_ctx(make_store(&tmp));

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "Bad chart",
                "renderer": "chart",
                "payload": {
                    "labels": ["Mon", "Tue"],
                    "series": [{"name": "Visits"}]
                }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let (message, recoverable) = as_error(out);
    assert!(message.contains("'labels' array and a 'series' array"), "got: {message}");
    assert!(!recoverable);
}

#[tokio::test]
async fn whole_artifact_refresh_intent_without_refresh_prompt_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = make_ctx(make_store(&tmp));

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "Live dashboard",
                "renderer": "cards",
                "payload": { "cards": [] },
                "refresh_intent": "whole_artifact"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let (message, recoverable) = as_error(out);
    assert!(message.contains("refresh_prompt is required"), "got: {message}");
    assert!(!recoverable);
}

#[tokio::test]
async fn happy_path_persists_and_stamps_source_message_id() {
    let tmp = tempfile::tempdir().unwrap();
    let store = make_store(&tmp);
    let cwd = std::env::temp_dir();
    let ctx = RunnerContext::new_with_cwd("session-1", "agent-1", cwd)
        .with_artifact_store(store.clone())
        .with_current_message_id("msg-42");

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "Inbox highlights",
                "renderer": "list",
                "payload": { "items": ["a", "b"] }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let value = as_structured(out);
    let id = value["id"].as_str().expect("id present").to_string();
    assert_eq!(value["renderer"], json!("list"));
    assert_eq!(value["refresh_intent"], json!("none"));
    assert_eq!(value["title"], json!("Inbox highlights"));

    let record = store.get("agent-1", &id).await.unwrap();
    assert_eq!(record.title, "Inbox highlights");
    assert_eq!(record.source_message_id, Some("msg-42".to_string()));

    let (_record, bytes) = store.get_payload("agent-1", &id).await.unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(payload, json!({ "items": ["a", "b"] }));
}

#[tokio::test]
async fn update_by_id_changes_body_bumps_updated_at_preserves_pin_and_group() {
    let tmp = tempfile::tempdir().unwrap();
    let store = make_store(&tmp);
    let ctx = make_ctx(store.clone());

    let created = ArtifactWrite
        .invoke(
            json!({
                "title": "Inbox highlights",
                "renderer": "list",
                "payload": { "items": ["a", "b"] }
            }),
            &ctx,
        )
        .await
        .unwrap();
    let id = as_structured(created)["id"].as_str().unwrap().to_string();

    store.set_pinned("agent-1", &id, true).await.unwrap();
    store.set_group("agent-1", &id, Some("group-1".to_string())).await.unwrap();
    let before = store.get("agent-1", &id).await.unwrap();

    let out = ArtifactWrite
        .invoke(
            json!({
                "id": id,
                "title": "Inbox highlights",
                "renderer": "list",
                "payload": { "items": ["a", "b", "c"] }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let value = as_structured(out);
    assert_eq!(value["id"], json!(id));
    assert_eq!(value["renderer"], json!("list"));
    assert_eq!(value["title"], json!("Inbox highlights"));

    let after = store.get("agent-1", &id).await.unwrap();
    assert!(after.updated_at >= before.updated_at);
    assert_ne!(after.checksum_sha256, before.checksum_sha256);
    // Pin state and group membership survive the update untouched.
    assert!(after.pinned);
    assert_eq!(after.group_id.as_deref(), Some("group-1"));

    let (_record, bytes) = store.get_payload("agent-1", &id).await.unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(payload, json!({ "items": ["a", "b", "c"] }));

    // Only one artifact exists — the update did not spawn a duplicate.
    let all = store.list_by_agent("agent-1").await.unwrap();
    assert_eq!(all.len(), 1);
}

#[tokio::test]
async fn update_with_unknown_id_errors_without_creating() {
    let tmp = tempfile::tempdir().unwrap();
    let store = make_store(&tmp);
    let ctx = make_ctx(store.clone());

    let out = ArtifactWrite
        .invoke(
            json!({
                "id": "ghost-id",
                "title": "Inbox highlights",
                "renderer": "list",
                "payload": { "items": ["a"] }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let (message, recoverable) = as_error(out);
    assert!(message.contains("No artifact found with id 'ghost-id'"), "got: {message}");
    assert!(recoverable);

    let all = store.list_by_agent("agent-1").await.unwrap();
    assert!(all.is_empty(), "update with unknown id must not fall back to create");
}

#[tokio::test]
async fn update_with_mismatched_renderer_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let store = make_store(&tmp);
    let ctx = make_ctx(store.clone());

    let created = ArtifactWrite
        .invoke(
            json!({
                "title": "Good table",
                "renderer": "table",
                "payload": { "columns": ["Task"], "rows": [["Write docs"]] }
            }),
            &ctx,
        )
        .await
        .unwrap();
    let id = as_structured(created)["id"].as_str().unwrap().to_string();

    let out = ArtifactWrite
        .invoke(
            json!({
                "id": id,
                "title": "Good table",
                "renderer": "list",
                "payload": { "items": ["a"] }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let (message, recoverable) = as_error(out);
    assert!(message.contains("cannot change an artifact's renderer"), "got: {message}");
    assert!(recoverable);
}

#[tokio::test]
async fn create_path_is_unchanged_when_id_is_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = make_store(&tmp);
    let ctx = make_ctx(store.clone());

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "Inbox highlights",
                "renderer": "list",
                "payload": { "items": ["a", "b"] }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let value = as_structured(out);
    let id = value["id"].as_str().expect("id present").to_string();
    assert_eq!(value["renderer"], json!("list"));
    assert_eq!(value["refresh_intent"], json!("none"));
    assert_eq!(value["title"], json!("Inbox highlights"));

    let all = store.list_by_agent("agent-1").await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, id);
    assert_eq!(all[0].refresh_count, 0);
}

#[tokio::test]
async fn missing_store_returns_graceful_error() {
    let cwd = std::env::temp_dir();
    let ctx = RunnerContext::new_with_cwd("session-1", "agent-1", cwd);

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "No store",
                "renderer": "list",
                "payload": { "items": [] }
            }),
            &ctx,
        )
        .await
        .unwrap();

    let (message, recoverable) = as_error(out);
    assert!(message.contains("Artifact store not available"), "got: {message}");
    assert!(!recoverable);
}

#[tokio::test]
async fn create_seeds_one_intent_ledger_entry_with_source_create() {
    let tmp = tempfile::tempdir().unwrap();
    let store = make_store(&tmp);
    let ctx = RunnerContext::new_with_cwd("session-1", "agent-1", std::env::temp_dir())
        .with_artifact_store(store.clone())
        .with_current_message_id("msg-1");

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "Inbox highlights",
                "renderer": "list",
                "payload": { "items": ["a"] },
                "intent_note": "A running list of inbox highlights"
            }),
            &ctx,
        )
        .await
        .unwrap();

    // Output shape is untouched by the new field.
    let value = as_structured(out);
    let id = value["id"].as_str().expect("id present").to_string();
    assert_eq!(
        value.as_object().unwrap().keys().map(String::as_str).collect::<std::collections::BTreeSet<_>>(),
        ["id", "renderer", "refresh_intent", "title"].into_iter().collect()
    );

    let record = store.get("agent-1", &id).await.unwrap();
    assert_eq!(record.intent_ledger.len(), 1);
    assert_eq!(record.intent_ledger[0].source, IntentSource::Create);
    assert_eq!(
        record.intent_ledger[0].intent_note.as_deref(),
        Some("A running list of inbox highlights")
    );
    assert_eq!(record.intent_ledger[0].source_message_id.as_deref(), Some("msg-1"));
}

#[tokio::test]
async fn edit_in_place_appends_second_ledger_entry_preserving_first() {
    let tmp = tempfile::tempdir().unwrap();
    let store = make_store(&tmp);
    let ctx = RunnerContext::new_with_cwd("session-1", "agent-1", std::env::temp_dir())
        .with_artifact_store(store.clone())
        .with_current_message_id("msg-1");

    let created = ArtifactWrite
        .invoke(
            json!({
                "title": "Inbox highlights",
                "renderer": "list",
                "payload": { "items": ["a"] },
                "intent_note": "Track inbox highlights"
            }),
            &ctx,
        )
        .await
        .unwrap();
    let id = as_structured(created)["id"].as_str().unwrap().to_string();

    let edit_ctx = RunnerContext::new_with_cwd("session-1", "agent-1", std::env::temp_dir())
        .with_artifact_store(store.clone())
        .with_current_message_id("msg-2");

    ArtifactWrite
        .invoke(
            json!({
                "id": id,
                "title": "Inbox highlights",
                "renderer": "list",
                "payload": { "items": ["a", "b"] },
                "intent_note": "Add the second highlight"
            }),
            &edit_ctx,
        )
        .await
        .unwrap();

    let record = store.get("agent-1", &id).await.unwrap();
    assert_eq!(record.intent_ledger.len(), 2);

    // The creation entry survives untouched.
    assert_eq!(record.intent_ledger[0].source, IntentSource::Create);
    assert_eq!(record.intent_ledger[0].intent_note.as_deref(), Some("Track inbox highlights"));
    assert_eq!(record.intent_ledger[0].source_message_id.as_deref(), Some("msg-1"));

    // The edit appended its own entry, stamped from its own ctx.
    assert_eq!(record.intent_ledger[1].source, IntentSource::Chat);
    assert_eq!(record.intent_ledger[1].intent_note.as_deref(), Some("Add the second highlight"));
    assert_eq!(record.intent_ledger[1].source_message_id.as_deref(), Some("msg-2"));
}

#[tokio::test]
async fn omitting_intent_note_still_works_and_yields_none() {
    let tmp = tempfile::tempdir().unwrap();
    let store = make_store(&tmp);
    let ctx = make_ctx(store.clone());

    let out = ArtifactWrite
        .invoke(
            json!({
                "title": "No note",
                "renderer": "list",
                "payload": { "items": [] }
            }),
            &ctx,
        )
        .await
        .unwrap();

    // Old callers that never pass intent_note are unaffected — the call
    // still succeeds (the raw model-input field parses to `None`, per
    // `input.get("intent_note")` in `mod.rs`).
    let value = as_structured(out);
    let id = value["id"].as_str().expect("id present").to_string();

    // The persisted ledger entry is NOT left blank, though: `resolve_intent_note`
    // falls back to the artifact's title so an omitted `intent_note` never
    // produces an empty ledger entry.
    let record = store.get("agent-1", &id).await.unwrap();
    assert_eq!(record.intent_ledger.len(), 1);
    assert_eq!(record.intent_ledger[0].intent_note.as_deref(), Some("No note"));
}

#[test]
fn resolve_intent_note_falls_back_to_truncated_title_when_absent_or_blank() {
    // Explicit, non-blank note wins outright.
    assert_eq!(
        resolve_intent_note(Some("Add the Q4 rows"), "Ignored title"),
        Some("Add the Q4 rows".to_string())
    );
    // A whitespace-only note is treated the same as an omitted one.
    assert_eq!(
        resolve_intent_note(Some("   "), "Fallback title"),
        Some("Fallback title".to_string())
    );
    // No note at all: falls back to the title, trimmed.
    assert_eq!(resolve_intent_note(None, "  Quarterly rollup  "), Some("Quarterly rollup".to_string()));
    // A title past the fallback cap is truncated with a trailing ellipsis
    // rather than persisted verbatim.
    let long_title = "x".repeat(250);
    let resolved = resolve_intent_note(None, &long_title).expect("non-empty title always resolves");
    assert_eq!(resolved.chars().count(), INTENT_NOTE_FALLBACK_MAX_CHARS + 1);
    assert!(resolved.ends_with('…'));
}
