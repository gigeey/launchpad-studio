use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, RunnerContext, TasklistServiceHandle, ToolOutput};
use ao_protocol::{
    error::AoError,
    tasklist::{
        Task, TaskAssignment, TaskComment, TaskCommentAuthorKind, TaskGroup, TaskGroupMode,
        Tasklist, TasklistOwner, TasklistStatus,
    },
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::TodoComment;

struct MockSvc {
    active: Option<Tasklist>,
    task_not_found: bool,
}

impl MockSvc {
    fn with_active() -> Arc<Self> {
        Arc::new(Self { active: Some(fake_tasklist()), task_not_found: false })
    }
    fn no_active() -> Arc<Self> {
        Arc::new(Self { active: None, task_not_found: false })
    }
    fn task_not_found() -> Arc<Self> {
        Arc::new(Self { active: Some(fake_tasklist()), task_not_found: true })
    }
}

fn fake_tasklist() -> Tasklist {
    Tasklist {
        id: "tl-1".to_string(),
        owner: TasklistOwner::Agent { agent_id: "agent1".to_string() },
        team_id: None,
        title: "My List".to_string(),
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
    async fn add_group_for_agent(
        &self,
        _: &str,
        _: &str,
        _: Vec<Task>,
        _: TaskGroupMode,
    ) -> Result<Tasklist, AoError> {
        unimplemented!()
    }
    async fn update_task_for_agent(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<String>,
        _: Option<String>,
        _: Option<Vec<String>>,
    ) -> Result<Tasklist, AoError> {
        unimplemented!()
    }
    async fn complete_task_for_agent(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> {
        unimplemented!()
    }
    async fn add_comment_for_agent(
        &self,
        agent_id: &str,
        _tasklist_id: &str,
        task_id: &str,
        body: String,
    ) -> Result<TaskComment, AoError> {
        if self.task_not_found {
            return Err(AoError::TaskNotFound(task_id.to_string()));
        }
        Ok(TaskComment {
            id: "c-1".to_string(),
            author_id: agent_id.to_string(),
            author_kind: TaskCommentAuthorKind::Agent,
            body,
            created_at: Utc::now(),
        })
    }
    async fn terminal_watcher(
        &self,
        _: &str,
    ) -> Result<ao_engine_tools_core::TerminalWatcherGuard, AoError> {
        Err(AoError::Internal("not implemented".into()))
    }
    async fn cancel_for_agent(
        &self,
        _: &str,
    ) -> Result<ao_engine_tools_core::CancelOutcome, AoError> {
        Err(AoError::Internal("not implemented".into()))
    }
    async fn set_assignment(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<TaskAssignment>,
        _: u64,
    ) -> Result<bool, AoError> {
        unimplemented!()
    }
}

fn ctx(svc: Arc<dyn TasklistServiceHandle + Send + Sync>) -> RunnerContext {
    RunnerContext::new("s", "agent1").unwrap().with_tasklist_service(svc)
}

#[tokio::test]
async fn happy_path() {
    let c = ctx(MockSvc::with_active());
    let out = TodoComment
        .invoke(json!({"task_id": "t1", "comment": "looks good"}), &c)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("comment added to task 't1'"), "got: {s}"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_task_id_error() {
    let c = ctx(MockSvc::with_active());
    let out = TodoComment
        .invoke(json!({"task_id": "", "comment": "note"}), &c)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("task_id"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_task_id_error() {
    let c = ctx(MockSvc::task_not_found());
    let out = TodoComment
        .invoke(json!({"task_id": "missing", "comment": "note"}), &c)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("not found"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn no_tasklist_service_error() {
    let c = RunnerContext::new("s", "agent1").unwrap();
    let out = TodoComment
        .invoke(json!({"task_id": "t1", "comment": "note"}), &c)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("not available"), "got: {message}");
            assert!(!recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn no_active_tasklist_error() {
    let c = ctx(MockSvc::no_active());
    let out = TodoComment
        .invoke(json!({"task_id": "t1", "comment": "note"}), &c)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("No active tasklist"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
