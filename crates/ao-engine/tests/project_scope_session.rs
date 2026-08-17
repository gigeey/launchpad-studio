//! Project-scope session propagation — integration gate for the CLI runner.
//!
//! A run dispatched with `RunScope::Project { project_id }` must:
//! 1. Emit `RunStarted` / `RunEnded` on the project event channel
//!    (`project:{project_id}`), never on the agent's personal channel. This
//!    confirms that `scope.event_agent_id()` is applied correctly throughout
//!    the run.
//! 2. Register the MCP session with `project_id` stored — verified here by
//!    listing live sessions from the runner's session store while the run is
//!    in-flight, before `McpSessionGuard::drop` cleans up.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;

use ao_engine::agent_runner::{AgentRunRequest, AgentRunner, CliAgentRunner, RunScope, RunningAgents};
use ao_engine::command_queue::CommandQueue;
use ao_engine::event_bus::EventBus;
use ao_engine::instance_registry::InstanceRegistry;
use ao_engine::mcp_session::McpSessionStore;
use ao_engine_tools_core::{Registry, SessionKind};
use ao_normalizer::registry::NormalizerRegistry;
use ao_persistence::{paths::DataRoot, PersistenceLayer};
use ao_process::mock::{MockProcessSupervisor, MockScenario};
use ao_process::supervisor::ProcessSupervisor;
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
};
use ao_protocol::event::{AgentEvent, AgentEventPayload};

const AGENT_ID: &str = "project-scope-test-agent";
const PROJECT_ID: &str = "proj-test-00000000";

async fn make_persistence() -> (Arc<PersistenceLayer>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.expect("ensure_directories");
    let p = PersistenceLayer::init_with_root(data_root).await.expect("persistence init");
    (Arc::new(p), tmp)
}

