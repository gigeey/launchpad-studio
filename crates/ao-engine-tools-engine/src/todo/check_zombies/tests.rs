use std::sync::Arc;

use ao_engine_tools_core::{
    EngineTool, RunnerContext, TasklistServiceHandle, TerminalWatcherGuard, ToolOutput,
    ZombieReport,
};
use ao_protocol::{
    error::AoError,
    tasklist::{Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistOwner, TasklistStatus},
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::TodoCheckZombies;

struct MockSvc {
    active: Option<Tasklist>,
    zombies: Vec<ZombieReport>,
    requeue_ok: bool,
    requeue_calls: Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockSvc {
    fn clean(active: Option<Tasklist>) -> Arc<Self> {
        Arc::new(Self {
            active,
            zombies: vec![],
            requeue_ok: true,
            requeue_calls: Default::default(),
        })
    }

    fn with_zombies(zombies: Vec<ZombieReport>) -> Arc<Self> {
        Arc::new(Self {
            active: Some(fake_tasklist()),
            zombies,
            requeue_ok: true,
            requeue_calls: Default::default(),
        })
    }

    fn with_requeue_fail(zombies: Vec<ZombieReport>) -> Arc<Self> {
        Arc::new(Self {
            active: Some(fake_tasklist()),
            zombies,
            requeue_ok: false,
            requeue_calls: Default::default(),
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
                prompt: "Do the analysis work for the quarterly report".to_string(),
                owner_agent_id: "worker".to_string(),
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
                    owner_agent_id: "worker".to_string(),
                    mode: ao_protocol::tasklist::AssignmentMode::Classified,
                }),
                classifier_token: 1,
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

fn one_zombie() -> ZombieReport {
    ZombieReport {
        task_id: "t1".to_string(),
        task_title: "Do the analysis work for the quarterly report".to_string(),
        secs_since_dispatch: Some(143),
        agent_id: "worker".to_string(),
        tasklist_id: "tl-1".to_string(),
    }
}

fn zombie_no_ts() -> ZombieReport {
    ZombieReport {
        task_id: "t2".to_string(),
        task_title: "Write the report".to_string(),
        secs_since_dispatch: None,
        agent_id: "worker".to_string(),
        tasklist_id: "tl-1".to_string(),
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
        Err(AoError::Internal("not implemented".into()))
    }
    async fn cancel_for_agent(&self, _: &str) -> Result<ao_engine_tools_core::CancelOutcome, AoError> {
        Err(AoError::Internal("not implemented".into()))
    }
    async fn set_assignment(&self, _: &str, _: &str, _: &str, _: Option<TaskAssignment>, _: u64) -> Result<bool, AoError> {
        unimplemented!()
    }
    async fn requeue_task_for_agent(&self, _agent_id: &str, _tl: &str, task_id: &str) -> Result<(), AoError> {
        self.requeue_calls.lock().unwrap().push(task_id.to_string());
        if self.requeue_ok {
            Ok(())
        } else {
            Err(AoError::InvalidTasklistTransition(format!("cannot requeue {task_id}")))
        }
    }
    async fn check_zombies_for_agent(&self, _agent_id: &str, _grace_secs: u64) -> Result<Vec<ZombieReport>, AoError> {
        Ok(self.zombies.clone())
    }
}

fn ctx(svc: Arc<dyn TasklistServiceHandle + Send + Sync>) -> RunnerContext {
    RunnerContext::new("s", "agent1").unwrap().with_tasklist_service(svc)
}

// ── detection mode ───────────────────────────────────────────────────────────

#[tokio::test]
async fn zombie_detected_reports_task() {
    let c = ctx(MockSvc::with_zombies(vec![one_zombie()]));
    let out = TodoCheckZombies.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("1 zombie"), "got: {s}");
            assert!(s.contains("t1"), "got: {s}");
            assert!(s.contains("143s ago"), "got: {s}");
            assert!(s.contains("worker"), "got: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn no_dispatch_timestamp_shows_server_restart_hint() {
    let c = ctx(MockSvc::with_zombies(vec![zombie_no_ts()]));
    let out = TodoCheckZombies.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("t2"), "got: {s}");
            assert!(s.contains("server restart"), "got: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn healthy_runner_not_reported() {
    // Service returns no zombies (all runners alive).
    let c = ctx(MockSvc::clean(Some(fake_tasklist())));
    let out = TodoCheckZombies.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("No zombie"), "got: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn no_active_tasklist_reports_no_zombies() {
    let c = ctx(MockSvc::clean(None));
    let out = TodoCheckZombies.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("No zombie"), "got: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── auto_requeue mode ────────────────────────────────────────────────────────

#[tokio::test]
async fn auto_requeue_calls_requeue_for_each_zombie() {
    let svc = MockSvc::with_zombies(vec![one_zombie(), zombie_no_ts()]);
    let calls = Arc::clone(&svc.requeue_calls);
    let c = ctx(svc);
    let out = TodoCheckZombies
        .invoke(json!({"auto_requeue": true}), &c)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("2 zombie"), "got: {s}");
            assert!(s.contains("Pending"), "got: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "expected 2 requeue calls");
    assert!(calls.contains(&"t1".to_string()));
    assert!(calls.contains(&"t2".to_string()));
}

#[tokio::test]
async fn auto_requeue_reports_per_task_failure() {
    let svc = MockSvc::with_requeue_fail(vec![one_zombie()]);
    let c = ctx(svc);
    let out = TodoCheckZombies
        .invoke(json!({"auto_requeue": true}), &c)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("requeue failed"), "got: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn service_unavailable_returns_error() {
    let c = RunnerContext::new("s", "agent1").unwrap();
    let out = TodoCheckZombies.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Error { message, recoverable: false } => {
            assert!(message.contains("not available"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
