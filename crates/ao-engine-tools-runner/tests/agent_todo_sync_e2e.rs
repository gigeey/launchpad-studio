//! E2E tests for TodoCreate sync dispatch mode.
//!
//! Verifies that:
//!   - sync mode blocks until the tasklist reaches a terminal state and returns
//!     a structured TerminalReport (not an "active" fire-and-forget response)
//!   - async mode returns immediately with status "active"
//!   - at least one ToolProgress heartbeat event fires while the watcher is pending
//!
//! Uses a `SyncMockService` that exposes a real `terminal_watcher` backed by a
//! `TerminalWatcherRegistry`, mirroring the production `TaskFeeder` wiring
//! without requiring a live `ao-engine` AppState.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ao_engine_tools_core::{
    CancelOutcome, EngineTool, EventSink, RunnerContext, TasklistServiceHandle, TerminalCounts,
    TerminalReport, TerminalTaskEntry, TerminalWatcherGuard, TerminalWatcherRegistry, ToolOutput,
    UserEvent,
};
use ao_engine_tools_engine::todo::cancel::TodoCancel;
use ao_engine_tools_engine::todo::create::TodoCreate;
use ao_protocol::{
    error::AoError,
    tasklist::{Task, TaskAssignment, TaskGroup, TaskGroupMode, TasklistOwner, TaskStatus, TasklistStatus, Tasklist},
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use tokio::sync::oneshot;

// ─── Mock ─────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct MockState {
    active: Option<Tasklist>,
}

/// A `TasklistServiceHandle` mock that supports real terminal watchers via its
/// own `TerminalWatcherRegistry`. Tests fire the watcher by calling
/// `fire_for_tasklist` — this mirrors what `TaskFeeder::fire_terminal_watcher`
/// does in production.
struct SyncMockService {
    state: Mutex<MockState>,
    watcher_registry: TerminalWatcherRegistry,
}

impl SyncMockService {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MockState::default()),
            watcher_registry: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Simulate the tasklist reaching a terminal state by firing the watcher.
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

