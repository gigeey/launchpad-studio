//! CLI-runner delegate isolation — parity gate with the native runner.
//!
//! A delegated child run (isolate_history + transcript_override + event_channel)
//! must:
//! 1. leave the agent's personal transcript untouched — for clone-parent
//!    delegates the child's agent_id IS the parent's, so an ungated persistence
//!    attach would splice child turns into the parent's chat history;
//! 2. write its turn-by-turn transcript to the override file (the delegate
//!    sidechain JSONL), not just the terminal events;
//! 3. emit every live bus event on the hidden delegate channel — never on the
//!    agent's own channel, where it would stream into the parent's chat feed.
//!
//! The floor case (isolate_history without an override) must skip persistence
//! entirely rather than fall back to the personal file.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;

use ao_engine::agent_runner::{
    AgentRunRequest, AgentRunner, CliAgentRunner, RunScope, RunningAgents,
};
use ao_engine::command_queue::CommandQueue;
use ao_engine::event_bus::EventBus;
use ao_engine::instance_registry::InstanceRegistry;
use ao_engine_tools_core::{Registry, SessionKind};
use ao_normalizer::registry::NormalizerRegistry;
use ao_persistence::{paths::DataRoot, PersistenceLayer};
use ao_process::mock::{MockProcessSupervisor, MockScenario};
use ao_process::supervisor::ProcessSupervisor;
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
};
use ao_protocol::event::AgentEvent;

const AGENT_ID: &str = "delegated-cli-agent";

/// Build a `PersistenceLayer` backed by a fresh temporary directory.
/// The returned `TempDir` must stay alive for the duration of the test.
async fn make_persistence() -> (Arc<PersistenceLayer>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.expect("ensure_directories");
    let p = PersistenceLayer::init_with_root(data_root).await.expect("persistence init");
    (Arc::new(p), tmp)
}

/// Construct a `CliAgentRunner` as a trait object so `runner.run(request)`
/// resolves to the `AgentRunner` trait method.
fn make_runner(
    supervisor: Arc<dyn ProcessSupervisor>,
    bus: Arc<EventBus>,
    persistence: Arc<PersistenceLayer>,
) -> Arc<dyn AgentRunner> {
    Arc::new(CliAgentRunner::new(
        supervisor,
        Arc::new(NormalizerRegistry::new()),
        bus,
        persistence,
        Arc::new(CommandQueue::new()),
        Arc::new(InstanceRegistry::new()),
        Arc::new(RunningAgents::new()),
        Arc::new(Registry::new()),
    ))
}

