use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ao_engine_tools_core::{
    EngineTool, RunnerContext, ThreadSummarizationEngine, ThreadSummarizationInput, ToolOutput,
};
use ao_persistence::paths::DataRoot;
use ao_persistence::thread_store::ThreadStore;
use ao_persistence::transcript::TranscriptStore;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::json;

use super::SummarizeThread;

struct MockSummarizer {
    reply: String,
    calls: Arc<Mutex<Vec<ThreadSummarizationInput>>>,
}

#[async_trait]
impl ThreadSummarizationEngine for MockSummarizer {
    async fn summarize(&self, input: ThreadSummarizationInput) -> Result<String, String> {
        self.calls.lock().unwrap().push(input);
        Ok(self.reply.clone())
    }
}

struct FailingSummarizer;

#[async_trait]
impl ThreadSummarizationEngine for FailingSummarizer {
    async fn summarize(&self, _input: ThreadSummarizationInput) -> Result<String, String> {
        Err("provider is down".to_string())
    }
}

async fn temp_stores() -> (tempfile::TempDir, Arc<ThreadStore>, Arc<TranscriptStore>) {
    let dir = tempfile::TempDir::new().unwrap();
    let data_root = DataRoot::new(dir.path());
    data_root.ensure_directories().await.unwrap();
    let threads = Arc::new(ThreadStore::load(data_root.clone()).await.unwrap());
    let transcripts = Arc::new(TranscriptStore::new(data_root));
    (dir, threads, transcripts)
}

fn base_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
}

fn assert_error(out: ToolOutput, recoverable: bool, contains: &str) {
    match out {
        ToolOutput::Error { recoverable: r, message } => {
            assert_eq!(r, recoverable, "recoverable mismatch, message: {message}");
            assert!(message.contains(contains), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

async fn append(transcripts: &TranscriptStore, path: &str, content: &str, secs_ago: i64) {
    let entry = TranscriptEntry {
        ts: Utc::now() - Duration::seconds(secs_ago),
        role: TranscriptRole::System("user".to_string()),
        content: content.to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    };
    transcripts.append_at(Path::new(path), &entry).await.unwrap();
}

#[tokio::test]
async fn missing_thread_id_returns_recoverable_error() {
    let ctx = base_ctx();
    let out = SummarizeThread.invoke(json!({}), &ctx).await.unwrap();
    assert_error(out, true, "thread_id");
}

#[tokio::test]
async fn missing_thread_store_returns_unrecoverable_error() {
    let ctx = base_ctx();
    let out = SummarizeThread
        .invoke(json!({"thread_id": "t1"}), &ctx)
        .await
        .unwrap();
    assert_error(out, false, "Thread store");
}

#[tokio::test]
async fn missing_transcript_store_returns_unrecoverable_error() {
    let (_dir, threads, _transcripts) = temp_stores().await;
    let ctx = base_ctx().with_thread_store(threads);
    let out = SummarizeThread
        .invoke(json!({"thread_id": "t1"}), &ctx)
        .await
        .unwrap();
    assert_error(out, false, "Transcript store");
}

#[tokio::test]
async fn missing_engine_returns_unrecoverable_error() {
    let (_dir, threads, transcripts) = temp_stores().await;
    let ctx = base_ctx()
        .with_thread_store(threads)
        .with_transcript_store(transcripts);
    let out = SummarizeThread
        .invoke(json!({"thread_id": "t1"}), &ctx)
        .await
        .unwrap();
    assert_error(out, false, "not available");
}

#[tokio::test]
async fn unknown_thread_id_returns_recoverable_not_found() {
    let (_dir, threads, transcripts) = temp_stores().await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let ctx = base_ctx()
        .with_thread_store(threads)
        .with_transcript_store(transcripts)
        .with_thread_summarization_engine(Arc::new(MockSummarizer {
            reply: "summary".to_string(),
            calls: calls.clone(),
        }));

    let out = SummarizeThread
        .invoke(json!({"thread_id": "does-not-exist"}), &ctx)
        .await
        .unwrap();
    assert_error(out, true, "not found");
    assert!(calls.lock().unwrap().is_empty(), "engine must not be called");
}

#[tokio::test]
async fn cannot_summarize_another_agents_thread() {
    let (_dir, threads, transcripts) = temp_stores().await;
    let other = threads.build_fresh_thread("agent-b", Some("Not yours".to_string()));
    threads.create(other.clone()).await.unwrap();

    let calls = Arc::new(Mutex::new(Vec::new()));
    let ctx = base_ctx() // agent-1
        .with_thread_store(threads)
        .with_transcript_store(transcripts)
        .with_thread_summarization_engine(Arc::new(MockSummarizer {
            reply: "summary".to_string(),
            calls: calls.clone(),
        }));

    let out = SummarizeThread
        .invoke(json!({"thread_id": other.id}), &ctx)
        .await
        .unwrap();
    assert_error(out, true, "not found");
    assert!(calls.lock().unwrap().is_empty(), "engine must not be called");
}

#[tokio::test]
async fn empty_thread_short_circuits_without_calling_engine() {
    let (_dir, threads, transcripts) = temp_stores().await;
    let row = threads.build_fresh_thread("agent-1", Some("Empty".to_string()));
    threads.create(row.clone()).await.unwrap();

    let calls = Arc::new(Mutex::new(Vec::new()));
    let ctx = base_ctx()
        .with_thread_store(threads)
        .with_transcript_store(transcripts)
        .with_thread_summarization_engine(Arc::new(MockSummarizer {
            reply: "summary".to_string(),
            calls: calls.clone(),
        }));

    let out = SummarizeThread
        .invoke(json!({"thread_id": row.id}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["message_count"], 0);
            assert_eq!(v["truncated"], false);
            assert!(v["summary"].as_str().unwrap().contains("no messages"));
        }
        other => panic!("expected Structured, got {other:?}"),
    }
    assert!(calls.lock().unwrap().is_empty(), "engine must not be called for an empty thread");
}

#[tokio::test]
async fn happy_path_summarizes_and_forwards_focus() {
    let (_dir, threads, transcripts) = temp_stores().await;
    let row = threads.build_fresh_thread("agent-1", Some("Pricing".to_string()));
    threads.create(row.clone()).await.unwrap();
    append(&transcripts, &row.transcript_path, "what should pricing look like?", 120).await;
    append(&transcripts, &row.transcript_path, "let's do tiered pricing with a trial", 60).await;

    let calls = Arc::new(Mutex::new(Vec::new()));
    let ctx = base_ctx()
        .with_thread_store(threads)
        .with_transcript_store(transcripts)
        .with_thread_summarization_engine(Arc::new(MockSummarizer {
            reply: "Tiered pricing with a free trial.".to_string(),
            calls: calls.clone(),
        }));

    let out = SummarizeThread
        .invoke(
            json!({"thread_id": row.id, "focus": "what was decided?"}),
            &ctx,
        )
        .await
        .unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["thread_id"], row.id);
            assert_eq!(v["title"], "Pricing");
            assert_eq!(v["summary"], "Tiered pricing with a free trial.");
            assert_eq!(v["message_count"], 2);
            assert_eq!(v["truncated"], false);
        }
        other => panic!("expected Structured, got {other:?}"),
    }

    let captured = calls.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].focus.as_deref(), Some("what was decided?"));
    assert_eq!(captured[0].thread_title.as_deref(), Some("Pricing"));
    assert!(captured[0].transcript_text.contains("tiered pricing"));
}

