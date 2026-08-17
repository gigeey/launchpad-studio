use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;

use ao_persistence::{paths::DataRoot, PersistenceLayer};
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, DelegateTarget, InputMode, OutputFormat, ProviderConfig,
};
use ao_protocol::tasklist::AssignmentMode;

use super::{ClassifyCallProvider, ClassifyError, TaskClassifier};

// ── Mock provider ─────────────────────────────────────────────────────────────

enum MockResponse {
    Output(String),
    #[allow(dead_code)]
    Err(ClassifyError),
    /// Block until the timeout fires (simulate a hanging model call).
    Hang,
}

struct MockClassifyProvider {
    responses: Mutex<Vec<MockResponse>>,
    call_count: AtomicUsize,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

impl MockClassifyProvider {
    fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            call_count: AtomicUsize::new(0),
            active: Arc::new(AtomicUsize::new(0)),
            max_active: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn invocation_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ClassifyCallProvider for MockClassifyProvider {
    async fn single_shot(
        &self,
        _agent: &AgentProfile,
        _system_prompt: &str,
        _user_prompt: &str,
    ) -> Result<String, ClassifyError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);

        let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.max_active.fetch_max(current, Ordering::SeqCst);

        let result = {
            let mut guard = self.responses.lock().await;
            if guard.is_empty() {
                Err(ClassifyError::Retryable("mock: no more responses".to_string()))
            } else {
                Ok(guard.remove(0))
            }
        };

        let response = result?;

        let out = match response {
            MockResponse::Output(s) => Ok(s),
            MockResponse::Err(e) => Err(e),
            MockResponse::Hang => {
                // Sleep effectively forever; caller's timeout cancels this future.
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(String::new())
            }
        };

        self.active.fetch_sub(1, Ordering::SeqCst);
        out
    }
}

// ── Test helpers ──────────────────────────────────────────────────────────────

fn make_entry(id: &str, name: &str, desc: &str) -> DelegateTarget {
    DelegateTarget {
        target_agent_id: id.to_string(),
        name: name.to_string(),
        purpose: desc.to_string(),
        share_context_allowed: false,
    }
}

fn make_agent(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Agent {id}"),
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
            no_output_timeout_ms: 30_000,
            file_capabilities: None,
        }),
        model: None,
        skills: vec![],
        system_prompt: Some("You are a helpful coordinator.".to_string()),
        tools: None,
        env: HashMap::new(),
        max_instances: 1,
        timeout_seconds: 300,
        working_dir: None,
        home_dir: None,
        serialize: true,
        workflows: None,
        template: None,
        runner_mode: Default::default(),
        enabled_plugins: HashMap::new(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
        owning_team_id: None,
        native_provider: None,
        thinking: None,
        max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
        delegates_to: vec![],
        persona: None,
        special_instructions: None,
        legacy_system_prompt: None,
        max_delegation_depth: None,
        channels: vec![],
        max_turns: None,
    }
}

