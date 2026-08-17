use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ao_engine_tools_core::{
    ClassifierHandle, ClassifyOutcome, EngineTool, RunnerContext, TasklistServiceHandle, ToolOutput,
};
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

use super::TodoUpdate;

// ---------------------------------------------------------------------------
// Mock TasklistService
// ---------------------------------------------------------------------------

struct MockSvc {
    active: Option<Tasklist>,
    task_not_found: bool,
    set_assignment_calls: Arc<Mutex<Vec<(String, Option<TaskAssignment>, u64)>>>,
    set_assignment_returns: bool,
}

impl MockSvc {
    fn with_active() -> Arc<Self> {
        Arc::new(Self {
            active: Some(fake_tasklist_empty()),
            task_not_found: false,
            set_assignment_calls: Arc::new(Mutex::new(Vec::new())),
            set_assignment_returns: true,
        })
    }

    fn with_task(task: Task) -> Arc<Self> {
        Arc::new(Self {
            active: Some(tasklist_with_task(task)),
            task_not_found: false,
            set_assignment_calls: Arc::new(Mutex::new(Vec::new())),
            set_assignment_returns: true,
        })
    }

    fn with_task_stale_cas(task: Task) -> Arc<Self> {
        Arc::new(Self {
            active: Some(tasklist_with_task(task)),
            task_not_found: false,
            set_assignment_calls: Arc::new(Mutex::new(Vec::new())),
            set_assignment_returns: false,
        })
    }

    fn with_task_team(task: Task) -> Arc<Self> {
        Arc::new(Self {
            active: Some(team_tasklist_with_task(task)),
            task_not_found: false,
            set_assignment_calls: Arc::new(Mutex::new(Vec::new())),
            set_assignment_returns: true,
        })
    }

    fn no_active() -> Arc<Self> {
        Arc::new(Self {
            active: None,
            task_not_found: false,
            set_assignment_calls: Arc::new(Mutex::new(Vec::new())),
            set_assignment_returns: true,
        })
    }

    fn task_not_found() -> Arc<Self> {
        Arc::new(Self {
            active: Some(fake_tasklist_empty()),
            task_not_found: true,
            set_assignment_calls: Arc::new(Mutex::new(Vec::new())),
            set_assignment_returns: true,
        })
    }
}

