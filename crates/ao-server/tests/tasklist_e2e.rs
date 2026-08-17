//! End-to-end integration test for the tasklist runtime's failure path: a
//! task declares an expected output that is never written, so the feeder
//! reprompts to the attempt cap, fails the task, and halts the tasklist —
//! proving halt-on-failure prevents the downstream group from dispatching.
//!
//! This exercises the full pipeline:
//!   TaskFeeder → TasklistQueueDispatcher → TasklistQueueManager → AgentRunner
//!     → MockProcessSupervisor → tag extraction → validate_and_complete
//!     → TasklistTaskUpdated / TasklistFailed SSE events
//!
//! The mock supervisor returns canned nested `<task action="complete">` tags
//! (with `<task-item-notification>` for the notification gate) per scenario.
//! The expected output is deliberately NOT written, which is what drives the
//! reprompt-then-fail behaviour under test.
//!
//! The happy path is covered for agent-owned tasklists in
//! `agent_tasklist_e2e.rs`; the team-owned variant that used to live here was
//! removed with the team subsystem.

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

// Serialize setup() across tests sharing the process-wide
// LAUNCHPAD_STUDIO_DATA_DIR env var.
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_agent_profile(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Agent {}", id),
        description: "tasklist e2e agent".to_string(),
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

/// Build a nested `<task action="complete">` tag with the required
/// `<task-item-notification>` block — the format the agent_runner's
/// tag extractor + notification gate recognize at TextComplete.
fn complete_scenario(task_id: &str) -> MockScenario {
    MockScenario {
        stdout_lines: vec![format!(
            "<task action=\"complete\" task_id=\"{}\"><task-item-notification><status>complete</status><summary>Task completed successfully.</summary></task-item-notification></task>",
            task_id
        )],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 0,
    }
}

async fn setup_state(scenarios: Vec<MockScenario>) -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let mock = MockProcessSupervisor::new(scenarios);
        Arc::new(AppState::new_with_mock(mock).await.expect("init state"))
    };
    (state, tmp)
}

/// Wait for an event matching `pred` on the event bus, with a 10s deadline.
async fn wait_for_event<F>(
    rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>,
    pred: F,
) -> AgentEvent
where
    F: Fn(&AgentEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(ev)) => {
                if pred(&ev) {
                    return ev;
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => panic!("event bus closed before predicate matched"),
            Err(_) => panic!("timed out waiting for event"),
        }
    }
}

/// Failure variant: a task in Group 1 declares an output the workspace will
/// never have. The feeder reprompts up to max_attempts (default 3) then fails
/// the task; the tasklist halts and downstream Group 2 never dispatches.
#[tokio::test]
async fn e2e_missing_output_halts_downstream_group() {
    let researcher = "researcher-fail";
    let downstream_owner = "downstream-fail";
    let task_bad = "task-bad";
    let task_downstream = "task-downstream";

    // 3 attempts at task_bad (default max_attempts) — same agent so QM
    // serializes them. No scenario for task_downstream because it must never
    // dispatch.
    let scenarios = vec![
        complete_scenario(task_bad),
        complete_scenario(task_bad),
        complete_scenario(task_bad),
    ];

    let (state, _tmp) = setup_state(scenarios).await;

    state
        .persistence
        .agents
        .create(&make_agent_profile(researcher))
        .await
        .unwrap();
    state
        .persistence
        .agents
        .create(&make_agent_profile(downstream_owner))
        .await
        .unwrap();
    // Created through the live service entry point, which persists the
    // tasklist and bootstraps dispatch. Group 1's task declares an output the
    // workspace will never have; Group 2 must never dispatch.
    let groups = vec![
        TaskGroup {
            id: "g1".to_string(),
            mode: TaskGroupMode::Seq,
            tasks: vec![Task {
                id: task_bad.to_string(),
                owner_agent_id: researcher.to_string(),
                prompt: "Pretend to research but never write the file".to_string(),
                expected_outputs: vec!["never-written.md".to_string()],
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
                    owner_agent_id: researcher.to_string(),
                    mode: AssignmentMode::Pinned,
                }),
                classifier_token: 0,
                dispatch_token: 0,
            }],
        },
        TaskGroup {
            id: "g2".to_string(),
            mode: TaskGroupMode::Seq,
            tasks: vec![Task {
                id: task_downstream.to_string(),
                owner_agent_id: downstream_owner.to_string(),
                prompt: "Should never dispatch".to_string(),
                expected_outputs: vec![],
                status: TaskStatus::Pending,
                group_id: "g2".to_string(),
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
                    owner_agent_id: downstream_owner.to_string(),
                    mode: AssignmentMode::Pinned,
                }),
                classifier_token: 0,
                dispatch_token: 0,
            }],
        },
    ];

    let mut rx = state.event_bus.subscribe();

    // Intentionally do NOT pre-write the expected output.
    let tasklist = state
        .tasklist_service
        .create(
            TasklistOwner::Agent {
                agent_id: researcher.to_string(),
            },
            "Failure path".to_string(),
            "Missing-output reprompt-then-fail".to_string(),
            groups,
            false,
        )
        .await
        .unwrap();
    let tasklist_id = tasklist.id.as_str();

    // Wait for the tasklist to halt with TasklistFailed.
    let failed = wait_for_event(&mut rx, |e| {
        matches!(
            &e.payload,
            AgentEventPayload::TasklistFailed { tasklist_id: tid, .. } if tid == tasklist_id
        )
    })
    .await;
    assert!(matches!(failed.payload, AgentEventPayload::TasklistFailed { .. }));

    // Settle: agent_runner finishes its final closure work after the failed
    // emit so the persisted task state observably catches up.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let final_state = state
        .persistence
        .tasklists
        .get_for_agent(researcher, tasklist_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_state.status, TasklistStatus::Failed);

    let bad_task = final_state
        .groups
        .iter()
        .flat_map(|g| g.tasks.iter())
        .find(|t| t.id == task_bad)
        .unwrap();
    assert_eq!(bad_task.status, TaskStatus::Failed);
    assert_eq!(bad_task.attempt_count, 3);
    assert!(
        bad_task.error_log.iter().all(|e| e.contains("never-written.md")),
        "every error log entry should name the missing file: {:?}",
        bad_task.error_log
    );

    // Downstream task must NEVER have dispatched.
    let downstream = final_state
        .groups
        .iter()
        .flat_map(|g| g.tasks.iter())
        .find(|t| t.id == task_downstream)
        .unwrap();
    assert_eq!(
        downstream.status,
        TaskStatus::Pending,
        "downstream task must remain Pending after halt"
    );
    assert_eq!(downstream.attempt_count, 0);
}