#[tokio::test]
async fn summarization_failure_returns_recoverable_error() {
    let (_dir, threads, transcripts) = temp_stores().await;
    let row = threads.build_fresh_thread("agent-1", Some("Pricing".to_string()));
    threads.create(row.clone()).await.unwrap();
    append(&transcripts, &row.transcript_path, "hello", 10).await;

    let ctx = base_ctx()
        .with_thread_store(threads)
        .with_transcript_store(transcripts)
        .with_thread_summarization_engine(Arc::new(FailingSummarizer));

    let out = SummarizeThread
        .invoke(json!({"thread_id": row.id}), &ctx)
        .await
        .unwrap();
    assert_error(out, true, "provider is down");
}

#[tokio::test]
async fn long_thread_is_truncated_head_and_tail() {
    let (_dir, threads, transcripts) = temp_stores().await;
    let row = threads.build_fresh_thread("agent-1", Some("Long".to_string()));
    threads.create(row.clone()).await.unwrap();

    append(&transcripts, &row.transcript_path, "ORIGINAL GOAL: ship v2", 100_000).await;
    // Pad with enough bulk to exceed the truncation budget.
    let filler = "x".repeat(2_000);
    for i in 0..40 {
        append(&transcripts, &row.transcript_path, &filler, 100_000 - (i + 1)).await;
    }
    append(&transcripts, &row.transcript_path, "MOST RECENT MESSAGE", 1).await;

    let calls = Arc::new(Mutex::new(Vec::new()));
    let ctx = base_ctx()
        .with_thread_store(threads)
        .with_transcript_store(transcripts)
        .with_thread_summarization_engine(Arc::new(MockSummarizer {
            reply: "summary".to_string(),
            calls: calls.clone(),
        }));

    let out = SummarizeThread
        .invoke(json!({"thread_id": row.id}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["truncated"], true);
        }
        other => panic!("expected Structured, got {other:?}"),
    }

    let captured = calls.lock().unwrap();
    let text = &captured[0].transcript_text;
    assert!(text.contains("ORIGINAL GOAL"), "head must survive truncation");
    assert!(text.contains("MOST RECENT MESSAGE"), "tail must survive truncation");
    assert!(text.contains("omitted"), "must note elision");
}
