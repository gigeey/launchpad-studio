use std::sync::Arc;

use ao_engine_tools_core::{
    CancelOutcome, EngineTool, RunnerContext, TasklistServiceHandle, TerminalWatcherGuard,
    ToolOutput,
};
use ao_protocol::{
    error::AoError,
    tasklist::{Task, TaskAssignment, TaskGroup, TaskGroupMode, Tasklist, TasklistOwner, TasklistStatus},
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::TodoCancel;

// ─── Mock ─────────────────────────────────────────────────────────────────────

struct MockSvc {
    active: Option<Tasklist>,
    cancel_result: Result<CancelOutcome, AoError>,
}

impl MockSvc {
    fn with_active_ok(skipped: usize, in_flight: usize) -> Arc<Self> {
        let tl = fake_tasklist("tl-1");
        Arc::new(Self {
            active: Some(tl),
            cancel_result: Ok(CancelOutcome {
                tasklist_id: "tl-1".to_string(),
                skipped_count: skipped,
                in_flight_count: in_flight,
            }),
        })
    }

    fn no_active() -> Arc<Self> {
        Arc::new(Self {
            active: None,
            cancel_result: Err(AoError::ValidationError(
                "agent 'agent1' has no active tasklist to cancel".into(),
            )),
        })
    }
}

fn fake_tasklist(id: &str) -> Tasklist {
    Tasklist {
        id: id.to_string(),
        owner: TasklistOwner::Agent { agent_id: "agent1".to_string() },
        team_id: None,
        title: "Test List".to_string(),
        description: String::new(),
        status: TasklistStatus::Active,
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
    async fn cancel_for_agent(&self, _agent_id: &str) -> Result<CancelOutcome, AoError> {
        match &self.cancel_result {
            Ok(o) => Ok(o.clone()),
            Err(e) => Err(AoError::ValidationError(e.to_string())),
        }
    }
    async fn set_assignment(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        _task_id: &str,
        _assignment: Option<TaskAssignment>,
        _expected_token: u64,
    ) -> Result<bool, AoError> {
        unimplemented!()
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
async fn happy_path_returns_cancelled_response() {
    let c = ctx(MockSvc::with_active_ok(3, 1));
    let out = TodoCancel.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["tasklist_id"], "tl-1");
            assert_eq!(v["status"], "cancelled");
            assert_eq!(v["skipped_count"], 3);
            assert_eq!(v["in_flight_count"], 1);
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn no_active_tasklist_returns_recoverable_error() {
    let c = ctx(MockSvc::no_active());
    let out = TodoCancel.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("no active tasklist"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn subagent_context_rejected() {
    let c = subagent_ctx(MockSvc::with_active_ok(0, 0));
    let out = TodoCancel.invoke(json!({}), &c).await.unwrap();
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
    let out = TodoCancel.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("not available"), "got: {message}");
            assert!(!recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
