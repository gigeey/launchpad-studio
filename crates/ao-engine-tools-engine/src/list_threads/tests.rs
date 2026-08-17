use std::path::PathBuf;
use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput};
use ao_persistence::paths::DataRoot;
use ao_persistence::thread_store::ThreadStore;
use serde_json::json;

use super::ListThreads;

async fn temp_thread_store() -> (tempfile::TempDir, Arc<ThreadStore>) {
    let dir = tempfile::TempDir::new().unwrap();
    let data_root = DataRoot::new(dir.path());
    data_root.ensure_directories().await.unwrap();
    let store = Arc::new(ThreadStore::load(data_root).await.unwrap());
    (dir, store)
}

fn base_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
}

#[tokio::test]
async fn missing_thread_store_returns_unrecoverable_error() {
    let ctx = base_ctx();
    let out = ListThreads.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable);
            assert!(message.contains("not available"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn lists_default_thread_when_no_others_exist() {
    let (_dir, store) = temp_thread_store().await;
    let ctx = base_ctx().with_thread_store(store);

    let out = ListThreads.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["count"], 1);
            assert_eq!(v["threads"][0]["kind"], "default");
            assert_eq!(v["threads"][0]["is_current"], true);
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn lists_multiple_threads_with_display_titles_and_current_flag() {
    let (_dir, store) = temp_thread_store().await;
    let default = store.ensure_default_thread("agent-1").await.unwrap();
    let mut fresh = store.build_fresh_thread("agent-1", Some("Pricing".to_string()));
    fresh.auto_title = Some("Should be ignored".to_string());
    store.create(fresh.clone()).await.unwrap();

    let mut untitled = store.build_fresh_thread("agent-1", None);
    untitled.auto_title = Some("What is the API rate limit?".to_string());
    store.create(untitled.clone()).await.unwrap();

    // Run scoped to the "fresh" thread — it should be flagged current, not the default.
    let ctx = base_ctx()
        .with_thread(fresh.id.clone())
        .with_thread_store(store);

    let out = ListThreads.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["count"], 3);
            let threads = v["threads"].as_array().unwrap();

            let default_entry = threads.iter().find(|t| t["thread_id"] == default.id).unwrap();
            assert_eq!(default_entry["is_current"], false);

            let fresh_entry = threads.iter().find(|t| t["thread_id"] == fresh.id).unwrap();
            assert_eq!(fresh_entry["title"], "Pricing");
            assert_eq!(fresh_entry["is_current"], true);

            let untitled_entry = threads.iter().find(|t| t["thread_id"] == untitled.id).unwrap();
            assert_eq!(untitled_entry["title"], "What is the API rate limit?");
            assert_eq!(untitled_entry["is_current"], false);
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn never_leaks_another_agents_threads() {
    let (_dir, store) = temp_thread_store().await;
    store.ensure_default_thread("agent-a").await.unwrap();
    let other = store.build_fresh_thread("agent-b", Some("Not yours".to_string()));
    store.create(other).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-a", PathBuf::from("/tmp"))
        .with_thread_store(store);

    let out = ListThreads.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["count"], 1);
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}