/// Agent profile selecting `ClaudeNormalizer` (stream-json) over a mock binary.
fn make_agent() -> AgentProfile {
    AgentProfile {
        id: AGENT_ID.to_string(),
        name: "Delegate Isolation Test".to_string(),
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

/// Forward every broadcast event into an unbounded mpsc for post-run draining.
fn capture_events(bus: &Arc<EventBus>) -> mpsc::UnboundedReceiver<AgentEvent> {
    let mut bcast = bus.subscribe();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match bcast.recv().await {
                Ok(ev) => {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });
    rx
}

/// Stream-json line terminated with `\n` for `ClaudeNormalizer`.
fn sjl(json_str: &str) -> String {
    format!("{}\n", json_str)
}

fn single_text_scenario(text: &str) -> Arc<dyn ProcessSupervisor> {
    let line = sjl(&format!(
        r#"{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"{}"}}}}"#,
        text
    ));
    Arc::new(MockProcessSupervisor::new(vec![MockScenario {
        stdout_lines: vec![line],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 0,
    }]))
}

/// Delegated run with override + channel: transcript goes to the sidechain
/// file, events ride the delegate channel, personal transcript stays clean.
#[tokio::test]
async fn isolated_cli_run_routes_transcript_and_events_to_sidechain() {
    let (persistence, _tmp) = make_persistence().await;
    let bus = Arc::new(EventBus::new(256));
    let mut event_rx = capture_events(&bus);

    let supervisor = single_text_scenario("cli child says hi");
    let runner = make_runner(supervisor, Arc::clone(&bus), Arc::clone(&persistence));

    let override_path = persistence
        .data_root
        .root()
        .join("messages")
        .join("data")
        .join("bg-cli-test.jsonl");

    let (complete_tx, _complete_rx) = mpsc::channel(1);
    let request = AgentRunRequest {
        agent: make_agent(),
        prompt: "do the delegated thing".to_string(),
        attachments: vec![],
        run_complete_tx: complete_tx,
        focus_path: None,
        scope: RunScope::Standalone,
        thread_id: None,
        session_kind: SessionKind::Autonomous,
        pre_registered_run_id: None,
        isolate_history: true,
        transcript_override: Some(override_path.clone()),
        event_channel: Some("delegate:bg-cli-test".to_string()),
        ..Default::default()
    };

    let result = timeout(Duration::from_secs(15), runner.run(request))
        .await
        .expect("run timed out")
        .expect("run errored");
    assert!(
        result.output_text.contains("cli child says hi"),
        "run output must carry the child's text; got: {:?}",
        result.output_text,
    );

    // Personal transcript must stay empty — the whole point of the isolation.
    let personal = persistence
        .transcripts
        .read_recent(AGENT_ID, 10)
        .await
        .unwrap_or_default();
    assert!(
        personal.is_empty(),
        "personal transcript must not receive child entries; got {} entries",
        personal.len()
    );

    // The override file must hold the child's response (rich sidechain record).
    let sidechain = persistence
        .transcripts
        .read_recent_for_run(AGENT_ID, Some(override_path.as_path()), 10)
        .await
        .expect("read sidechain transcript");
    assert!(
        sidechain.iter().any(|e| e.content.contains("cli child says hi")),
        "sidechain transcript must contain the child's response; got {} entries",
        sidechain.len()
    );

    // Every live event must ride the delegate channel — none on the agent's own.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    let mut saw_delegate_channel = false;
    while let Ok(ev) = event_rx.try_recv() {
        assert_ne!(
            ev.agent_id, AGENT_ID,
            "no live event may emit on the agent's own channel (payload: {:?})",
            ev.payload
        );
        if ev.agent_id == "delegate:bg-cli-test" {
            saw_delegate_channel = true;
        }
    }
    assert!(saw_delegate_channel, "events must emit on the delegate channel");
}

/// Floor case: isolate_history with no override must skip persistence rather
/// than fall back to the personal transcript file.
#[tokio::test]
async fn isolated_cli_run_without_override_skips_personal_transcript() {
    let (persistence, _tmp) = make_persistence().await;
    let bus = Arc::new(EventBus::new(256));

    let supervisor = single_text_scenario("quiet cli child");
    let runner = make_runner(supervisor, Arc::clone(&bus), Arc::clone(&persistence));

    let (complete_tx, _complete_rx) = mpsc::channel(1);
    let request = AgentRunRequest {
        agent: make_agent(),
        prompt: "do the delegated thing".to_string(),
        attachments: vec![],
        run_complete_tx: complete_tx,
        focus_path: None,
        scope: RunScope::Standalone,
        thread_id: None,
        session_kind: SessionKind::Autonomous,
        pre_registered_run_id: None,
        isolate_history: true,
        ..Default::default()
    };

    let result = timeout(Duration::from_secs(15), runner.run(request))
        .await
        .expect("run timed out")
        .expect("run errored");
    assert!(
        result.output_text.contains("quiet cli child"),
        "run output must carry the child's text; got: {:?}",
        result.output_text,
    );

    let personal = persistence
        .transcripts
        .read_recent(AGENT_ID, 10)
        .await
        .unwrap_or_default();
    assert!(
        personal.is_empty(),
        "isolated run without override must not write to the personal transcript; got {} entries",
        personal.len()
    );
}
