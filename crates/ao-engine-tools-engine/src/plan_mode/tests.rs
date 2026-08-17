use super::*;
use ao_engine_tools_core::{EventSink, PermissionStore, Registry, RunnerContext};
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

fn make_ctx(sink: Arc<RecordingSink>, perms: Arc<PermissionStore>) -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_event_sink(sink as Arc<dyn EventSink + Send + Sync>)
        .with_permissions(perms)
}

#[tokio::test]
async fn enter_plan_mode_happy_path_emits_event() {
    let sink = RecordingSink::new();
    let perms = Arc::new(PermissionStore::default());
    let ctx = make_ctx(sink.clone(), perms.clone());

    let out = EnterPlanMode.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Text(s) => assert_eq!(s, "plan mode"),
        _ => panic!("expected Text"),
    }
    assert_eq!(perms.mode(), PermissionMode::Plan);

    let events = sink.take();
    assert_eq!(events.len(), 1);
    match &events[0] {
        UserEvent::PermissionModeChanged { from, to } => {
            assert_eq!(*from, PermissionMode::Default);
            assert_eq!(*to, PermissionMode::Plan);
        }
        _ => panic!("expected PermissionModeChanged"),
    }
}

#[tokio::test]
async fn exit_plan_mode_happy_path_emits_event() {
    let sink = RecordingSink::new();
    let perms = Arc::new(PermissionStore::default());
    let ctx = make_ctx(sink.clone(), perms.clone());

    // Enter plan mode first.
    EnterPlanMode.invoke(json!({}), &ctx).await.unwrap();
    sink.take(); // discard the enter event

    let out = ExitPlanMode.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Text(_) => {}
        _ => panic!("expected Text"),
    }
    assert_eq!(perms.mode(), PermissionMode::Default);

    let events = sink.take();
    assert_eq!(events.len(), 1);
    match &events[0] {
        UserEvent::PermissionModeChanged { from, to } => {
            assert_eq!(*from, PermissionMode::Plan);
            assert_eq!(*to, PermissionMode::Default);
        }
        _ => panic!("expected PermissionModeChanged"),
    }
}

#[tokio::test]
async fn enter_plan_mode_idempotent_no_event() {
    let sink = RecordingSink::new();
    let perms = Arc::new(PermissionStore::default());
    let ctx = make_ctx(sink.clone(), perms.clone());

    EnterPlanMode.invoke(json!({}), &ctx).await.unwrap();
    sink.take(); // discard first event

    // Second call: already in Plan → no event.
    let out = EnterPlanMode.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Text(_) => {}
        _ => panic!("expected Text"),
    }
    assert!(sink.take().is_empty());
    assert_eq!(perms.mode(), PermissionMode::Plan);
}

#[tokio::test]
async fn exit_plan_mode_idempotent_no_event_when_not_in_plan() {
    let sink = RecordingSink::new();
    let perms = Arc::new(PermissionStore::default());
    let ctx = make_ctx(sink.clone(), perms.clone());

    // Not in plan: no-op.
    let out = ExitPlanMode.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Text(_) => {}
        _ => panic!("expected Text"),
    }
    assert!(sink.take().is_empty());
    assert_eq!(perms.mode(), PermissionMode::Default);
}

#[test]
fn enter_plan_mode_is_not_concurrency_safe() {
    assert!(!EnterPlanMode.is_concurrency_safe());
}

#[test]
fn exit_plan_mode_is_not_concurrency_safe() {
    assert!(!ExitPlanMode.is_concurrency_safe());
}

#[test]
fn enter_plan_mode_name() {
    assert_eq!(EnterPlanMode.name(), "EnterPlanMode");
}

#[test]
fn exit_plan_mode_name() {
    assert_eq!(ExitPlanMode.name(), "ExitPlanMode");
}

#[test]
fn lookup_enter_plan_mode_through_registry() {
    let mut r = Registry::new();
    r.register_engine(Arc::new(EnterPlanMode));
    assert!(r.lookup_engine("EnterPlanMode").is_some());
}

#[test]
fn lookup_exit_plan_mode_through_registry() {
    let mut r = Registry::new();
    r.register_engine(Arc::new(ExitPlanMode));
    assert!(r.lookup_engine("ExitPlanMode").is_some());
}
