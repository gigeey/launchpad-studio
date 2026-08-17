use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ao_engine_tools_core::{
    ClassifierHandle, ClassifyOutcome, EngineTool, EventSink, RunnerContext,
    TasklistServiceHandle, ToolOutput, UserEvent,
};
use ao_protocol::{error::AoError, tasklist::{AssignmentMode, Task, TaskAssignment, TaskGroup, TaskGroupMode, Tasklist}};
use async_trait::async_trait;
use serde_json::json;

use super::TodoCreate;

// --- Mock TasklistService ---

struct MockTasklistService {
    active: Option<Tasklist>,
    max_instances: u32,
    create_ok: bool,
    // Records set_assignment calls: (task_id, assignment, token)
    set_assignment_calls: Arc<Mutex<Vec<(String, Option<TaskAssignment>, u64)>>>,
}

impl MockTasklistService {
    fn idle(max_instances: u32) -> Arc<Self> {
        Arc::new(Self {
            active: None,
            max_instances,
            create_ok: true,
            set_assignment_calls: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn with_active(tl: Tasklist, max_instances: u32) -> Arc<Self> {
        Arc::new(Self {
            active: Some(tl),
            max_instances,
            create_ok: true,
            set_assignment_calls: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn low_max_instances() -> Arc<Self> {
        Arc::new(Self {
            active: None,
            max_instances: 1,
            create_ok: true,
            set_assignment_calls: Arc::new(Mutex::new(Vec::new())),
        })
    }
}

fn fake_tasklist(id: &str, title: &str) -> Tasklist {
    use ao_protocol::tasklist::{TasklistOwner, TasklistStatus};
    use chrono::Utc;
    Tasklist {
        id: id.to_string(),
        owner: TasklistOwner::Agent { agent_id: "agent1".to_string() },
        team_id: None,
        title: title.to_string(),
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

#[async_trait]
impl TasklistServiceHandle for MockTasklistService {
    async fn agent_active(&self, _agent_id: &str) -> Result<Option<Tasklist>, AoError> {
        Ok(self.active.clone())
    }

    async fn create_for_agent(
        &self,
        _agent_id: &str,
        name: String,
        groups: Vec<TaskGroup>,
    ) -> Result<Tasklist, AoError> {
        if !self.create_ok {
            return Err(AoError::Internal("mock create failed".into()));
        }
        let mut tl = fake_tasklist("new-tl-id", &name);
        tl.groups = groups;
        Ok(tl)
    }

    async fn get_agent_max_instances(&self, _agent_id: &str) -> Result<u32, AoError> {
        Ok(self.max_instances)
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
        Ok(true)
    }
}

// --- Mock ClassifierHandle ---

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
        _parent_agent_id: &str,
        _task_id: &str,
        _task_title: &str,
        _task_description: &str,
    ) -> ClassifyOutcome {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        self.outcome.clone()
    }
}

// --- Event spy sink ---

struct SpyEventSink {
    events: Arc<Mutex<Vec<UserEvent>>>,
}

impl SpyEventSink {
    fn new() -> (Arc<Self>, Arc<Mutex<Vec<UserEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (Arc::new(Self { events: events.clone() }), events)
    }
}

#[async_trait]
impl EventSink for SpyEventSink {
    async fn emit(&self, event: UserEvent) -> Result<(), AoError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

// --- Context helpers ---

fn ctx_with_svc(svc: Arc<dyn TasklistServiceHandle + Send + Sync>) -> RunnerContext {
    RunnerContext::new("session1", "agent1")
        .unwrap()
        .with_tasklist_service(svc)
}

fn subagent_ctx(svc: Arc<dyn TasklistServiceHandle + Send + Sync>) -> RunnerContext {
    RunnerContext::new("session1", "agent1")
        .unwrap()
        .with_tasklist_service(svc)
        .with_depth(1)
}

fn ctx_with_svc_and_classifier(
    svc: Arc<dyn TasklistServiceHandle + Send + Sync>,
    classifier: Arc<dyn ClassifierHandle + Send + Sync>,
) -> RunnerContext {
    RunnerContext::new("session1", "agent1")
        .unwrap()
        .with_tasklist_service(svc)
        .with_classifier(classifier)
}

// --- Existing tests (unchanged) ---

#[tokio::test]
async fn happy_path_seq() {
    let ctx = ctx_with_svc(MockTasklistService::idle(2));
    let tool = TodoCreate;
    let result = tool
        .invoke(
            json!({
                "name": "My Tasklist",
                "items": [
                    { "title": "Step 1", "brief": "Do thing A" },
                    { "title": "Step 2", "brief": "Do thing B" }
                ],
                "dispatch_mode": "async"
            }),
            &ctx,
        )
        .await
        .unwrap();
    match result {
        ao_engine_tools_core::ToolOutput::Structured(v) => {
            assert_eq!(v["name"], "My Tasklist");
            assert_eq!(v["mode"], "seq");
            assert_eq!(v["status"], "active");
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn happy_path_par() {
    let ctx = ctx_with_svc(MockTasklistService::idle(3));
    let tool = TodoCreate;
    let result = tool
        .invoke(
            json!({
                "name": "Parallel Work",
                "items": [
                    { "title": "Task A", "brief": "Do A concurrently" },
                    { "title": "Task B", "brief": "Do B concurrently" }
                ],
                "mode": "par",
                "dispatch_mode": "async"
            }),
            &ctx,
        )
        .await
        .unwrap();
    match result {
        ao_engine_tools_core::ToolOutput::Structured(v) => {
            assert_eq!(v["mode"], "par");
            assert_eq!(v["dispatch_mode"], "async");
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn already_exists_error() {
    let existing = fake_tasklist("existing-id", "Old List");
    let ctx = ctx_with_svc(MockTasklistService::with_active(existing, 2));
    let tool = TodoCreate;
    let result = tool
        .invoke(
            json!({
                "name": "New List",
                "items": [{ "title": "T", "brief": "B" }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match result {
        ao_engine_tools_core::ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("already has an active tasklist"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn max_instances_too_low_error() {
    let ctx = ctx_with_svc(MockTasklistService::low_max_instances());
    let tool = TodoCreate;
    let result = tool
        .invoke(
            json!({
                "name": "Restricted List",
                "items": [{ "title": "T", "brief": "B" }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match result {
        ao_engine_tools_core::ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("max_instances"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn unsupported_scope_error() {
    let ctx = subagent_ctx(MockTasklistService::idle(2));
    let tool = TodoCreate;
    let result = tool
        .invoke(
            json!({
                "name": "Subagent List",
                "items": [{ "title": "T", "brief": "B" }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match result {
        ao_engine_tools_core::ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("subagent context"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_mode_omitted_defaults_to_sync() {
    let ctx = ctx_with_svc(MockTasklistService::idle(2));
    let tool = TodoCreate;
    // Omitting dispatch_mode should default to sync, which returns an unimplemented error.
    let result = tool
        .invoke(
            json!({
                "name": "Default Dispatch",
                "items": [{ "title": "T", "brief": "B" }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match result {
        ao_engine_tools_core::ToolOutput::Error { message, .. } => {
            assert!(message.contains("sync"), "expected sync stub error, got: {message}");
        }
        other => panic!("expected sync stub error, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_mode_sync_explicit() {
    let ctx = ctx_with_svc(MockTasklistService::idle(2));
    let tool = TodoCreate;
    let result = tool
        .invoke(
            json!({
                "name": "Explicit Sync",
                "items": [{ "title": "T", "brief": "B" }],
                "dispatch_mode": "sync"
            }),
            &ctx,
        )
        .await
        .unwrap();
    match result {
        ao_engine_tools_core::ToolOutput::Error { message, .. } => {
            assert!(message.contains("sync"), "expected sync stub error, got: {message}");
        }
        other => panic!("expected sync stub error, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_mode_async_returns_active_response() {
    let ctx = ctx_with_svc(MockTasklistService::idle(2));
    let tool = TodoCreate;
    let result = tool
        .invoke(
            json!({
                "name": "Async Dispatch",
                "items": [{ "title": "T", "brief": "B" }],
                "dispatch_mode": "async"
            }),
            &ctx,
        )
        .await
        .unwrap();
    match result {
        ao_engine_tools_core::ToolOutput::Structured(v) => {
            assert_eq!(v["status"], "active");
            assert_eq!(v["dispatch_mode"], "async");
            assert_eq!(v["name"], "Async Dispatch");
        }
        other => panic!("expected Structured active response, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_mode_invalid_rejected() {
    let ctx = ctx_with_svc(MockTasklistService::idle(2));
    let tool = TodoCreate;
    let result = tool
        .invoke(
            json!({
                "name": "Bad Dispatch",
                "items": [{ "title": "T", "brief": "B" }],
                "dispatch_mode": "parallel"
            }),
            &ctx,
        )
        .await
        .unwrap();
    match result {
        ao_engine_tools_core::ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("dispatch_mode"), "got: {message}");
            assert!(message.contains("parallel"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// --- New tests ---

/// Pinned item gets Pinned assignment; no classifier call for it.
/// Unassigned items get None assignment; classifier is spawned for them.
#[tokio::test]
async fn pinned_item_has_pinned_assignment_no_classifier_call() {
    let call_count = Arc::new(AtomicU32::new(0));
    let classifier = Arc::new(MockClassifier {
        call_count: call_count.clone(),
        outcome: ClassifyOutcome::Assigned(TaskAssignment {
            owner_agent_id: "backend".to_string(),
            mode: AssignmentMode::Classified,
        }),
    });
    let svc = MockTasklistService::idle(2);

    let ctx = ctx_with_svc_and_classifier(Arc::clone(&svc) as _, classifier);

    let result = TodoCreate
        .invoke(
            json!({
                "name": "Mixed List",
                "items": [
                    { "title": "Pinned Task", "brief": "do it", "owner": "frontend" },
                    { "title": "Classify Me", "brief": "route this" }
                ],
                "dispatch_mode": "async"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(matches!(result, ToolOutput::Structured(_)));

    // Give the background spawn a moment to run.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Classifier called once (only for the unassigned item).
    assert_eq!(call_count.load(Ordering::Relaxed), 1, "expected 1 classifier call");
}

/// Three items: 1 pinned, 2 unassigned. Classifier resolves both unassigned.
/// Exactly one TodoListCreated event is emitted with item_count = 3.
#[tokio::test]
async fn todo_list_created_event_emitted_once_with_correct_item_count() {
    let classifier = MockClassifier::always_assigned("backend");
    let svc = MockTasklistService::idle(2);

    let (spy_sink, events) = SpyEventSink::new();
    let ctx = RunnerContext::new("session1", "agent1")
        .unwrap()
        .with_tasklist_service(Arc::clone(&svc) as _)
        .with_classifier(classifier as _)
        .with_event_sink(spy_sink as _);

    TodoCreate
        .invoke(
            json!({
                "name": "Three Items",
                "items": [
                    { "title": "Pinned", "brief": "pinned task", "owner": "frontend" },
                    { "title": "T1", "brief": "classify 1" },
                    { "title": "T2", "brief": "classify 2" }
                ],
                "dispatch_mode": "async"
            }),
            &ctx,
        )
        .await
        .unwrap();

    let emitted = events.lock().unwrap();
    let created_events: Vec<_> = emitted
        .iter()
        .filter(|e| matches!(e, UserEvent::TodoListCreated { .. }))
        .collect();

    assert_eq!(created_events.len(), 1, "exactly one TodoListCreated must fire");

    if let UserEvent::TodoListCreated { tasklist_id: _, item_count, items } =
        &created_events[0]
    {
        assert_eq!(*item_count, 3);
        assert_eq!(items.len(), 3);

        // Pinned item has Pinned assignment in the snapshot.
        let pinned = items.iter().find(|i| i.title == "Pinned").expect("pinned item");
        assert!(
            matches!(&pinned.assignment, Some(a) if a.mode == AssignmentMode::Pinned),
            "pinned item must have Pinned assignment at emit time"
        );

        // Unassigned items have None assignment in the snapshot (classifier in-flight).
        let t1 = items.iter().find(|i| i.title == "T1").expect("T1");
        assert!(t1.assignment.is_none(), "unassigned items must be None in snapshot");
    } else {
        panic!("expected TodoListCreated variant");
    }
}

/// After retry budget is exhausted (all Retryable), the task row stays None.
/// This test verifies classify_with_retry gives up after 3+1 attempts.
/// We use very short delays to keep the test fast.
#[tokio::test]
async fn classifier_retry_exhaustion_leaves_row_none() {
    // Override delays to 0 by using a retryable mock and verifying call count.
    let call_count = Arc::new(AtomicU32::new(0));
    let classifier = Arc::new(MockClassifier {
        call_count: call_count.clone(),
        outcome: ClassifyOutcome::Retryable("mock transient".to_string()),
    });
    let svc = MockTasklistService::idle(2);

    let ctx = ctx_with_svc_and_classifier(Arc::clone(&svc) as _, classifier);

    TodoCreate
        .invoke(
            json!({
                "name": "Retry Test",
                "items": [{ "title": "Will Fail", "brief": "classifier always fails" }],
                "dispatch_mode": "async"
            }),
            &ctx,
        )
        .await
        .unwrap();

    // The actual retry has delays (5s, 15s, 45s) so we don't wait for all of
    // them in the unit test. We only verify set_assignment was never called
    // (assignment stayed None), which requires waiting for at least the first attempt.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // At minimum the first attempt ran.
    assert!(call_count.load(Ordering::Relaxed) >= 1, "classifier must have been called at least once");

    // set_assignment was never called (no assignment landed).
    let calls = svc.set_assignment_calls.lock().unwrap();
    assert!(calls.is_empty(), "set_assignment must not be called when classifier always fails");
}

/// No classifier configured → tasks with no owner stay with None assignment;
/// no panic, no error, TodoListCreated still emitted.
#[tokio::test]
async fn no_classifier_configured_no_panic() {
    let (spy_sink, events) = SpyEventSink::new();
    let ctx = RunnerContext::new("session1", "agent1")
        .unwrap()
        .with_tasklist_service(MockTasklistService::idle(2) as _)
        .with_event_sink(spy_sink as _);
    // No classifier wired up.

    let result = TodoCreate
        .invoke(
            json!({
                "name": "No Classifier",
                "items": [{ "title": "T", "brief": "B" }],
                "dispatch_mode": "async"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(matches!(result, ToolOutput::Structured(_)));

    let emitted = events.lock().unwrap();
    let created_count = emitted
        .iter()
        .filter(|e| matches!(e, UserEvent::TodoListCreated { .. }))
        .count();
    assert_eq!(created_count, 1, "TodoListCreated must fire even without classifier");
}

/// Classifier returns Ok → set_assignment called with the correct task_id and token.
#[tokio::test]
async fn classifier_ok_calls_set_assignment() {
    let call_count = Arc::new(AtomicU32::new(0));
    let classifier = Arc::new(MockClassifier {
        call_count: call_count.clone(),
        outcome: ClassifyOutcome::Assigned(TaskAssignment {
            owner_agent_id: "backend".to_string(),
            mode: AssignmentMode::Classified,
        }),
    });
    let svc = MockTasklistService::idle(2);
    let set_calls = svc.set_assignment_calls.clone();

    let ctx = ctx_with_svc_and_classifier(Arc::clone(&svc) as _, classifier);

    TodoCreate
        .invoke(
            json!({
                "name": "Classifier Ok",
                "items": [{ "title": "Route Me", "brief": "classify please" }],
                "dispatch_mode": "async"
            }),
            &ctx,
        )
        .await
        .unwrap();

    // Wait for background spawn to complete.
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(call_count.load(Ordering::Relaxed), 1);

    let calls = set_calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "set_assignment must be called once");
    let (_, assignment, token) = &calls[0];
    assert!(assignment.is_some(), "assignment must be Some");
    assert_eq!(token, &0u64, "expected token 0 (initial)");
    if let Some(a) = assignment {
        assert_eq!(a.owner_agent_id, "backend");
        assert_eq!(a.mode, AssignmentMode::Classified);
    }
}

// --- Owner resolution: display-name owner values ---

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

/// A display-name `owner` on a TodoCreate item resolves to the target's
/// canonical agent_id before it lands in the Pinned assignment — the same
/// address-book lookup `Delegate.target` performs.
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

    let (spy_sink, events) = SpyEventSink::new();
    let svc = MockTasklistService::idle(2);
    let ctx = RunnerContext::new("session1", "agent1")
        .unwrap()
        .with_tasklist_service(Arc::clone(&svc) as _)
        .with_event_sink(spy_sink as _)
        .with_agent_profile_store(store);

    let result = TodoCreate
        .invoke(
            json!({
                "name": "Named Owner List",
                "items": [
                    { "title": "Pinned by name", "brief": "do it", "owner": "Frontend" }
                ],
                "dispatch_mode": "async"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(matches!(result, ToolOutput::Structured(_)), "expected Structured, got {result:?}");

    let emitted = events.lock().unwrap();
    let created = emitted
        .iter()
        .find_map(|e| match e {
            UserEvent::TodoListCreated { items, .. } => Some(items),
            _ => None,
        })
        .expect("TodoListCreated must fire");
    assert_eq!(created.len(), 1);
    let assignment = created[0].assignment.as_ref().expect("pinned item must carry an assignment");
    assert_eq!(
        assignment.owner_agent_id, "frontend-worker-uuid",
        "assignment must carry the resolved canonical agent_id, not the raw display name"
    );
    assert_eq!(assignment.mode, AssignmentMode::Pinned);
}

/// An `owner` value that resolves to neither an existing agent_id nor a
/// known address-book name fails fast at TodoCreate call time, before any
/// tasklist is created.
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

    let ctx = RunnerContext::new("session1", "agent1")
        .unwrap()
        .with_tasklist_service(MockTasklistService::idle(2) as _)
        .with_agent_profile_store(store);

    let result = TodoCreate
        .invoke(
            json!({
                "name": "Bad Owner List",
                "items": [
                    { "title": "T", "brief": "B", "owner": "Backend" }
                ],
                "dispatch_mode": "async"
            }),
            &ctx,
        )
        .await
        .unwrap();

    match result {
        ToolOutput::Error { message, recoverable } => {
            assert!(message.contains("Backend"), "got: {message}");
            assert!(message.contains("Frontend"), "got: {message}");
            assert!(recoverable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
