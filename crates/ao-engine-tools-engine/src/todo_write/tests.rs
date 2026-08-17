use super::*;
use ao_engine_tools_core::{EventSink, Registry, RunnerContext, TodoStore};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct RecordingSink {
    events: Mutex<Vec<UserEvent>>,
}

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
        })
    }

    fn take(&self) -> Vec<UserEvent> {
        self.events.lock().unwrap().drain(..).collect()
    }
}

#[async_trait]
impl EventSink for RecordingSink {
    async fn emit(&self, event: UserEvent) -> Result<(), AoError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

fn make_ctx(sink: Arc<RecordingSink>) -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_event_sink(sink as Arc<dyn EventSink + Send + Sync>)
}

#[tokio::test]
async fn happy_path_emits_event_and_returns_summary() {
    let sink = RecordingSink::new();
    let ctx = make_ctx(sink.clone());
    let out = TodoWrite
        .invoke(
            json!({
                "todos": [
                    { "id": "1", "content": "do a", "status": "in_progress" },
                    { "id": "2", "content": "do b", "status": "pending" },
                    { "id": "3", "content": "do c", "status": "completed" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("3 todos"));
            assert!(s.contains("1 in_progress"));
            assert!(s.contains("1 pending"));
            assert!(s.contains("1 completed"));
        }
        _ => panic!("expected Text"),
    }

    let events = sink.take();
    assert_eq!(events.len(), 1);
    match &events[0] {
        UserEvent::TodosUpdated { count, in_progress, pending, completed } => {
            assert_eq!(*count, 3);
            assert_eq!(*in_progress, 1);
            assert_eq!(*pending, 1);
            assert_eq!(*completed, 1);
        }
        _ => panic!("expected TodosUpdated event"),
    }
}

#[tokio::test]
async fn replace_semantics_writing_twice_leaves_only_second() {
    let sink = RecordingSink::new();
    let store = Arc::new(TodoStore::default());
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_event_sink(sink.clone() as Arc<dyn EventSink + Send + Sync>)
        .with_todos(store.clone());

    // First write: [a, b]
    TodoWrite
        .invoke(
            json!({
                "todos": [
                    { "id": "a", "content": "a", "status": "pending" },
                    { "id": "b", "content": "b", "status": "pending" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    // Second write: [c]
    TodoWrite
        .invoke(
            json!({
                "todos": [
                    { "id": "c", "content": "c", "status": "pending" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let items = store.get("agent");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, "c");
}

#[tokio::test]
async fn empty_list_clears_todos() {
    let sink = RecordingSink::new();
    let store = Arc::new(TodoStore::default());
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_event_sink(sink.clone() as Arc<dyn EventSink + Send + Sync>)
        .with_todos(store.clone());

    // Write one item first
    TodoWrite
        .invoke(
            json!({ "todos": [{ "id": "x", "content": "x", "status": "pending" }] }),
            &ctx,
        )
        .await
        .unwrap();

    // Clear with empty list
    let out = TodoWrite.invoke(json!({ "todos": [] }), &ctx).await.unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("0 todos")),
        _ => panic!("expected Text"),
    }
    assert!(store.get("agent").is_empty());
}

#[tokio::test]
async fn active_form_defaults_to_content() {
    let sink = RecordingSink::new();
    let store = Arc::new(TodoStore::default());
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_event_sink(sink.clone() as Arc<dyn EventSink + Send + Sync>)
        .with_todos(store.clone());

    TodoWrite
        .invoke(
            json!({ "todos": [{ "id": "1", "content": "my content", "status": "pending" }] }),
            &ctx,
        )
        .await
        .unwrap();

    let items = store.get("agent");
    assert_eq!(items[0].active_form, "my content");
}

#[tokio::test]
async fn active_form_uses_provided_value() {
    let sink = RecordingSink::new();
    let store = Arc::new(TodoStore::default());
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_event_sink(sink.clone() as Arc<dyn EventSink + Send + Sync>)
        .with_todos(store.clone());

    TodoWrite
        .invoke(
            json!({
                "todos": [{
                    "id": "1",
                    "content": "my content",
                    "status": "pending",
                    "active_form": "short label"
                }]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let items = store.get("agent");
    assert_eq!(items[0].active_form, "short label");
}

#[tokio::test]
async fn duplicate_id_returns_error_zero_events() {
    let sink = RecordingSink::new();
    let ctx = make_ctx(sink.clone());

    let out = TodoWrite
        .invoke(
            json!({
                "todos": [
                    { "id": "dup", "content": "a", "status": "pending" },
                    { "id": "dup", "content": "b", "status": "pending" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        _ => panic!("expected Error"),
    }
    assert!(sink.take().is_empty());
}

#[tokio::test]
async fn unknown_status_returns_error_zero_events() {
    let sink = RecordingSink::new();
    let ctx = make_ctx(sink.clone());

    let out = TodoWrite
        .invoke(
            json!({
                "todos": [{ "id": "1", "content": "x", "status": "invalid" }]
            }),
            &ctx,
        )
        .await
        .unwrap();

    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        _ => panic!("expected Error"),
    }
    assert!(sink.take().is_empty());
}

#[tokio::test]
async fn missing_todos_field_returns_error() {
    let sink = RecordingSink::new();
    let ctx = make_ctx(sink.clone());

    let out = TodoWrite.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        _ => panic!("expected Error"),
    }
    assert!(sink.take().is_empty());
}

#[test]
fn is_not_concurrency_safe() {
    assert!(!TodoWrite.is_concurrency_safe());
}

#[test]
fn tool_name_is_todo_write() {
    assert_eq!(TodoWrite.name(), "TodoWrite");
}

#[test]
fn lookup_through_registry() {
    let mut r = Registry::new();
    r.register_engine(Arc::new(TodoWrite));
    assert!(r.lookup_engine("TodoWrite").is_some());
}
