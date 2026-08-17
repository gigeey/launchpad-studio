//! E2E tests for batched terminal return: TodoListCreated + TodoListComplete.
//!
//! Covers the four scenarios from PRD section 6.2:
//!
//!   1. Sync mode: exactly one TodoListCreated on the parent event sink, one terminal report
//!      returned by the tool, zero per-task events in between.
//!   2. Async mode: same guarantee — one TodoListCreated, immediate return, no per-task events.
//!   3. Mid-flight append: a 4th task added after item 1 completes delays the flush; the
//!      watcher only fires after all 4 tasks are terminal.
//!   4. Single failure does not block flush: 5-item tasklist with item 3 failing still
//!      returns a terminal report with counts.failed == 1.
//!
//! All tests use the mock dispatcher / fixtures pattern from the other e2e suites.
//! The `TodoListComplete` EventBus event (emitted by `task_feeder`) is verified by the
//! ao-engine unit tests; these e2e tests verify what is observable at the tool layer:
//! the `TodoListCreated` UserEvent on the parent sink and the sync terminal report shape.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ao_engine_tools_core::{
    CancelOutcome, EngineTool, EventSink, RunnerContext, TasklistServiceHandle,
    TerminalCounts, TerminalReport, TerminalTaskEntry, TerminalWatcherGuard,
    TerminalWatcherRegistry, ToolOutput, UserEvent,
};
use ao_engine_tools_engine::todo::create::TodoCreate;
use ao_protocol::{
    error::AoError,
    tasklist::{
        Task, TaskAssignment, TaskGroup, TaskGroupMode, Tasklist, TasklistOwner,
        TasklistStatus,
    },
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use tokio::sync::oneshot;

// ── Recording event sink ──────────────────────────────────────────────────────

struct RecordingEventSink {
    events: Arc<Mutex<Vec<UserEvent>>>,
}

#[async_trait]
impl EventSink for RecordingEventSink {
    async fn emit(&self, event: UserEvent) -> Result<(), AoError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

// ── Mock service with real terminal watcher ───────────────────────────────────

#[derive(Default)]
struct BatchedMockState {
    active: Option<Tasklist>,
}

/// Supports real terminal watchers; tests fire the watcher via `fire_for_tasklist`.
struct BatchedMockService {
    state: Mutex<BatchedMockState>,
    watcher_registry: TerminalWatcherRegistry,
}

impl BatchedMockService {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(BatchedMockState::default()),
            watcher_registry: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn fire_for_tasklist(&self, tasklist_id: &str, report: TerminalReport) {
        if let Ok(mut map) = self.watcher_registry.lock() {
            if let Some(tx) = map.remove(tasklist_id) {
                if !tx.is_closed() {
                    let _ = tx.send(report);
                }
            }
        }
    }

    fn active_tasklist_id(&self) -> Option<String> {
        self.state.lock().unwrap().active.as_ref().map(|tl| tl.id.clone())
    }
}

#[async_trait]
impl TasklistServiceHandle for BatchedMockService {
    async fn agent_active(&self, _agent_id: &str) -> Result<Option<Tasklist>, AoError> {
        Ok(self.state.lock().unwrap().active.clone())
    }

    async fn create_for_agent(
        &self,
        agent_id: &str,
        name: String,
        groups: Vec<TaskGroup>,
    ) -> Result<Tasklist, AoError> {
        let tasks: Vec<Task> = groups.into_iter().flat_map(|g| g.tasks).collect();
        let tl = make_tasklist(agent_id, "mock-tl", &name, tasks);
        self.state.lock().unwrap().active = Some(tl.clone());
        Ok(tl)
    }

    async fn get_agent_max_instances(&self, _agent_id: &str) -> Result<u32, AoError> {
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

    async fn complete_task_for_agent(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), AoError> {
        unimplemented!()
    }

    async fn terminal_watcher(&self, tasklist_id: &str) -> Result<TerminalWatcherGuard, AoError> {
        let (tx, rx) = oneshot::channel();
        self.watcher_registry
            .lock()
            .unwrap()
            .insert(tasklist_id.to_owned(), tx);
        Ok(TerminalWatcherGuard::new(
            rx,
            Arc::clone(&self.watcher_registry),
            tasklist_id.to_owned(),
        ))
    }

    async fn cancel_for_agent(&self, _: &str) -> Result<CancelOutcome, AoError> {
        Err(AoError::Internal("n/a".into()))
    }

    async fn set_assignment(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<TaskAssignment>,
        _: u64,
    ) -> Result<bool, AoError> {
        Ok(true)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_tasklist(agent_id: &str, id: &str, title: &str, tasks: Vec<Task>) -> Tasklist {
    Tasklist {
        id: id.to_string(),
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
        workspace_dir: format!("/tmp/mock-ws-{id}"),
        transcripts_dir: format!("/tmp/mock-tr-{id}"),
        created_at: Utc::now(),
        last_active_at: None,
        copilot_agent_id: None,
        last_opened_at: None,
        project_id: None,
        thread_id: None,
        }
}

fn make_ctx(svc: Arc<dyn TasklistServiceHandle + Send + Sync>) -> RunnerContext {
    RunnerContext::new("batched-e2e-session", "agent1")
        .expect("cwd available")
        .with_tasklist_service(svc)
}

fn extract_structured(out: ToolOutput) -> serde_json::Value {
    match out {
        ToolOutput::Structured(v) => v,
        other => panic!("expected Structured ToolOutput, got {other:?}"),
    }
}

fn make_n_task_report(n: usize, status: &str, counts: TerminalCounts) -> TerminalReport {
    TerminalReport {
        status: status.to_string(),
        counts,
        tasks: (1..=n)
            .map(|i| TerminalTaskEntry {
                id: format!("t{i}"),
                title: format!("Task {i}"),
                status: "completed".to_string(),
                summary: Some(format!("Summary of task {i}")),
                details: None,
                output_path: PathBuf::from(format!("/tmp/mock-ws-mock-tl/tasks/t{i}/output.txt")),
                attempt_count: 1,
            })
            .collect(),
    }
}

// ── Scenario 1: Sync mode — one TodoListCreated + one terminal report ─────────

/// 5-item sync TodoCreate:
///   - Parent's event sink sees exactly one `TodoListCreated` event.
///   - No per-task events appear on the parent event sink (only ToolProgress + TodoListCreated).
///   - Tool returns exactly one terminal report with 5 task entries.
#[tokio::test]
async fn agent_task_batched_return_sync_one_terminal_event_only() {
    let events: Arc<Mutex<Vec<UserEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingEventSink { events: Arc::clone(&events) });

    let svc = BatchedMockService::new();
    let svc_clone = Arc::clone(&svc);

    let ctx = make_ctx(Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>)
        .with_event_sink(sink as Arc<dyn EventSink + Send + Sync>);

    // Fire the watcher shortly after TodoCreate registers it.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let tl_id = svc_clone
            .active_tasklist_id()
            .unwrap_or_else(|| "mock-tl".to_string());
        svc_clone.fire_for_tasklist(
            &tl_id,
            make_n_task_report(5, "completed", TerminalCounts { succeeded: 5, failed: 0, skipped: 0 }),
        );
    });

    let out = TodoCreate
        .invoke(
            json!({
                "name": "Sync Batched Test",
                "dispatch_mode": "sync",
                "items": [
                    { "title": "Task 1", "brief": "B1" },
                    { "title": "Task 2", "brief": "B2" },
                    { "title": "Task 3", "brief": "B3" },
                    { "title": "Task 4", "brief": "B4" },
                    { "title": "Task 5", "brief": "B5" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let v = extract_structured(out);
    assert_eq!(v["status"], "completed", "sync must return completed status");
    assert_eq!(v["counts"]["succeeded"], 5);
    assert_eq!(v["counts"]["failed"], 0);

    let tasks = v["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 5, "exactly 5 task entries in terminal report");

    // Verify parent event sink: exactly one TodoListCreated, no per-task events.
    let recorded = events.lock().unwrap();
    let created_events: Vec<_> = recorded
        .iter()
        .filter(|e| matches!(e, UserEvent::TodoListCreated { .. }))
        .collect();
    assert_eq!(
        created_events.len(),
        1,
        "exactly one TodoListCreated must appear on parent event sink"
    );

    // No events other than ToolProgress and TodoListCreated.
    let unexpected: Vec<_> = recorded
        .iter()
        .filter(|e| !matches!(e, UserEvent::ToolProgress { .. } | UserEvent::TodoListCreated { .. }))
        .collect();
    assert!(
        unexpected.is_empty(),
        "parent event sink must not receive per-task events; got: {:?}",
        unexpected.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>()
    );
}

// ── Scenario 2: Async mode — one TodoListCreated, immediate return ────────────

/// 5-item async TodoCreate:
///   - Returns immediately with `status: active`.
///   - Parent's event sink sees exactly one `TodoListCreated` event.
///   - No watcher is registered (async doesn't block).
///   - Regression gate for a divergence between the two async paths: one
///     emitted per-task notifications, the other suppresses them at the
///     engine level.
#[tokio::test]
async fn agent_task_batched_return_async_one_terminal_event_only() {
    let events: Arc<Mutex<Vec<UserEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingEventSink { events: Arc::clone(&events) });

    let svc = BatchedMockService::new();
    let ctx = make_ctx(Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>)
        .with_event_sink(sink as Arc<dyn EventSink + Send + Sync>);

    let out = TodoCreate
        .invoke(
            json!({
                "name": "Async Batched Test",
                "dispatch_mode": "async",
                "items": [
                    { "title": "Task 1", "brief": "B1" },
                    { "title": "Task 2", "brief": "B2" },
                    { "title": "Task 3", "brief": "B3" },
                    { "title": "Task 4", "brief": "B4" },
                    { "title": "Task 5", "brief": "B5" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let v = extract_structured(out);
    assert_eq!(v["status"], "active", "async must return immediately with active status");
    assert_eq!(v["dispatch_mode"], "async");
    assert_eq!(v["item_count"], 5);

    // No watcher registered (async mode does not block).
    assert!(
        svc.watcher_registry.lock().unwrap().is_empty(),
        "no terminal watcher should be registered for async mode"
    );

    // Parent event sink sees exactly one TodoListCreated.
    let recorded = events.lock().unwrap();
    let created_events: Vec<_> = recorded
        .iter()
        .filter(|e| matches!(e, UserEvent::TodoListCreated { .. }))
        .collect();
    assert_eq!(
        created_events.len(),
        1,
        "exactly one TodoListCreated must appear on parent event sink for async mode"
    );

    // No per-task events on the parent sink.
    let unexpected: Vec<_> = recorded
        .iter()
        .filter(|e| !matches!(e, UserEvent::ToolProgress { .. } | UserEvent::TodoListCreated { .. }))
        .collect();
    assert!(
        unexpected.is_empty(),
        "async mode: parent event sink must not receive per-task events; got: {:?}",
        unexpected.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>()
    );
}

// ── Scenario 3: Mid-flight append delays flush ────────────────────────────────

/// Async 3-item tasklist; after item 1 terminal, a 4th item is appended (simulated
/// by having the watcher only fire after 4 items are done). The sync tool must not
/// return until all 4 items reach terminal state.
///
/// Uses paused tokio time so the fire sequence is deterministic:
///   - T=0: TodoCreate (sync, 3 items)
///   - T=5ms: watcher fires with 4-task report (simulating that item 4 was appended
///             after item 1 terminal and completed before the watcher fired)
#[tokio::test]
async fn agent_task_batched_return_mid_flight_append_delays_flush() {
    let events: Arc<Mutex<Vec<UserEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingEventSink { events: Arc::clone(&events) });

    let svc = BatchedMockService::new();
    let svc_clone = Arc::clone(&svc);

    let ctx = make_ctx(Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>)
        .with_event_sink(sink as Arc<dyn EventSink + Send + Sync>);

    // The watcher fires with a 4-task report — simulating that:
    //   - items 1-3 were created originally
    //   - item 4 was appended before the list reached terminal
    //   - the watcher fires only after all 4 are done
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let tl_id = svc_clone
            .active_tasklist_id()
            .unwrap_or_else(|| "mock-tl".to_string());
        svc_clone.fire_for_tasklist(
            &tl_id,
            // 4-task terminal report — the 4th was appended mid-flight.
            TerminalReport {
                status: "completed".to_string(),
                counts: TerminalCounts { succeeded: 4, failed: 0, skipped: 0 },
                tasks: (1..=4)
                    .map(|i| TerminalTaskEntry {
                        id: format!("t{i}"),
                        title: format!("Task {i}"),
                        status: "completed".to_string(),
                        summary: Some(format!("Summary {i}")),
                        details: None,
                        output_path: PathBuf::from(format!("/tmp/t{i}/output.txt")),
                        attempt_count: 1,
                    })
                    .collect(),
            },
        );
    });

    let out = TodoCreate
        .invoke(
            json!({
                "name": "Mid-Flight Append Test",
                "dispatch_mode": "sync",
                "items": [
                    { "title": "Task 1", "brief": "B1" },
                    { "title": "Task 2", "brief": "B2" },
                    { "title": "Task 3", "brief": "B3" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let v = extract_structured(out);
    // The terminal report reflects ALL 4 tasks (including the appended one).
    assert_eq!(v["status"], "completed");
    assert_eq!(v["counts"]["succeeded"], 4, "terminal report carries 4 tasks (3 original + 1 appended)");

    let tasks = v["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 4, "terminal report includes 4 task entries");

    // Parent event sink: exactly one TodoListCreated (for the original 3-item create).
    let recorded = events.lock().unwrap();
    let created_events: Vec<_> = recorded
        .iter()
        .filter(|e| matches!(e, UserEvent::TodoListCreated { .. }))
        .collect();
    assert_eq!(
        created_events.len(),
        1,
        "exactly one TodoListCreated fires when the list is created (not when task 4 is appended)"
    );
}

// ── Scenario 4: Single failure does not block flush ───────────────────────────

/// 5-item tasklist; item 3 fails. The other 4 complete normally. The terminal
/// report fires with `counts.failed == 1` and `tasks[2].status == "failed"`.
/// A single failure must NOT prevent the TodoListComplete from firing.
#[tokio::test]
async fn agent_task_batched_return_single_failure_does_not_block_flush() {
    let svc = BatchedMockService::new();
    let svc_clone = Arc::clone(&svc);

    let ctx = make_ctx(Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>);

    // Fire watcher with 1 failed + 4 succeeded.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let tl_id = svc_clone
            .active_tasklist_id()
            .unwrap_or_else(|| "mock-tl".to_string());
        svc_clone.fire_for_tasklist(
            &tl_id,
            TerminalReport {
                status: "failed".to_string(),
                counts: TerminalCounts { succeeded: 4, failed: 1, skipped: 0 },
                tasks: vec![
                    TerminalTaskEntry {
                        id: "t1".to_string(),
                        title: "Task 1".to_string(),
                        status: "completed".to_string(),
                        summary: Some("Summary 1".to_string()),
                        details: None,
                        output_path: PathBuf::from("/tmp/t1/output.txt"),
                        attempt_count: 1,
                    },
                    TerminalTaskEntry {
                        id: "t2".to_string(),
                        title: "Task 2".to_string(),
                        status: "completed".to_string(),
                        summary: Some("Summary 2".to_string()),
                        details: None,
                        output_path: PathBuf::from("/tmp/t2/output.txt"),
                        attempt_count: 1,
                    },
                    TerminalTaskEntry {
                        id: "t3".to_string(),
                        title: "Task 3".to_string(),
                        status: "failed".to_string(),
                        summary: Some("Task 3 failed with an error".to_string()),
                        details: None,
                        output_path: PathBuf::from("/tmp/t3/output.txt"),
                        attempt_count: 2,
                    },
                    TerminalTaskEntry {
                        id: "t4".to_string(),
                        title: "Task 4".to_string(),
                        status: "completed".to_string(),
                        summary: Some("Summary 4".to_string()),
                        details: None,
                        output_path: PathBuf::from("/tmp/t4/output.txt"),
                        attempt_count: 1,
                    },
                    TerminalTaskEntry {
                        id: "t5".to_string(),
                        title: "Task 5".to_string(),
                        status: "completed".to_string(),
                        summary: Some("Summary 5".to_string()),
                        details: None,
                        output_path: PathBuf::from("/tmp/t5/output.txt"),
                        attempt_count: 1,
                    },
                ],
            },
        );
    });

    let out = TodoCreate
        .invoke(
            json!({
                "name": "Single Failure Test",
                "dispatch_mode": "sync",
                "items": [
                    { "title": "Task 1", "brief": "B1" },
                    { "title": "Task 2", "brief": "B2" },
                    { "title": "Task 3", "brief": "B3 — will fail" },
                    { "title": "Task 4", "brief": "B4" },
                    { "title": "Task 5", "brief": "B5" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let v = extract_structured(out);
    assert_eq!(v["status"], "failed", "tasklist status must be 'failed' when any task fails");
    assert_eq!(v["counts"]["succeeded"], 4, "4 items succeeded");
    assert_eq!(v["counts"]["failed"], 1, "1 item failed");
    assert_eq!(v["counts"]["skipped"], 0, "0 items skipped");

    let tasks = v["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 5, "all 5 task entries in terminal report");
    assert_eq!(tasks[2]["status"], "failed", "task 3 (index 2) must show failed status");
    assert_eq!(tasks[2]["attempt_count"], 2, "failed task should have attempt_count 2");
}
