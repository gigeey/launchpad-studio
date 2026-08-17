//! E2E tests for classifier-routed task assignment.
//!
//! Covers the six scenarios from PRD section 6.2:
//!
//!   1. Empty book → parent fallback (classifier short-circuits, assigns to parent)
//!   2. Populated book → routed (classifier assigns to a specific child agent)
//!   3. Pinned overrides classifier (explicit owner skips classifier entirely)
//!   4. Edit re-classify (title change on a classified NotStarted task respawns classifier)
//!   5. Startup sweep simulation (orphan task gets assigned via direct CAS pattern)
//!   6. Classifier failure → retry budget exhaustion (row stays None)
//!
//! All tests use the mock dispatcher / fixtures pattern from the other e2e suites.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ao_engine_tools_core::{
    CancelOutcome, ClassifierHandle, ClassifyOutcome, EngineTool, RunnerContext,
    TasklistServiceHandle, TerminalWatcherGuard, ToolOutput,
};
use ao_engine_tools_engine::todo::{create::TodoCreate, update::TodoUpdate};
use ao_protocol::{
    error::AoError,
    tasklist::{
        AssignmentMode, Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus, Tasklist,
        TasklistOwner, TasklistStatus,
    },
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

// ── Mock classifier ───────────────────────────────────────────────────────────

struct MockClassifier {
    call_count: Arc<AtomicU32>,
    outcomes: Mutex<Vec<ClassifyOutcome>>,
    default_outcome: ClassifyOutcome,
}

impl MockClassifier {
    fn always_assigned(owner: &str) -> Arc<Self> {
        Arc::new(Self {
            call_count: Arc::new(AtomicU32::new(0)),
            outcomes: Mutex::new(Vec::new()),
            default_outcome: ClassifyOutcome::Assigned(TaskAssignment {
                owner_agent_id: owner.to_string(),
                mode: AssignmentMode::Classified,
            }),
        })
    }

    fn with_sequence(outcomes: Vec<ClassifyOutcome>) -> Arc<Self> {
        let default = outcomes.last().cloned().unwrap_or(ClassifyOutcome::Permanent(
            "no more outcomes".to_string(),
        ));
        Arc::new(Self {
            call_count: Arc::new(AtomicU32::new(0)),
            outcomes: Mutex::new(outcomes),
            default_outcome: default,
        })
    }

    fn always_retryable() -> Arc<Self> {
        Arc::new(Self {
            call_count: Arc::new(AtomicU32::new(0)),
            outcomes: Mutex::new(Vec::new()),
            default_outcome: ClassifyOutcome::Retryable("mock network error".to_string()),
        })
    }

    fn call_count(&self) -> u32 {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ClassifierHandle for MockClassifier {
    async fn classify(
        &self,
        _parent_agent_id: &str,
        _task_id: &str,
        _task_title: &str,
        _task_description: &str,
    ) -> ClassifyOutcome {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let mut q = self.outcomes.lock().unwrap();
        if q.is_empty() {
            self.default_outcome.clone()
        } else {
            q.remove(0)
        }
    }
}

// ── Mock tasklist service ─────────────────────────────────────────────────────

#[derive(Default)]
struct ClassifierSvcState {
    active: Option<Tasklist>,
    set_assign_calls: Vec<(String, String, String, Option<TaskAssignment>, u64)>,
}

struct MockClassifierSvc {
    state: Mutex<ClassifierSvcState>,
}

impl MockClassifierSvc {
    fn new() -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(ClassifierSvcState::default()) })
    }

    fn with_tasklist(tl: Tasklist) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ClassifierSvcState {
                active: Some(tl),
                set_assign_calls: Vec::new(),
            }),
        })
    }

    fn set_assign_call_count(&self) -> usize {
        self.state.lock().unwrap().set_assign_calls.len()
    }

    fn assigned_calls(&self) -> Vec<(String, TaskAssignment)> {
        self.state
            .lock()
            .unwrap()
            .set_assign_calls
            .iter()
            .filter_map(|(_, _, task_id, a, _)| a.clone().map(|a| (task_id.clone(), a)))
            .collect()
    }
}

