//! E2E tests for agent-delete cascade rules.
//!
//! Covers the three scenarios from PRD section 6.2:
//!
//!   1. Agent delete cascades delegate targets — every other agent's `delegates_to`
//!      list is cleaned of the removed agent
//!   2. Agent delete cancels in-flight + re-classifies pending tasks
//!   3. Confirmation payload (dry_run) — impact preview without mutation
//!
//! These tests use the mock dispatcher / fixtures pattern from the other
//! e2e suites. The cascade service itself (AgentCascadeService in ao-engine)
//! is integration-tested separately; these e2e tests verify the observable
//! behavior at the delegate-target and task-assignment layer.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ao_engine_tools_core::{
    CancelOutcome, ClassifierHandle, ClassifyOutcome, TasklistServiceHandle, TerminalWatcherGuard,
};
use ao_protocol::{
    agent::DelegateTarget,
    error::AoError,
    tasklist::{
        AssignmentMode, Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus, Tasklist,
        TasklistOwner, TasklistStatus,
    },
};
use async_trait::async_trait;
use chrono::Utc;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_tasklist(agent_id: &str, id: &str, tasks: Vec<Task>) -> Tasklist {
    Tasklist {
        id: id.to_string(),
        owner: TasklistOwner::Agent { agent_id: agent_id.to_string() },
        team_id: None,
        title: "test".to_string(),
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

fn make_task(id: &str, owner: &str, status: TaskStatus) -> Task {
    Task {
        id: id.to_string(),
        group_id: "g1".to_string(),
        prompt: format!("{id}: task description"),
        owner_agent_id: owner.to_string(),
        status,
        expected_outputs: vec![],
        error_log: vec![],
        attempt_count: 0,
        comments: vec![],
        attachments: vec![],
        notification_parse_retry_count: 0,
        parse_failed: false,
        remind_me: None,
        assignment: Some(TaskAssignment {
            owner_agent_id: owner.to_string(),
            mode: AssignmentMode::Classified,
        }),
        classifier_token: 0,
        dispatch_token: 0,
    }
}

fn make_target(agent_id: &str) -> DelegateTarget {
    DelegateTarget {
        target_agent_id: agent_id.to_string(),
        name: format!("{} Agent", agent_id),
        purpose: format!("Description for {}", agent_id),
        share_context_allowed: false,
    }
}

/// Remove a target from a delegate list in place, mirroring the cascade's
/// `retain` step. Returns true when an entry was actually removed.
fn remove_target(targets: &mut Vec<DelegateTarget>, agent_id: &str) -> bool {
    let before = targets.len();
    targets.retain(|t| t.target_agent_id != agent_id);
    targets.len() != before
}

/// Find a target by its agent id, mirroring an address-book lookup.
fn find_target<'a>(targets: &'a [DelegateTarget], agent_id: &str) -> Option<&'a DelegateTarget> {
    targets.iter().find(|t| t.target_agent_id == agent_id)
}

// ── Mock tasklist service (cascade-aware) ─────────────────────────────────────

#[derive(Default)]
struct CascadeSvcState {
    tasklists: Vec<Tasklist>,
    set_assign_calls: Vec<(String, String, String, Option<TaskAssignment>, u64)>,
    cancelled_tasks: Vec<String>,
}

struct MockCascadeSvc {
    state: Mutex<CascadeSvcState>,
}

impl MockCascadeSvc {
    #[allow(dead_code)]
    fn new() -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(CascadeSvcState::default()) })
    }

    fn with_tasklists(tasklists: Vec<Tasklist>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CascadeSvcState {
                tasklists,
                set_assign_calls: Vec::new(),
                cancelled_tasks: Vec::new(),
            }),
        })
    }

    fn set_assign_calls(&self) -> Vec<(String, String, String, Option<TaskAssignment>, u64)> {
        self.state.lock().unwrap().set_assign_calls.clone()
    }

    fn cancelled_tasks(&self) -> Vec<String> {
        self.state.lock().unwrap().cancelled_tasks.clone()
    }
}

