use std::sync::Arc;

use ao_engine_tools_core::{
    CancelOutcome, EngineTool, ResumeOutcome, RunnerContext, TasklistServiceHandle,
    TerminalWatcherGuard, ToolOutput,
};
use ao_protocol::{
    error::AoError,
    tasklist::{Task, TaskAssignment, TaskGroup, TaskGroupMode, Tasklist, TasklistOwner, TasklistStatus},
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::TodoResume;

// ─── Mock ─────────────────────────────────────────────────────────────────────

struct MockSvc {
    active: Option<Tasklist>,
    resume_result: Result<ResumeOutcome, AoError>,
}

impl MockSvc {
    /// Happy path: two failed tasks get reset.
    fn with_two_failed_resets() -> Arc<Self> {
        Arc::new(Self {
            active: None,
            resume_result: Ok(ResumeOutcome {
                tasklist_id: "tl-1".to_string(),
                reset_count: 2,
            }),
        })
    }

    /// No failed tasklist available.
    fn no_failed() -> Arc<Self> {
        Arc::new(Self {
            active: None,
            resume_result: Err(AoError::InvalidTasklistTransition(
                "agent 'agent1' has no failed tasklist to resume".into(),
            )),
        })
    }

    /// Active tasklist already occupies the slot.
    fn blocked_by_active() -> Arc<Self> {
        let tl = fake_tasklist("tl-active", TasklistStatus::Active);
        Arc::new(Self {
            active: Some(tl),
            resume_result: Err(AoError::InvalidTasklistTransition(
                "agent 'agent1' already has a active tasklist 'tl-active'; cancel or complete it first".into(),
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
    async fn start_for_agent(&self, _: &str) -> Result<ao_engine_tools_core::StartOutcome, AoError> {
        unimplemented!()
    }
    async fn resume_for_agent(&self, _agent_id: &str) -> Result<ResumeOutcome, AoError> {
        match &self.resume_result {
            Ok(o) => Ok(o.clone()),
            Err(e) => Err(AoError::InvalidTasklistTransition(e.to_string())),
        }
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
async fn resumes_failed_tasklist_and_reports_reset_count() {
    let c = ctx(MockSvc::with_two_failed_resets());
    let out = TodoResume.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["tasklist_id"], "tl-1");
            assert_eq!(v["status"], "active");
            assert_eq!(v["reset_count"], 2);
            let msg = v["message"].as_str().unwrap();
            assert!(msg.contains("2 failed task"), "got: {msg}");
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn no_failed_tasklist_returns_recoverable_error() {
    let c = ctx(MockSvc::no_failed());
    let out = TodoResume.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("no failed tasklist"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn active_slot_occupied_returns_recoverable_error() {
    let c = ctx(MockSvc::blocked_by_active());
    let out = TodoResume.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("already has a"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn subagent_context_rejected() {
    let c = subagent_ctx(MockSvc::with_two_failed_resets());
    let out = TodoResume.invoke(json!({}), &c).await.unwrap();
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
    let out = TodoResume.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("not available"), "got: {message}");
            assert!(!recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