fn fake_tasklist_empty() -> Tasklist {
    Tasklist {
        id: "tl-1".to_string(),
        owner: TasklistOwner::Agent { agent_id: "agent1".to_string() },
        team_id: None,
        title: "My List".to_string(),
        description: String::new(),
        status: TasklistStatus::Active,
        groups: vec![],
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

fn tasklist_with_task(task: Task) -> Tasklist {
    let mut tl = fake_tasklist_empty();
    tl.groups = vec![TaskGroup {
        id: "g1".to_string(),
        mode: TaskGroupMode::Seq,
        tasks: vec![task],
    }];
    tl
}

/// Same as [`tasklist_with_task`] but Team-owned, for exercising the
/// guard that owner-pin only applies to Agent-owned tasklists.
fn team_tasklist_with_task(task: Task) -> Tasklist {
    let mut tl = tasklist_with_task(task);
    tl.owner = TasklistOwner::Team { team_id: "team1".to_string() };
    tl
}

fn make_task(
    id: &str,
    status: TaskStatus,
    assignment: Option<TaskAssignment>,
    classifier_token: u64,
) -> Task {
    Task {
        id: id.to_string(),
        group_id: "g1".to_string(),
        prompt: "Old Title: old description".to_string(),
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
        assignment,
        classifier_token,
        dispatch_token: 0,
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
        _agent_id: &str,
        _tasklist_id: &str,
        task_id: &str,
        _prompt: Option<String>,
        _owner: Option<String>,
        _expected_outputs: Option<Vec<String>>,
    ) -> Result<Tasklist, AoError> {
        if self.task_not_found {
            return Err(AoError::TaskNotFound(task_id.to_string()));
        }
        Ok(self.active.clone().unwrap_or_else(fake_tasklist_empty))
    }

    async fn complete_task_for_agent(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> {
        unimplemented!()
    }

    async fn terminal_watcher(
        &self,
        _tasklist_id: &str,
    ) -> Result<ao_engine_tools_core::TerminalWatcherGuard, ao_protocol::error::AoError> {
        Err(ao_protocol::error::AoError::Internal(
            "terminal_watcher not implemented in mock".into(),
        ))
    }

    async fn cancel_for_agent(
        &self,
        _: &str,
    ) -> Result<ao_engine_tools_core::CancelOutcome, ao_protocol::error::AoError> {
        Err(ao_protocol::error::AoError::Internal(
            "cancel_for_agent not implemented in mock".into(),
        ))
    }

    async fn set_assignment(
        &self,
        _agent_id: &str,
        _tasklist_id: &str,
        task_id: &str,
        assignment: Option<TaskAssignment>,
        expected_token: u64,
    ) -> Result<bool, AoError> {
        self.set_assignment_calls
            .lock()
            .unwrap()
            .push((task_id.to_string(), assignment, expected_token));
        Ok(self.set_assignment_returns)
    }
}

// ---------------------------------------------------------------------------
// Mock ClassifierHandle
// ---------------------------------------------------------------------------

struct MockClassifier {
    call_count: Arc<AtomicU32>,
    outcome: ClassifyOutcome,
}

impl MockClassifier {
    fn always_assigned(owner: &str) -> Arc<Self> {
        Arc::new(Self {
            call_count: Arc::new(AtomicU32::new(0)),
            outcome: ClassifyOutcome::Assigned(TaskAssignment {
                owner_agent_id: owner.to_string(),
                mode: AssignmentMode::Classified,
            }),
        })
    }
}

#[async_trait]
impl ClassifierHandle for MockClassifier {
    async fn classify(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> ClassifyOutcome {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        self.outcome.clone()
    }
}

// ---------------------------------------------------------------------------
// Context helpers
// ---------------------------------------------------------------------------

fn ctx(svc: Arc<dyn TasklistServiceHandle + Send + Sync>) -> RunnerContext {
    RunnerContext::new("s", "agent1").unwrap().with_tasklist_service(svc)
}

fn ctx_with_classifier(
    svc: Arc<dyn TasklistServiceHandle + Send + Sync>,
    classifier: Arc<dyn ClassifierHandle + Send + Sync>,
) -> RunnerContext {
    RunnerContext::new("s", "agent1")
        .unwrap()
        .with_tasklist_service(svc)
        .with_classifier(classifier)
}

// ---------------------------------------------------------------------------
// Existing tests (unchanged)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path() {
    let c = ctx(MockSvc::with_active());
    let out = TodoUpdate
        .invoke(json!({"task_id": "t1", "prompt": "Updated prompt"}), &c)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("updated successfully"), "got: {s}"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn no_active_tasklist_error() {
    let c = ctx(MockSvc::no_active());
    let out = TodoUpdate
        .invoke(json!({"task_id": "t1", "prompt": "x"}), &c)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("No active tasklist"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn invalid_task_id_error() {
    let c = ctx(MockSvc::task_not_found());
    let out = TodoUpdate
        .invoke(json!({"task_id": "missing", "prompt": "x"}), &c)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("not found"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Re-classify gate matrix test
//
// Covers all 6 TaskStatus variants × 2 AssignmentMode variants × 3 edit shapes.
// Classifier is invoked ONLY for (Pending, Classified, prompt_changed).
// ---------------------------------------------------------------------------

fn classified_assignment() -> Option<TaskAssignment> {
    Some(TaskAssignment { owner_agent_id: "backend".to_string(), mode: AssignmentMode::Classified })
}

fn pinned_assignment() -> Option<TaskAssignment> {
    Some(TaskAssignment { owner_agent_id: "backend".to_string(), mode: AssignmentMode::Pinned })
}

/// Calls TodoUpdate and returns whether set_assignment was called on the mock.
async fn run_update_and_check_reclassify(
    status: TaskStatus,
    assignment: Option<TaskAssignment>,
    prompt_update: Option<&str>,
) -> (u32, usize) {
    let task = make_task("t1", status, assignment, 0);
    let classifier = MockClassifier::always_assigned("backend");
    let call_count = classifier.call_count.clone();
    let svc = MockSvc::with_task(task);
    let set_assignment_calls = svc.set_assignment_calls.clone();
    let c = ctx_with_classifier(svc, classifier);

    let mut input = json!({"task_id": "t1"});
    if let Some(p) = prompt_update {
        input["prompt"] = json!(p);
    } else {
        // Must provide at least one field; use expected_outputs as a no-prompt edit.
        input["expected_outputs"] = json!(["output.txt"]);
    }

    let out = TodoUpdate.invoke(input, &c).await.unwrap();
    assert!(matches!(out, ToolOutput::Text(_)), "update must succeed: {out:?}");

    // Give background spawn a moment to run.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let classify_calls = call_count.load(Ordering::Relaxed);
    let set_assign_count = set_assignment_calls.lock().unwrap().len();
    (classify_calls, set_assign_count)
}

#[tokio::test]
async fn reclassify_gate_matrix() {
    let statuses = [
        TaskStatus::Pending,
        TaskStatus::InProgress,
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Blocked,
        TaskStatus::Skipped,
    ];
    let modes = [
        ("classified", classified_assignment()),
        ("pinned", pinned_assignment()),
    ];

    for status in statuses {
        for (mode_name, assignment) in &modes {
            // Edit: prompt changed
            let (classify_calls, set_assign_count) =
                run_update_and_check_reclassify(status, assignment.clone(), Some("New prompt")).await;
            let expected_reclassify = status == TaskStatus::Pending && *mode_name == "classified";
            if expected_reclassify {
                assert!(
                    classify_calls >= 1,
                    "status={status:?} mode={mode_name}: expected classifier call when prompt changed, got 0"
                );
                assert!(
                    set_assign_count >= 1,
                    "status={status:?} mode={mode_name}: expected set_assignment call (clear)"
                );
            } else {
                assert_eq!(
                    classify_calls, 0,
                    "status={status:?} mode={mode_name}: expected NO classifier call when prompt changed"
                );
            }

            // Edit: no prompt change (only expected_outputs changed)
            let (classify_calls, _) =
                run_update_and_check_reclassify(status, assignment.clone(), None).await;
            assert_eq!(
                classify_calls, 0,
                "status={status:?} mode={mode_name}: expected NO classifier call when prompt NOT changed"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_before_start_reclassifies() {
    // Classified + Pending task: editing prompt clears assignment and spawns classifier.
    let task = make_task("t1", TaskStatus::Pending, classified_assignment(), 7);
    let classifier = MockClassifier::always_assigned("new-owner");
    let classify_count = classifier.call_count.clone();
    let svc = MockSvc::with_task(task);
    let set_assignment_calls = svc.set_assignment_calls.clone();

    let c = ctx_with_classifier(svc, classifier);
    let out = TodoUpdate
        .invoke(json!({"task_id": "t1", "prompt": "New Title: new description"}), &c)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Text(_)));

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let calls = set_assignment_calls.lock().unwrap();
    // First call: set_assignment(None, 7) to clear and bump token.
    assert_eq!(calls.len(), 2, "expected 2 set_assignment calls: clear + write-back");
    assert!(calls[0].1.is_none(), "first call must clear assignment");
    assert_eq!(calls[0].2, 7, "first call must use original token");
    // Second call: classifier write-back with token 8.
    assert!(calls[1].1.is_some(), "second call must set assignment");
    assert_eq!(calls[1].2, 8, "second call must use bumped token");
    assert_eq!(classify_count.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn edit_after_start_does_not_reclassify() {
    // InProgress task: editing prompt must NOT trigger re-classify.
    let task = make_task("t1", TaskStatus::InProgress, classified_assignment(), 3);
    let classifier = MockClassifier::always_assigned("backend");
    let classify_count = classifier.call_count.clone();
    let svc = MockSvc::with_task(task);

    let c = ctx_with_classifier(svc, classifier);
    let out = TodoUpdate
        .invoke(json!({"task_id": "t1", "prompt": "Updated prompt for in-progress task"}), &c)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Text(_)));

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(classify_count.load(Ordering::Relaxed), 0, "must not classify InProgress task");
}

#[tokio::test]
async fn edit_pinned_never_reclassifies() {
    // Pinned + Pending task: editing prompt must NOT re-classify.
    let task = make_task("t1", TaskStatus::Pending, pinned_assignment(), 2);
    let classifier = MockClassifier::always_assigned("backend");
    let classify_count = classifier.call_count.clone();
    let svc = MockSvc::with_task(task);

    let c = ctx_with_classifier(svc, classifier);
    let out = TodoUpdate
        .invoke(json!({"task_id": "t1", "prompt": "New prompt for pinned task"}), &c)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Text(_)));

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(classify_count.load(Ordering::Relaxed), 0, "must not classify Pinned task");
}

#[tokio::test]
async fn edit_race_loses_to_user() {
    // Simulates the user editing before an in-flight classifier returns.
    // The user's edit calls set_assignment(None, token) which bumps the token.
    // A new classifier spawn uses the bumped token. The old classifier's write-back
    // (using the original token) would be rejected as stale by the CAS (tested in
    // tasklist_service). Here we verify:
    //   (a) set_assignment(None, token=5) is called — invalidating in-flight T=5
    //   (b) new classifier spawn uses expected_token = 6
    let task = make_task("t1", TaskStatus::Pending, classified_assignment(), 5);
    let classifier = MockClassifier::always_assigned("backend");
    let classify_count = classifier.call_count.clone();
    let svc = MockSvc::with_task(task);
    let set_assignment_calls = svc.set_assignment_calls.clone();

    let c = ctx_with_classifier(svc, classifier);
    let _ = TodoUpdate
        .invoke(json!({"task_id": "t1", "prompt": "T2 title: t2 description"}), &c)
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let calls = set_assignment_calls.lock().unwrap();
    // First call: clear + bump (token=5, assignment=None) — invalidates old classifier.
    assert!(!calls.is_empty());
    assert!(calls[0].1.is_none(), "first call must clear assignment (token invalidation)");
    assert_eq!(calls[0].2, 5, "must use original token to invalidate old classifier");
    // New spawn runs with token=6 and writes back (second call).
    if calls.len() > 1 {
        assert_eq!(calls[1].2, 6, "new classifier must use bumped token");
    }
    assert_eq!(classify_count.load(Ordering::Relaxed), 1, "new classifier must have run");
}

#[tokio::test]
async fn stale_cas_skips_spawn() {
    // When set_assignment returns false (stale token), no classifier is spawned.
    let task = make_task("t1", TaskStatus::Pending, classified_assignment(), 0);
    let classifier = MockClassifier::always_assigned("backend");
    let classify_count = classifier.call_count.clone();
    let svc = MockSvc::with_task_stale_cas(task);

    let c = ctx_with_classifier(svc, classifier);
    let out = TodoUpdate
        .invoke(json!({"task_id": "t1", "prompt": "new prompt"}), &c)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Text(_)));

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        classify_count.load(Ordering::Relaxed),
        0,
        "stale CAS must not spawn a new classifier"
    );
}

#[tokio::test]
async fn no_classifier_in_context_skips_silently() {
    // Even if should_reclassify is true, no classifier = no action.
    let task = make_task("t1", TaskStatus::Pending, classified_assignment(), 0);
    let svc = MockSvc::with_task(task);
    let set_assignment_calls = svc.set_assignment_calls.clone();
    // No classifier injected into context.
    let c = ctx(svc);
    let out = TodoUpdate
        .invoke(json!({"task_id": "t1", "prompt": "new prompt"}), &c)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Text(_)));

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let calls = set_assignment_calls.lock().unwrap();
    assert!(calls.is_empty(), "no set_assignment without classifier in context");
}

#[tokio::test]
async fn loop_i_todo_update_tests_unchanged() {
    // Smoke-test the original happy-path and error paths continue to work.
    // Happy path: existing active tasklist, no task in groups.
    let c = ctx(MockSvc::with_active());
    let out = TodoUpdate
        .invoke(json!({"task_id": "any", "prompt": "x"}), &c)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Text(_)));

    // Task not found.
    let c = ctx(MockSvc::task_not_found());
    let out = TodoUpdate
        .invoke(json!({"task_id": "missing", "prompt": "x"}), &c)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Error { .. }));
}