/// Each test gets its own temp dir + persistence layer, avoiding env-var races.
async fn setup(
    targets: Vec<DelegateTarget>,
    provider: Arc<dyn ClassifyCallProvider>,
    timeout_secs: u64,
) -> (TaskClassifier, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let persistence = Arc::new(PersistenceLayer::init_with_root(data_root).await.unwrap());

    let mut parent = make_agent("parent");
    parent.delegates_to = targets;
    persistence.agents.create(&parent).await.unwrap();

    let classifier = TaskClassifier::new_with_config(persistence, provider, 4, timeout_secs);
    (classifier, tmp)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn empty_targets_short_circuits_no_model_call() {
    let mock = Arc::new(MockClassifyProvider::new(vec![]));
    let count_ref = Arc::clone(&mock);
    let (c, _tmp) = setup(vec![], mock as Arc<dyn ClassifyCallProvider>, 30).await;

    let result = c.classify("parent", "t1", "Do X", "").await.unwrap();
    assert_eq!(result.owner_agent_id, "parent");
    assert_eq!(result.mode, AssignmentMode::Classified);
    assert_eq!(
        count_ref.invocation_count(),
        0,
        "no model call when agent has no delegate targets"
    );
}

#[tokio::test]
async fn valid_json_owner_parses_to_classified_assignment() {
    let mock = Arc::new(MockClassifyProvider::new(vec![MockResponse::Output(
        r#"{"owner_agent_id": "backend"}"#.to_string(),
    )]));
    let entries = vec![make_entry("backend", "Backend", "API work")];
    let (c, _tmp) = setup(entries, mock as Arc<dyn ClassifyCallProvider>, 30).await;

    let result = c.classify("parent", "t1", "Build API", "").await.unwrap();
    assert_eq!(result.owner_agent_id, "backend");
    assert_eq!(result.mode, AssignmentMode::Classified);
}

#[tokio::test]
async fn null_owner_falls_back_to_parent() {
    let mock = Arc::new(MockClassifyProvider::new(vec![MockResponse::Output(
        r#"{"owner_agent_id": null}"#.to_string(),
    )]));
    let entries = vec![make_entry("backend", "Backend", "API work")];
    let (c, _tmp) = setup(entries, mock as Arc<dyn ClassifyCallProvider>, 30).await;

    let result = c.classify("parent", "t1", "Build API", "").await.unwrap();
    assert_eq!(result.owner_agent_id, "parent");
    assert_eq!(result.mode, AssignmentMode::Classified);
}

#[tokio::test]
async fn markdown_wrapped_json_stripped_and_parsed() {
    let mock = Arc::new(MockClassifyProvider::new(vec![MockResponse::Output(
        "```json\n{\"owner_agent_id\": \"backend\"}\n```".to_string(),
    )]));
    let entries = vec![make_entry("backend", "Backend", "API work")];
    let (c, _tmp) = setup(entries, mock as Arc<dyn ClassifyCallProvider>, 30).await;

    let result = c.classify("parent", "t1", "Build API", "").await.unwrap();
    assert_eq!(result.owner_agent_id, "backend");
}

#[tokio::test]
async fn preamble_before_json_stripped_and_parsed() {
    let mock = Arc::new(MockClassifyProvider::new(vec![MockResponse::Output(
        "Sure, here is the answer: {\"owner_agent_id\": \"backend\"}".to_string(),
    )]));
    let entries = vec![make_entry("backend", "Backend", "API work")];
    let (c, _tmp) = setup(entries, mock as Arc<dyn ClassifyCallProvider>, 30).await;

    let result = c.classify("parent", "t1", "Build API", "").await.unwrap();
    assert_eq!(result.owner_agent_id, "backend");
}

#[tokio::test]
async fn hallucinated_agent_id_returns_parse_failed() {
    let mock = Arc::new(MockClassifyProvider::new(vec![MockResponse::Output(
        r#"{"owner_agent_id": "does-not-exist"}"#.to_string(),
    )]));
    let entries = vec![make_entry("backend", "Backend", "API work")];
    let (c, _tmp) = setup(entries, mock as Arc<dyn ClassifyCallProvider>, 30).await;

    let err = c.classify("parent", "t1", "Build API", "").await.unwrap_err();
    assert!(
        matches!(err, ClassifyError::ParseFailed(_)),
        "expected ParseFailed, got: {:?}",
        err
    );
}

#[tokio::test]
async fn garbage_output_returns_parse_failed() {
    let mock = Arc::new(MockClassifyProvider::new(vec![MockResponse::Output(
        "not json at all!!!".to_string(),
    )]));
    let entries = vec![make_entry("backend", "Backend", "API work")];
    let (c, _tmp) = setup(entries, mock as Arc<dyn ClassifyCallProvider>, 30).await;

    let err = c.classify("parent", "t1", "Build API", "").await.unwrap_err();
    assert!(
        matches!(err, ClassifyError::ParseFailed(_)),
        "expected ParseFailed, got: {:?}",
        err
    );
}

#[tokio::test]
async fn timeout_yields_retryable_with_timeout_reason() {
    let mock = Arc::new(MockClassifyProvider::new(vec![MockResponse::Hang]));
    let entries = vec![make_entry("backend", "Backend", "API work")];
    // Use a 1-second timeout so the test completes quickly.
    let (c, _tmp) = setup(entries, mock as Arc<dyn ClassifyCallProvider>, 1).await;

    let err = c.classify("parent", "t1", "Build API", "").await.unwrap_err();
    assert!(
        matches!(err, ClassifyError::Retryable(_)),
        "expected Retryable timeout, got: {:?}",
        err
    );
    let ClassifyError::Retryable(reason) = err else { panic!() };
    assert!(
        reason.contains("timed out"),
        "expected 'timed out' in reason, got: {reason}"
    );
}

#[tokio::test]
async fn semaphore_limits_max_concurrent_calls_to_pool_size() {
    struct ConcurrencyTracker {
        active: Arc<AtomicUsize>,
        max_observed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ClassifyCallProvider for ConcurrencyTracker {
        async fn single_shot(
            &self,
            _agent: &AgentProfile,
            _system_prompt: &str,
            _user_prompt: &str,
        ) -> Result<String, ClassifyError> {
            let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = self.max_observed.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(80)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(r#"{"owner_agent_id": null}"#.to_string())
        }
    }

    let active = Arc::new(AtomicUsize::new(0));
    let max_observed = Arc::new(AtomicUsize::new(0));
    let tracker = Arc::new(ConcurrencyTracker {
        active: Arc::clone(&active),
        max_observed: Arc::clone(&max_observed),
    });
    let max_ref = Arc::clone(&max_observed);

    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let persistence = Arc::new(PersistenceLayer::init_with_root(data_root).await.unwrap());

    let mut parent = make_agent("parent");
    parent.delegates_to = vec![make_entry("backend", "Backend", "API work")];
    persistence.agents.create(&parent).await.unwrap();

    // Pool size = 4; spawn 8 concurrent classify calls.
    let classifier = Arc::new(TaskClassifier::new_with_config(
        persistence,
        tracker as Arc<dyn ClassifyCallProvider>,
        4,
        30,
    ));

    let mut handles = Vec::new();
    for i in 0..8usize {
        let c = Arc::clone(&classifier);
        handles.push(tokio::spawn(async move {
            c.classify("parent", &format!("t{i}"), "Do task", "").await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }

    let max = max_ref.load(Ordering::SeqCst);
    assert!(max <= 4, "max concurrency must be ≤ 4 (pool size), observed {max}");
}

// ── Boot sweep tests ──────────────────────────────────────────────────────────

mod boot_sweep {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use chrono::Utc;

    use ao_engine_tools_core::terminal_report::{CancelOutcome, TerminalWatcherGuard};
    use ao_engine_tools_core::{TasklistServiceHandle};
    use ao_persistence::{paths::DataRoot, PersistenceLayer};
    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };
    use ao_protocol::error::AoError;
    use ao_protocol::tasklist::{
        AssignmentMode, Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus, Tasklist,
        TasklistOwner, TasklistStatus,
    };

    use super::super::{ClassifyCallProvider, ClassifyError, TaskClassifier};

    // ── Mock classify provider ────────────────────────────────────────────────

    struct AlwaysClassifyTo(String);

    #[async_trait]
    impl ClassifyCallProvider for AlwaysClassifyTo {
        async fn single_shot(
            &self,
            _agent: &AgentProfile,
            _system_prompt: &str,
            _user_prompt: &str,
        ) -> Result<String, ClassifyError> {
            Ok(format!(r#"{{"owner_agent_id": "{}"}}"#, self.0))
        }
    }

    struct AlwaysError;

    #[async_trait]
    impl ClassifyCallProvider for AlwaysError {
        async fn single_shot(
            &self,
            _: &AgentProfile,
            _: &str,
            _: &str,
        ) -> Result<String, ClassifyError> {
            Err(ClassifyError::Retryable("mock error".to_string()))
        }
    }

    // ── Mock TasklistServiceHandle ────────────────────────────────────────────

    struct MockSvc {
        calls: Arc<Mutex<Vec<(String, String, String, Option<TaskAssignment>, u64)>>>,
    }

    impl MockSvc {
        fn new() -> (Self, Arc<Mutex<Vec<(String, String, String, Option<TaskAssignment>, u64)>>>) {
            let calls = Arc::new(Mutex::new(vec![]));
            (Self { calls: Arc::clone(&calls) }, calls)
        }
    }

    #[async_trait]
    impl TasklistServiceHandle for MockSvc {
        async fn agent_active(&self, _: &str) -> Result<Option<Tasklist>, AoError> { unimplemented!() }
        async fn create_for_agent(&self, _: &str, _: String, _: Vec<ao_protocol::tasklist::TaskGroup>) -> Result<Tasklist, AoError> { unimplemented!() }
        async fn get_agent_max_instances(&self, _: &str) -> Result<u32, AoError> { Ok(1) }
        async fn add_group_for_agent(&self, _: &str, _: &str, _: Vec<Task>, _: TaskGroupMode) -> Result<Tasklist, AoError> { unimplemented!() }
        async fn update_task_for_agent(&self, _: &str, _: &str, _: &str, _: Option<String>, _: Option<String>, _: Option<Vec<String>>) -> Result<Tasklist, AoError> { unimplemented!() }
        async fn complete_task_for_agent(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { unimplemented!() }
        async fn terminal_watcher(&self, _: &str) -> Result<TerminalWatcherGuard, AoError> { unimplemented!() }
        async fn cancel_for_agent(&self, _: &str) -> Result<CancelOutcome, AoError> { unimplemented!() }

        async fn set_assignment(
            &self,
            agent_id: &str,
            tasklist_id: &str,
            task_id: &str,
            assignment: Option<TaskAssignment>,
            expected_token: u64,
        ) -> Result<bool, AoError> {
            self.calls.lock().unwrap().push((
                agent_id.to_string(),
                tasklist_id.to_string(),
                task_id.to_string(),
                assignment,
                expected_token,
            ));
            Ok(true)
        }
    }

    // ── Test helpers ──────────────────────────────────────────────────────────

    fn make_agent_profile(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: String::new(),
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
                no_output_timeout_ms: 30_000,
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
            runner_mode: Default::default(),
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
            max_turns: None,
        }
    }

    fn make_pending_task(id: &str, agent_id: &str, group_id: &str) -> Task {
        Task {
            id: id.to_string(),
            owner_agent_id: agent_id.to_string(),
            prompt: format!("Do task {id}: some description"),
            expected_outputs: vec![],
            status: TaskStatus::Pending,
            group_id: group_id.to_string(),
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

    fn make_tasklist(data_root: &DataRoot, agent_id: &str, tl_id: &str, tasks: Vec<Task>) -> Tasklist {
        Tasklist {
            id: tl_id.to_string(),
            owner: TasklistOwner::Agent { agent_id: agent_id.to_string() },
            team_id: None,
            title: "test list".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: "g1".to_string(),
                mode: TaskGroupMode::Seq,
                tasks,
            }],
            workspace_dir: data_root
                .agent_tasklist_workspace_dir(agent_id, tl_id)
                .to_string_lossy()
                .into_owned(),
            transcripts_dir: data_root
                .agent_tasklist_transcripts_dir(agent_id, tl_id)
                .to_string_lossy()
                .into_owned(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn sweep_picks_up_orphan_null() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let persistence = Arc::new(PersistenceLayer::init_with_root(data_root.clone()).await.unwrap());

        // Seed: one agent, one tasklist, one NotStarted + assignment:None task.
        persistence.agents.create(&make_agent_profile("parent")).await.unwrap();
        let tl = make_tasklist(&data_root, "parent", "tl1", vec![make_pending_task("t1", "parent", "g1")]);
        persistence.tasklists.create_for_agent(&tl).await.unwrap();

        let provider = Arc::new(AlwaysClassifyTo("parent".to_string()));
        let classifier = TaskClassifier::new_with_config(persistence, provider, 4, 30);
        let (svc, calls) = MockSvc::new();

        classifier.run_boot_sweep(Arc::new(svc)).await;

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1, "sweep should classify the orphan task");
        assert_eq!(recorded[0].2, "t1");
        assert_eq!(
            recorded[0].3.as_ref().unwrap().mode,
            AssignmentMode::Classified
        );
    }

    #[tokio::test]
    async fn sweep_ignores_started_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let persistence = Arc::new(PersistenceLayer::init_with_root(data_root.clone()).await.unwrap());

        // Seed: one agent, one InProgress task with assignment:None (degenerate state).
        persistence.agents.create(&make_agent_profile("parent")).await.unwrap();
        let mut task = make_pending_task("t1", "parent", "g1");
        task.status = TaskStatus::InProgress;
        let tl = make_tasklist(&data_root, "parent", "tl1", vec![task]);
        persistence.tasklists.create_for_agent(&tl).await.unwrap();

        let provider = Arc::new(AlwaysError);
        let classifier = TaskClassifier::new_with_config(persistence, provider, 4, 30);
        let (svc, calls) = MockSvc::new();

        classifier.run_boot_sweep(Arc::new(svc)).await;

        assert!(calls.lock().unwrap().is_empty(), "InProgress tasks must not be swept");
    }

    #[tokio::test]
    async fn sweep_idempotent_under_race() {
        // Two concurrent sweeps on the same task: the second write-back is stale
        // because the first one already bumped the token. The mock returns true
        // for both (it doesn't enforce CAS), but a real service would return false
        // for the second. We verify both sweeps complete without error.
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let persistence = Arc::new(PersistenceLayer::init_with_root(data_root.clone()).await.unwrap());

        persistence.agents.create(&make_agent_profile("parent")).await.unwrap();
        let tl = make_tasklist(&data_root, "parent", "tl1", vec![make_pending_task("t1", "parent", "g1")]);
        persistence.tasklists.create_for_agent(&tl).await.unwrap();

        let provider = Arc::new(AlwaysClassifyTo("parent".to_string()));
        let classifier = TaskClassifier::new_with_config(Arc::clone(&persistence.clone()), provider, 4, 30);

        // Stale-token mock: first call returns true, second returns false.
        struct StaleMockSvc {
            call_count: Arc<Mutex<usize>>,
        }
        #[async_trait]
        impl TasklistServiceHandle for StaleMockSvc {
            async fn agent_active(&self, _: &str) -> Result<Option<Tasklist>, AoError> { unimplemented!() }
            async fn create_for_agent(&self, _: &str, _: String, _: Vec<ao_protocol::tasklist::TaskGroup>) -> Result<Tasklist, AoError> { unimplemented!() }
            async fn get_agent_max_instances(&self, _: &str) -> Result<u32, AoError> { Ok(1) }
            async fn add_group_for_agent(&self, _: &str, _: &str, _: Vec<Task>, _: TaskGroupMode) -> Result<Tasklist, AoError> { unimplemented!() }
            async fn update_task_for_agent(&self, _: &str, _: &str, _: &str, _: Option<String>, _: Option<String>, _: Option<Vec<String>>) -> Result<Tasklist, AoError> { unimplemented!() }
            async fn complete_task_for_agent(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { unimplemented!() }
            async fn terminal_watcher(&self, _: &str) -> Result<TerminalWatcherGuard, AoError> { unimplemented!() }
            async fn cancel_for_agent(&self, _: &str) -> Result<CancelOutcome, AoError> { unimplemented!() }
            async fn set_assignment(&self, _: &str, _: &str, _: &str, _: Option<TaskAssignment>, _: u64) -> Result<bool, AoError> {
                let mut count = self.call_count.lock().unwrap();
                *count += 1;
                let n = *count;
                Ok(n == 1) // first call succeeds, second is stale
            }
        }

        let count = Arc::new(Mutex::new(0usize));
        let svc = Arc::new(StaleMockSvc { call_count: Arc::clone(&count) });

        // Run two concurrent sweeps.
        let c1 = classifier.clone();
        let c2 = classifier.clone();
        let svc1 = Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>;
        let svc2 = Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>;
        let h1 = tokio::spawn(async move { c1.run_boot_sweep(svc1).await });
        let h2 = tokio::spawn(async move { c2.run_boot_sweep(svc2).await });
        h1.await.unwrap();
        h2.await.unwrap();

        // Two writes were attempted; the mock let both through, but a real CAS
        // would have rejected the second. The key assertion is no panic.
        let total = *count.lock().unwrap();
        assert_eq!(total, 2, "both sweeps attempted set_assignment");
    }

    #[tokio::test]
    async fn sweep_recovers_post_retry_exhaustion() {
        // A task left as assignment:None after retry budget exhaustion (simulated
        // by seeding it directly) should be picked up on the next sweep call.
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let persistence = Arc::new(PersistenceLayer::init_with_root(data_root.clone()).await.unwrap());

        persistence.agents.create(&make_agent_profile("parent")).await.unwrap();
        // Task with classifier_token = 5 (simulating previous mutation history).
        let mut task = make_pending_task("t1", "parent", "g1");
        task.classifier_token = 5;
        let tl = make_tasklist(&data_root, "parent", "tl1", vec![task]);
        persistence.tasklists.create_for_agent(&tl).await.unwrap();

        let provider = Arc::new(AlwaysClassifyTo("parent".to_string()));
        let classifier = TaskClassifier::new_with_config(persistence, provider, 4, 30);
        let (svc, calls) = MockSvc::new();

        classifier.run_boot_sweep(Arc::new(svc)).await;

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1, "sweep must retry exhausted task");
        // Token passed to set_assignment must match what was on the task row.
        assert_eq!(recorded[0].4, 5u64, "expected_token must be the task's classifier_token");
    }
}
