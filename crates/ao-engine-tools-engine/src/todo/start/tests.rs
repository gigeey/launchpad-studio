use std::sync::Arc;

use ao_engine_tools_core::{
    CancelOutcome, EngineTool, ResumeOutcome, RunnerContext, StartOutcome, StartOutcomeKind,
    TasklistServiceHandle, TerminalWatcherGuard, ToolOutput,
};
use ao_protocol::{
    error::AoError,
    tasklist::{Task, TaskAssignment, TaskGroup, TaskGroupMode, Tasklist, TasklistOwner, TasklistStatus},
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::TodoStart;

// ─── Mock ─────────────────────────────────────────────────────────────────────

struct MockSvc {
    active: Option<Tasklist>,
    start_result: Result<StartOutcome, AoError>,
}

impl MockSvc {
    /// Happy path: the feeder actually dispatched a fresh task this call.
    fn with_dispatch(task_ids: Vec<&str>) -> Arc<Self> {
        let tl = fake_tasklist("tl-1", TasklistStatus::Paused);
        Arc::new(Self {
            active: Some(tl),
            start_result: Ok(StartOutcome {
                tasklist_id: "tl-1".to_string(),
                kind: StartOutcomeKind::Dispatched {
                    task_ids: task_ids.into_iter().map(str::to_string).collect(),
                },
            }),
        })
    }

    /// Already active with a task in flight: idempotent re-kick, nothing new.
    fn with_already_running() -> Arc<Self> {
        let tl = fake_tasklist("tl-1", TasklistStatus::Active);
        Arc::new(Self {
            active: Some(tl),
            start_result: Ok(StartOutcome {
                tasklist_id: "tl-1".to_string(),
                kind: StartOutcomeKind::AlreadyRunning,
            }),
        })
    }

    /// Active tasklist with zero pending tasks left to dispatch.
    fn with_no_pending() -> Arc<Self> {
        let tl = fake_tasklist("tl-1", TasklistStatus::Active);
        Arc::new(Self {
            active: Some(tl),
            start_result: Ok(StartOutcome {
                tasklist_id: "tl-1".to_string(),
                kind: StartOutcomeKind::NoPending,
            }),
        })
    }

    /// The start path reached the tasklist but the feeder never dispatched a
    /// ready pending task (e.g. the feeder/dispatcher is unavailable). This
    /// MUST surface as an error, never as a fake "active" success.
    fn with_dispatch_failure() -> Arc<Self> {
        let tl = fake_tasklist("tl-1", TasklistStatus::Active);
        Arc::new(Self {
            active: Some(tl),
            start_result: Err(AoError::Internal(
                "start_for_agent: tasklist 'tl-1' has a ready pending task but the feeder \
                 dispatched nothing this call; the dispatcher may be unavailable"
                    .to_string(),
            )),
        })
    }

    /// No active/paused tasklist.
    fn no_active() -> Arc<Self> {
        Arc::new(Self {
            active: None,
            start_result: Err(AoError::InvalidTasklistTransition(
                "agent 'agent1' has no active or paused tasklist to start".into(),
            )),
        })
    }
}

fn fake_tasklist(id: &str, status: TasklistStatus) -> Tasklist {
    Tasklist {
        id: id.to_string(),
        owner: TasklistOwner::Agent { agent_id: "agent1".to_string() },
        team_id: None,
        title: "Test List".to_string(),
        description: String::new(),
        status,
        groups: vec![],
        workspace_dir: String::new(),
        transcripts_dir: String::new(),
        created_at: Utc::now(),
        last_active_at: None,
        copilot_agent_id: None,
        last_opened_at: None,
        project_id: None,
        thread_id: None,
        }
}

#[async_trait]
impl TasklistServiceHandle for MockSvc {
    async fn agent_active(&self, _: &str) -> Result<Option<Tasklist>, AoError> {
        Ok(self.active.clone())
    }
    async fn create_for_agent(&self, _: &str, _: String, _: Vec<TaskGroup>) -> Result<Tasklist, AoError> {
        unimplemented!()
    }
    async fn get_agent_max_instances(&self, _: &str) -> Result<u32, AoError> {
        Ok(2)
    }
    async fn add_group_for_agent(&self, _: &str, _: &str, _: Vec<Task>, _: TaskGroupMode) -> Result<Tasklist, AoError> {
        unimplemented!()
    }
    async fn update_task_for_agent(&self, _: &str, _: &str, _: &str, _: Option<String>, _: Option<String>, _: Option<Vec<String>>) -> Result<Tasklist, AoError> {
        unimplemented!()
    }
    async fn complete_task_for_agent(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> {
        unimplemented!()
    }
    async fn terminal_watcher(&self, _: &str) -> Result<TerminalWatcherGuard, AoError> {
        Err(AoError::Internal("not needed".into()))
    }
    async fn cancel_for_agent(&self, _: &str) -> Result<CancelOutcome, AoError> {
        unimplemented!()
    }
    async fn set_assignment(&self, _: &str, _: &str, _: &str, _: Option<TaskAssignment>, _: u64) -> Result<bool, AoError> {
        unimplemented!()
    }
    async fn start_for_agent(&self, _agent_id: &str) -> Result<StartOutcome, AoError> {
        match &self.start_result {
            Ok(outcome) => Ok(outcome.clone()),
            Err(e) => Err(clone_error(e)),
        }
    }
    async fn resume_for_agent(&self, _: &str) -> Result<ResumeOutcome, AoError> {
        unimplemented!()
    }
}

/// `AoError` isn't `Clone`, so reconstruct an equivalent value for the mock's
/// canned-error path (preserves variant + message for the assertions below).
fn clone_error(e: &AoError) -> AoError {
    match e {
        AoError::InvalidTasklistTransition(msg) => AoError::InvalidTasklistTransition(msg.clone()),
        AoError::Internal(msg) => AoError::Internal(msg.clone()),
        other => AoError::Internal(other.to_string()),
    }
}

fn ctx(svc: Arc<dyn TasklistServiceHandle + Send + Sync>) -> RunnerContext {
    RunnerContext::new("s", "agent1").unwrap().with_tasklist_service(svc)
}

fn subagent_ctx(svc: Arc<dyn TasklistServiceHandle + Send + Sync>) -> RunnerContext {
    RunnerContext::new("s", "agent1").unwrap().with_tasklist_service(svc).with_depth(1)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn dispatched_reports_task_ids_not_a_fixed_string() {
    let c = ctx(MockSvc::with_dispatch(vec!["task-1"]));
    let out = TodoStart.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["tasklist_id"], "tl-1");
            assert_eq!(v["status"], "active");
            assert_eq!(v["outcome"], "dispatched");
            assert_eq!(v["dispatched_count"], 1);
            assert_eq!(v["dispatched_task_ids"], json!(["task-1"]));
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn no_pending_reports_distinct_outcome_from_dispatched() {
    // This is the acceptance-criteria pairing: a tasklist with zero pending
    // items (this test) vs one with pending items (`dispatched_reports_*`
    // above) must produce distinct, machine-readable `outcome` values rather
    // than the same hardcoded "active" success string either way.
    let c = ctx(MockSvc::with_no_pending());
    let out = TodoStart.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["outcome"], "no_pending");
            assert_eq!(v["dispatched_count"], 0);
            assert_eq!(v["dispatched_task_ids"], json!([]));
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn already_running_reports_distinct_outcome() {
    let c = ctx(MockSvc::with_already_running());
    let out = TodoStart.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["outcome"], "already_running");
            assert_eq!(v["dispatched_count"], 0);
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_failure_is_surfaced_as_an_error_not_a_fake_active_status() {
    let c = ctx(MockSvc::with_dispatch_failure());
    let out = TodoStart.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.starts_with("dispatch_failed:"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn no_active_tasklist_returns_recoverable_error() {
    let c = ctx(MockSvc::no_active());
    let out = TodoStart.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("no active or paused tasklist"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn subagent_context_rejected() {
    let c = subagent_ctx(MockSvc::with_dispatch(vec!["task-1"]));
    let out = TodoStart.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("subagent context"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn no_service_returns_non_recoverable_error() {
    let c = RunnerContext::new("s", "agent1").unwrap();
    let out = TodoStart.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("not available"), "got: {message}");
            assert!(!recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
