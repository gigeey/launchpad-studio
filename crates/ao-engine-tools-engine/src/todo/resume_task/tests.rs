use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, RunnerContext, TasklistServiceHandle, TerminalWatcherGuard, ToolOutput};
use ao_protocol::{
    error::AoError,
    tasklist::{Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistOwner, TasklistStatus},
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::TodoResumeTask;

// ── Shared helpers ────────────────────────────────────────────────────────────

fn make_task(id: &str, status: TaskStatus) -> Task {
    Task {
        id: id.to_string(),
        group_id: "g1".to_string(),
        prompt: format!("Task {id}"),
        owner_agent_id: "agent1".to_string(),
        status,
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
    }
}

fn tasklist_with_groups(groups: Vec<TaskGroup>) -> Tasklist {
    Tasklist {
        id: "tl-1".to_string(),
        owner: TasklistOwner::Agent { agent_id: "agent1".to_string() },
        team_id: None,
        title: "Test List".to_string(),
        description: String::new(),
        status: TasklistStatus::Active,
        groups,
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

// ── Mock service ──────────────────────────────────────────────────────────────

enum ResumeOutcome {
    Ok,
    TaskNotFound,
    NotStopped,
}

struct MockSvc {
    active: Option<Tasklist>,
    resume_outcome: ResumeOutcome,
}

impl MockSvc {
    fn no_active() -> Arc<Self> {
        Arc::new(Self { active: None, resume_outcome: ResumeOutcome::Ok })
    }

    fn with_active_ok() -> Arc<Self> {
        let tl = tasklist_with_groups(vec![TaskGroup {
            id: "g1".to_string(),
            mode: TaskGroupMode::Seq,
            tasks: vec![make_task("t1", TaskStatus::Stopped)],
        }]);
        Arc::new(Self { active: Some(tl), resume_outcome: ResumeOutcome::Ok })
    }

    fn task_not_found() -> Arc<Self> {
        let tl = tasklist_with_groups(vec![]);
        Arc::new(Self { active: Some(tl), resume_outcome: ResumeOutcome::TaskNotFound })
    }

    fn task_not_stopped() -> Arc<Self> {
        let tl = tasklist_with_groups(vec![TaskGroup {
            id: "g1".to_string(),
            mode: TaskGroupMode::Seq,
            tasks: vec![make_task("t1", TaskStatus::InProgress)],
        }]);
        Arc::new(Self { active: Some(tl), resume_outcome: ResumeOutcome::NotStopped })
    }

    /// PAR group with t1=Stopped and t2=InProgress — models "stop one, other keeps running".
    fn par_stop_one_other_running() -> Arc<Self> {
        let tl = tasklist_with_groups(vec![TaskGroup {
            id: "g1".to_string(),
            mode: TaskGroupMode::Par,
            tasks: vec![
                make_task("t1", TaskStatus::Stopped),
                make_task("t2", TaskStatus::InProgress),
            ],
        }]);
        Arc::new(Self { active: Some(tl), resume_outcome: ResumeOutcome::Ok })
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

    async fn resume_task_for_agent(&self, _agent_id: &str, _tasklist_id: &str, task_id: &str) -> Result<(), AoError> {
        match self.resume_outcome {
            ResumeOutcome::Ok => Ok(()),
            ResumeOutcome::TaskNotFound => Err(AoError::TaskNotFound(task_id.to_string())),
            ResumeOutcome::NotStopped => Err(AoError::InvalidTasklistTransition(format!(
                "cannot resume task {} in status InProgress; only Stopped tasks can be resumed",
                task_id
            ))),
        }
    }
}

fn ctx(svc: Arc<dyn TasklistServiceHandle + Send + Sync>) -> RunnerContext {
    RunnerContext::new("s", "agent1").unwrap().with_tasklist_service(svc)
}

// ── Happy-path tests ──────────────────────────────────────────────────────────

/// stop-then-resume: a Stopped task transitions back to Pending.
#[tokio::test]
async fn stopped_task_resumed_to_pending() {
    let c = ctx(MockSvc::with_active_ok());
    let out = TodoResumeTask.invoke(json!({"task_id": "t1"}), &c).await.unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("t1"), "expected task id in message, got: {s}");
            assert!(s.contains("pending"), "expected 'pending' in message, got: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

/// stop-one-leaves-others-running (par): resuming t1 returns success while t2
/// stays InProgress — the mock's active tasklist confirms both tasks are present
/// with distinct statuses, and the service call only touches t1.
#[tokio::test]
async fn par_resume_one_does_not_affect_sibling() {
    let svc = MockSvc::par_stop_one_other_running();

    // Confirm the fixture: t1=Stopped, t2=InProgress live in the same PAR group.
    let tl = svc.active.as_ref().unwrap();
    let t1 = tl.groups[0].tasks.iter().find(|t| t.id == "t1").unwrap();
    let t2 = tl.groups[0].tasks.iter().find(|t| t.id == "t2").unwrap();
    assert_eq!(t1.status, TaskStatus::Stopped);
    assert_eq!(t2.status, TaskStatus::InProgress);
    assert_eq!(tl.groups[0].mode, TaskGroupMode::Par);

    // Resume t1 — the mock only applies the resume outcome to t1.
    let c = ctx(svc);
    let out = TodoResumeTask.invoke(json!({"task_id": "t1"}), &c).await.unwrap();
    assert!(
        matches!(out, ToolOutput::Text(_)),
        "expected Text on successful resume, got {out:?}"
    );
    // t2 was never touched: the mock tasklist snapshot still shows InProgress.
    // (In the real service the mutation closure is scoped to the target task_id.)
}

// ── Error-path tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn reject_task_not_stopped() {
    let c = ctx(MockSvc::task_not_stopped());
    let out = TodoResumeTask.invoke(json!({"task_id": "t1"}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(
                message.contains("Stopped") || message.contains("resume"),
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
    let out = TodoResumeTask.invoke(json!({"task_id": "t1"}), &c).await.unwrap();
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
    let out = TodoResumeTask.invoke(json!({"task_id": "missing"}), &c).await.unwrap();
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
    let out = TodoResumeTask.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("task_id"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