#[async_trait]
impl TasklistServiceHandle for SyncMockService {
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
        let tl = make_tasklist(agent_id, "mock-sync-tl", &name, tasks);
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
        unimplemented!("not needed in sync E2E test")
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
        unimplemented!("not needed in sync E2E test")
    }

    async fn complete_task_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        _task_id: &str,
    ) -> Result<(), AoError> {
        unimplemented!("not needed in sync E2E test")
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

    async fn cancel_for_agent(&self, _agent_id: &str) -> Result<CancelOutcome, AoError> {
        let mut state = self.state.lock().unwrap();
        let tl = state.active.as_mut().ok_or_else(|| {
            AoError::ValidationError("no active tasklist to cancel".into())
        })?;
        let tasklist_id = tl.id.clone();
        let mut skipped_count = 0usize;
        for group in &mut tl.groups {
            for task in &mut group.tasks {
                if task.status == TaskStatus::Pending {
                    task.status = TaskStatus::Skipped;
                    skipped_count += 1;
                }
            }
        }
        let in_flight_count = tl
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .filter(|t| t.status == TaskStatus::InProgress)
            .count();
        tl.status = TasklistStatus::Cancelled;
        state.active = None;
        Ok(CancelOutcome { tasklist_id, skipped_count, in_flight_count })
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
    RunnerContext::new("sync-e2e-session", "agent1")
        .expect("cwd available")
        .with_tasklist_service(svc)
}

fn extract_structured(out: ToolOutput) -> serde_json::Value {
    match out {
        ToolOutput::Structured(v) => v,
        other => panic!("expected Structured ToolOutput, got {other:?}"),
    }
}

fn three_task_report() -> TerminalReport {
    TerminalReport {
        status: "completed".to_string(),
        counts: TerminalCounts { succeeded: 3, failed: 0, skipped: 0 },
        tasks: vec![
            TerminalTaskEntry {
                id: "t1".to_string(),
                title: "Task 1".to_string(),
                status: "completed".to_string(),
                summary: Some("Summary of task 1".to_string()),
                details: None,
                output_path: PathBuf::from("/tmp/mock-ws-mock-sync-tl/tasks/t1/output.txt"),
                attempt_count: 1,
            },
            TerminalTaskEntry {
                id: "t2".to_string(),
                title: "Task 2".to_string(),
                status: "completed".to_string(),
                summary: Some("Summary of task 2".to_string()),
                details: None,
                output_path: PathBuf::from("/tmp/mock-ws-mock-sync-tl/tasks/t2/output.txt"),
                attempt_count: 1,
            },
            TerminalTaskEntry {
                id: "t3".to_string(),
                title: "Task 3".to_string(),
                status: "completed".to_string(),
                summary: Some("Summary of task 3".to_string()),
                details: None,
                output_path: PathBuf::from("/tmp/mock-ws-mock-sync-tl/tasks/t3/output.txt"),
                attempt_count: 1,
            },
        ],
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// TodoCreate with dispatch_mode "sync" blocks until the watcher fires and
/// returns a structured TerminalReport with 3 task entries.
#[tokio::test]
async fn agent_todo_sync_happy_path() {
    let svc = SyncMockService::new();
    let ctx = make_ctx(Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>);

    // Fire the watcher concurrently after a short delay (gives TodoCreate time
    // to register the watcher before the fire).
    let svc_clone = Arc::clone(&svc);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let tl_id = svc_clone
            .active_tasklist_id()
            .unwrap_or_else(|| "mock-sync-tl".to_string());
        svc_clone.fire_for_tasklist(&tl_id, three_task_report());
    });

    let out = TodoCreate
        .invoke(
            json!({
                "name": "Sync Happy Path",
                "dispatch_mode": "sync",
                "items": [
                    { "title": "Task 1", "brief": "Do thing 1" },
                    { "title": "Task 2", "brief": "Do thing 2" },
                    { "title": "Task 3", "brief": "Do thing 3" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let v = extract_structured(out);

    assert_eq!(v["status"], "completed", "sync response should carry terminal status");
    assert_eq!(v["counts"]["succeeded"], 3);
    assert_eq!(v["counts"]["failed"], 0);
    assert_eq!(v["counts"]["skipped"], 0);

    let tasks = v["tasks"].as_array().expect("tasks must be an array");
    assert_eq!(tasks.len(), 3, "response should carry all 3 task entries");
    assert_eq!(tasks[0]["summary"], "Summary of task 1");
    assert_eq!(tasks[1]["summary"], "Summary of task 2");
    assert_eq!(tasks[2]["summary"], "Summary of task 3");
    assert_eq!(tasks[0]["attempt_count"], 1);

    let progress_log = v["progress_log"].as_str().expect("progress_log must be a string");
    assert!(
        progress_log.ends_with("progress.jsonl"),
        "progress_log should point to progress.jsonl; got: {progress_log}"
    );

    // tasklist_id must be present
    assert!(v["tasklist_id"].as_str().is_some(), "tasklist_id must be present");
}

// ─── Recording event sink ─────────────────────────────────────────────────────

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

// ─── Tests ────────────────────────────────────────────────────────────────────

/// async mode returns immediately with status "active" (fire-and-forget);
/// sync mode blocks and returns a terminal report (not "active").
/// At the tool level this verifies the dispatch-mode shapes; the actual
/// completion-summary suppression in task_feeder.rs is covered by ao-engine unit tests.
#[tokio::test]
async fn agent_todo_sync_suppresses_completion_summary_but_async_still_fires() {
    // ── async: returns immediately ─────────────────────────────────────────
    let svc_async = SyncMockService::new();
    let ctx_async =
        make_ctx(Arc::clone(&svc_async) as Arc<dyn TasklistServiceHandle + Send + Sync>);

    let async_out = TodoCreate
        .invoke(
            json!({
                "name": "Async List",
                "dispatch_mode": "async",
                "items": [{ "title": "T1", "brief": "B1" }]
            }),
            &ctx_async,
        )
        .await
        .unwrap();

    let async_v = extract_structured(async_out);
    assert_eq!(
        async_v["status"], "active",
        "async mode must return immediately with status active"
    );
    assert_eq!(async_v["dispatch_mode"], "async");

    // ── sync: blocks until watcher fires ──────────────────────────────────
    let svc_sync = SyncMockService::new();
    let ctx_sync =
        make_ctx(Arc::clone(&svc_sync) as Arc<dyn TasklistServiceHandle + Send + Sync>);

    let svc_clone = Arc::clone(&svc_sync);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let tl_id = svc_clone
            .active_tasklist_id()
            .unwrap_or_else(|| "mock-sync-tl".to_string());
        let report = TerminalReport {
            status: "completed".to_string(),
            counts: TerminalCounts { succeeded: 1, failed: 0, skipped: 0 },
            tasks: vec![TerminalTaskEntry {
                id: "t1".to_string(),
                title: "T1".to_string(),
                status: "completed".to_string(),
                summary: None,
                details: None,
                output_path: PathBuf::from("/tmp"),
                attempt_count: 1,
            }],
        };
        svc_clone.fire_for_tasklist(&tl_id, report);
    });

    let sync_out = TodoCreate
        .invoke(
            json!({
                "name": "Sync List",
                "dispatch_mode": "sync",
                "items": [{ "title": "T1", "brief": "B1" }]
            }),
            &ctx_sync,
        )
        .await
        .unwrap();

    let sync_v = extract_structured(sync_out);
    assert_eq!(
        sync_v["status"], "completed",
        "sync mode must return terminal status, not 'active'"
    );
    assert!(
        sync_v.get("progress_log").is_some(),
        "sync response must include progress_log"
    );
    assert!(
        sync_v.get("tasks").is_some(),
        "sync response must include tasks array"
    );
}

/// While a sync TodoCreate is waiting for the watcher, at least one
/// ToolProgress heartbeat event must arrive on the parent's event sink.
///
/// Uses paused tokio time so the test advances simulated time deterministically:
///  - heartbeat fires at T=10s
///  - watcher fires at T=12s
/// Both happen within a single `advance(15s)` call, keeping the test instant.
///
/// Named `agent_todo_sync_heartbeat_firing` so it is matched by both
/// `-- agent_todo_sync` (regression suite) and `-- heartbeat` (acceptance criteria).
#[tokio::test(start_paused = true)]
async fn agent_todo_sync_heartbeat_firing() {
    // Recording event sink to capture ToolProgress events.
    let events: Arc<Mutex<Vec<UserEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);
    let sink = Arc::new(RecordingEventSink { events: events_clone });

    let svc = SyncMockService::new();
    let svc_clone = Arc::clone(&svc);

    let ctx = make_ctx(Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>)
        .with_event_sink(sink as Arc<dyn EventSink + Send + Sync>);

    // Spawn the invoke — it will block until the watcher fires.
    let invoke_task = tokio::spawn(async move {
        TodoCreate
            .invoke(
                json!({
                    "name": "Heartbeat Test",
                    "dispatch_mode": "sync",
                    "items": [{ "title": "Slow Task", "brief": "Takes 12s to complete" }]
                }),
                &ctx,
            )
            .await
            .unwrap()
    });

    // Fire the watcher at T=12s of simulated time (after the first heartbeat at T=10s).
    let fire_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(12)).await;
        let tl_id = svc_clone
            .active_tasklist_id()
            .unwrap_or_else(|| "mock-sync-tl".to_string());
        svc_clone.fire_for_tasklist(
            &tl_id,
            TerminalReport {
                status: "completed".to_string(),
                counts: TerminalCounts { succeeded: 1, failed: 0, skipped: 0 },
                tasks: vec![TerminalTaskEntry {
                    id: "t1".to_string(),
                    title: "Slow Task".to_string(),
                    status: "completed".to_string(),
                    summary: None,
                    details: None,
                    output_path: PathBuf::from("/tmp"),
                    attempt_count: 1,
                }],
            },
        );
    });

    // Advance simulated time by 15s: heartbeat fires at T=10, watcher at T=12.
    tokio::time::advance(Duration::from_secs(15)).await;

    fire_task.await.expect("fire task must complete");
    invoke_task.await.expect("invoke task must complete");

    let recorded = events.lock().unwrap();
    let progress_events: Vec<_> = recorded
        .iter()
        .filter(|e| matches!(e, UserEvent::ToolProgress { .. }))
        .collect();
    assert!(
        !progress_events.is_empty(),
        "expected at least one ToolProgress event during sync wait; got events: {:?}",
        recorded.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>()
    );
}

