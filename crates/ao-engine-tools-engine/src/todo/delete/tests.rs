use std::sync::Arc;

use ao_engine_tools_core::{
    EngineTool, RunnerContext, TasklistServiceHandle, TerminalWatcherGuard, ToolOutput,
};
use ao_protocol::{
    error::AoError,
    tasklist::{
        Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistOwner,
        TasklistStatus,
    },
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::TodoDelete;

// ---------------------------------------------------------------------------
// Mock TasklistService
// ---------------------------------------------------------------------------

enum DeleteOutcome {
    Ok,
    TaskNotFound,
    NotPending,
}

struct MockSvc {
    active: Option<Tasklist>,
    delete_outcome: DeleteOutcome,
}

impl MockSvc {
    fn no_active() -> Arc<Self> {
        Arc::new(Self {
            active: None,
            delete_outcome: DeleteOutcome::Ok,
        })
    }

    fn with_active_ok() -> Arc<Self> {
        Arc::new(Self {
            active: Some(fake_tasklist()),
            delete_outcome: DeleteOutcome::Ok,
        })
    }

    fn task_not_found() -> Arc<Self> {
        Arc::new(Self {
            active: Some(fake_tasklist()),
            delete_outcome: DeleteOutcome::TaskNotFound,
        })
    }

    fn task_not_pending() -> Arc<Self> {
        Arc::new(Self {
            active: Some(fake_tasklist()),
            delete_outcome: DeleteOutcome::NotPending,
        })
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
        groups: vec![TaskGroup {
            id: "g1".to_string(),
            mode: TaskGroupMode::Seq,
            tasks: vec![Task {
                id: "t1".to_string(),
                group_id: "g1".to_string(),
                prompt: "Do the thing".to_string(),
                owner_agent_id: "agent1".to_string(),
                status: TaskStatus::Pending,
                expected_outputs: vec![],
                error_log: vec![],
                attempt_count: 0,
                comments: vec![],
                attachments: vec![],
                notification_parse_retry_count: 0,
                parse_failed: false,
                remind_me: None,
                assignment: None,
                classifier_token: 0,
                dispatch_token: 0,
            }],
        }],
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

    async fn create_for_agent(
        &self,
        _: &str,
        _: String,
        _: Vec<TaskGroup>,
    ) -> Result<Tasklist, AoError> {
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

    async fn terminal_watcher(&self, _: &str) -> Result<TerminalWatcherGuard, AoError> {
        Err(AoError::Internal("not implemented in mock".into()))
    }

    async fn cancel_for_agent(
        &self,
        _: &str,
    ) -> Result<ao_engine_tools_core::CancelOutcome, AoError> {
        Err(AoError::Internal("not implemented in mock".into()))
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

    async fn delete_task_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        task_id: &str,
    ) -> Result<(), AoError> {
        match self.delete_outcome {
            DeleteOutcome::Ok => Ok(()),
            DeleteOutcome::TaskNotFound => Err(AoError::TaskNotFound(task_id.to_string())),
            DeleteOutcome::NotPending => Err(AoError::InvalidTasklistTransition(format!(
                "cannot skip task {} in status InProgress; only Pending tasks can be skipped on agent-owned tasklists",
                task_id
            ))),
        }
    }
}

fn ctx(svc: Arc<dyn TasklistServiceHandle + Send + Sync>) -> RunnerContext {
    RunnerContext::new("s", "agent1").unwrap().with_tasklist_service(svc)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_returns_success_message() {
    let c = ctx(MockSvc::with_active_ok());
    let out = TodoDelete.invoke(json!({"task_id": "t1"}), &c).await.unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("t1"), "expected task id in message, got: {s}");
            assert!(s.contains("removed"), "expected 'removed' in message, got: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn no_active_tasklist_returns_recoverable_error() {
    let c = ctx(MockSvc::no_active());
    let out = TodoDelete.invoke(json!({"task_id": "t1"}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("No active tasklist"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_task_id_returns_recoverable_error() {
    let c = ctx(MockSvc::with_active_ok());
    let out = TodoDelete.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("task_id"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn task_not_found_returns_recoverable_error() {
    let c = ctx(MockSvc::task_not_found());
    let out = TodoDelete.invoke(json!({"task_id": "t1"}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("not found"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn task_not_pending_returns_recoverable_error() {
    let c = ctx(MockSvc::task_not_pending());
    let out = TodoDelete.invoke(json!({"task_id": "t1"}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(
                message.contains("InProgress") || message.contains("Pending"),
                "got: {message}"
            );
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn subagent_context_returns_error() {
    let svc = MockSvc::with_active_ok();
    // depth > 0 simulates a subagent context.
    let c = RunnerContext::new("s", "agent1")
        .unwrap()
        .with_tasklist_service(svc)
        .with_depth(1);
    let out = TodoDelete.invoke(json!({"task_id": "t1"}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, .. } => {
            assert!(message.contains("subagent"), "got: {message}");
        }
        other => panic!("expected Error for subagent depth, got {other:?}"),
    }
}