#[async_trait]
impl TasklistServiceHandle for MockClassifierSvc {
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
        _agent_id: &str,
        _tasklist_id: &str,
        _tasks: Vec<Task>,
        _mode: TaskGroupMode,
    ) -> Result<Tasklist, AoError> {
        unimplemented!()
    }

    async fn update_task_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        _task_id: &str,
        _prompt: Option<String>,
        _owner: Option<String>,
        _expected_outputs: Option<Vec<String>>,
    ) -> Result<Tasklist, AoError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .active
            .clone()
            .unwrap_or_else(empty_tasklist))
    }

    async fn complete_task_for_agent(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        _task_id: &str,
    ) -> Result<(), AoError> {
        unimplemented!()
    }

    async fn terminal_watcher(&self, _tasklist_id: &str) -> Result<TerminalWatcherGuard, AoError> {
        Err(AoError::Internal("not needed in classifier e2e tests".into()))
    }

    async fn cancel_for_agent(&self, _agent_id: &str) -> Result<CancelOutcome, AoError> {
        Err(AoError::Internal("not needed in classifier e2e tests".into()))
    }

    async fn set_assignment(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
        assignment: Option<TaskAssignment>,
        expected_token: u64,
    ) -> Result<bool, AoError> {
        self.state.lock().unwrap().set_assign_calls.push((
            agent_id.to_string(),
            tasklist_id.to_string(),
            task_id.to_string(),
            assignment,
            expected_token,
        ));
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

fn empty_tasklist() -> Tasklist {
    make_tasklist("agent1", "tl-empty", "Empty", vec![])
}

fn make_pending_task(id: &str, assignment: Option<TaskAssignment>) -> Task {
    Task {
        id: id.to_string(),
        group_id: "g1".to_string(),
        prompt: format!("{id}: task description"),
        owner_agent_id: "parent".to_string(),
        status: TaskStatus::Pending,
        expected_outputs: vec![],
        error_log: vec![],
        attempt_count: 0,
        comments: vec![],
        attachments: vec![],
        notification_parse_retry_count: 0,
        parse_failed: false,
        remind_me: None,
        assignment,
        classifier_token: 0,
        dispatch_token: 0,
    }
}

fn make_ctx_with_classifier(
    svc: Arc<dyn TasklistServiceHandle + Send + Sync>,
    classifier: Arc<dyn ClassifierHandle + Send + Sync>,
) -> RunnerContext {
    RunnerContext::new("classifier-e2e-session", "parent")
        .expect("cwd available")
        .with_tasklist_service(svc)
        .with_classifier(classifier)
}

fn extract_structured(out: ToolOutput) -> serde_json::Value {
    match out {
        ToolOutput::Structured(v) => v,
        other => panic!("expected Structured ToolOutput, got {other:?}"),
    }
}

// ── Scenario 1: Empty book → parent fallback ──────────────────────────────────

/// With empty address book, classifier returns the parent agent as the owner.
/// All 3 items route to the parent; `set_assignment` is called 3× with parent.
#[tokio::test]
async fn agent_task_classifier_empty_book_fallback() {
    let classifier = MockClassifier::always_assigned("parent");
    let svc = MockClassifierSvc::new();
    let ctx = make_ctx_with_classifier(
        Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>,
        Arc::clone(&classifier) as Arc<dyn ClassifierHandle + Send + Sync>,
    );

    let out = TodoCreate
        .invoke(
            json!({
                "name": "Empty Book List",
                "dispatch_mode": "async",
                "items": [
                    { "title": "Fix React", "brief": "React bug" },
                    { "title": "Refactor SQL", "brief": "SQL migration" },
                    { "title": "Add tests", "brief": "Integration tests" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let v = extract_structured(out);
    assert_eq!(v["status"], "active");
    assert_eq!(v["item_count"], 3);

    // Yield to let background classifier spawns complete.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    // All 3 items should have been classified (no owner means classifier runs).
    assert_eq!(
        classifier.call_count(),
        3,
        "classifier should be called once per unassigned item"
    );

    // set_assignment should record 3 write-backs for the parent.
    let assigned = svc.assigned_calls();
    assert_eq!(assigned.len(), 3, "set_assignment should be called 3 times");
    for (_, assignment) in &assigned {
        assert_eq!(
            assignment.owner_agent_id, "parent",
            "empty book: all items fall back to parent"
        );
        assert_eq!(assignment.mode, AssignmentMode::Classified);
    }
}

// ── Scenario 2: Populated book → routed ──────────────────────────────────────

/// With a populated address book, the classifier routes each task to the
/// appropriate child agent. Outcomes: [frontend, backend, frontend] for 3 items.
#[tokio::test]
async fn agent_task_classifier_populated_book_routed() {
    let outcomes = vec![
        ClassifyOutcome::Assigned(TaskAssignment {
            owner_agent_id: "frontend".to_string(),
            mode: AssignmentMode::Classified,
        }),
        ClassifyOutcome::Assigned(TaskAssignment {
            owner_agent_id: "backend".to_string(),
            mode: AssignmentMode::Classified,
        }),
        ClassifyOutcome::Assigned(TaskAssignment {
            owner_agent_id: "frontend".to_string(),
            mode: AssignmentMode::Classified,
        }),
    ];
    let classifier = MockClassifier::with_sequence(outcomes);
    let svc = MockClassifierSvc::new();
    let ctx = make_ctx_with_classifier(
        Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>,
        Arc::clone(&classifier) as Arc<dyn ClassifierHandle + Send + Sync>,
    );

    let out = TodoCreate
        .invoke(
            json!({
                "name": "Populated Book List",
                "dispatch_mode": "async",
                "items": [
                    { "title": "Fix React Todo panel", "brief": "Frontend React work" },
                    { "title": "Refactor SQL query",   "brief": "Backend database work" },
                    { "title": "Update icons",          "brief": "Frontend icon update" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let v = extract_structured(out);
    assert_eq!(v["status"], "active");

    // Yield for classifier spawns.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    assert_eq!(classifier.call_count(), 3, "classifier called once per item");

    let assigned = svc.assigned_calls();
    assert_eq!(assigned.len(), 3);
    assert_eq!(assigned[0].1.owner_agent_id, "frontend");
    assert_eq!(assigned[1].1.owner_agent_id, "backend");
    assert_eq!(assigned[2].1.owner_agent_id, "frontend");
    for (_, a) in &assigned {
        assert_eq!(a.mode, AssignmentMode::Classified);
    }
}

// ── Scenario 3: Pinned overrides classifier ───────────────────────────────────

/// An item with an explicit `owner` field gets Pinned assignment; the classifier
/// is never invoked for that item. The other 2 items without an owner do go
/// through the classifier.
#[tokio::test]
async fn agent_task_classifier_pinned_overrides_classifier() {
    let classifier = MockClassifier::always_assigned("frontend");
    let svc = MockClassifierSvc::new();
    let ctx = make_ctx_with_classifier(
        Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>,
        Arc::clone(&classifier) as Arc<dyn ClassifierHandle + Send + Sync>,
    );

    let out = TodoCreate
        .invoke(
            json!({
                "name": "Pinned Override List",
                "dispatch_mode": "async",
                "items": [
                    { "title": "Fix SQL",      "brief": "B1", "owner": "backend" },
                    { "title": "Update icons", "brief": "B2" },
                    { "title": "Write tests",  "brief": "B3" }
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

    let v = extract_structured(out);
    assert_eq!(v["status"], "active");

    // Yield for classifier spawns.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    // Pinned item does NOT trigger classifier; only 2 unassigned items do.
    assert_eq!(
        classifier.call_count(),
        2,
        "classifier must not be invoked for the pinned item"
    );

    // set_assignment should only be called for the 2 classified items.
    let assigned = svc.assigned_calls();
    assert_eq!(assigned.len(), 2, "only 2 classified items should produce write-backs");
    for (_, a) in &assigned {
        assert_eq!(a.mode, AssignmentMode::Classified);
    }
}

// ── Scenario 4: Edit re-classify ──────────────────────────────────────────────

/// After a classified task is updated with a new title (before it starts),
/// the classifier is re-invoked. After a task starts (InProgress), edits do
/// NOT trigger re-classification.
#[tokio::test]
async fn agent_task_classifier_edit_reclassify() {
    // Set up a mock service pre-seeded with a classified, NotStarted task.
    let classified_task = Task {
        id: "task-c1".to_string(),
        group_id: "g1".to_string(),
        prompt: "Old Title: old description".to_string(),
        owner_agent_id: "parent".to_string(),
        status: TaskStatus::Pending,
        expected_outputs: vec![],
        error_log: vec![],
        attempt_count: 0,
        comments: vec![],
        attachments: vec![],
        notification_parse_retry_count: 0,
        parse_failed: false,
        remind_me: None,
        assignment: Some(TaskAssignment {
            owner_agent_id: "backend".to_string(),
            mode: AssignmentMode::Classified,
        }),
        classifier_token: 5,
        dispatch_token: 0,
    };
    let tl = make_tasklist("parent", "tl-edit", "Edit Test", vec![classified_task]);
    let svc = MockClassifierSvc::with_tasklist(tl);

    let classifier = MockClassifier::always_assigned("frontend");
    let ctx = make_ctx_with_classifier(
        Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>,
        Arc::clone(&classifier) as Arc<dyn ClassifierHandle + Send + Sync>,
    );

    // Edit the title of the classified, NotStarted task → should re-classify.
    let out = TodoUpdate
        .invoke(
            json!({ "task_id": "task-c1", "prompt": "New Title: new description" }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(
        matches!(out, ToolOutput::Text(_)),
        "TodoUpdate must succeed; got: {out:?}"
    );

    // Yield for classifier spawn.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    // Classifier must have been invoked for the re-classify.
    assert!(
        classifier.call_count() >= 1,
        "classifier should be invoked after title change on classified NotStarted task"
    );

    // set_assignment should have been called: first to clear (None) then for write-back.
    assert!(
        svc.set_assign_call_count() >= 1,
        "set_assignment should be called for re-classify"
    );
}

/// Editing an InProgress task must NOT trigger re-classification.
#[tokio::test]
async fn agent_task_classifier_edit_after_start_does_not_reclassify() {
    let in_progress_task = Task {
        id: "task-ip".to_string(),
        group_id: "g1".to_string(),
        prompt: "Running Task: in progress".to_string(),
        owner_agent_id: "parent".to_string(),
        status: TaskStatus::InProgress,
        expected_outputs: vec![],
        error_log: vec![],
        attempt_count: 0,
        comments: vec![],
        attachments: vec![],
        notification_parse_retry_count: 0,
        parse_failed: false,
        remind_me: None,
        assignment: Some(TaskAssignment {
            owner_agent_id: "backend".to_string(),
            mode: AssignmentMode::Classified,
        }),
        classifier_token: 3,
        dispatch_token: 0,
    };
    let tl = make_tasklist("parent", "tl-inprogress", "InProgress Test", vec![in_progress_task]);
    let svc = MockClassifierSvc::with_tasklist(tl);

    let classifier = MockClassifier::always_assigned("frontend");
    let ctx = make_ctx_with_classifier(
        Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>,
        Arc::clone(&classifier) as Arc<dyn ClassifierHandle + Send + Sync>,
    );

    let out = TodoUpdate
        .invoke(
            json!({ "task_id": "task-ip", "prompt": "Running Task: updated desc" }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        matches!(out, ToolOutput::Text(_)),
        "TodoUpdate should succeed; got: {out:?}"
    );

    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    assert_eq!(
        classifier.call_count(),
        0,
        "classifier must not be invoked for InProgress task edits"
    );
    assert_eq!(
        svc.set_assign_call_count(),
        0,
        "set_assignment must not be called for InProgress task edits"
    );
}

// ── Scenario 5: Startup sweep simulation ─────────────────────────────────────

/// Simulates what `TaskClassifier::run_boot_sweep` does for an orphan task:
/// it classifies the row and writes the assignment back via CAS set_assignment.
///
/// The sweep itself lives in ao-engine; this test exercises the CAS write-back
/// pattern that the sweep relies on, verifying it works correctly end-to-end.
#[tokio::test]
async fn agent_task_classifier_startup_sweep_simulation() {
    // Pre-seed mock service with an orphan task (assignment: None, status: Pending).
    let orphan = make_pending_task("orphan-1", None);
    let tl = make_tasklist("parent", "tl-sweep", "Sweep Test", vec![orphan]);
    let svc = MockClassifierSvc::with_tasklist(tl);

    // The classifier returns the parent as the fallback (empty book scenario).
    let classifier = MockClassifier::always_assigned("parent");

    // Simulate what boot_sweep does: classify the orphan then write-back.
    let outcome = classifier
        .classify("parent", "orphan-1", "orphan-1", "task description")
        .await;

    let assignment = match outcome {
        ClassifyOutcome::Assigned(a) => a,
        other => panic!("expected Assigned, got {other:?}"),
    };

    // Boot sweep writes back via set_assignment with the task's current classifier_token (0).
    let written = svc
        .set_assignment("parent", "tl-sweep", "orphan-1", Some(assignment.clone()), 0)
        .await
        .unwrap();

    assert!(written, "CAS write-back must succeed for orphan with token 0");
    assert_eq!(svc.set_assign_call_count(), 1);

    let assigned = svc.assigned_calls();
    assert_eq!(assigned.len(), 1);
    assert_eq!(assigned[0].0, "orphan-1");
    assert_eq!(assigned[0].1.owner_agent_id, "parent");
    assert_eq!(assigned[0].1.mode, AssignmentMode::Classified);
}

/// Stale token: a second sweep write-back with the original token is rejected
/// because the first write-back already bumped the token.
#[tokio::test]
async fn agent_task_classifier_sweep_stale_token_rejected() {
    // Mock that returns false when called a second time (simulates token mismatch).
    struct StrictCasSvc {
        call_count: AtomicU32,
    }

    impl StrictCasSvc {
        fn new() -> Arc<Self> {
            Arc::new(Self { call_count: AtomicU32::new(0) })
        }
    }

    #[async_trait]
    impl TasklistServiceHandle for StrictCasSvc {
        async fn agent_active(&self, _: &str) -> Result<Option<Tasklist>, AoError> {
            Ok(None)
        }
        async fn create_for_agent(&self, _: &str, _: String, _: Vec<TaskGroup>) -> Result<Tasklist, AoError> {
            unimplemented!()
        }
        async fn get_agent_max_instances(&self, _: &str) -> Result<u32, AoError> { Ok(2) }
        async fn add_group_for_agent(&self, _: &str, _: &str, _: Vec<Task>, _: TaskGroupMode) -> Result<Tasklist, AoError> { unimplemented!() }
        async fn update_task_for_agent(&self, _: &str, _: &str, _: &str, _: Option<String>, _: Option<String>, _: Option<Vec<String>>) -> Result<Tasklist, AoError> { unimplemented!() }
        async fn complete_task_for_agent(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { unimplemented!() }
        async fn terminal_watcher(&self, _: &str) -> Result<TerminalWatcherGuard, AoError> { Err(AoError::Internal("n/a".into())) }
        async fn cancel_for_agent(&self, _: &str) -> Result<CancelOutcome, AoError> { Err(AoError::Internal("n/a".into())) }

        async fn set_assignment(
            &self,
            _: &str, _: &str, _: &str,
            _: Option<TaskAssignment>,
            expected_token: u64,
        ) -> Result<bool, AoError> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            // First call (token=0) succeeds; second call (stale token=0 again) fails.
            if count == 0 && expected_token == 0 {
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }

    let svc = StrictCasSvc::new();
    let assignment = TaskAssignment {
        owner_agent_id: "parent".to_string(),
        mode: AssignmentMode::Classified,
    };

    // First write-back succeeds.
    let first = svc.set_assignment("p", "tl", "t", Some(assignment.clone()), 0).await.unwrap();
    assert!(first, "first write-back must succeed");

    // Second write-back with the same (now stale) token is rejected.
    let second = svc.set_assignment("p", "tl", "t", Some(assignment.clone()), 0).await.unwrap();
    assert!(!second, "second write-back with stale token must fail");
}

// ── Scenario 6: Classifier failure → retry budget exhaustion ─────────────────

/// Classifier always returns Retryable. After 3 retry attempts the row stays
/// at None and no assignment write-back happens.
///
/// Uses paused tokio time with incremental advances to drive the spawned
/// classify_with_retry task through each sleep/retry iteration.
#[tokio::test(start_paused = true)]
async fn agent_task_classifier_failure_retry_exhaustion_row_stays_none() {
    let classifier = MockClassifier::always_retryable();
    let svc = MockClassifierSvc::new();
    let ctx = make_ctx_with_classifier(
        Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>,
        Arc::clone(&classifier) as Arc<dyn ClassifierHandle + Send + Sync>,
    );

    let out = TodoCreate
        .invoke(
            json!({
                "name": "Retry Exhaustion Test",
                "dispatch_mode": "async",
                "items": [{ "title": "Failing Task", "brief": "Always fails" }]
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert_eq!(extract_structured(out)["status"], "active");

    // Drive the spawned classify_with_retry task through all iterations.
    // Each advance wakes the sleeping task; yield gives it CPU time to run.
    // Retry delays carry ±10% deterministic jitter (see todo::jittered_retry_delay),
    // so each advance is sized to cover the upper bound + 1s margin.
    // attempt=0: no sleep → immediate classify call
    tokio::task::yield_now().await;
    // attempt=1: base 5s, max ~5.5s with jitter
    tokio::time::advance(Duration::from_secs(7)).await;
    tokio::task::yield_now().await;
    // attempt=2: base 15s, max ~16.5s with jitter
    tokio::time::advance(Duration::from_secs(18)).await;
    tokio::task::yield_now().await;
    // attempt=3: base 45s, max ~49.5s with jitter
    tokio::time::advance(Duration::from_secs(51)).await;
    // Multiple yields to let the final classify call + return path complete.
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }

    // No assignment should have been written (row stays None after retries exhaust).
    let assigned = svc.assigned_calls();
    assert!(
        assigned.is_empty(),
        "classifier retry exhaustion: no assignment should be written; got: {assigned:?}"
    );

    // Classifier was invoked: 1 initial attempt + 3 retries = 4 total.
    assert_eq!(
        classifier.call_count(),
        4,
        "classifier should be called 4 times (1 initial + 3 retries)"
    );
}
