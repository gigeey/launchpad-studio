//! Integration test: CLI run dying without a terminal task tag triggers the
//! stale-run safety net — the task is requeued up to DEFAULT_MAX_ATTEMPTS times,
//! then transitions to Failed with the tasklist halted.
//!
//! Each mock run exits naturally with a non-zero code and no
//! <task action="complete|fail"> emission, mimicking a context-overflow death
//! where the vendor CLI exits mid-run. The test verifies the full pipeline:
//!   CliAgentRunner detects no terminal tag → calls TaskFeeder::on_run_ended
//!   → reprompt dispatched (requeue) → repeated until cap → task Failed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ao_engine::AppState;
use ao_process::mock::{MockProcessSupervisor, MockScenario};
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
};
use ao_protocol::event::{AgentEvent, AgentEventPayload};
use ao_protocol::tasklist::{
    AssignmentMode, Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus, TasklistOwner,
    TasklistStatus,
};

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_agent(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Agent {}", id),
        description: "stale-run requeue test agent".to_string(),
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
        max_instances: 5,
        timeout_seconds: 300,
        working_dir: None,
        home_dir: None,
        serialize: true,
        workflows: None,
        template: None,
        enabled_plugins: HashMap::new(),
        runner_mode: Default::default(),
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

/// Simulates a CLI run that exits without emitting a terminal task tag.
/// exit_code=1 mimics a non-zero exit (context overflow, unexpected crash, etc.).
fn stale_scenario() -> MockScenario {
    MockScenario {
        stdout_lines: vec!["Working on the task...".to_string()],
        stderr_lines: vec![],
        exit_code: 1,
        delay_per_line_ms: 0,
    }
}

async fn setup_state(scenarios: Vec<MockScenario>) -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let mock = MockProcessSupervisor::new(scenarios);
        Arc::new(AppState::new_with_mock(mock).await.expect("init AppState"))
    };
    (state, tmp)
}

/// Wait until a TasklistFailed or TasklistCompleted event arrives for the
/// given agent-owned tasklist, or panic on timeout.
async fn wait_for_tasklist_terminal(
    rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>,
    agent_id: &str,
    tasklist_id: &str,
) -> AgentEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(ev)) => {
                let matched = match &ev.payload {
                    AgentEventPayload::TasklistFailed {
                        tasklist_id: tid,
                        owner: Some(TasklistOwner::Agent { agent_id: aid }),
                        ..
                    } => tid == tasklist_id && aid == agent_id,
                    AgentEventPayload::TasklistCompleted {
                        tasklist_id: tid,
                        owner: Some(TasklistOwner::Agent { agent_id: aid }),
                        ..
                    } => tid == tasklist_id && aid == agent_id,
                    _ => false,
                };
                if matched {
                    return ev;
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => panic!("event bus closed before terminal event"),
            Err(_) => panic!("timed out waiting for tasklist terminal event"),
        }
    }
}

/// Stale runs (no terminal task tag) trigger the requeue safety net.
/// After DEFAULT_MAX_ATTEMPTS stale runs the task transitions to Failed
/// and the tasklist halts — no further dispatch occurs.
#[tokio::test]
async fn stale_run_requeue_and_cap() {
    let worker = "stale-worker";
    let task_id = "stale-t1";

    // DEFAULT_MAX_ATTEMPTS = 3. Each stale run increments attempt_count.
    // At count = 3 (>= cap) the task transitions to Failed — no further dispatch.
    // Scenarios needed: initial dispatch + (max_attempts - 1) reprompts = 3 total.
    let cap = ao_engine::task_feeder::DEFAULT_MAX_ATTEMPTS as usize;
    let scenarios: Vec<MockScenario> = (0..cap).map(|_| stale_scenario()).collect();

    let (state, _tmp) = setup_state(scenarios).await;

    state
        .persistence
        .agents
        .create(&make_agent(worker))
        .await
        .unwrap();

    let mut rx = state.event_bus.subscribe();

    // Created through the live service entry point, which persists the
    // tasklist and bootstraps dispatch.
    let groups = vec![TaskGroup {
        id: "g1".to_string(),
        mode: TaskGroupMode::Seq,
        tasks: vec![Task {
            id: task_id.to_string(),
            owner_agent_id: worker.to_string(),
            prompt: "Do some long work that will overflow context".to_string(),
            expected_outputs: vec![],
            status: TaskStatus::Pending,
            group_id: "g1".to_string(),
            attempt_count: 0,
            error_log: vec![],
            comments: vec![],
            attachments: vec![],
            remind_me: None,
            parse_failed: false,
            notification_parse_retry_count: 0,
            // Agent-owned tasklists dispatch on `assignment`, not
            // `owner_agent_id`; pin it so this test exercises the
            // feeder rather than the classifier.
            assignment: Some(TaskAssignment {
                owner_agent_id: worker.to_string(),
                mode: AssignmentMode::Pinned,
            }),
            classifier_token: 0,
            dispatch_token: 0,
        }],
    }];

    let tasklist = state
        .tasklist_service
        .create(
            TasklistOwner::Agent {
                agent_id: worker.to_string(),
            },
            "Stale-run requeue integration test".to_string(),
            String::new(),
            groups,
            false,
        )
        .await
        .unwrap();

    let terminal_ev = wait_for_tasklist_terminal(&mut rx, worker, &tasklist.id).await;

    assert!(
        matches!(
            &terminal_ev.payload,
            AgentEventPayload::TasklistFailed { .. }
        ),
        "tasklist should fail after cap exhaustion, got {:?}",
        terminal_ev.payload,
    );

    let final_tl = state
        .persistence
        .tasklists
        .get_for_agent(worker, &tasklist.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        final_tl.status,
        TasklistStatus::Failed,
        "tasklist must be Failed after cap"
    );

    let task = &final_tl.groups[0].tasks[0];
    assert_eq!(
        task.status,
        TaskStatus::Failed,
        "task must be Failed after cap"
    );
    assert_eq!(
        task.attempt_count,
        ao_engine::task_feeder::DEFAULT_MAX_ATTEMPTS,
        "attempt_count must equal the retry cap"
    );
    assert_eq!(
        task.error_log.len(),
        cap,
        "one error_log entry per stale attempt"
    );
    assert!(
        task.error_log.iter().all(|e| e.contains("agent run ended")),
        "each error_log entry must describe the stale run"
    );
}
