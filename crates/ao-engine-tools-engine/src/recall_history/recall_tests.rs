use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_persistence::{paths::DataRoot, transcript::TranscriptStore};
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use chrono::{Duration, TimeZone, Utc};
use serde_json::json;
use std::{path::PathBuf, sync::Arc};

use super::recall::RecallHistory;

async fn make_store_with_entries(
    dir: &tempfile::TempDir,
    agent_id: &str,
    count: usize,
) -> Arc<TranscriptStore> {
    let data_root = DataRoot::new(dir.path());
    let store = Arc::new(TranscriptStore::new(data_root));
    let base = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    for i in 0..count {
        let entry = TranscriptEntry {
            ts: base + Duration::seconds(i as i64),
            role: TranscriptRole::Agent {
                agent: agent_id.to_string(),
            },
            content: format!("message {}", i),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        };
        store.append(agent_id, &entry).await.unwrap();
    }
    store
}

#[tokio::test]
async fn happy_path_returns_messages_before_window() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = make_store_with_entries(&dir, "agent-1", 30).await;
    let base = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    // Window floor at message 20 — messages 0-19 are before it
    let floor_ts = base + Duration::seconds(20);
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_transcript_store(store)
        .with_window_floor_ts(floor_ts);

    let tool = RecallHistory;
    let result = tool.invoke(json!({ "count": 10 }), &ctx).await.unwrap();
    match result {
        ToolOutput::Text(text) => {
            assert!(
                text.contains("[Recalled context (10 messages)]"),
                "expected header, got: {}",
                text
            );
            assert!(text.contains("message 10"), "expected msg 10, got: {}", text);
            assert!(text.contains("message 19"), "expected msg 19, got: {}", text);
            assert!(
                !text.contains("message 20"),
                "should not contain msg 20, got: {}",
                text
            );
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn boundary_at_start_returns_structured_message() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = make_store_with_entries(&dir, "agent-2", 5).await;
    let base = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    // Window floor at message 0 — nothing before it
    let floor_ts = base;
    let ctx = RunnerContext::new_with_cwd("sess", "agent-2", PathBuf::from("/tmp"))
        .with_transcript_store(store)
        .with_window_floor_ts(floor_ts);

    let tool = RecallHistory;
    let result = tool.invoke(json!({}), &ctx).await.unwrap();
    match result {
        ToolOutput::Text(text) => {
            assert!(
                text.contains("Already at beginning"),
                "expected at-start message, got: {}",
                text
            );
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn count_clamped_to_max_100() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = make_store_with_entries(&dir, "agent-3", 150).await;
    let base = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    // Window floor at message 120 — 120 messages before it; request 200, get 100
    let floor_ts = base + Duration::seconds(120);
    let ctx = RunnerContext::new_with_cwd("sess", "agent-3", PathBuf::from("/tmp"))
        .with_transcript_store(store)
        .with_window_floor_ts(floor_ts);

    let tool = RecallHistory;
    let result = tool.invoke(json!({ "count": 200 }), &ctx).await.unwrap();
    match result {
        ToolOutput::Text(text) => {
            assert!(
                text.contains("[Recalled context (100 messages)]"),
                "expected 100 messages, got: {}",
                text
            );
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_store_returns_non_recoverable_error() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent-4", PathBuf::from("/tmp"));
    let tool = RecallHistory;
    let result = tool.invoke(json!({}), &ctx).await.unwrap();
    match result {
        ToolOutput::Error {
            recoverable: false, ..
        } => {}
        other => panic!("expected non-recoverable error, got {:?}", other),
    }
}