// ---------------------------------------------------------------------------
// owner-only update pins a fresh assignment (write-path fix)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn owner_update_pins_assignment_without_prompt_change() {
    // Pending Classified task in an Agent-owned tasklist. Reassigning `owner`
    // alone (no prompt field at all) must set assignment { owner: X, Pinned }
    // via the existing CAS `set_assignment`, using the task's current
    // classifier_token, and must NOT invoke the classifier (an explicit
    // owner change is itself the resolution — the classifier must never
    // re-stomp a pin).
    let task = make_task("t1", TaskStatus::Pending, classified_assignment(), 4);
    let classifier = MockClassifier::always_assigned("backend");
    let classify_count = classifier.call_count.clone();
    let svc = MockSvc::with_task(task);
    let set_assignment_calls = svc.set_assignment_calls.clone();
    let c = ctx_with_classifier(svc, classifier);

    // No `prompt` field at all — owner-only edit.
    let out = TodoUpdate
        .invoke(json!({"task_id": "t1", "owner": "new-owner"}), &c)
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("updated successfully"), "got: {s}"),
        other => panic!("expected Text, got {other:?}"),
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let calls = set_assignment_calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "expected exactly one set_assignment call (the owner pin)");
    assert_eq!(calls[0].0, "t1");
    assert_eq!(
        calls[0].1,
        Some(TaskAssignment { owner_agent_id: "new-owner".to_string(), mode: AssignmentMode::Pinned }),
        "assignment must be pinned to the new owner"
    );
    assert_eq!(calls[0].2, 4, "must CAS against the task's current classifier_token");

    assert_eq!(
        classify_count.load(Ordering::Relaxed),
        0,
        "an explicit owner change must not also trigger re-classification"
    );
}

