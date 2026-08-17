use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, RunnerContext, TasklistServiceHandle, ToolOutput};
use ao_protocol::{
    error::AoError,
    tasklist::{
        AssignmentMode, Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistOwner,
        TasklistStatus,
    },
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::TodoList;

fn fake_task(id: &str, status: TaskStatus, owner: &str) -> Task {
    Task {
        id: id.to_string(),
        owner_agent_id: owner.to_string(),
        prompt: format!("do {id}"),
        expected_outputs: vec![],
        status,
        group_id: "g1".to_string(),
        attempt_count: 0,
        error_log: vec![],
        comments: vec![],
        attachments: vec![],
        remind_me: None,
        parse_failed: false,
        notification_parse_retry_count: 0,
        assignment: None,
        classifier_token: 0,
        dispatch_token: 0,
    }
}

fn fake_task_with_assignment(id: &str, status: TaskStatus, owner: &str, assignment: TaskAssignment) -> Task {
    Task {
        assignment: Some(assignment),
        ..fake_task(id, status, owner)
    }
}

fn fake_tasklist_with_tasks() -> Tasklist {
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
            tasks: vec![
                fake_task("t1", TaskStatus::Completed, "delegate1"),
                fake_task("t2", TaskStatus::InProgress, ""),
                fake_task("t3", TaskStatus::Pending, ""),
                fake_task_with_assignment(
                    "t4",
                    TaskStatus::Pending,
                    "old-owner",
                    TaskAssignment {
                        owner_agent_id: "new-owner".to_string(),
                        mode: AssignmentMode::Pinned,
                    },
                ),
            ],
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

struct MockSvc {
    active: Option<Tasklist>,
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
    async fn terminal_watcher(
        &self,
        _tasklist_id: &str,
    ) -> Result<ao_engine_tools_core::TerminalWatcherGuard, ao_protocol::error::AoError> {
        Err(ao_protocol::error::AoError::Internal("terminal_watcher not implemented in mock".into()))
    }
    async fn cancel_for_agent(&self, _: &str) -> Result<ao_engine_tools_core::CancelOutcome, ao_protocol::error::AoError> {
        Err(ao_protocol::error::AoError::Internal("cancel_for_agent not implemented in mock".into()))
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

#[tokio::test]
async fn happy_path_with_tasks() {
    let svc = Arc::new(MockSvc { active: Some(fake_tasklist_with_tasks()) });
    let c = ctx(svc);
    let out = TodoList.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["active"], true);
            assert_eq!(v["tasklist_id"], "tl-1");
            assert_eq!(v["name"], "My List");
            let groups = v["groups"].as_array().unwrap();
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0]["mode"], "seq");
            let tasks = groups[0]["tasks"].as_array().unwrap();
            assert_eq!(tasks.len(), 4);
            assert_eq!(tasks[0]["id"], "t1");
            assert_eq!(tasks[0]["status"], "completed");
            assert_eq!(tasks[0]["assignee"], "delegate1");
            assert!(tasks[0]["assignment_mode"].is_null());

            // t4 carries a pinned assignment whose owner differs from the
            // (stale) base `owner_agent_id` — the authoritative assignment
            // owner must win, matching the feeder's `resolve_executor_agent_id`.
            assert_eq!(tasks[3]["id"], "t4");
            assert_eq!(tasks[3]["assignee"], "new-owner");
            assert_eq!(tasks[3]["assignment_mode"], "pinned");
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn assignee_falls_back_to_base_owner_when_unassigned() {
    let svc = Arc::new(MockSvc { active: Some(fake_tasklist_with_tasks()) });
    let c = ctx(svc);
    let out = TodoList.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            let tasks = v["groups"][0]["tasks"].as_array().unwrap();
            // t2/t3 have no assignment and an empty base owner_agent_id — assignee
            // must be null (not an empty string), and assignment_mode must be null.
            assert_eq!(tasks[1]["id"], "t2");
            assert!(tasks[1]["assignee"].is_null());
            assert!(tasks[1]["assignment_mode"].is_null());
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn assignee_and_assignment_mode_reflect_owner_update() {
    // Round-trip check: a task that started Classified to "old-owner" and
    // was then reassigned via `TodoUpdate { owner: "new-owner" }` ends up
    // on-disk exactly like `t4` above (assignment = Some({owner: new-owner,
    // Pinned}), base owner_agent_id left stale at "old-owner" since the base
    // write and the assignment write are independent). TodoList must report
    // the authoritative post-update owner, not the stale base field.
    let task = fake_task_with_assignment(
        "t-reassigned",
        TaskStatus::Pending,
        "old-owner",
        TaskAssignment { owner_agent_id: "new-owner".to_string(), mode: AssignmentMode::Pinned },
    );
    let mut tl = fake_tasklist_with_tasks();
    tl.groups[0].tasks = vec![task];
    let svc = Arc::new(MockSvc { active: Some(tl) });
    let c = ctx(svc);
    let out = TodoList.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            let tasks = v["groups"][0]["tasks"].as_array().unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0]["id"], "t-reassigned");
            assert_eq!(tasks[0]["assignee"], "new-owner", "must reflect the post-update owner");
            assert_eq!(tasks[0]["assignment_mode"], "pinned", "must reflect the pin set by TodoUpdate");
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn no_active_tasklist() {
    let svc = Arc::new(MockSvc { active: None });
    let c = ctx(svc);
    let out = TodoList.invoke(json!({}), &c).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["active"], false);
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}
