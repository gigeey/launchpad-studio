//! End-to-end tool-loop test for the Todo* tool family.
//!
//! Exercises the full agent-perspective lifecycle of an agent-owned tasklist:
//!
//!   TodoCreate  →  TodoList (verify initial state)
//!     →  TodoComplete(task-1)  →  TodoList (task-1 done, task-2 pending)
//!     →  TodoComplete(task-2)  →  TodoList (no active tasklist — list complete)
//!
//! Uses a stateful `MockTasklistService` that implements `TasklistServiceHandle`
//! and tracks task-completion calls so the test can verify the correct sequence
//! was invoked without requiring a live `ao-engine` AppState.

use std::sync::{Arc, Mutex};

use ao_engine_tools_core::{EngineTool, RunnerContext, TasklistServiceHandle, ToolOutput};
use ao_engine_tools_engine::todo::{
    complete::TodoComplete, create::TodoCreate, list::TodoList,
};
use ao_protocol::{
    error::AoError,
    tasklist::{Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistOwner, TasklistStatus},
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};

// ─── Stateful mock ────────────────────────────────────────────────────────────

#[derive(Default)]
struct State {
    /// The currently active tasklist, or None once all tasks are completed.
    active: Option<Tasklist>,
    /// Number of times `complete_task_for_agent` was called.
    complete_calls: u32,
}

/// A `TasklistServiceHandle` mock that maintains an in-memory tasklist and
/// records completion calls so tests can make assertions.
struct MockTasklistService {
    state: Mutex<State>,
}

impl MockTasklistService {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
        })
    }

    fn complete_call_count(&self) -> u32 {
        self.state.lock().unwrap().complete_calls
    }

}

fn make_seq_tasklist(agent_id: &str, title: &str, tasks: Vec<Task>) -> Tasklist {
    Tasklist {
        id: "mock-tl-id".to_string(),
        owner: TasklistOwner::Agent { agent_id: agent_id.to_string() },
        team_id: None,
        title: title.to_string(),
        description: String::new(),
        status: TasklistStatus::Active,
        groups: vec![TaskGroup {
            id: "g1".to_string(),
            mode: TaskGroupMode::Seq,
            tasks,
        }],
        workspace_dir: "/tmp/mock-workspace".to_string(),
        transcripts_dir: "/tmp/mock-transcripts".to_string(),
        created_at: Utc::now(),
        last_active_at: None,
        copilot_agent_id: None,
        last_opened_at: None,
        project_id: None,
        thread_id: None,
        }
}


#[async_trait]
impl TasklistServiceHandle for MockTasklistService {
    async fn agent_active(&self, _agent_id: &str) -> Result<Option<Tasklist>, AoError> {
        Ok(self.state.lock().unwrap().active.clone())
    }

    async fn create_for_agent(
        &self,
        agent_id: &str,
        name: String,
        groups: Vec<TaskGroup>,
    ) -> Result<Tasklist, AoError> {
        let tasks: Vec<Task> = groups
            .into_iter()
            .flat_map(|g| g.tasks)
            .collect();

        let tl = make_seq_tasklist(agent_id, &name, tasks);
        self.state.lock().unwrap().active = Some(tl.clone());
        Ok(tl)
    }

    async fn get_agent_max_instances(&self, _agent_id: &str) -> Result<u32, AoError> {
        Ok(2)
    }

