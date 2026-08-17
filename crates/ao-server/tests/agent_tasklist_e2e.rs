//! End-to-end integration tests for the agent-owned tasklist lifecycle.
//!
//! Covers three scenarios:
//! (1) Leaf agent creates 3-item SEQ tasklist → all items auto-route to self via
//!     the leaf-agent fast path → dispatcher drives to completion → completion-
//!     summary message posted to the owning agent's queue.
//! (2) Agent with two delegates creates 3-item tasklist → classifier routes items
//!     across the two delegates (alternating) → all items complete → summary fires.
//! (3) User message sent while tasklist is mid-flight → effective max_instances is
//!     bumped to at least 2, confirming both runs can spawn concurrently.
//!
//! All three tests use the same `setup_state` helper (MockProcessSupervisor +
//! a tempdir) that mirrors the pattern in `tasklist_e2e.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ao_engine::AppState;
use ao_process::mock::{MockProcessSupervisor, MockScenario};
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, DelegateTarget, InputMode, OutputFormat, ProviderConfig,
};
use ao_protocol::event::{AgentEvent, AgentEventPayload};
use ao_protocol::tasklist::{
    Task, TaskGroup, TaskGroupMode, TaskStatus, TasklistOwner, TasklistStatus,
};

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_agent_profile(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Agent {}", id),
        description: "agent tasklist e2e agent".to_string(),
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
        max_instances: 2,
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

fn make_agent_with_delegates(id: &str, delegates: &[&str]) -> AgentProfile {
    let mut profile = make_agent_profile(id);
    profile.delegates_to = delegates
        .iter()
        .map(|&d| DelegateTarget {
            target_agent_id: d.to_string(),
            name: d.to_string(),
            purpose: format!("{} handles delegated tasks", d),
            share_context_allowed: false,
        })
        .collect();
    profile
}

/// MockScenario emitting a nested `<task action="complete">` tag with the required
/// `<task-item-notification>` block — passes the notification gate so the
/// task transitions to Completed rather than triggering an auto-reprompt.
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

/// MockScenario returning a `<task_owner>agent_id</task_owner>` classifier
/// response — consumed by `one_shot_classify` inside `AgentRoutingQueueManager`.
fn classify_scenario(chosen_agent_id: &str) -> MockScenario {
    MockScenario {
        stdout_lines: vec![format!(
            "I assign this task to {}. <task_owner>{}</task_owner>",
            chosen_agent_id, chosen_agent_id
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

fn unowned_task(id: &str, prompt: &str, group_id: &str) -> Task {
    Task {
        id: id.to_string(),
        owner_agent_id: String::new(), // empty → routing will self-assign or classify
        prompt: prompt.to_string(),
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

/// Collect all events until the agent-owned tasklist reaches a terminal state.
async fn collect_agent_tasklist_terminal(
    rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>,
    agent_id: &str,
    tasklist_id: &str,
) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(ev)) => {
                let is_terminal = match &ev.payload {
                    AgentEventPayload::TasklistCompleted {
                        tasklist_id: tid,
                        owner: Some(TasklistOwner::Agent { agent_id: aid }),
                        ..
                    } => tid == tasklist_id && aid == agent_id,
                    AgentEventPayload::TasklistFailed {
                        tasklist_id: tid,
                        owner: Some(TasklistOwner::Agent { agent_id: aid }),
                        ..
                    } => tid == tasklist_id && aid == agent_id,
                    _ => false,
                };
                events.push(ev);
                if is_terminal {
                    return events;
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => panic!("event bus closed before agent tasklist terminal event"),
            Err(_) => panic!(
                "timed out waiting for agent tasklist terminal event; collected {} events",
                events.len()
            ),
        }
    }
}

// ─── Test 1 ───────────────────────────────────────────────────────────────────

/// Leaf agent (no delegates) creates a 3-item SEQ tasklist with unowned tasks.
/// The routing fast-path stamps owner_agent_id = self for each task without any
/// LLM call.  All three tasks complete via MockProcessSupervisor, transitioning
/// the list to Completed.  The completion-summary follow-up is posted to the
/// agent's queue.
#[tokio::test]
async fn e2e_leaf_agent_seq_completes_and_posts_summary() {
    let agent_id = "leaf-e2e-agent";
    let task_1 = "leaf-task-1";
    let task_2 = "leaf-task-2";
    let task_3 = "leaf-task-3";

    // 3 dispatch scenarios (SEQ: one at a time); no routing scenarios because the
    // leaf-agent fast-path stamps owner without an LLM call.
    let scenarios = vec![
        complete_scenario(task_1),
        complete_scenario(task_2),
        complete_scenario(task_3),
    ];

    let (state, _tmp) = setup_state(scenarios).await;

    state
        .persistence
        .agents
        .create(&make_agent_profile(agent_id))
        .await
        .unwrap();

    let mut rx = state.event_bus.subscribe();

    let groups = vec![TaskGroup {
        id: "g1".to_string(),
        mode: TaskGroupMode::Seq,
        tasks: vec![
            unowned_task(task_1, "Leaf task 1", "g1"),
            unowned_task(task_2, "Leaf task 2", "g1"),
            unowned_task(task_3, "Leaf task 3", "g1"),
        ],
    }];

    let tasklist = state
        .tasklist_service
        .create(
            TasklistOwner::Agent { agent_id: agent_id.to_string() },
            "Leaf agent E2E".to_string(),
            "Three-item SEQ tasklist for leaf-agent routing test".to_string(),
            groups,
            false,
        )
        .await
        .unwrap();

    // Wait for all tasks to complete and the list to reach Completed.
    let events =
        collect_agent_tasklist_terminal(&mut rx, agent_id, &tasklist.id).await;

    // --- Assert tasklist reached Completed ----------------------------------
    let final_state = state
        .persistence
        .tasklists
        .get_for_agent(agent_id, &tasklist.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        final_state.status,
        TasklistStatus::Completed,
        "tasklist should be Completed"
    );
    for group in &final_state.groups {
        for task in &group.tasks {
            assert_eq!(
                task.status,
                TaskStatus::Completed,
                "task {} should be Completed, got {:?}",
                task.id,
                task.status
            );
            // Leaf-agent routing assigned owner_agent_id = agent_id for each task.
            assert_eq!(
                task.owner_agent_id, agent_id,
                "leaf-agent routing should have stamped owner_agent_id = {}",
                agent_id
            );
        }
    }

    // --- Assert the terminal event was TasklistCompleted (not Failed) --------
    let completed_count = events
        .iter()
        .filter(|e| {
            matches!(
                &e.payload,
                AgentEventPayload::TasklistCompleted {
                    tasklist_id: tid,
                    owner: Some(TasklistOwner::Agent { agent_id: aid }),
                    ..
                } if tid == &tasklist.id && aid == agent_id
            )
        })
        .count();
    assert_eq!(
        completed_count, 1,
        "expected exactly one TasklistCompleted event"
    );

    // --- Assert completion-summary dispatched to the agent's queue ---
    // post_completion_summary submits the message synchronously before
    // TasklistCompleted fires; the queue manager dispatches it immediately
    // (can_spawn=true, no ongoing personal runs), so depth briefly peaks at 1
    // before dropping back to 0 — checking depth > 0 is racy.  Instead, wait
    // for the MessageProcessingStarted event the queue manager emits just
    // before kicking off the dispatch run.
    let summary_dispatched = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if matches!(
                        &ev.payload,
                        AgentEventPayload::MessageProcessingStarted { .. }
                    ) {
                        return true;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        summary_dispatched,
        "expected completion-summary dispatch (MessageProcessingStarted not seen)"
    );
}

// ─── Test 2 ───────────────────────────────────────────────────────────────────

/// Agent with two delegates creates a 3-item SEQ tasklist with unowned tasks.
/// The classifier is invoked for each task (via MockProcessSupervisor) and
/// routes them across the two delegates (task-1 → delegate-a, task-2 → delegate-b,
/// task-3 → delegate-a).  After all three dispatch runs complete the list
/// transitions to Completed and a summary is queued for the owning agent.
///
/// Mock scenario ordering for SEQ mode (deterministic):
///   classify(task-1), dispatch(task-1), classify(task-2), dispatch(task-2),
///   classify(task-3), dispatch(task-3).
#[tokio::test]
async fn e2e_delegate_routed_seq_completes_and_posts_summary() {
    let coord_id = "del-coord-e2e";
    let delegate_a = "del-agent-a-e2e";
    let delegate_b = "del-agent-b-e2e";
    let task_1 = "del-task-1";
    let task_2 = "del-task-2";
    let task_3 = "del-task-3";

    // SEQ ordering: classify then dispatch for each task in turn.
    let scenarios = vec![
        classify_scenario(delegate_a), // routes task-1 to delegate-a
        complete_scenario(task_1),     // delegate-a completes task-1
        classify_scenario(delegate_b), // routes task-2 to delegate-b
        complete_scenario(task_2),     // delegate-b completes task-2
        classify_scenario(delegate_a), // routes task-3 to delegate-a
        complete_scenario(task_3),     // delegate-a completes task-3
    ];

    let (state, _tmp) = setup_state(scenarios).await;

    // All three agents must exist in persistence for routing and dispatch.
    state
        .persistence
        .agents
        .create(&make_agent_with_delegates(coord_id, &[delegate_a, delegate_b]))
        .await
        .unwrap();
    state
        .persistence
        .agents
        .create(&make_agent_profile(delegate_a))
        .await
        .unwrap();
    state
        .persistence
        .agents
        .create(&make_agent_profile(delegate_b))
        .await
        .unwrap();

    let mut rx = state.event_bus.subscribe();

    let groups = vec![TaskGroup {
        id: "g1".to_string(),
        mode: TaskGroupMode::Seq,
        tasks: vec![
            unowned_task(task_1, "Research signal A", "g1"),
            unowned_task(task_2, "Analyse findings", "g1"),
            unowned_task(task_3, "Write summary", "g1"),
        ],
    }];

    let tasklist = state
        .tasklist_service
        .create(
            TasklistOwner::Agent { agent_id: coord_id.to_string() },
            "Delegate routing E2E".to_string(),
            "Three-item SEQ tasklist routed across two delegates".to_string(),
            groups,
            false,
        )
        .await
        .unwrap();

    let events =
        collect_agent_tasklist_terminal(&mut rx, coord_id, &tasklist.id).await;

    // --- Assert tasklist Completed ------------------------------------------
    let final_state = state
        .persistence
        .tasklists
        .get_for_agent(coord_id, &tasklist.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_state.status, TasklistStatus::Completed);

    let tasks: Vec<_> = final_state.groups.iter().flat_map(|g| g.tasks.iter()).collect();
    assert_eq!(tasks.len(), 3);

    // Verify the classifier routed each task to the right delegate.
    let owners: Vec<&str> = tasks.iter().map(|t| t.owner_agent_id.as_str()).collect();
    assert_eq!(owners, vec![delegate_a, delegate_b, delegate_a]);

    for task in &tasks {
        assert_eq!(
            task.status,
            TaskStatus::Completed,
            "task {} should be Completed",
            task.id
        );
    }

    // --- Assert terminal event was Completed (not Failed) -------------------
    assert!(
        events.iter().any(|e| matches!(
            &e.payload,
            AgentEventPayload::TasklistCompleted {
                tasklist_id: tid,
                owner: Some(TasklistOwner::Agent { agent_id: aid }),
                ..
            } if tid == &tasklist.id && aid == coord_id
        )),
        "expected TasklistCompleted event for coord agent"
    );

    // --- Assert completion-summary dispatched -------------------------------
    // Same rationale as in test 1: queue depth is 0 by the time we read it
    // because the queue manager dispatches immediately.  Wait for the
    // MessageProcessingStarted event emitted just before dispatch instead.
    let summary_dispatched = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if matches!(
                        &ev.payload,
                        AgentEventPayload::MessageProcessingStarted { .. }
                    ) {
                        return true;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        summary_dispatched,
        "expected completion-summary dispatch (MessageProcessingStarted not seen)"
    );
}

// ─── Test 3 ───────────────────────────────────────────────────────────────────

/// Confirms that while an agent-owned tasklist is active, the effective
/// max_instances is bumped to max(configured_max, 2) by the InstanceRegistry,
/// so a user-message run can spawn concurrently with the tasklist dispatch
/// run even when configured max_instances = 1.
///
/// This test exercises the InstanceRegistry API directly rather than dispatching
/// real runs: the "mid-flight" state is simulated by registering synthetic run IDs
/// and verifying `can_spawn` returns the expected boolean at each step.
#[tokio::test]
async fn e2e_parallel_chat_allowed_while_tasklist_active() {
    let agent_id = "parallel-cap-e2e";

    // No mock scenarios needed — this test does not dispatch actual task runs.
    let (state, _tmp) = setup_state(vec![]).await;

    let agent_str = agent_id.to_string();

    // Baseline: no tasklist, no runs → can spawn the first run.
    assert!(
        state.instance_registry.can_spawn(&agent_str, 1).await,
        "should be able to spawn first run when no existing runs and no active tasklist"
    );

    // Simulate the tasklist dispatch occupying the sole configured slot.
    state
        .instance_registry
        .register_run(&agent_str, "tasklist-dispatch-run")
        .await;
    assert!(
        !state.instance_registry.can_spawn(&agent_str, 1).await,
        "second spawn should be blocked (running=1 >= configured_max=1) without active tasklist"
    );

    // Mark agent as having an active tasklist → effective cap bumps to max(1,2)=2.
    state
        .instance_registry
        .mark_has_active_tasklist(&agent_str)
        .await;
    assert!(
        state.instance_registry.can_spawn(&agent_str, 1).await,
        "user-chat run should be allowed once active tasklist bumps effective cap to 2"
    );

    // Both slots occupied → third spawn blocked even with active tasklist.
    state
        .instance_registry
        .register_run(&agent_str, "user-chat-run")
        .await;
    assert!(
        !state.instance_registry.can_spawn(&agent_str, 1).await,
        "third spawn must be blocked at effective cap of 2"
    );

    // Tasklist completes → cap drops back to configured_max=1; user-chat run still occupies it.
    state
        .instance_registry
        .clear_has_active_tasklist(&agent_str)
        .await;
    state
        .instance_registry
        .unregister_run(&agent_str, "tasklist-dispatch-run")
        .await;
    assert!(
        !state.instance_registry.can_spawn(&agent_str, 1).await,
        "after tasklist clears, configured_max=1 applies; user-chat run still occupies the slot"
    );

    // After user-chat run also finishes → back to idle, can spawn again.
    state
        .instance_registry
        .unregister_run(&agent_str, "user-chat-run")
        .await;
    assert!(
        state.instance_registry.can_spawn(&agent_str, 1).await,
        "should be able to spawn again once all runs are unregistered and no active tasklist"
    );
}
