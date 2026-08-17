use std::path::PathBuf;
use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput};
use ao_persistence::paths::DataRoot;
use ao_persistence::thread_store::ThreadStore;
use ao_protocol::thread::{BranchSource, ThreadScope};
use chrono::Utc;
use serde_json::json;

use super::RenameThread;

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

fn assert_error(out: ToolOutput, recoverable: bool, contains: &str) {
    match out {
        ToolOutput::Error { recoverable: r, message } => {
            assert_eq!(r, recoverable, "recoverable mismatch, message: {message}");
            assert!(message.contains(contains), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_title_returns_recoverable_error() {
    let ctx = base_ctx().with_thread("t1".to_string());
    let out = RenameThread.invoke(json!({}), &ctx).await.unwrap();
    assert_error(out, true, "title");
}

#[tokio::test]
async fn blank_title_returns_recoverable_error() {
    let ctx = base_ctx().with_thread("t1".to_string());
    let out = RenameThread
        .invoke(json!({"title": "   "}), &ctx)
        .await
        .unwrap();
    assert_error(out, true, "blank");
}

#[tokio::test]
async fn missing_thread_scope_returns_unrecoverable_error() {
    let ctx = base_ctx();
    let out = RenameThread
        .invoke(json!({"title": "New name"}), &ctx)
        .await
        .unwrap();
    assert_error(out, false, "thread-scoped");
}

#[tokio::test]
async fn missing_thread_store_returns_unrecoverable_error() {
    let ctx = base_ctx().with_thread("t1".to_string());
    let out = RenameThread
        .invoke(json!({"title": "New name"}), &ctx)
        .await
        .unwrap();
    assert_error(out, false, "not available");
}

#[tokio::test]
async fn unknown_thread_returns_unrecoverable_error() {
    let (_dir, store) = temp_thread_store().await;
    let ctx = base_ctx()
        .with_thread("does-not-exist".to_string())
        .with_thread_store(store);
    let out = RenameThread
        .invoke(json!({"title": "New name"}), &ctx)
        .await
        .unwrap();
    assert_error(out, false, "not found");
}

#[tokio::test]
async fn default_thread_refuses_rename() {
    let (_dir, store) = temp_thread_store().await;
    let default = store.ensure_default_thread("agent-1").await.unwrap();
    let ctx = base_ctx()
        .with_thread(default.id.clone())
        .with_thread_store(store);
    let out = RenameThread
        .invoke(json!({"title": "New name"}), &ctx)
        .await
        .unwrap();
    assert_error(out, true, "fixed");
}

#[tokio::test]
async fn team_chat_scope_refuses_rename() {
    let (_dir, store) = temp_thread_store().await;
    let mut row = store.build_fresh_thread("agent-1", None);
    row.scope = ThreadScope::TeamChat { team_id: "team-1".to_string() };
    store.create(row.clone()).await.unwrap();

    let ctx = base_ctx()
        .with_thread(row.id.clone())
        .with_thread_store(store);
    let out = RenameThread
        .invoke(json!({"title": "New name"}), &ctx)
        .await
        .unwrap();
    assert_error(out, true, "team or delegation");
}

#[tokio::test]
async fn delegation_scope_refuses_rename() {
    let (_dir, store) = temp_thread_store().await;
    let mut row = store.build_fresh_thread("agent-1", None);
    row.scope = ThreadScope::Delegation {
        team_id: "team-1".to_string(),
        delegation_id: "del-1".to_string(),
    };
    store.create(row.clone()).await.unwrap();

    let ctx = base_ctx()
        .with_thread(row.id.clone())
        .with_thread_store(store);
    let out = RenameThread
        .invoke(json!({"title": "New name"}), &ctx)
        .await
        .unwrap();
    assert_error(out, true, "team or delegation");
}

#[tokio::test]
async fn already_titled_thread_refuses_rename() {
    let (_dir, store) = temp_thread_store().await;
    let row = store.build_fresh_thread("agent-1", Some("Existing".to_string()));
    store.create(row.clone()).await.unwrap();

    let ctx = base_ctx()
        .with_thread(row.id.clone())
        .with_thread_store(store);
    let out = RenameThread
        .invoke(json!({"title": "New name"}), &ctx)
        .await
        .unwrap();
    assert_error(out, true, "already named");
}

#[tokio::test]
async fn branch_thread_with_no_title_can_be_renamed() {
    let (_dir, store) = temp_thread_store().await;
    let default = store.ensure_default_thread("agent-1").await.unwrap();
    let branch_source = BranchSource {
        source_thread_id: default.id.clone(),
        branch_at: Utc::now(),
        source_message_id: None,
    };
    let row = store.build_branch_thread("agent-1", None, branch_source);
    store.create(row.clone()).await.unwrap();

    let ctx = base_ctx()
        .with_thread(row.id.clone())
        .with_thread_store(store.clone());
    let out = RenameThread
        .invoke(json!({"title": "Investigating the bug"}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["thread_id"], row.id);
            assert_eq!(v["title"], "Investigating the bug");
        }
        other => panic!("expected Structured, got {other:?}"),
    }

    let saved = store.get(&row.id).await.unwrap().unwrap();
    assert_eq!(saved.title.as_deref(), Some("Investigating the bug"));
}

#[tokio::test]
async fn happy_path_renames_fresh_thread_and_persists() {
    let (_dir, store) = temp_thread_store().await;
    let row = store.build_fresh_thread("agent-1", None);
    store.create(row.clone()).await.unwrap();

    let ctx = base_ctx()
        .with_thread(row.id.clone())
        .with_thread_store(store.clone());
    let out = RenameThread
        .invoke(json!({"title": "  Fix   login   redirect  "}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Structured(v) => {
            // Whitespace is collapsed/trimmed via the same normalization as auto_title.
            assert_eq!(v["title"], "Fix login redirect");
        }
        other => panic!("expected Structured, got {other:?}"),
    }

    let saved = store.get(&row.id).await.unwrap().unwrap();
    assert_eq!(saved.title.as_deref(), Some("Fix login redirect"));
    // auto_title is untouched by an explicit rename.
    assert!(saved.auto_title.is_none());
}

#[tokio::test]
async fn long_title_is_truncated() {
    let (_dir, store) = temp_thread_store().await;
    let row = store.build_fresh_thread("agent-1", None);
    store.create(row.clone()).await.unwrap();

    let long_title = "a".repeat(200);
    let ctx = base_ctx()
        .with_thread(row.id.clone())
        .with_thread_store(store.clone());
    RenameThread
        .invoke(json!({"title": long_title}), &ctx)
        .await
        .unwrap();

    let saved = store.get(&row.id).await.unwrap().unwrap();
    let title = saved.title.unwrap();
    assert!(title.chars().count() <= ao_protocol::thread::MAX_TITLE_LEN + 1);
    assert!(title.ends_with('…'));
}