    async fn add_group_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        _tasks: Vec<Task>,
        _mode: TaskGroupMode,
    ) -> Result<Tasklist, AoError> {
        unimplemented!("add_group_for_agent not needed in this E2E test")
    }

    async fn update_task_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        _task_id: &str,
        _prompt: Option<String>,
        _owner_agent_id: Option<String>,
        _expected_outputs: Option<Vec<String>>,
    ) -> Result<Tasklist, AoError> {
        unimplemented!("update_task_for_agent not needed in this E2E test")
    }

    /// Mark a task as Completed.  When all tasks in the list are done, clears
    /// `active` to simulate the service returning None on the next `agent_active` call.
    async fn complete_task_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        task_id: &str,
    ) -> Result<(), AoError> {
        let mut st = self.state.lock().unwrap();
        st.complete_calls += 1;

        let tl = st.active.as_mut().ok_or_else(|| {
            AoError::Internal("no active tasklist in mock".into())
        })?;

        let task = tl
            .groups
            .iter_mut()
            .flat_map(|g| g.tasks.iter_mut())
            .find(|t| t.id == task_id)
            .ok_or_else(|| AoError::TaskNotFound(task_id.to_string()))?;

        task.status = TaskStatus::Completed;

        // If all tasks are now terminal, clear the active tasklist to simulate
        // the service returning None (list completed).
        let all_done = tl
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .all(|t| matches!(t.status, TaskStatus::Completed | TaskStatus::Skipped | TaskStatus::Failed));

        if all_done {
            st.active = None;
        }

        Ok(())
    }

    async fn terminal_watcher(
        &self,
        _tasklist_id: &str,
    ) -> Result<ao_engine_tools_core::TerminalWatcherGuard, AoError> {
        Err(AoError::Internal("terminal_watcher not implemented in mock".into()))
    }

    async fn cancel_for_agent(&self, _agent_id: &str) -> Result<ao_engine_tools_core::CancelOutcome, AoError> {
        Err(AoError::Internal("cancel_for_agent not implemented in mock".into()))
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

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn make_ctx(svc: Arc<dyn TasklistServiceHandle + Send + Sync>) -> RunnerContext {
    RunnerContext::new("e2e-session", "agent1")
        .expect("cwd available")
        .with_tasklist_service(svc)
}

fn extract_structured(output: ToolOutput) -> Value {
    match output {
        ToolOutput::Structured(v) => v,
        other => panic!("expected Structured ToolOutput, got {:?}", other),
    }
}

fn extract_text(output: ToolOutput) -> String {
    match output {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text ToolOutput, got {:?}", other),
    }
}

// ─── E2E test ─────────────────────────────────────────────────────────────────

/// Full SEQ tasklist lifecycle via the Todo* tool family:
///
///   1. TodoCreate   → creates 2-item SEQ list; returns structured success
///   2. TodoList     → shows both tasks as Pending
///   3. TodoComplete(task-a) → advances the SEQ list; mock records the call
///   4. TodoList     → task-a is Completed, task-b is Pending
///   5. TodoComplete(task-b) → marks last task done; mock transitions list to complete
///   6. TodoList     → "No active tasklist" (list is done — final completion state)
///
/// The "mocked dispatcher" is the test itself: it calls TodoComplete to simulate
/// what the agent dispatcher would do when driving an item to completion.
#[tokio::test]
async fn todo_tool_full_seq_lifecycle() {
    let svc = MockTasklistService::new();
    let ctx = make_ctx(Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>);

    // ── Step 1: TodoCreate ────────────────────────────────────────────────────
    let create_out = TodoCreate
        .invoke(
            json!({
                "name": "E2E SEQ List",
                "dispatch_mode": "async",
                "items": [
                    { "title": "Task A", "brief": "Investigate the logs" },
                    { "title": "Task B", "brief": "Write a summary" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let created = extract_structured(create_out);
    assert_eq!(created["name"], "E2E SEQ List");
    assert_eq!(created["mode"], "seq");
    assert_eq!(created["status"], "active");
    assert_eq!(created["item_count"], 2);

    // ── Step 2: TodoList — verify initial state ───────────────────────────────
    let list_out = TodoList.invoke(json!({}), &ctx).await.unwrap();
    let listed = extract_structured(list_out);
    assert_eq!(listed["active"], true, "tasklist should be active");
    assert_eq!(listed["name"], "E2E SEQ List");

    let groups = listed["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    let tasks = groups[0]["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["status"], "pending");
    assert_eq!(tasks[1]["status"], "pending");

    // Capture task IDs from the list output for use in TodoComplete calls.
    let task_a_id = tasks[0]["id"].as_str().unwrap().to_string();
    let task_b_id = tasks[1]["id"].as_str().unwrap().to_string();

    // ── Step 3: TodoComplete(task-a) — mocked dispatcher drives item 1 ───────
    let complete_a_out = TodoComplete
        .invoke(json!({ "task_id": task_a_id }), &ctx)
        .await
        .unwrap();

    let msg_a = extract_text(complete_a_out);
    assert!(
        msg_a.contains(&task_a_id),
        "completion message should reference task_id; got: {msg_a}"
    );
    assert_eq!(svc.complete_call_count(), 1);

    // ── Step 4: TodoList — task-a done, task-b still pending ─────────────────
    let list2_out = TodoList.invoke(json!({}), &ctx).await.unwrap();
    let listed2 = extract_structured(list2_out);
    assert_eq!(listed2["active"], true);

    let statuses: Vec<&str> = listed2["groups"][0]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        statuses,
        vec!["completed", "pending"],
        "task-a should be completed, task-b still pending"
    );

    // ── Step 5: TodoComplete(task-b) — mocked dispatcher drives last item ────
    let complete_b_out = TodoComplete
        .invoke(json!({ "task_id": task_b_id }), &ctx)
        .await
        .unwrap();

    let msg_b = extract_text(complete_b_out);
    assert!(
        msg_b.contains(&task_b_id),
        "completion message should reference task_id; got: {msg_b}"
    );
    assert_eq!(svc.complete_call_count(), 2, "complete_task should have been called twice total");

    // ── Step 6: TodoList — list is complete; final completion state ───────────
    // After the last item completes, MockTasklistService clears `active`.
    // TodoList returns the "No active tasklist" structured response.
    let list3_out = TodoList.invoke(json!({}), &ctx).await.unwrap();
    let listed3 = extract_structured(list3_out);
    assert_eq!(
        listed3["active"], false,
        "final state: no active tasklist after all items complete (got: {listed3})"
    );
    assert!(
        listed3["message"]
            .as_str()
            .unwrap_or("")
            .contains("TodoCreate"),
        "final message should suggest TodoCreate; got: {}",
        listed3["message"]
    );
}

/// Verifies that TodoCreate returns AlreadyExists when an active tasklist
/// already exists, and that the subsequent TodoList still shows the original list.
#[tokio::test]
async fn todo_create_blocked_when_active_list_exists() {
    let svc = MockTasklistService::new();
    let ctx = make_ctx(Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>);

    // First create succeeds.
    let first = TodoCreate
        .invoke(
            json!({
                "name": "First List",
                "dispatch_mode": "async",
                "items": [{ "title": "T1", "brief": "B1" }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        matches!(first, ToolOutput::Structured(_)),
        "first create should succeed"
    );

    // Second create must fail with AlreadyExists.
    let second = TodoCreate
        .invoke(
            json!({
                "name": "Duplicate List",
                "dispatch_mode": "async",
                "items": [{ "title": "T2", "brief": "B2" }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match second {
        ToolOutput::Error { message, recoverable } => {
            assert!(
                message.contains("already has an active tasklist"),
                "error should mention active tasklist; got: {message}"
            );
            assert!(recoverable, "AlreadyExists should be recoverable");
        }
        other => panic!("expected Error for duplicate create, got {other:?}"),
    }

    // TodoList still shows the original list.
    let list_out = TodoList.invoke(json!({}), &ctx).await.unwrap();
    let listed = extract_structured(list_out);
    assert_eq!(listed["name"], "First List");
}