/// Sync mode with a middle item that fails: tasklist status becomes "failed"
/// and the tool response carries partial counts.
#[tokio::test]
async fn agent_todo_sync_failure_path() {
    let svc = SyncMockService::new();
    let ctx = make_ctx(Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>);

    let svc_clone = Arc::clone(&svc);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let tl_id = svc_clone
            .active_tasklist_id()
            .unwrap_or_else(|| "mock-sync-tl".to_string());
        svc_clone.fire_for_tasklist(
            &tl_id,
            TerminalReport {
                status: "failed".to_string(),
                counts: TerminalCounts { succeeded: 1, failed: 1, skipped: 1 },
                tasks: vec![
                    TerminalTaskEntry {
                        id: "t1".to_string(),
                        title: "Task 1".to_string(),
                        status: "completed".to_string(),
                        summary: Some("Summary of task 1".to_string()),
                        details: None,
                        output_path: PathBuf::from(
                            "/tmp/mock-ws-mock-sync-tl/tasks/t1/output.txt",
                        ),
                        attempt_count: 1,
                    },
                    TerminalTaskEntry {
                        id: "t2".to_string(),
                        title: "Task 2".to_string(),
                        status: "failed".to_string(),
                        summary: Some("Task 2 failed with an error".to_string()),
                        details: None,
                        output_path: PathBuf::from(
                            "/tmp/mock-ws-mock-sync-tl/tasks/t2/output.txt",
                        ),
                        attempt_count: 2,
                    },
                    TerminalTaskEntry {
                        id: "t3".to_string(),
                        title: "Task 3".to_string(),
                        status: "skipped".to_string(),
                        summary: None,
                        details: None,
                        output_path: PathBuf::from(
                            "/tmp/mock-ws-mock-sync-tl/tasks/t3/output.txt",
                        ),
                        attempt_count: 0,
                    },
                ],
            },
        );
    });

    let out = TodoCreate
        .invoke(
            json!({
                "name": "Failure Path Test",
                "dispatch_mode": "sync",
                "items": [
                    { "title": "Task 1", "brief": "Do thing 1" },
                    { "title": "Task 2", "brief": "Do thing 2" },
                    { "title": "Task 3", "brief": "Do thing 3" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let v = extract_structured(out);
    assert_eq!(v["status"], "failed", "middle-item failure must make tasklist status Failed");
    assert_eq!(v["counts"]["succeeded"], 1);
    assert_eq!(v["counts"]["failed"], 1);
    assert_eq!(v["counts"]["skipped"], 1);
    let tasks = v["tasks"].as_array().expect("tasks must be an array");
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[1]["status"], "failed");
    assert_eq!(tasks[1]["attempt_count"], 2, "failed task should report attempt_count");
    assert_eq!(tasks[2]["status"], "skipped");
    assert!(v["progress_log"].as_str().is_some(), "sync response must include progress_log");
}

/// Async mode returns immediately with status "active" and does not include
/// the terminal-report fields (progress_log, tasks, counts).
#[tokio::test]
async fn agent_todo_sync_async_happy_path() {
    let svc = SyncMockService::new();
    let ctx = make_ctx(Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>);

    let out = TodoCreate
        .invoke(
            json!({
                "name": "Async Happy Path",
                "dispatch_mode": "async",
                "items": [
                    { "title": "Task 1", "brief": "Do thing 1" },
                    { "title": "Task 2", "brief": "Do thing 2" },
                    { "title": "Task 3", "brief": "Do thing 3" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let v = extract_structured(out);
    assert_eq!(v["status"], "active", "async mode must return immediately with status active");
    assert_eq!(v["dispatch_mode"], "async");
    assert_eq!(v["item_count"], 3);
    // Async does not block — no terminal-report fields.
    assert!(v.get("progress_log").is_none(), "async response must not include progress_log");
    assert!(v.get("tasks").is_none(), "async response must not include tasks array");
    // Watcher is NOT registered for async — fire_for_tasklist would be a no-op.
    assert!(svc.watcher_registry.lock().unwrap().is_empty(), "no watcher registered for async");
}

/// Cancel a running async tasklist via TodoCancel: mock marks all pending tasks
/// Skipped and transitions to Cancelled. The tool response carries the counts.
#[tokio::test]
async fn agent_todo_sync_cancel_mid_flight() {
    let svc = SyncMockService::new();
    let ctx = make_ctx(Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>);

    // Start an async 5-item tasklist.
    let create_out = TodoCreate
        .invoke(
            json!({
                "name": "5-Item Cancel Test",
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

    let created = extract_structured(create_out);
    assert_eq!(created["status"], "active", "async create must return active");
    assert_eq!(created["item_count"], 5);

    // Issue TodoCancel — mock marks all 5 pending tasks Skipped and transitions
    // the tasklist to Cancelled.
    let cancel_out = TodoCancel.invoke(json!({}), &ctx).await.unwrap();
    let cancelled = extract_structured(cancel_out);

    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(
        cancelled["skipped_count"], 5,
        "all 5 pending tasks should be skipped on cancel; got: {cancelled}"
    );
    assert_eq!(cancelled["in_flight_count"], 0);
    assert!(
        cancelled["tasklist_id"].as_str().is_some(),
        "cancel response must carry tasklist_id"
    );

    // After cancel the mock clears active — a second cancel must fail gracefully.
    let second = TodoCancel.invoke(json!({}), &ctx).await.unwrap();
    assert!(
        matches!(second, ToolOutput::Error { recoverable: true, .. }),
        "second cancel must return a recoverable error (no active tasklist); got: {second:?}"
    );
}

/// Channel isolation: during a sync TodoCreate wait, only ToolProgress events
/// arrive on the parent's event sink. Task-run events (text deltas, run_started)
/// route to the `tasklist:{id}` SSE channel via the EventBus — they never reach
/// `ctx.event_sink` and therefore never appear in the parent chat channel.
#[tokio::test(start_paused = true)]
async fn agent_todo_sync_channel_isolation() {
    let events: Arc<Mutex<Vec<UserEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RecordingEventSink { events: Arc::clone(&events) });

    let svc = SyncMockService::new();
    let svc_clone = Arc::clone(&svc);

    let ctx = make_ctx(Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>)
        .with_event_sink(sink as Arc<dyn EventSink + Send + Sync>);

    let invoke_task = tokio::spawn(async move {
        TodoCreate
            .invoke(
                json!({
                    "name": "Channel Isolation Test",
                    "dispatch_mode": "sync",
                    "items": [{ "title": "Isolated Task", "brief": "Runs on tasklist channel" }]
                }),
                &ctx,
            )
            .await
            .unwrap()
    });

    // Fire watcher at T=12s (after first heartbeat at T=10s).
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(12)).await;
        let tl_id = svc_clone
            .active_tasklist_id()
            .unwrap_or_else(|| "mock-sync-tl".to_string());
        svc_clone.fire_for_tasklist(
            &tl_id,
            TerminalReport {
                status: "completed".to_string(),
                counts: TerminalCounts { succeeded: 1, failed: 0, skipped: 0 },
                tasks: vec![TerminalTaskEntry {
                    id: "t1".to_string(),
                    title: "Isolated Task".to_string(),
                    status: "completed".to_string(),
                    summary: None,
                    details: None,
                    output_path: PathBuf::from("/tmp"),
                    attempt_count: 1,
                }],
            },
        );
    });

    tokio::time::advance(Duration::from_secs(15)).await;
    invoke_task.await.expect("invoke task must complete");

    let recorded = events.lock().unwrap();
    // Parent sink should receive ToolProgress events and TodoListCreated (the
    // intentional tool-level notification that the list was created). Task-run
    // events (text deltas, run_started, tool_use) route to tasklist:{id} via
    // EventBus and must NOT appear here.
    let non_progress: Vec<_> = recorded
        .iter()
        .filter(|e| !matches!(e, UserEvent::ToolProgress { .. } | UserEvent::TodoListCreated { .. }))
        .collect();
    assert!(
        non_progress.is_empty(),
        "parent event sink must not receive non-ToolProgress/non-TodoListCreated events; got: {:?}",
        non_progress.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>()
    );
    let progress_count = recorded
        .iter()
        .filter(|e| matches!(e, UserEvent::ToolProgress { .. }))
        .count();
    assert!(
        progress_count >= 1,
        "parent event sink must receive at least one ToolProgress event"
    );
}