#[async_trait]
impl TasklistServiceHandle for MockCascadeSvc {
    async fn agent_active(&self, agent_id: &str) -> Result<Option<Tasklist>, AoError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .tasklists
            .iter()
            .find(|tl| match &tl.owner {
                TasklistOwner::Agent { agent_id: id } => id == agent_id,
                _ => false,
            })
            .cloned())
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

    async fn complete_task_for_agent(
        &self,
        _: &str,
        _: &str,
        task_id: &str,
    ) -> Result<(), AoError> {
        // Record as cancelled (cascade marks in-flight tasks as terminal).
        self.state.lock().unwrap().cancelled_tasks.push(task_id.to_string());
        Ok(())
    }

    async fn terminal_watcher(&self, _: &str) -> Result<TerminalWatcherGuard, AoError> {
        Err(AoError::Internal("n/a".into()))
    }

    async fn cancel_for_agent(&self, _: &str) -> Result<CancelOutcome, AoError> {
        Err(AoError::Internal("n/a".into()))
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

// ── Mock classifier (for re-classify after cascade) ───────────────────────────

struct MockCascadeClassifier {
    call_count: AtomicU32,
    owner: String,
}

impl MockCascadeClassifier {
    fn new(owner: &str) -> Arc<Self> {
        Arc::new(Self { call_count: AtomicU32::new(0), owner: owner.to_string() })
    }

    fn call_count(&self) -> u32 {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ClassifierHandle for MockCascadeClassifier {
    async fn classify(&self, _: &str, _: &str, _: &str, _: &str) -> ClassifyOutcome {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        ClassifyOutcome::Assigned(TaskAssignment {
            owner_agent_id: self.owner.clone(),
            mode: AssignmentMode::Classified,
        })
    }
}

// ── Cascade impact type (mirrors AgentCascadeService::compute_impact) ─────────

/// Preview of cascade impact — computed without mutations.
struct CascadeImpact {
    delegate_refs: Vec<(String, DelegateTarget)>,
    in_flight_tasks: Vec<String>,
    not_started_tasks: Vec<String>,
}

/// Compute cascade impact from mock data (dry-run helper for e2e tests).
fn compute_cascade_impact(
    delegate_lists: &[(String, Vec<DelegateTarget>)],
    tasklists: &[Tasklist],
    deleted_agent_id: &str,
) -> CascadeImpact {
    let mut delegate_refs = Vec::new();
    let mut in_flight_tasks = Vec::new();
    let mut not_started_tasks = Vec::new();

    for (owner_id, targets) in delegate_lists {
        if let Some(target) = find_target(targets, deleted_agent_id) {
            delegate_refs.push((owner_id.clone(), target.clone()));
        }
    }

    for tl in tasklists {
        for group in &tl.groups {
            for task in &group.tasks {
                let is_owned = task
                    .assignment
                    .as_ref()
                    .map(|a| a.owner_agent_id == deleted_agent_id)
                    .unwrap_or(false);
                if !is_owned {
                    continue;
                }
                match task.status {
                    TaskStatus::InProgress => in_flight_tasks.push(task.id.clone()),
                    TaskStatus::Pending => not_started_tasks.push(task.id.clone()),
                    _ => {}
                }
            }
        }
    }

    CascadeImpact { delegate_refs, in_flight_tasks, not_started_tasks }
}

// ── Scenario 1: Agent delete cascades delegate targets ────────────────────────

/// Two parents (A, B) both list Child X as a delegate target. Simulating
/// cascade: remove X from both lists and verify neither references X afterwards.
#[tokio::test]
async fn agent_task_cascade_delegates_only() {
    // Seed two delegate lists, both referencing "child-x".
    let mut list_a = vec![make_target("child-x")];
    let mut list_b = vec![make_target("child-x")];

    // Simulate cascade: remove "child-x" from both lists.
    for list in [&mut list_a, &mut list_b] {
        let removed = remove_target(list, "child-x");
        assert!(removed, "remove must return true for a present entry");
    }

    // Verify both lists no longer reference "child-x".
    assert!(
        find_target(&list_a, "child-x").is_none(),
        "list A must not reference child-x after cascade"
    );
    assert!(
        find_target(&list_b, "child-x").is_none(),
        "list B must not reference child-x after cascade"
    );
    assert!(list_a.is_empty(), "list A should be empty after removing only entry");
    assert!(list_b.is_empty(), "list B should be empty after removing only entry");
}

/// After cascade removes X from a delegate list, the other entries remain intact.
#[tokio::test]
async fn agent_task_cascade_delegate_cleanup_preserves_other_entries() {
    // List has two entries: child-x (to remove) and other-agent (to keep).
    let mut targets = vec![make_target("child-x"), make_target("other-agent")];

    let removed = remove_target(&mut targets, "child-x");
    assert!(removed);

    assert!(find_target(&targets, "child-x").is_none(), "child-x must be removed");
    assert!(
        find_target(&targets, "other-agent").is_some(),
        "other-agent must remain in the list"
    );
    assert_eq!(targets.len(), 1);
}

// ── Scenario 2: Agent delete cancels in-flight + re-classifies pending ────────

/// Child X has 2 InProgress tasks and 3 NotStarted tasks across two parent
/// tasklists. Simulating cascade:
///   - InProgress tasks → transition to terminal (cancelled)
///   - NotStarted tasks → assignment cleared (None) + classifier re-spawned
#[tokio::test]
async fn agent_task_cascade_cancels_in_flight_and_reclassifies_pending() {
    // Set up tasklists with X-owned tasks.
    let in_flight: Vec<Task> = (0..2)
        .map(|i| make_task(&format!("if-{i}"), "child-x", TaskStatus::InProgress))
        .collect();
    let pending: Vec<Task> = (0..3)
        .map(|i| make_task(&format!("pend-{i}"), "child-x", TaskStatus::Pending))
        .collect();

    let tl_a = make_tasklist("parent-a", "tl-a", in_flight.clone());
    let tl_b = make_tasklist("parent-b", "tl-b", pending.clone());

    // Compute dry-run impact to verify preview is correct before mutation.
    let all_delegate_lists: Vec<(String, Vec<DelegateTarget>)> =
        vec![("parent-a".to_string(), vec![make_target("child-x")])];
    let all_tasklists = vec![tl_a.clone(), tl_b.clone()];

    let impact = compute_cascade_impact(&all_delegate_lists, &all_tasklists, "child-x");
    assert_eq!(impact.in_flight_tasks.len(), 2, "2 in-flight tasks");
    assert_eq!(impact.not_started_tasks.len(), 3, "3 pending tasks");
    assert_eq!(impact.delegate_refs.len(), 1, "1 delegate reference");

    // Execute cascade step 1: cancel in-flight tasks.
    let svc = MockCascadeSvc::with_tasklists(vec![tl_a, tl_b]);
    for task_id in &impact.in_flight_tasks {
        svc.complete_task_for_agent("parent-a", "tl-a", task_id).await.unwrap();
    }
    assert_eq!(svc.cancelled_tasks().len(), 2, "2 tasks must be cancelled");

    // Execute cascade step 3: clear assignment on pending orphans.
    for task_id in &impact.not_started_tasks {
        let cleared = svc
            .set_assignment("parent-b", "tl-b", task_id, None, 0)
            .await
            .unwrap();
        assert!(cleared, "clearing assignment must succeed");
    }

    // All 3 pending orphans had their assignment cleared.
    let calls = svc.set_assign_calls();
    let clear_calls: Vec<_> = calls.iter().filter(|(_, _, _, a, _)| a.is_none()).collect();
    assert_eq!(clear_calls.len(), 3, "3 tasks must have their assignment cleared");

    // Simulate re-classification for cleared orphans (what cascade spawns).
    let classifier = MockCascadeClassifier::new("parent-b");
    for task in &impact.not_started_tasks {
        let outcome = classifier.classify("parent-b", task, "task", "desc").await;
        let assignment = match outcome {
            ClassifyOutcome::Assigned(a) => a,
            other => panic!("expected Assigned, got {other:?}"),
        };
        let written = svc
            .set_assignment("parent-b", "tl-b", task, Some(assignment), 1)
            .await
            .unwrap();
        assert!(written, "re-classify write-back must succeed");
    }

    // Classifier was invoked 3× for the 3 orphaned pending tasks.
    assert_eq!(classifier.call_count(), 3, "classifier must be invoked once per pending orphan");

    // 3 clear calls + 3 write-back calls = 6 set_assignment calls total.
    let total_calls = svc.set_assign_calls();
    assert_eq!(total_calls.len(), 6, "total set_assignment calls: 3 clears + 3 write-backs");
}

// ── Scenario 3: Confirmation payload (dry_run) ────────────────────────────────

/// `compute_impact` returns the correct CascadeImpact counts for a complex
/// scenario (3 delegate refs, 2 in-flight, 5 not-started) WITHOUT mutating any
/// delegate list or task assignment. Repeat with real mutation and assert match.
#[tokio::test]
async fn agent_task_cascade_dry_run_preview_matches_real() {
    // Seed delegate lists for three parents referencing "child-x" plus "other".
    let mut delegate_lists: Vec<(String, Vec<DelegateTarget>)> = ["parent-a", "parent-b", "parent-c"]
        .iter()
        .map(|name| {
            (
                name.to_string(),
                vec![make_target("child-x"), make_target("other")],
            )
        })
        .collect();

    // Tasklist A: 2 in-flight
    let in_flight_a: Vec<Task> = (0..2)
        .map(|i| make_task(&format!("if-{i}"), "child-x", TaskStatus::InProgress))
        .collect();
    // Tasklist B: 2 not-started
    let pending_b: Vec<Task> = (0..2)
        .map(|i| make_task(&format!("pb-{i}"), "child-x", TaskStatus::Pending))
        .collect();
    // Tasklist C: 3 not-started
    let pending_c: Vec<Task> = (0..3)
        .map(|i| make_task(&format!("pc-{i}"), "child-x", TaskStatus::Pending))
        .collect();

    let all_tasklists = vec![
        make_tasklist("parent-a", "tl-a", in_flight_a),
        make_tasklist("parent-b", "tl-b", pending_b),
        make_tasklist("parent-c", "tl-c", pending_c),
    ];

    // --- Dry run: compute impact without mutation ---
    // Snapshot delegate lists before the dry run.
    let snapshot_before = delegate_lists.clone();

    let impact = compute_cascade_impact(&delegate_lists, &all_tasklists, "child-x");

    // Verify nothing changed (dry-run does not mutate).
    assert_eq!(
        snapshot_before, delegate_lists,
        "delegate lists mutated during dry run"
    );

    assert_eq!(impact.delegate_refs.len(), 3, "3 delegate references expected");
    assert_eq!(impact.in_flight_tasks.len(), 2, "2 in-flight tasks expected");
    assert_eq!(impact.not_started_tasks.len(), 5, "5 not-started tasks expected");

    // --- Real cascade: execute and verify results match the preview ---
    let svc = MockCascadeSvc::with_tasklists(all_tasklists.clone());

    // Step 1: Cancel in-flight tasks.
    for task_id in &impact.in_flight_tasks {
        svc.complete_task_for_agent("parent-a", "tl-a", task_id).await.unwrap();
    }
    assert_eq!(
        svc.cancelled_tasks().len(),
        impact.in_flight_tasks.len(),
        "cancelled count must match preview"
    );

    // Step 2: Clean delegate lists.
    for (_, targets) in delegate_lists.iter_mut() {
        remove_target(targets, "child-x");
    }

    // Verify lists no longer reference "child-x" but keep "other".
    for (name, targets) in &delegate_lists {
        assert!(
            find_target(targets, "child-x").is_none(),
            "list {name} must not reference child-x after cascade"
        );
        assert!(
            find_target(targets, "other").is_some(),
            "list {name} must still contain 'other' after cascade"
        );
    }

    // Step 3: Clear and re-classify orphaned pending tasks.
    let tasklist_by_task: Vec<(&str, &str)> = impact
        .not_started_tasks
        .iter()
        .map(|task_id| {
            let parent = if task_id.starts_with("pb-") { "parent-b" } else { "parent-c" };
            let tl = if task_id.starts_with("pb-") { "tl-b" } else { "tl-c" };
            (parent, tl)
        })
        .collect();

    for (i, task_id) in impact.not_started_tasks.iter().enumerate() {
        let (parent, tl) = tasklist_by_task[i];
        svc.set_assignment(parent, tl, task_id, None, 0).await.unwrap();
    }

    let clear_calls: Vec<_> = svc
        .set_assign_calls()
        .into_iter()
        .filter(|(_, _, _, a, _)| a.is_none())
        .collect();
    assert_eq!(
        clear_calls.len(),
        impact.not_started_tasks.len(),
        "clear calls must match preview orphan count"
    );
}
