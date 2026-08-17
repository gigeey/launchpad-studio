use super::*;
use ao_engine_tools_core::{EventSink, RunnerContext};
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
async fn happy_path_no_details() {
    let sink = RecordingSink::new();
    let ctx = make_ctx(sink.clone());
    let out = Brief.invoke(json!({"summary": "all good"}), &ctx).await.unwrap();
    match out {
        ToolOutput::Text(s) => assert_eq!(s, "all good"),
        _ => panic!("expected Text"),
    }
    let events = sink.take();
    assert_eq!(events.len(), 1);
    match &events[0] {
        UserEvent::Brief { content } => assert_eq!(content, "all good"),
        _ => panic!("expected Brief event"),
    }
}

#[tokio::test]
async fn happy_path_with_details() {
    let sink = RecordingSink::new();
    let ctx = make_ctx(sink.clone());
    let out = Brief
        .invoke(
            json!({"summary": "step 1", "details": "more info"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert_eq!(s, "step 1"),
        _ => panic!("expected Text"),
    }
    let events = sink.take();
    assert_eq!(events.len(), 1);
    match &events[0] {
        UserEvent::Brief { content } => assert_eq!(content, "step 1\n\nmore info"),
        _ => panic!("expected Brief event"),
    }
}

#[tokio::test]
async fn missing_summary_returns_error_zero_events() {
    let sink = RecordingSink::new();
    let ctx = make_ctx(sink.clone());
    let out = Brief.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        _ => panic!("expected Error"),
    }
    assert!(sink.take().is_empty());
}

#[tokio::test]
async fn whitespace_only_summary_returns_error_zero_events() {
    let sink = RecordingSink::new();
    let ctx = make_ctx(sink.clone());
    let out = Brief.invoke(json!({"summary": "   "}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        _ => panic!("expected Error"),
    }
    assert!(sink.take().is_empty());
}

#[tokio::test]
async fn summary_without_details_content_equals_summary() {
    let sink = RecordingSink::new();
    let ctx = make_ctx(sink.clone());
    Brief.invoke(json!({"summary": "exact"}), &ctx).await.unwrap();
    let events = sink.take();
    match &events[0] {
        UserEvent::Brief { content } => assert_eq!(content, "exact"),
        _ => panic!("expected Brief event"),
    }
}

#[tokio::test]
async fn is_concurrency_safe() {
    assert!(Brief.is_concurrency_safe());
}

#[test]
fn tool_name_is_brief() {
    assert_eq!(Brief.name(), "Brief");
}

#[test]
fn lookup_through_registry() {
    use ao_engine_tools_core::Registry;
    let mut r = Registry::new();
    r.register_engine(Arc::new(Brief));
    assert!(r.lookup_engine("Brief").is_some());
}