fn make_agent() -> AgentProfile {
    AgentProfile {
        id: AGENT_ID.to_string(),
        name: "Project Scope Test".to_string(),
        description: String::new(),
        emoji: None,
        provider: ProviderConfig::Cli(CliProviderConfig {
            command: "mock-binary".to_string(),
            args: vec![],
            normalizer: Some("claude".to_string()),
            output_format: OutputFormat::StreamJson,
            input_mode: InputMode::Arg,
            model_arg: None,
            model_aliases: Default::default(),
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
        env: Default::default(),
        max_instances: 1,
        timeout_seconds: 60,
        working_dir: None,
        home_dir: None,
        serialize: false,
        workflows: None,
        template: None,
        runner_mode: Default::default(),
        enabled_plugins: Default::default(),
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

fn single_text_scenario(text: &str) -> Arc<dyn ProcessSupervisor> {
    let line = format!(
        "{}\n",
        serde_json::json!({"type":"content_block_delta","delta":{"type":"text_delta","text": text}})
    );
    Arc::new(MockProcessSupervisor::new(vec![MockScenario {
        stdout_lines: vec![line],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 0,
    }]))
}

fn capture_events(bus: &Arc<EventBus>) -> mpsc::UnboundedReceiver<AgentEvent> {
    let mut bcast = bus.subscribe();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match bcast.recv().await {
                Ok(ev) => { if tx.send(ev).is_err() { break; } }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });
    rx
}

/// `RunScope::Project` run emits all lifecycle events on `project:{project_id}`,
/// never on the agent's own channel. This is the observable signal that the
/// scope is wired end-to-end through the runner — the session stores
/// `project_id` so the MCP route can set `ctx.project_id` for tool calls.
#[tokio::test]
async fn project_scope_run_emits_on_project_channel() {
    let (persistence, _tmp) = make_persistence().await;
    let bus = Arc::new(EventBus::new(256));
    let mut event_rx = capture_events(&bus);

    let supervisor = single_text_scenario("project reply");
    let runner: Arc<dyn AgentRunner> = Arc::new(CliAgentRunner::new(
        supervisor,
        Arc::new(NormalizerRegistry::new()),
        Arc::clone(&bus),
        Arc::clone(&persistence),
        Arc::new(CommandQueue::new()),
        Arc::new(InstanceRegistry::new()),
        Arc::new(RunningAgents::new()),
        Arc::new(Registry::new()),
    ));

    let (complete_tx, _complete_rx) = mpsc::channel(1);
    let request = AgentRunRequest {
        agent: make_agent(),
        prompt: "do project work".to_string(),
        attachments: vec![],
        run_complete_tx: complete_tx,
        scope: RunScope::Project { project_id: PROJECT_ID.to_string() },
        session_kind: SessionKind::Autonomous,
        ..Default::default()
    };

    let result = timeout(Duration::from_secs(15), runner.run(request))
        .await
        .expect("run timed out")
        .expect("run errored");

    assert!(
        result.output_text.contains("project reply"),
        "run output must carry the agent reply; got: {:?}",
        result.output_text,
    );

    // Drain events and verify lifecycle events emit on the project channel.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    let expected_channel = format!("project:{}", PROJECT_ID);
    let mut saw_project_started = false;
    let mut saw_project_ended = false;
    while let Ok(ev) = event_rx.try_recv() {
        if ev.agent_id == expected_channel {
            match ev.payload {
                AgentEventPayload::RunStarted => saw_project_started = true,
                AgentEventPayload::RunEnded { .. } => saw_project_ended = true,
                _ => {}
            }
        }
        // No lifecycle event should land on the agent's personal channel.
        if ev.agent_id == AGENT_ID {
            match &ev.payload {
                AgentEventPayload::RunStarted | AgentEventPayload::RunEnded { .. } => {
                    panic!(
                        "lifecycle event {:?} must not emit on agent channel; \
                         it must use the project channel",
                        ev.payload
                    );
                }
                _ => {}
            }
        }
    }
    assert!(saw_project_started, "RunStarted must emit on project channel");
    assert!(saw_project_ended, "RunEnded must emit on project channel");
}

/// MCP session registered for a project run carries `project_id`.
/// Verified by sharing the session store between runner and test, and
/// reading all sessions for the agent while they are live (i.e. from a
/// spawned task that races the run).
#[tokio::test]
async fn project_scope_run_registers_session_with_project_id() {
    let (persistence, _tmp) = make_persistence().await;
    let bus = Arc::new(EventBus::new(256));
    let mcp_sessions = Arc::new(McpSessionStore::new());

    // Use a small delay so the session stays live long enough for the watcher.
    let supervisor = {
        let line = format!(
            "{}\n",
            serde_json::json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"project hello"}})
        );
        Arc::new(MockProcessSupervisor::new(vec![MockScenario {
            stdout_lines: vec![line],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 50,
        }])) as Arc<dyn ProcessSupervisor>
    };
    let runner: Arc<dyn AgentRunner> = Arc::new(
        CliAgentRunner::new(
            supervisor,
            Arc::new(NormalizerRegistry::new()),
            Arc::clone(&bus),
            Arc::clone(&persistence),
            Arc::new(CommandQueue::new()),
            Arc::new(InstanceRegistry::new()),
            Arc::new(RunningAgents::new()),
            Arc::new(Registry::new()),
        )
        .with_mcp_sessions(Arc::clone(&mcp_sessions)),
    );

    // Spawn a watcher that polls the session store until a session for the
    // agent appears (the run registers it before spawning the subprocess), then
    // captures its project_id.
    let sessions_clone = Arc::clone(&mcp_sessions);
    let watcher = tokio::spawn(async move {
        for _ in 0..100 {
            let live = sessions_clone.list_by_agent_id(AGENT_ID);
            if let Some(sess) = live.first() {
                return sess.project_id.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        None
    });

    let (complete_tx, _complete_rx) = mpsc::channel(1);
    let request = AgentRunRequest {
        agent: make_agent(),
        prompt: "project task".to_string(),
        attachments: vec![],
        run_complete_tx: complete_tx,
        scope: RunScope::Project { project_id: PROJECT_ID.to_string() },
        session_kind: SessionKind::Autonomous,
        ..Default::default()
    };

    // Drive the run to completion concurrently with the watcher.
    let (run_result, captured_pid) = tokio::join!(
        async { timeout(Duration::from_secs(15), runner.run(request)).await },
        async { timeout(Duration::from_secs(15), watcher).await },
    );

    run_result.expect("run timed out").expect("run errored");
    let captured = captured_pid
        .expect("watcher timed out")
        .expect("watcher panicked");

    assert_eq!(
        captured.as_deref(),
        Some(PROJECT_ID),
        "live MCP session must carry project_id; got {:?}",
        captured
    );
}