#[tokio::test]
async fn owner_update_pins_assignment_even_with_prompt_change() {
    // When owner AND prompt are both provided, the owner pin still wins over
    // the re-classify gate — an explicit owner change is a deliberate pin
    // that the classifier must never re-stomp, regardless of what else
    // changed in the same call.
    let task = make_task("t1", TaskStatus::Pending, classified_assignment(), 0);
    let classifier = MockClassifier::always_assigned("backend");
    let classify_count = classifier.call_count.clone();
    let svc = MockSvc::with_task(task);
    let set_assignment_calls = svc.set_assignment_calls.clone();
    let c = ctx_with_classifier(svc, classifier);

    let out = TodoUpdate
        .invoke(json!({"task_id": "t1", "owner": "new-owner", "prompt": "New Title: new description"}), &c)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Text(_)));

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let calls = set_assignment_calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "only the owner-pin set_assignment call, no reclassify clear/write-back");
    assert_eq!(
        calls[0].1,
        Some(TaskAssignment { owner_agent_id: "new-owner".to_string(), mode: AssignmentMode::Pinned })
    );
    assert_eq!(classify_count.load(Ordering::Relaxed), 0, "owner pin suppresses re-classification");
}

#[tokio::test]
async fn owner_update_on_team_tasklist_does_not_pin_assignment() {
    // Team-owned tasklists dispatch via the base owner_agent_id field only
    // (resolve_executor_agent_id's Team branch never reads `assignment`).
    // TodoUpdate must not force a Pinned assignment onto a Team task — the
    // base-field write from update_task_for_agent already covers it.
    let task = make_task("t1", TaskStatus::Pending, classified_assignment(), 0);
    let svc = MockSvc::with_task_team(task);
    let set_assignment_calls = svc.set_assignment_calls.clone();
    let c = ctx(svc);

    let out = TodoUpdate
        .invoke(json!({"task_id": "t1", "owner": "new-owner"}), &c)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Text(_)));

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        set_assignment_calls.lock().unwrap().is_empty(),
        "owner update on a Team-owned tasklist must not write an assignment"
    );
}

