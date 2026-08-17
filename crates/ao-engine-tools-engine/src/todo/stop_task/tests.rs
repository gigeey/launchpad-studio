use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, RunnerContext, TasklistServiceHandle, TerminalWatcherGuard, ToolOutput};
use ao_protocol::{
    error::AoError,
    tasklist::{Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistOwner, TasklistStatus},
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::TodoStopTask;

enum StopOutcome {
    Ok,
    TaskNotFound,
    NotInProgress,
}

struct MockSvc {
    active: Option<Tasklist>,
    stop_outcome: StopOutcome,
}

impl MockSvc {
    fn no_active() -> Arc<Self> {
        Arc::new(Self { active: None, stop_outcome: StopOutcome::Ok })
    }

    fn with_active_ok() -> Arc<Self> {
        Arc::new(Self { active: Some(fake_tasklist()), stop_outcome: StopOutcome::Ok })
    }

    fn task_not_found() -> Arc<Self> {
        Arc::new(Self {
            active: Some(fake_tasklist()),
            stop_outcome: StopOutcome::TaskNotFound,
        })
    }

    fn task_not_inprogress() -> Arc<Self> {
        Arc::new(Self {
            active: Some(fake_tasklist()),
            stop_outcome: StopOutcome::NotInProgress,
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
                status: TaskStatus::InProgress,
                expected_outputs: vec![],
                error_log: vec![],
                attempt_count: 1,
                comments: vec![],
                attachments: vec![],
                notification_parse_retry_count: 0,
                parse_failed: false,
                remind_me: None,
                assignment: Some(TaskAssignment {
                    owner_agent_id: "runner-agent".to_string(),
                    mode: ao_protocol::tasklist::AssignmentMode::Classified,
                }),
                classifier_token: 2,
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
        Err(AoError::Internal("not implemented in mock".into()))
    }

    async fn cancel_for_agent(&self, _: &str) -> Result<ao_engine_tools_core::CancelOutcome, AoError> {
        Err(AoError::Internal("not implemented in mock".into()))
    }

    async fn set_assignment(&self, _: &str, _: &str, _: &str, _: Option<TaskAssignment>, _: u64) -> Result<bool, AoError> {
        unimplemented!()
    }

    async fn stop_task_for_agent(&self, _agent_id: &str, _tasklist_id: &str, task_id: &str) -> Result<(), AoError> {
        match self.stop_outcome {
            StopOutcome::Ok => Ok(()),
            StopOutcome::TaskNotFound => Err(AoError::TaskNotFound(task_id.to_string())),
            StopOutcome::NotInProgress => Err(AoError::InvalidTasklistTransition(format!(
                "cannot stop task {} in status Pending; only InProgress tasks can be stopped",
                task_id
            ))),
        }
    }
}

fn ctx(svc: Arc<dyn TasklistServiceHandle + Send + Sync>) -> RunnerContext {
    RunnerContext::new("s", "agent1").unwrap().with_tasklist_service(svc)
}

#[tokio::test]
async fn inprogress_task_stopped_successfully() {
    let c = ctx(MockSvc::with_active_ok());
    let out = TodoStopTask.invoke(json!({"task_id": "t1"}), &c).await.unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("t1"), "expected task id in message, got: {s}");
            assert!(s.contains("stopped"), "expected 'stopped' in message, got: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn reject_task_not_inprogress() {
    let c = ctx(MockSvc::task_not_inprogress());
    let out = TodoStopTask.invoke(json!({"task_id": "t1"}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(
                message.contains("InProgress") || message.contains("stop"),
                "expected transition error, got: {message}"
            );
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn no_active_tasklist_returns_error() {
    let c = ctx(MockSvc::no_active());
    let out = TodoStopTask.invoke(json!({"task_id": "t1"}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("No active tasklist"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn task_not_found_returns_error() {
    let c = ctx(MockSvc::task_not_found());
    let out = TodoStopTask.invoke(json!({"task_id": "missing"}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("not found"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_task_id_returns_error() {
    let c = ctx(MockSvc::with_active_ok());
    let out = TodoStopTask.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("task_id"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
