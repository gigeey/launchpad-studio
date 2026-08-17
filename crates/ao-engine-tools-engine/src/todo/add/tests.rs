use std::sync::{Arc, Mutex};

use ao_engine_tools_core::{EngineTool, RunnerContext, TasklistServiceHandle, ToolOutput};
use ao_protocol::{
    error::AoError,
    tasklist::{AssignmentMode, Task, TaskAssignment, TaskGroup, TaskGroupMode, Tasklist, TasklistOwner, TasklistStatus},
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::TodoAdd;

struct MockSvc {
    active: Option<Tasklist>,
    // Records each `tasks` argument passed to add_group_for_agent, so tests
    // can assert on the assignment that would have been persisted.
    add_calls: Arc<Mutex<Vec<Vec<Task>>>>,
}

impl MockSvc {
    fn with_active() -> Arc<Self> {
        Arc::new(Self {
            active: Some(fake_tasklist()),
            add_calls: Arc::new(Mutex::new(Vec::new())),
        })
    }
    fn no_active() -> Arc<Self> {
        Arc::new(Self {
            active: None,
            add_calls: Arc::new(Mutex::new(Vec::new())),
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
    async fn add_group_for_agent(
        &self,
        _: &str,
        _: &str,
        tasks: Vec<Task>,
        mode: TaskGroupMode,
    ) -> Result<Tasklist, AoError> {
        self.add_calls.lock().unwrap().push(tasks.clone());
        let mut tl = fake_tasklist();
        tl.groups.push(TaskGroup { id: "g1".to_string(), mode, tasks });
        Ok(tl)
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
async fn happy_path() {
    let c = ctx(MockSvc::with_active());
    let out = TodoAdd
        .invoke(
            json!({"items": [{"title": "T1", "brief": "B1"}, {"title": "T2", "brief": "B2"}]}),
            &c,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["added_count"], 2);
            assert_eq!(v["mode"], "seq");
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn no_active_tasklist_error() {
    let c = ctx(MockSvc::no_active());
    let out = TodoAdd
        .invoke(json!({"items": [{"title": "T", "brief": "B"}]}), &c)
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
async fn invalid_items_error() {
    let c = ctx(MockSvc::with_active());
    let out = TodoAdd
        .invoke(json!({"items": [{"title": "", "brief": "B"}]}), &c)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { message, .. } => {
            assert!(message.contains("title"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
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

/// A display-name `owner` on a TodoAdd item resolves to the target's
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

    let svc = MockSvc::with_active();
    let c = RunnerContext::new("s", "agent1")
        .unwrap()
        .with_tasklist_service(Arc::clone(&svc) as _)
        .with_agent_profile_store(store);

    let out = TodoAdd
        .invoke(
            json!({"items": [{"title": "Pinned by name", "brief": "do it", "owner": "Frontend"}]}),
            &c,
        )
        .await
        .unwrap();

    assert!(matches!(out, ToolOutput::Structured(_)), "expected Structured, got {out:?}");

    let calls = svc.add_calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "add_group_for_agent must be called once");
    let assignment = calls[0][0]
        .assignment
        .as_ref()
        .expect("pinned item must carry an assignment");
    assert_eq!(
        assignment.owner_agent_id, "frontend-worker-uuid",
        "assignment must carry the resolved canonical agent_id, not the raw display name"
    );
    assert_eq!(assignment.mode, AssignmentMode::Pinned);
}

/// An `owner` value that resolves to neither an existing agent_id nor a
/// known address-book name fails fast at TodoAdd call time, before any
/// task group is persisted.
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

    let svc = MockSvc::with_active();
    let c = RunnerContext::new("s", "agent1")
        .unwrap()
        .with_tasklist_service(Arc::clone(&svc) as _)
        .with_agent_profile_store(store);

    let out = TodoAdd
        .invoke(
            json!({"items": [{"title": "T", "brief": "B", "owner": "Backend"}]}),
            &c,
        )
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
        svc.add_calls.lock().unwrap().is_empty(),
        "add_group_for_agent must not be called when owner resolution fails"
    );
}