// ---------------------------------------------------------------------------
// Owner resolution: display-name owner values
// ---------------------------------------------------------------------------

fn make_agent_profile(id: &str, name: &str) -> ao_protocol::agent::AgentProfile {
    use ao_protocol::agent::{AgentRunnerMode, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
    use std::collections::HashMap;
    ao_protocol::agent::AgentProfile {
        id: id.to_string(),
        name: name.to_string(),
        description: "test agent".to_string(),
        emoji: None,
        provider: ProviderConfig::Cli(CliProviderConfig {
            command: "echo".to_string(),
            args: vec![],
            normalizer: None,
            output_format: OutputFormat::Text,
            input_mode: InputMode::Arg,
            model_arg: None,
            model_aliases: HashMap::new(),
            system_prompt_arg: None,
            session_arg: None,
            resume_args: vec![],
            session_id_fields: vec![],
            clear_env: false,
            no_output_timeout_ms: 30000,
            file_capabilities: None,
        }),
        model: None,
        skills: vec![],
        system_prompt: None,
        tools: None,
        env: HashMap::new(),
        max_instances: 1,
        timeout_seconds: 300,
        working_dir: None,
        home_dir: None,
        serialize: true,
        workflows: None,
        template: None,
        runner_mode: AgentRunnerMode::default(),
        native_provider: None,
        thinking: None,
        max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
        enabled_plugins: HashMap::new(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
        owning_team_id: None,
        delegates_to: vec![],
        persona: None,
        special_instructions: None,
        legacy_system_prompt: None,
        max_delegation_depth: None,
        channels: vec![],
        max_turns: None,
    }
}

async fn setup_agent_profile_store(
    tmp: &tempfile::TempDir,
    caller: &ao_protocol::agent::AgentProfile,
    target: &ao_protocol::agent::AgentProfile,
) -> Arc<ao_persistence::profiles::AgentProfileStore> {
    let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();
    let store = Arc::new(ao_persistence::profiles::AgentProfileStore::new(data_root));
    store.create(caller).await.unwrap();
    store.create(target).await.unwrap();
    store
}

/// A display-name `owner` on TodoUpdate resolves to the target's canonical
/// agent_id before it lands in the Pinned assignment written via
/// `set_assignment` — mirroring `Delegate.target`'s address-book lookup.
#[tokio::test]
async fn owner_display_name_resolves_to_canonical_agent_id_in_pinned_assignment() {
    use ao_protocol::agent::DelegateTarget;

    let tmp = tempfile::TempDir::new().unwrap();
    let mut caller = make_agent_profile("agent1", "Caller");
    caller.delegates_to = vec![DelegateTarget {
        target_agent_id: "frontend-worker-uuid".to_string(),
        name: "Frontend".to_string(),
        purpose: "handle frontend tasks".to_string(),
        share_context_allowed: false,
    }];
    let target = make_agent_profile("frontend-worker-uuid", "Frontend Worker");
    let store = setup_agent_profile_store(&tmp, &caller, &target).await;

    let task = make_task("t1", TaskStatus::Pending, classified_assignment(), 4);
    let svc = MockSvc::with_task(task);
    let set_assignment_calls = svc.set_assignment_calls.clone();
    let c = RunnerContext::new("s", "agent1")
        .unwrap()
        .with_tasklist_service(svc)
        .with_agent_profile_store(store);

    let out = TodoUpdate
        .invoke(json!({"task_id": "t1", "owner": "Frontend"}), &c)
        .await
        .unwrap();
    assert!(matches!(out, ToolOutput::Text(_)), "expected Text, got {out:?}");

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let calls = set_assignment_calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "expected exactly one set_assignment call (the owner pin)");
    assert_eq!(
        calls[0].1,
        Some(TaskAssignment {
            owner_agent_id: "frontend-worker-uuid".to_string(),
            mode: AssignmentMode::Pinned
        }),
        "assignment must carry the resolved canonical agent_id, not the raw display name"
    );
}

/// An `owner` value that resolves to neither an existing agent_id nor a
/// known address-book name fails fast at TodoUpdate call time, before any
/// assignment is written.
#[tokio::test]
async fn owner_unresolvable_name_fails_fast_with_available_targets() {
    use ao_protocol::agent::DelegateTarget;

    let tmp = tempfile::TempDir::new().unwrap();
    let mut caller = make_agent_profile("agent1", "Caller");
    caller.delegates_to = vec![DelegateTarget {
        target_agent_id: "frontend-worker-uuid".to_string(),
        name: "Frontend".to_string(),
        purpose: "handle frontend tasks".to_string(),
        share_context_allowed: false,
    }];
    let target = make_agent_profile("frontend-worker-uuid", "Frontend Worker");
    let store = setup_agent_profile_store(&tmp, &caller, &target).await;

    let task = make_task("t1", TaskStatus::Pending, classified_assignment(), 0);
    let svc = MockSvc::with_task(task);
    let set_assignment_calls = svc.set_assignment_calls.clone();
    let c = RunnerContext::new("s", "agent1")
        .unwrap()
        .with_tasklist_service(svc)
        .with_agent_profile_store(store);

    let out = TodoUpdate
        .invoke(json!({"task_id": "t1", "owner": "Backend"}), &c)
        .await
        .unwrap();

    match out {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("Backend"), "got: {message}");
            assert!(message.contains("Frontend"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
    assert!(
        set_assignment_calls.lock().unwrap().is_empty(),
        "no assignment must be written when owner resolution fails"
    );
}
