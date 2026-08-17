//! Unit tests for the `ao-engine` crate root.
//!
//! Declared from `lib.rs` as `#[cfg(test)] mod tests;` — `tests.rs` is the
//! same module as the inline `mod tests` block it replaces, so private items
//! of the crate root remain in scope here via `use super::*`.

use super::*;
use ao_protocol::event::AgentEventPayload;

// === EventBus tests ===

#[tokio::test]
async fn test_event_bus_agent_sink_brief_emits_system_message() {
    use ao_engine_tools_core::context::UserEvent;
    use ao_protocol::event::AgentEventPayload;

    let bus = std::sync::Arc::new(event_bus::EventBus::new(64));
    let agent_id = "sink-agent".to_string();
    let sink = event_bus::EventBusAgentSink {
        bus: std::sync::Arc::clone(&bus),
        agent_id: agent_id.clone(),
        thread_id: None,
    };
    let mut rx = bus.subscribe();

    let sink_arc: std::sync::Arc<dyn ao_engine_tools_core::context::EventSink + Send + Sync> =
        std::sync::Arc::new(sink);
    sink_arc
        .emit(UserEvent::Brief {
            content: "hello from tool".to_string(),
        })
        .await
        .expect("emit should succeed");

    let event = rx.recv().await.expect("should receive event");
    assert_eq!(event.agent_id, agent_id);
    match event.payload {
        AgentEventPayload::SystemMessage { text, .. } => {
            assert_eq!(text, "hello from tool");
        }
        other => panic!("expected SystemMessage, got {:?}", other),
    }
}

/// A sink constructed for a non-default-thread run must stamp every
/// forwarded event with that run's thread_id, so the invoking thread
/// (not just the agent-wide rollup) sees the event.
#[tokio::test]
async fn test_event_bus_agent_sink_stamps_configured_thread_id() {
    use ao_engine_tools_core::context::UserEvent;

    let bus = std::sync::Arc::new(event_bus::EventBus::new(64));
    let agent_id = "sink-agent-thread".to_string();
    let sink = event_bus::EventBusAgentSink {
        bus: std::sync::Arc::clone(&bus),
        agent_id: agent_id.clone(),
        thread_id: Some("thread-xyz".to_string()),
    };
    let mut rx = bus.subscribe();

    let sink_arc: std::sync::Arc<dyn ao_engine_tools_core::context::EventSink + Send + Sync> =
        std::sync::Arc::new(sink);
    sink_arc
        .emit(UserEvent::Brief {
            content: "hello from a named thread".to_string(),
        })
        .await
        .expect("emit should succeed");

    let event = rx.recv().await.expect("should receive event");
    assert_eq!(event.thread_id.as_deref(), Some("thread-xyz"));
}

/// A sink constructed for a default-thread run (no thread_id configured)
/// must keep stamping forwarded events with `None`, matching the
/// pre-existing default-thread convention.
#[tokio::test]
async fn test_event_bus_agent_sink_default_thread_stamps_none() {
    use ao_engine_tools_core::context::UserEvent;

    let bus = std::sync::Arc::new(event_bus::EventBus::new(64));
    let agent_id = "sink-agent-default-thread".to_string();
    let sink = event_bus::EventBusAgentSink {
        bus: std::sync::Arc::clone(&bus),
        agent_id: agent_id.clone(),
        thread_id: None,
    };
    let mut rx = bus.subscribe();

    let sink_arc: std::sync::Arc<dyn ao_engine_tools_core::context::EventSink + Send + Sync> =
        std::sync::Arc::new(sink);
    sink_arc
        .emit(UserEvent::Brief {
            content: "hello from the default thread".to_string(),
        })
        .await
        .expect("emit should succeed");

    let event = rx.recv().await.expect("should receive event");
    assert!(event.thread_id.is_none());
}

#[tokio::test]
async fn test_event_bus_agent_sink_non_brief_discarded() {
    use ao_engine_tools_core::context::UserEvent;
    use std::path::PathBuf;

    let bus = std::sync::Arc::new(event_bus::EventBus::new(64));
    let agent_id = "sink-agent-2".to_string();
    let sink = event_bus::EventBusAgentSink {
        bus: std::sync::Arc::clone(&bus),
        agent_id: agent_id.clone(),
        thread_id: None,
    };
    let sink_arc: std::sync::Arc<dyn ao_engine_tools_core::context::EventSink + Send + Sync> =
        std::sync::Arc::new(sink);

    sink_arc
        .emit(UserEvent::TodosUpdated {
            count: 3,
            in_progress: 1,
            pending: 1,
            completed: 1,
        })
        .await
        .expect("emit should succeed without panic");

    sink_arc
        .emit(UserEvent::PlanArtifact {
            plan_path: PathBuf::from("/tmp/plan.md"),
        })
        .await
        .expect("emit should succeed without panic");

    // No events should have been sent to the bus
    let mut rx = bus.subscribe();
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        rx.recv(),
    )
    .await;
    assert!(result.is_err(), "no events should be broadcast for non-Brief variants");
}

#[tokio::test]
async fn test_event_bus_seq_monotonic() {
    let bus = event_bus::EventBus::new(64);
    let mut rx = bus.subscribe();

    let run_id = "run-1";
    let agent_id = "agent-1".to_string();

    bus.emit(run_id, &agent_id, None, AgentEventPayload::RunStarted)
        .await;
    bus.emit(
        run_id,
        &agent_id,
        None,
        AgentEventPayload::TextDelta {
            text: "hello".to_string(),
        },
    )
    .await;
    bus.emit(
        run_id,
        &agent_id,
        None,
        AgentEventPayload::TextComplete {
            text: "hello".to_string(),
        },
    )
    .await;

    let e1 = rx.recv().await.unwrap();
    let e2 = rx.recv().await.unwrap();
    let e3 = rx.recv().await.unwrap();

    assert_eq!(e1.seq, 0);
    assert_eq!(e2.seq, 1);
    assert_eq!(e3.seq, 2);
    assert_eq!(e1.run_id, run_id);
    assert_eq!(e1.agent_id, agent_id);
}

#[tokio::test]
async fn test_event_bus_independent_run_seqs() {
    let bus = event_bus::EventBus::new(64);
    let mut rx = bus.subscribe();

    let agent_id = "agent-1".to_string();

    bus.emit("run-a", &agent_id, None, AgentEventPayload::RunStarted)
        .await;
    bus.emit("run-b", &agent_id, None, AgentEventPayload::RunStarted)
        .await;
    bus.emit(
        "run-a",
        &agent_id,
        None,
        AgentEventPayload::TextDelta {
            text: "a".to_string(),
        },
    )
    .await;
    bus.emit(
        "run-b",
        &agent_id,
        None,
        AgentEventPayload::TextDelta {
            text: "b".to_string(),
        },
    )
    .await;

    let e1 = rx.recv().await.unwrap(); // run-a seq 0
    let e2 = rx.recv().await.unwrap(); // run-b seq 0
    let e3 = rx.recv().await.unwrap(); // run-a seq 1
    let e4 = rx.recv().await.unwrap(); // run-b seq 1

    assert_eq!(e1.seq, 0);
    assert_eq!(e1.run_id, "run-a");
    assert_eq!(e2.seq, 0);
    assert_eq!(e2.run_id, "run-b");
    assert_eq!(e3.seq, 1);
    assert_eq!(e3.run_id, "run-a");
    assert_eq!(e4.seq, 1);
    assert_eq!(e4.run_id, "run-b");
}

#[tokio::test]
async fn test_event_bus_cleanup_run() {
    let bus = event_bus::EventBus::new(64);

    bus.emit(
        "run-x",
        &"agent-1".to_string(),
        None,
        AgentEventPayload::RunStarted,
    )
    .await;

    bus.cleanup_run("run-x").await;

    // After cleanup, emitting for same run_id starts seq at 0 again
    let mut rx = bus.subscribe();
    bus.emit(
        "run-x",
        &"agent-1".to_string(),
        None,
        AgentEventPayload::RunStarted,
    )
    .await;
    let event = rx.recv().await.unwrap();
    assert_eq!(event.seq, 0);
}

// === InstanceRegistry tests ===

#[tokio::test]
async fn test_instance_registry_register_and_count() {
    let registry = instance_registry::InstanceRegistry::new();
    let agent_id = "agent-1".to_string();

    assert_eq!(registry.running_count(&agent_id).await, 0);

    registry.register_run(&agent_id, "run-1").await;
    assert_eq!(registry.running_count(&agent_id).await, 1);

    registry.register_run(&agent_id, "run-2").await;
    assert_eq!(registry.running_count(&agent_id).await, 2);
}

#[tokio::test]
async fn test_instance_registry_can_spawn() {
    let registry = instance_registry::InstanceRegistry::new();
    let agent_id = "agent-1".to_string();

    assert!(registry.can_spawn(&agent_id, 3).await);

    registry.register_run(&agent_id, "run-1").await;
    registry.register_run(&agent_id, "run-2").await;
    assert!(registry.can_spawn(&agent_id, 3).await);
    assert!(!registry.can_spawn(&agent_id, 2).await);
}

#[tokio::test]
async fn test_instance_registry_unregister() {
    let registry = instance_registry::InstanceRegistry::new();
    let agent_id = "agent-1".to_string();

    registry.register_run(&agent_id, "run-1").await;
    registry.register_run(&agent_id, "run-2").await;
    assert_eq!(registry.running_count(&agent_id).await, 2);

    registry.unregister_run(&agent_id, "run-1").await;
    assert_eq!(registry.running_count(&agent_id).await, 1);

    registry.unregister_run(&agent_id, "run-2").await;
    assert_eq!(registry.running_count(&agent_id).await, 0);
}

// === CommandQueue tests ===

#[tokio::test]
async fn test_command_queue_acquire_permit() {
    let queue = command_queue::CommandQueue::new();
    let _permit = queue.acquire("lane-1", 1).await;
    // Permit acquired successfully
}

#[tokio::test]
async fn test_command_queue_blocks_when_full() {
    let queue = std::sync::Arc::new(command_queue::CommandQueue::new());

    // Acquire the only permit
    let permit1 = queue.acquire("lane-1", 1).await;

    // Second acquire should block — verify with timeout
    let queue_clone = queue.clone();
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        queue_clone.acquire("lane-1", 1),
    )
    .await;
    assert!(result.is_err(), "Second acquire should time out (blocked)");

    // Drop first permit, now second should succeed
    drop(permit1);
    let _permit2 = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        queue.acquire("lane-1", 1),
    )
    .await
    .expect("Should acquire after first permit dropped");
}

#[tokio::test]
async fn test_command_queue_independent_lanes() {
    let queue = command_queue::CommandQueue::new();

    // Acquire permits on different lanes simultaneously
    let _permit_a = queue.acquire("lane-a", 1).await;
    let _permit_b = queue.acquire("lane-b", 1).await;
    // Both succeed — lanes are independent
}

// === AgentRunner tests ===

use ao_normalizer::registry::NormalizerRegistry;
use ao_persistence::PersistenceLayer;
use ao_process::mock::{MockProcessSupervisor, MockScenario};
use ao_process::supervisor::ProcessSupervisor;
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
};
use ao_protocol::event::RunEndReason;
use std::collections::HashMap;

fn make_test_agent(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Test Agent {}", id),
        description: "A test agent".to_string(),
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

/// A `Cli` runner_mode profile resolves to a CLI-backed provider without
/// any `providers.toml` — the CLI path reads its configuration straight
/// from the profile, so the verifier shells out to the same binary the
/// coordinator uses.
#[test]
fn provider_client_for_profile_cli_yields_provider() {
    let agent = make_test_agent("cli-verify");
    assert!(matches!(
        agent.runner_mode,
        ao_protocol::agent::AgentRunnerMode::Cli
    ));
    assert!(
        super::provider_client_for_profile(&agent).is_some(),
        "Cli runner_mode must yield a CliProviderClient-backed provider"
    );
}

/// An `Api` runner_mode profile routes through `DefaultProviderFactory`,
/// which reads `providers.toml`. With a configured `[anthropic]` section the
/// factory builds successfully and the helper returns a provider.
#[tokio::test]
async fn provider_client_for_profile_api_uses_factory() {
    let _env_guard = crate::plugin_paths::tests::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
    std::fs::write(
        tmp.path().join("providers.toml"),
        "[anthropic]\napi_key = \"test-key\"\n",
    )
    .expect("write providers.toml");

    let mut agent = make_test_agent("api-verify");
    agent.runner_mode = ao_protocol::agent::AgentRunnerMode::Api;
    agent.native_provider = None;

    let provider = super::provider_client_for_profile(&agent);
    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");
    assert!(
        provider.is_some(),
        "Api runner_mode with a configured provider must yield a factory-backed provider"
    );
}

/// `build_full_verification_engine` with a `Cli` profile yields a
/// `CliInspectionVerifier` (the CLI inspection path). The returned engine
/// must not be `None` — no `providers.toml` is required for the Cli path.
#[test]
fn build_full_engine_cli_profile_yields_engine() {
    let agent = make_test_agent("full-cli");
    let registry = std::sync::Arc::new(ao_engine_tools_core::Registry::new());
    let engine = super::build_full_verification_engine(&agent, registry);
    assert!(
        engine.is_some(),
        "Cli profile must yield a CliInspectionVerifier engine (no providers.toml needed)"
    );
}

/// `build_full_verification_engine` with an `Api` profile yields a native
/// `InspectionVerifier` when `providers.toml` has an Anthropic key.
#[tokio::test]
async fn build_full_engine_api_profile_yields_engine() {
    let _env_guard = crate::plugin_paths::tests::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
    std::fs::write(
        tmp.path().join("providers.toml"),
        "[anthropic]\napi_key = \"test-key\"\n",
    )
    .expect("write providers.toml");

    let mut agent = make_test_agent("full-api");
    agent.runner_mode = ao_protocol::agent::AgentRunnerMode::Api;

    let registry = std::sync::Arc::new(ao_engine_tools_core::Registry::new());
    let engine = super::build_full_verification_engine(&agent, registry);
    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");
    assert!(
        engine.is_some(),
        "Api profile with a configured provider must yield an InspectionVerifier engine"
    );
}

/// Regression guard for a keychain-prompt incident: this crate's own test
/// binary is exactly the kind of process that used to reach the real OS
/// keychain from the two tests above — neither one sets
/// `LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK`, and until it was fixed there was
/// no keychain-avoidance for a `cargo test` binary at all. macOS keychain
/// ACLs are bound to a requesting binary's code signature, and a `cargo
/// test` harness binary gets a fresh signature (and a fresh
/// `target/*/deps/<name>-<hash>` name) every rebuild, so it could never
/// accumulate a durable "Always Allow" grant — every run would hit a GUI
/// prompt nobody is present to answer, hanging this test binary forever.
///
/// The fix lives entirely in `ao-engine-tools-provider-config::secret_vault`
/// (cross-crate test-harness auto-detection — see that module's doc
/// comment), not in this crate, so this test opens a vault the exact way
/// `DefaultProviderFactory::build` does, with every keychain-avoidance env
/// var left untouched, and asserts it never lands on the OS keychain
/// backend.
#[test]
fn secret_vault_never_selects_the_os_keychain_from_this_crates_test_binary() {
    let _env_guard = crate::plugin_paths::tests::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let vault = ao_engine_tools_provider_config::SecretVault::open().expect("open vault");
    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    assert!(
        !vault.is_keychain_backed(),
        "this crate's cargo-test binary must never be OS-keychain-backed, with or without a \
         test author remembering to force the file backend by hand"
    );
}

async fn setup_test_runner(
    scenarios: Vec<MockScenario>,
) -> (
    std::sync::Arc<agent_runner::CliAgentRunner>,
    std::sync::Arc<event_bus::EventBus>,
    std::sync::Arc<instance_registry::InstanceRegistry>,
    std::sync::Arc<PersistenceLayer>,
    tempfile::TempDir,
    std::sync::MutexGuard<'static, ()>,
) {
    let env_guard = crate::plugin_paths::tests::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let persistence =
        std::sync::Arc::new(PersistenceLayer::init().await.expect("init persistence"));
    let event_bus = std::sync::Arc::new(event_bus::EventBus::new(256));
    let mock_supervisor: std::sync::Arc<dyn ProcessSupervisor> =
        std::sync::Arc::new(MockProcessSupervisor::new(scenarios));
    let normalizer_registry = std::sync::Arc::new(NormalizerRegistry::new());
    let command_queue = std::sync::Arc::new(command_queue::CommandQueue::new());
    let instance_registry = std::sync::Arc::new(instance_registry::InstanceRegistry::new());

    let running_agents = std::sync::Arc::new(agent_runner::RunningAgents::new());
    let runner = std::sync::Arc::new(agent_runner::CliAgentRunner::new(
        mock_supervisor,
        normalizer_registry,
        std::sync::Arc::clone(&event_bus),
        std::sync::Arc::clone(&persistence),
        command_queue,
        std::sync::Arc::clone(&instance_registry),
        running_agents,
        std::sync::Arc::new(ao_engine_tools_core::Registry::new()),
    ));

    (runner, event_bus, instance_registry, persistence, tmp, env_guard)
}

#[test]
fn test_build_argv_basic() {
    let agent = make_test_agent("test-1");
    let argv = agent_runner::CliAgentRunner::build_argv(&agent, "hello world", None, None);
    assert_eq!(argv, vec!["echo", "hello world"]);
}

#[test]
fn test_build_argv_with_model_and_args() {
    let mut agent = make_test_agent("test-1");
    agent.model = Some("fast".to_string());
    let ProviderConfig::Cli(ref mut cli) = agent.provider;
    cli.command = "claude".to_string();
    cli.args = vec!["--output-format".to_string(), "json".to_string()];
    cli.model_arg = Some("--model".to_string());
    cli.model_aliases
        .insert("fast".to_string(), "claude-3-haiku".to_string());
    cli.system_prompt_arg = Some("--system-prompt".to_string());
    agent.system_prompt = Some("You are helpful.".to_string());

    let argv = agent_runner::CliAgentRunner::build_argv(&agent, "test prompt", None, None);
    assert_eq!(
        argv,
        vec![
            "claude",
            "--output-format",
            "json",
            "--model",
            "claude-3-haiku",
            "--system-prompt",
            "You are helpful.",
            "test prompt",
        ]
    );
}

#[test]
fn test_build_argv_stdin_mode_no_prompt_in_args() {
    let mut agent = make_test_agent("test-1");
    let ProviderConfig::Cli(ref mut cli) = agent.provider;
    cli.input_mode = InputMode::Stdin;
    let argv = agent_runner::CliAgentRunner::build_argv(&agent, "my prompt", None, None);
    // Stdin mode: prompt should NOT appear in argv
    assert_eq!(argv, vec!["echo"]);
}

/// Security guard: when the configured command is `echo` (or another
/// leak-prone utility), the system prompt MUST NOT be passed in argv —
/// the command would echo it back to the user as the agent's reply.
#[test]
fn test_build_argv_strips_system_prompt_for_echo_command() {
    // Case 1: echo + system_prompt_arg — neither the flag nor the prompt
    // text should appear.
    let mut agent = make_test_agent("leak-1");
    let ProviderConfig::Cli(ref mut cli) = agent.provider;
    cli.system_prompt_arg = Some("--system-prompt".to_string());
    agent.system_prompt = Some("SECRET INSTRUCTIONS DO NOT LEAK".to_string());
    let argv = agent_runner::CliAgentRunner::build_argv(&agent, "hi", None, None);
    assert_eq!(argv, vec!["echo", "hi"]);
    assert!(!argv.iter().any(|a| a.contains("SECRET")));
    assert!(!argv.iter().any(|a| a == "--system-prompt"));

    // Case 2: echo + no system_prompt_arg — the inline-prepend path that
    // wraps the prompt in `[System Instructions]...` must also be skipped.
    let mut agent2 = make_test_agent("leak-2");
    agent2.system_prompt = Some("SECRET INSTRUCTIONS DO NOT LEAK".to_string());
    let argv2 = agent_runner::CliAgentRunner::build_argv(&agent2, "hi", None, None);
    assert_eq!(argv2, vec!["echo", "hi"]);
    assert!(!argv2.iter().any(|a| a.contains("SECRET")));
    assert!(!argv2.iter().any(|a| a.contains("System Instructions")));

    // Case 3: absolute path to echo (`/bin/echo`) — basename match should
    // still catch it.
    let mut agent3 = make_test_agent("leak-3");
    let ProviderConfig::Cli(ref mut cli) = agent3.provider;
    cli.command = "/bin/echo".to_string();
    agent3.system_prompt = Some("SECRET".to_string());
    let argv3 = agent_runner::CliAgentRunner::build_argv(&agent3, "hi", None, None);
    assert!(!argv3.iter().any(|a| a.contains("SECRET")));

    // Case 4: a real CLI (`claude`) is unaffected — system prompt still
    // flows through normally.
    let mut agent4 = make_test_agent("safe");
    let ProviderConfig::Cli(ref mut cli) = agent4.provider;
    cli.command = "claude".to_string();
    cli.system_prompt_arg = Some("--system-prompt".to_string());
    agent4.system_prompt = Some("system text".to_string());
    let argv4 = agent_runner::CliAgentRunner::build_argv(&agent4, "hi", None, None);
    assert_eq!(
        argv4,
        vec!["claude", "--system-prompt", "system text", "hi"]
    );
}

/// `ThinkingConfig` on a `claude`-backed profile produces the bare CLI
/// flags the binary expects: `--thinking <mode>`, `--thinking-display
/// <display>`, and an optional `--max-thinking-tokens <N>`. The flags
/// must appear AFTER the model arg and BEFORE the system prompt so the
/// existing argv layout stays stable for callers that grep argv for
/// known prefixes.
#[test]
fn test_build_argv_emits_thinking_flags_for_claude() {
    use ao_protocol::agent::{ThinkingConfig, ThinkingDisplay, ThinkingMode};

    let mut agent = make_test_agent("thinker-1");
    let ProviderConfig::Cli(ref mut cli) = agent.provider;
    cli.command = "claude".to_string();
    cli.args = vec!["--output-format".to_string(), "stream-json".to_string()];
    cli.model_arg = Some("--model".to_string());
    agent.model = Some("opus".to_string());
    agent.thinking = Some(ThinkingConfig {
        mode: ThinkingMode::Adaptive,
        display: ThinkingDisplay::Summarized,
        budget_tokens: Some(8000),
    });

    let argv = agent_runner::CliAgentRunner::build_argv(&agent, "hi", None, None);
    // Find each flag and assert its companion value sits in the next slot.
    let pos_thinking = argv.iter().position(|s| s == "--thinking");
    let pos_display = argv.iter().position(|s| s == "--thinking-display");
    let pos_budget = argv.iter().position(|s| s == "--max-thinking-tokens");
    assert!(pos_thinking.is_some() && pos_display.is_some() && pos_budget.is_some(),
        "all three thinking flags expected; got {:?}", argv);
    assert_eq!(argv[pos_thinking.unwrap() + 1], "adaptive");
    assert_eq!(argv[pos_display.unwrap() + 1], "summarized");
    assert_eq!(argv[pos_budget.unwrap() + 1], "8000");
}

/// `ThinkingMode::Disabled` on a `claude` profile must NOT emit any
/// `--thinking*` flags — the CLI treats their absence as the off state
/// and a literal `--thinking disabled` would be rejected as an unknown
/// mode value.
#[test]
fn test_build_argv_disabled_thinking_emits_no_flags_for_claude() {
    use ao_protocol::agent::{ThinkingConfig, ThinkingDisplay, ThinkingMode};

    let mut agent = make_test_agent("no-think");
    let ProviderConfig::Cli(ref mut cli) = agent.provider;
    cli.command = "claude".to_string();
    agent.thinking = Some(ThinkingConfig {
        mode: ThinkingMode::Disabled,
        display: ThinkingDisplay::Summarized,
        budget_tokens: None,
    });

    let argv = agent_runner::CliAgentRunner::build_argv(&agent, "hi", None, None);
    assert!(
        !argv.iter().any(|a| a.starts_with("--thinking")),
        "no thinking flags expected: {:?}",
        argv
    );
}

/// Non-claude providers don't have a `--thinking` flag surface, so a
/// configured `ThinkingConfig` must NOT leak into their argv. This is
/// the protective half of the provider-neutral plumbing — adding e.g.
/// `gemini` later is an opt-in match arm, not a free regression risk.
#[test]
fn test_build_argv_thinking_skipped_for_non_claude_command() {
    use ao_protocol::agent::{ThinkingConfig, ThinkingDisplay, ThinkingMode};

    let mut agent = make_test_agent("future-provider");
    let ProviderConfig::Cli(ref mut cli) = agent.provider;
    cli.command = "gemini".to_string();
    agent.thinking = Some(ThinkingConfig {
        mode: ThinkingMode::Adaptive,
        display: ThinkingDisplay::Raw,
        budget_tokens: Some(2000),
    });

    let argv = agent_runner::CliAgentRunner::build_argv(&agent, "hi", None, None);
    assert!(
        !argv.iter().any(|a| a.starts_with("--thinking")),
        "thinking flags must not leak into non-claude argv: {:?}",
        argv
    );
}

#[test]
fn test_build_argv_includes_mcp_config_flag() {
    let mut agent = make_test_agent("mcp-agent");
    let ProviderConfig::Cli(ref mut cli) = agent.provider;
    cli.command = "claude".to_string();
    let mcp_path = std::path::Path::new("/data/agents/mcp-agent/mcp.json");
    let argv =
        agent_runner::CliAgentRunner::build_argv(&agent, "do something", Some(mcp_path), None);
    let pos = argv.iter().position(|s| s == "--mcp-config");
    assert!(pos.is_some(), "--mcp-config flag expected in argv: {:?}", argv);
    assert_eq!(
        argv[pos.unwrap() + 1],
        "/data/agents/mcp-agent/mcp.json",
        "mcp.json path must follow --mcp-config"
    );
    // Prompt must still be last arg
    assert_eq!(argv.last().unwrap(), "do something");
}

/// Codex has no `--mcp-config` flag and hard-errors on unrecognized
/// arguments, so it must never receive one; the launchpad server should
/// instead arrive via a `-c mcp_servers.<name>.url=...` override built
/// from the session URL rather than the JSON config file path.
#[test]
fn test_build_argv_uses_codex_config_override_not_mcp_config_flag() {
    let mut agent = make_test_agent("codex-agent");
    let ProviderConfig::Cli(ref mut cli) = agent.provider;
    cli.command = "codex".to_string();
    cli.args = vec![
        "exec".to_string(),
        "--json".to_string(),
        "--sandbox".to_string(),
        "workspace-write".to_string(),
        "--skip-git-repo-check".to_string(),
    ];
    let mcp_path = std::path::Path::new("/data/agents/codex-agent/mcp.json");
    let mcp_url = "http://localhost:3101/mcp/codex-agent/session-1";
    let argv = agent_runner::CliAgentRunner::build_argv(
        &agent,
        "do something",
        Some(mcp_path),
        Some(mcp_url),
    );
    assert!(
        !argv.iter().any(|a| a == "--mcp-config"),
        "codex must never receive --mcp-config: {:?}",
        argv
    );
    let pos = argv.iter().position(|s| s == "-c");
    assert!(pos.is_some(), "-c override expected in argv: {:?}", argv);
    assert_eq!(
        argv[pos.unwrap() + 1],
        format!(r#"mcp_servers.launchpad.url="{mcp_url}""#),
    );

    // No URL available (e.g. MCP session prep failed) means no override at all.
    let argv_no_url =
        agent_runner::CliAgentRunner::build_argv(&agent, "do something", Some(mcp_path), None);
    assert!(
        !argv_no_url.iter().any(|a| a == "-c" || a == "--mcp-config"),
        "no MCP flags expected without a url: {:?}",
        argv_no_url
    );
}

/// cursor-agent also has no `--mcp-config` flag — confirmed absent from its
/// `--help` output and from every registered option in the installed CLI
/// bundle — and its commander-based parser rejects unrecognized options.
/// Unlike Codex it has no argv-based override either: it discovers MCP
/// servers implicitly from `.cursor/mcp.json` in its workspace, which the
/// caller writes separately (see `merge_cursor_mcp_config` in
/// `agent_runner::cli`) once the spawn's cwd is known. `build_argv` itself
/// must therefore emit no MCP-related flags at all for cursor-agent.
#[test]
fn test_build_argv_cursor_agent_gets_no_mcp_flags() {
    let mut agent = make_test_agent("cursor-agent-profile");
    let ProviderConfig::Cli(ref mut cli) = agent.provider;
    cli.command = "cursor-agent".to_string();
    cli.args = vec![
        "--print".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--force".to_string(),
        "--approve-mcps".to_string(),
        "--trust".to_string(),
    ];
    let mcp_path = std::path::Path::new("/data/agents/cursor-agent-profile/mcp.json");
    let mcp_url = "http://localhost:3101/mcp/cursor-agent-profile/session-1";
    let argv = agent_runner::CliAgentRunner::build_argv(
        &agent,
        "do something",
        Some(mcp_path),
        Some(mcp_url),
    );
    assert!(
        !argv.iter().any(|a| a == "--mcp-config"),
        "cursor-agent must never receive --mcp-config, which it doesn't recognize: {:?}",
        argv
    );
    assert!(
        !argv.iter().any(|a| a == "-c"),
        "cursor-agent has no -c config-override surface, unlike codex: {:?}",
        argv
    );
    assert!(
        !argv.iter().any(|a| a.contains(mcp_url)),
        "the MCP url must not leak into cursor-agent argv; it is delivered via .cursor/mcp.json: {:?}",
        argv
    );
    // Prompt must still be last arg — the MCP delivery change must not
    // disturb ordinary argv construction.
    assert_eq!(argv.last().unwrap(), "do something");
}

#[tokio::test]
async fn test_agent_runner_event_sequence() {
    let scenarios = vec![MockScenario {
        stdout_lines: vec![
            "line 1".to_string(),
            "line 2".to_string(),
            "line 3".to_string(),
        ],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 10,
    }];

    let (runner, event_bus, instance_registry, persistence, _tmp, _env_guard) =
        setup_test_runner(scenarios).await;

    let agent = make_test_agent("runner-test");
    let mut rx = event_bus.subscribe();

    let (complete_tx, mut complete_rx) = tokio::sync::mpsc::channel(1);
    let run_id = runner.run(&agent, "hello", &[], complete_tx, None).await.unwrap();

    // Wait for run completion
    let run_complete = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        complete_rx.recv(),
    )
    .await
    .expect("run should complete in time")
    .expect("should receive RunComplete");

    assert_eq!(run_complete.run_id, run_id);

    // Collect all events
    let mut events = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) => events.push(event),
            Err(_) => break,
        }
    }

    assert!(!events.is_empty(), "Should have received events");

    // First event should be RunStarted
    assert!(
        matches!(events[0].payload, AgentEventPayload::RunStarted),
        "First event should be RunStarted, got: {:?}",
        events[0].payload
    );

    // Last event should be RunEnded
    let last = events.last().unwrap();
    match &last.payload {
        AgentEventPayload::RunEnded { reason } => {
            assert_eq!(*reason, RunEndReason::Completed);
        }
        other => panic!("Last event should be RunEnded, got: {:?}", other),
    }

    // Verify seq numbers are monotonic for this run
    let run_events: Vec<_> = events.iter().filter(|e| e.run_id == run_id).collect();
    for (i, event) in run_events.iter().enumerate() {
        assert_eq!(
            event.seq, i as u64,
            "Seq should be monotonic: expected {}, got {}",
            i, event.seq
        );
    }

    // Should have TextDelta or TextComplete events in between
    let text_events: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e.payload,
                AgentEventPayload::TextDelta { .. } | AgentEventPayload::TextComplete { .. }
            )
        })
        .collect();
    assert!(
        !text_events.is_empty(),
        "Should have text events from normalizer"
    );

    // Verify InstanceRegistry shows 0 running after completion
    assert_eq!(
        instance_registry
            .running_count(&"runner-test".to_string())
            .await,
        0,
        "No runs should be active after completion"
    );

    // Verify transcript has agent response
    // Note: user message is persisted by the route handler, not the runner
    let entries = persistence
        .transcripts
        .read_all("runner-test")
        .await
        .unwrap();
    assert!(entries.len() >= 1, "Transcript should have at least agent response, got {}", entries.len());

    // First entry should be the agent response
    assert_eq!(entries[0].event_type, "response");
}

#[tokio::test]
async fn test_agent_runner_returns_immediately() {
    let scenarios = vec![MockScenario {
        stdout_lines: vec!["slow output".to_string()],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 500, // Slow scenario
    }];

    let (runner, _event_bus, _instance_registry, _persistence, _tmp, _env_guard) =
        setup_test_runner(scenarios).await;

    let agent = make_test_agent("fast-return");
    let (complete_tx, _complete_rx) = tokio::sync::mpsc::channel(1);

    // run() should return nearly immediately (not wait for process completion)
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        runner.run(&agent, "hello", &[], complete_tx, None),
    )
    .await;

    assert!(
        result.is_ok(),
        "run() should return immediately, not wait for process"
    );
    assert!(result.unwrap().is_ok(), "run() should succeed");
}

// === QueueManager tests ===

use ao_protocol::message::QueuedMessage;

async fn setup_test_queue_manager(
    scenarios: Vec<MockScenario>,
) -> (
    std::sync::Arc<queue_manager::QueueManagerRegistry>,
    std::sync::Arc<event_bus::EventBus>,
    std::sync::Arc<instance_registry::InstanceRegistry>,
    std::sync::Arc<PersistenceLayer>,
    tempfile::TempDir,
    std::sync::MutexGuard<'static, ()>,
) {
    let env_guard = crate::plugin_paths::tests::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let persistence =
        std::sync::Arc::new(PersistenceLayer::init().await.expect("init persistence"));
    let event_bus = std::sync::Arc::new(event_bus::EventBus::new(256));
    let mock_supervisor: std::sync::Arc<dyn ProcessSupervisor> =
        std::sync::Arc::new(MockProcessSupervisor::new(scenarios));
    let normalizer_registry = std::sync::Arc::new(NormalizerRegistry::new());
    let command_queue = std::sync::Arc::new(command_queue::CommandQueue::new());
    let instance_registry = std::sync::Arc::new(instance_registry::InstanceRegistry::new());

    let running_agents = std::sync::Arc::new(agent_runner::RunningAgents::new());
    let runner = std::sync::Arc::new(agent_runner::CliAgentRunner::new(
        mock_supervisor,
        normalizer_registry,
        std::sync::Arc::clone(&event_bus),
        std::sync::Arc::clone(&persistence),
        command_queue,
        std::sync::Arc::clone(&instance_registry),
        std::sync::Arc::clone(&running_agents),
        std::sync::Arc::new(ao_engine_tools_core::Registry::new()),
    ));

    let dispatcher = std::sync::Arc::new(agent_runner::RunnerDispatcher::with_runners(
        runner.clone() as std::sync::Arc<dyn agent_runner::AgentRunner>,
        runner.clone() as std::sync::Arc<dyn agent_runner::AgentRunner>,
    ));

    let queue_managers = std::sync::Arc::new(queue_manager::QueueManagerRegistry::new(
        dispatcher,
        std::sync::Arc::clone(&instance_registry),
        std::sync::Arc::clone(&event_bus),
        std::sync::Arc::clone(&persistence),
    ));

    (queue_managers, event_bus, instance_registry, persistence, tmp, env_guard)
}

#[tokio::test]
async fn test_queue_manager_sequential_with_max_1() {
    // 3 scenarios for 3 messages, each producing 1 stdout line
    let scenarios = vec![
        MockScenario {
            stdout_lines: vec!["response-1".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 50,
        },
        MockScenario {
            stdout_lines: vec!["response-2".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 50,
        },
        MockScenario {
            stdout_lines: vec!["response-3".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 50,
        },
    ];

    let (queue_managers, event_bus, _instance_registry, _persistence, _tmp, _env_guard) =
        setup_test_queue_manager(scenarios).await;

    let agent = make_test_agent("seq-agent");
    let mut rx = event_bus.subscribe();

    // Submit 3 messages rapidly
    for i in 1..=3 {
        let msg = QueuedMessage {
            message_id: format!("msg-{}", i),
            content: format!("prompt-{}", i),
            queued_at: chrono::Utc::now(),
            attachments: vec![],
source: None,
            focus_path: None,
            thread_id: None,
            };
        queue_managers
            .submit_message(&agent, msg)
            .await
            .expect("submit should succeed");
    }

    // Collect events until we see 3 RunEnded events (with timeout)
    let mut events = Vec::new();
    let mut run_ended_count = 0;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);

    while run_ended_count < 3 {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) => {
                if matches!(event.payload, AgentEventPayload::RunEnded { .. }) {
                    run_ended_count += 1;
                }
                events.push(event);
            }
            Ok(Err(_)) => break, // lagged
            Err(_) => panic!("Timed out waiting for 3 RunEnded events, got {}", run_ended_count),
        }
    }

    // With max_instances=1, messages should process sequentially:
    // Each RunStarted should come after previous RunEnded
    let run_started_indices: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.payload, AgentEventPayload::RunStarted))
        .map(|(i, _)| i)
        .collect();

    let run_ended_indices: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.payload, AgentEventPayload::RunEnded { .. }))
        .map(|(i, _)| i)
        .collect();

    assert_eq!(run_started_indices.len(), 3, "Should have 3 RunStarted events");
    assert_eq!(run_ended_indices.len(), 3, "Should have 3 RunEnded events");

    // Each subsequent RunStarted must come after the previous RunEnded
    for i in 1..run_started_indices.len() {
        assert!(
            run_started_indices[i] > run_ended_indices[i - 1],
            "Run {} started (event idx {}) before run {} ended (event idx {})",
            i + 1,
            run_started_indices[i],
            i,
            run_ended_indices[i - 1]
        );
    }
}

/// Regression: the per-agent queue manager must book the
/// [`InstanceRegistry`] slot synchronously *before* the runner is
/// spawned, so subsequent `can_spawn` checks within the same pump
/// loop see the registration. Previously the queue manager called
/// `tokio::spawn(runner.run(req))` and let the runner book its own
/// slot deep inside `run_with_scope`. With `max_instances = 1`, a
/// burst of submitted messages all passed the `can_spawn` check
/// before the first spawned runner reached its `register_run` —
/// producing concurrent runs in violation of the configured cap.
/// The fix pre-allocates a `run_id` at the queue manager, registers
/// it synchronously, and forwards it to the runner via
/// [`AgentRunRequest::pre_registered_run_id`].
///
/// This test isolates the mechanism from end-to-end event ordering
/// by sampling `instance_registry.running_count` immediately after
/// each `submit_message` call and asserting the count never exceeds
/// `max_instances`.
#[tokio::test]
async fn test_queue_manager_pre_registers_slot_synchronously() {
    // Slow scenarios so the runners are still mid-flight by the time
    // we measure — that's what proves the cap is being enforced and
    // not just "the runs finished too fast to overlap".
    let scenarios = (0..5)
        .map(|i| MockScenario {
            stdout_lines: vec![format!("resp-{}", i)],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 300,
        })
        .collect();

    let (queue_managers, _event_bus, instance_registry, _persistence, _tmp, _env_guard) =
        setup_test_queue_manager(scenarios).await;

    let agent = make_test_agent("cap-1-agent");
    let agent_id = agent.id.clone();

    // Submit 5 messages back-to-back. Without the pre-registration
    // fix, multiple runners would be spawned before any of them
    // reached the registry — running_count would briefly exceed 1.
    for i in 1..=5 {
        let msg = QueuedMessage {
            message_id: format!("msg-{}", i),
            content: format!("prompt-{}", i),
            queued_at: chrono::Utc::now(),
            attachments: vec![],
            source: None,
            focus_path: None,
            thread_id: None,
            };
        queue_managers
            .submit_message(&agent, msg)
            .await
            .expect("submit should succeed");

        // Sample the running count immediately. With the fix, the
        // queue manager pre-registers before returning from the
        // synchronous pump path, so the count reflects whatever the
        // cap allows (here, at most 1).
        let count = instance_registry.running_count(&agent_id).await;
        assert!(
            count <= 1,
            "InstanceRegistry running_count={} after submit #{} \
             violates max_instances=1; queue manager must \
             pre-register before spawning",
            count,
            i,
        );
    }

    // Sample a few more times while runs are still in flight, just
    // to be sure no later iteration of the pump loop overshoots.
    for _ in 0..5 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let count = instance_registry.running_count(&agent_id).await;
        assert!(
            count <= 1,
            "InstanceRegistry running_count={} while runs in flight \
             violates max_instances=1",
            count,
        );
    }
}

#[tokio::test]
async fn test_queue_manager_concurrent_with_max_2() {
    // 3 scenarios: first 2 have delay to allow concurrent detection
    let scenarios = vec![
        MockScenario {
            stdout_lines: vec!["resp-1".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 150,
        },
        MockScenario {
            stdout_lines: vec!["resp-2".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 150,
        },
        MockScenario {
            stdout_lines: vec!["resp-3".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 50,
        },
    ];

    let (queue_managers, event_bus, _instance_registry, _persistence, _tmp, _env_guard) =
        setup_test_queue_manager(scenarios).await;

    let mut agent = make_test_agent("concurrent-agent");
    agent.max_instances = 2;
    agent.serialize = false; // Don't serialize for concurrent test

    let mut rx = event_bus.subscribe();

    // Submit 3 messages rapidly
    for i in 1..=3 {
        let msg = QueuedMessage {
            message_id: format!("msg-{}", i),
            content: format!("prompt-{}", i),
            queued_at: chrono::Utc::now(),
            attachments: vec![],
source: None,
            focus_path: None,
            thread_id: None,
            };
        queue_managers
            .submit_message(&agent, msg)
            .await
            .expect("submit should succeed");
    }

    // Collect events until we see 3 RunEnded events
    let mut events = Vec::new();
    let mut run_ended_count = 0;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);

    while run_ended_count < 3 {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) => {
                if matches!(event.payload, AgentEventPayload::RunEnded { .. }) {
                    run_ended_count += 1;
                }
                events.push(event);
            }
            Ok(Err(_)) => break,
            Err(_) => panic!("Timed out waiting for 3 RunEnded events, got {}", run_ended_count),
        }
    }

    // With max_instances=2, first 2 should start before any RunEnded
    let run_started_indices: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.payload, AgentEventPayload::RunStarted))
        .map(|(i, _)| i)
        .collect();

    let first_run_ended_idx = events
        .iter()
        .position(|e| matches!(e.payload, AgentEventPayload::RunEnded { .. }))
        .expect("Should have at least one RunEnded");

    // At least 2 RunStarted events should appear before first RunEnded
    let starts_before_first_end = run_started_indices
        .iter()
        .filter(|&&idx| idx < first_run_ended_idx)
        .count();
    assert!(
        starts_before_first_end >= 2,
        "Expected at least 2 RunStarted before first RunEnded, got {}",
        starts_before_first_end
    );
}

#[tokio::test]
async fn test_queue_manager_pump_on_run_complete() {
    // 3 scenarios: first run is slow, second and third are fast
    let scenarios = vec![
        MockScenario {
            stdout_lines: vec!["slow-1".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 100,
        },
        MockScenario {
            stdout_lines: vec!["fast-2".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 10,
        },
        MockScenario {
            stdout_lines: vec!["fast-3".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 10,
        },
    ];

    let (queue_managers, event_bus, _instance_registry, _persistence, _tmp, _env_guard) =
        setup_test_queue_manager(scenarios).await;

    let agent = make_test_agent("pump-agent");
    let mut rx = event_bus.subscribe();

    // Submit 3 messages
    for i in 1..=3 {
        let msg = QueuedMessage {
            message_id: format!("msg-{}", i),
            content: format!("prompt-{}", i),
            queued_at: chrono::Utc::now(),
            attachments: vec![],
source: None,
            focus_path: None,
            thread_id: None,
            };
        queue_managers
            .submit_message(&agent, msg)
            .await
            .expect("submit should succeed");
    }

    // Wait for all 3 to complete
    let mut run_ended_count = 0;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);

    while run_ended_count < 3 {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) => {
                if matches!(event.payload, AgentEventPayload::RunEnded { .. }) {
                    run_ended_count += 1;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => panic!("Timed out waiting for pump completion, got {} RunEnded", run_ended_count),
        }
    }

    assert_eq!(run_ended_count, 3, "All 3 messages should have completed runs");
}

/// Full-pump end-to-end regression: an assignment-triggered run must be
/// transitioned all the way through the production
/// [`queue_manager::AgentQueueManager`] to `Succeeded` with a populated
/// `output_summary` — not merely by direct writes to
/// `persistence.assignment_runs.update`.
///
/// Before the lifecycle write-back landed, `fire_assignment` inserted a
/// `Queued` row and the queue manager never moved it further, so this
/// assertion (Succeeded + `output_summary == mock stdout`) failed with
/// the row still stuck at `Queued`.
#[tokio::test]
async fn test_assignment_run_reaches_succeeded_through_full_pump() {
    use ao_protocol::assignment::{
        Assignment, AssignmentRunStatus, AssignmentTrigger, AssignmentTriggerKind, OutputMode,
    };
    use ao_protocol::scheduled_task::MessageSource;

    // Mock supervisor stdout becomes the runner's final assistant text
    // and therefore the AssignmentRun's `output_summary`.
    let scenarios = vec![MockScenario {
        stdout_lines: vec!["assignment-run-output".to_string()],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 10,
    }];

    let (queue_managers, event_bus, _instance_registry, persistence, _tmp, _env_guard) =
        setup_test_queue_manager(scenarios).await;

    let agent = make_test_agent("assignment-lifecycle-agent");
    persistence
        .agents
        .create(&agent)
        .await
        .expect("agent create");

    let now = chrono::Utc::now();
    let assignment_id = "assign-e2e-1";
    let assignment = Assignment {
        id: assignment_id.to_string(),
        agent_id: agent.id.clone(),
        name: "End-to-end lifecycle".to_string(),
        instruction: "Return the mock output.".to_string(),
        working_directory: None,
        trigger: AssignmentTrigger::Webhook {
            token: None,
            route_name: None,
            secret_ref: None,
            events: vec![],
            filters: None,
            prompt_template: None,
            deliver: Default::default(),
        },
        bindings: vec![],
        output_mode: OutputMode::Background,
        thread_policy: ao_protocol::assignment::AssignmentThreadPolicy::default(),
        dedicated_thread_id: None,
        enabled: true,
        expires_at: None,
        next_fire_at: None,
        last_run_at: None,
        last_event_cursor: None,
        liveness: ao_protocol::assignment::LivenessState::default(),
        created_ts: now,
        updated_ts: now,
    };
    persistence
        .assignments
        .add(assignment.clone())
        .await
        .expect("assignment add");

    // Drive the production trigger helper end-to-end. It creates the
    // AssignmentRun row in Queued status and enqueues the message via
    // the QueueManagerRegistry (which implements NotificationDispatcher).
    let dispatcher: std::sync::Arc<
        dyn queue_manager::NotificationDispatcher,
    > = std::sync::Arc::clone(&queue_managers)
        as std::sync::Arc<dyn queue_manager::NotificationDispatcher>;

    let run = crate::assignment_runner::fire_assignment(
        &persistence,
        &dispatcher,
        &event_bus,
        &assignment,
        AssignmentTriggerKind::Webhook,
        None,
        None,
        None,
    )
    .await
    .expect("fire_assignment should succeed");

    assert_eq!(
        run.status,
        AssignmentRunStatus::Queued,
        "fire_assignment must return the row in Queued status"
    );

    // Wait for the run to complete by watching the event stream.
    let mut rx = event_bus.subscribe();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut saw_run_ended = false;
    while !saw_run_ended {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) => {
                if matches!(event.payload, AgentEventPayload::RunEnded { .. }) {
                    saw_run_ended = true;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => panic!(
                "timed out waiting for RunEnded — production pump never fired completion write-back"
            ),
        }
    }

    // Give the completion branch a beat to write back after RunEnded.
    // The lifecycle write is async and runs in the same actor loop
    // *after* the RunEnded event has already been emitted on the bus.
    let final_run = {
        let mut attempt: Option<ao_protocol::assignment::AssignmentRun> = None;
        for _ in 0..50 {
            let stored = persistence
                .assignment_runs
                .get(assignment_id, &run.id)
                .await
                .expect("get after completion");
            if matches!(
                stored.as_ref().map(|r| r.status),
                Some(AssignmentRunStatus::Succeeded) | Some(AssignmentRunStatus::Failed)
            ) {
                attempt = stored;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        attempt.expect("AssignmentRun must exist and reach a terminal status after the run finished")
    };

    // Linchpin assertion — this is what would have caught the missing
    // production write-back: without the fix, `status` stays `Queued`
    // and `output_summary` is None forever.
    assert_eq!(
        final_run.status,
        AssignmentRunStatus::Succeeded,
        "AssignmentRun must reach Succeeded through the production pump (got {:?})",
        final_run.status,
    );
    assert!(
        final_run.finished_ts.is_some(),
        "finished_ts must be populated when the run terminates"
    );
    let summary = final_run
        .output_summary
        .as_deref()
        .expect("output_summary must be populated from the runner's final assistant text");
    assert!(
        summary.contains("assignment-run-output"),
        "output_summary must contain the mock runner's stdout — got {:?}",
        summary
    );

    // Sanity: the assignment-sourced message MUST classify as
    // non-interactive so it never blocks a real user turn. This mirrors
    // the invariant enforced elsewhere and guards against a future
    // regression that would strand the pump on serialization.
    let sample = QueuedMessage {
        message_id: "probe".to_string(),
        content: String::new(),
        queued_at: chrono::Utc::now(),
        attachments: vec![],
        source: Some(MessageSource::Assignment {
            assignment_id: assignment_id.to_string(),
            run_id: run.id.clone(),
            trigger_kind: "webhook".to_string(),
        }),
        focus_path: None,
        thread_id: None,
        };
    assert!(matches!(
        sample.source,
        Some(MessageSource::Assignment { .. })
    ));
}

/// Failure-path regression: when the runner returns `Err` (here induced by
/// an empty MockProcessSupervisor scenario list — `spawn` returns
/// `AoError::Process(..)` and no `RunComplete` is sent), the persisted
/// AssignmentRun row must reach a terminal state — never remain stuck at
/// `Running` or `Queued`. This is the trap the failure-watcher path exists
/// to close.
#[tokio::test]
async fn test_assignment_run_marked_failed_when_runner_errors() {
    use ao_protocol::assignment::{
        Assignment, AssignmentRunStatus, AssignmentTrigger, AssignmentTriggerKind, OutputMode,
    };

    // Zero scenarios: MockProcessSupervisor.spawn() returns an error on
    // the first call, so the CLI runner short-circuits before it can
    // emit RunComplete.
    let (queue_managers, event_bus, _instance_registry, persistence, _tmp, _env_guard) =
        setup_test_queue_manager(vec![]).await;

    let agent = make_test_agent("assignment-failure-agent");
    persistence
        .agents
        .create(&agent)
        .await
        .expect("agent create");

    let now = chrono::Utc::now();
    let assignment_id = "assign-fail-1";
    let assignment = Assignment {
        id: assignment_id.to_string(),
        agent_id: agent.id.clone(),
        name: "Failure lifecycle".to_string(),
        instruction: "Deliberate failure path.".to_string(),
        working_directory: None,
        trigger: AssignmentTrigger::Webhook {
            token: None,
            route_name: None,
            secret_ref: None,
            events: vec![],
            filters: None,
            prompt_template: None,
            deliver: Default::default(),
        },
        bindings: vec![],
        output_mode: OutputMode::Background,
        thread_policy: ao_protocol::assignment::AssignmentThreadPolicy::default(),
        dedicated_thread_id: None,
        enabled: true,
        expires_at: None,
        next_fire_at: None,
        last_run_at: None,
        last_event_cursor: None,
        liveness: ao_protocol::assignment::LivenessState::default(),
        created_ts: now,
        updated_ts: now,
    };
    persistence
        .assignments
        .add(assignment.clone())
        .await
        .expect("assignment add");

    let dispatcher: std::sync::Arc<
        dyn queue_manager::NotificationDispatcher,
    > = std::sync::Arc::clone(&queue_managers)
        as std::sync::Arc<dyn queue_manager::NotificationDispatcher>;

    let run = crate::assignment_runner::fire_assignment(
        &persistence,
        &dispatcher,
        &event_bus,
        &assignment,
        AssignmentTriggerKind::Webhook,
        None,
        None,
        None,
    )
    .await
    .expect("fire_assignment should succeed even if the run will later fail");

    // Poll for a terminal status. Without the failure-watcher write-back,
    // the row would be stranded at `Queued` (spawn failed before the
    // Running transition raced through), so this loop would time out.
    let final_run = {
        let mut attempt: Option<ao_protocol::assignment::AssignmentRun> = None;
        for _ in 0..100 {
            let stored = persistence
                .assignment_runs
                .get(assignment_id, &run.id)
                .await
                .expect("get after runner failure");
            if matches!(
                stored.as_ref().map(|r| r.status),
                Some(AssignmentRunStatus::Failed) | Some(AssignmentRunStatus::Succeeded)
            ) {
                attempt = stored;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        attempt.expect(
            "AssignmentRun must reach a terminal status even when the runner errors — otherwise it is stranded",
        )
    };

    assert_eq!(
        final_run.status,
        AssignmentRunStatus::Failed,
        "runner error must transition the AssignmentRun to Failed (got {:?})",
        final_run.status,
    );
    assert!(
        final_run.finished_ts.is_some(),
        "finished_ts must be set on the failure path"
    );
    assert!(
        final_run.error.is_some(),
        "error field must carry the runner failure reason"
    );
}

#[tokio::test]
async fn test_queue_manager_emits_message_processing_started() {
    let scenarios = vec![
        MockScenario {
            stdout_lines: vec!["response".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 10,
        },
    ];

    let (queue_managers, event_bus, _instance_registry, _persistence, _tmp, _env_guard) =
        setup_test_queue_manager(scenarios).await;

    let agent = make_test_agent("mps-agent");
    let mut rx = event_bus.subscribe();

    let msg = QueuedMessage {
        message_id: "test-msg-1".to_string(),
        content: "hello".to_string(),
        queued_at: chrono::Utc::now(),
        attachments: vec![],
source: None,
        focus_path: None,
        thread_id: None,
        };
    queue_managers
        .submit_message(&agent, msg)
        .await
        .expect("submit should succeed");

    // Collect events until RunEnded
    let mut found_mps = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);

    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) => {
                if let AgentEventPayload::MessageProcessingStarted { ref message_id } =
                    event.payload
                {
                    assert_eq!(message_id, "test-msg-1");
                    found_mps = true;
                }
                if matches!(event.payload, AgentEventPayload::RunEnded { .. }) {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => panic!("Timed out waiting for events"),
        }
    }

    assert!(
        found_mps,
        "Should have received MessageProcessingStarted event"
    );
}

#[tokio::test]
async fn test_submit_message_wake_on_deliver_enrolls_dormant_copilot() {
    // A QueuedMessage submitted to a dormant co-pilot
    // (template == COPILOT_PROFILE_ID, not currently enrolled) ends up
    // in the enrolled set by the time `submit_message` returns. The
    // message itself is then dispatched normally — RunEnded must fire
    // for the same message_id, demonstrating that the wake-on-deliver
    // path doesn't block delivery.
    let scenarios = vec![MockScenario {
        stdout_lines: vec!["copilot reply".to_string()],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 10,
    }];

    let (queue_managers, event_bus, _instance_registry, _persistence, _tmp, _env_guard) =
        setup_test_queue_manager(scenarios).await;

    // Wire an enrolled set into the registry, mirroring AppState.
    let enrolled =
        std::sync::Arc::new(crate::mailbox_poller::EnrolledCopilots::new());
    queue_managers.set_enrolled_copilots(std::sync::Arc::clone(&enrolled));

    // Construct a co-pilot agent: template = `tasklist-copilot`. Initially
    // dormant — not in the enrolled set.
    let mut agent = make_test_agent("copilot-A");
    agent.template = Some(prompt_sections::COPILOT_PROFILE_ID.to_string());

    assert!(
        !enrolled.is_enrolled("copilot-A").await,
        "co-pilot should start dormant"
    );

    let mut rx = event_bus.subscribe();
    let msg = QueuedMessage {
        message_id: "notif-1".to_string(),
        content: "<task-item-notification>...</task-item-notification>".to_string(),
        queued_at: chrono::Utc::now(),
        attachments: vec![],
        source: None,
        focus_path: None,
        thread_id: None,
        };
    queue_managers
        .submit_message(&agent, msg)
        .await
        .expect("submit should succeed");

    // Wake-on-deliver enrolls the co-pilot synchronously, before
    // delivery returns.
    assert!(
        enrolled.is_enrolled("copilot-A").await,
        "co-pilot must be enrolled by wake-on-deliver"
    );
    assert_eq!(enrolled.len().await, 1);

    // The message is dispatched normally — confirm by waiting for
    // the corresponding RunEnded event.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) => {
                if matches!(event.payload, AgentEventPayload::RunEnded { .. }) {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => panic!("timed out waiting for RunEnded after wake-on-deliver"),
        }
    }
}

#[tokio::test]
async fn test_submit_message_does_not_enroll_non_copilot_agents() {
    // Non-co-pilot preservation: a regular agent is not added to
    // the enrolled set just because a message was delivered to it.
    let scenarios = vec![MockScenario {
        stdout_lines: vec!["plain reply".to_string()],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 10,
    }];

    let (queue_managers, _event_bus, _instance_registry, _persistence, _tmp, _env_guard) =
        setup_test_queue_manager(scenarios).await;
    let enrolled =
        std::sync::Arc::new(crate::mailbox_poller::EnrolledCopilots::new());
    queue_managers.set_enrolled_copilots(std::sync::Arc::clone(&enrolled));

    // Plain agent — no template.
    let agent = make_test_agent("plain-1");
    let msg = QueuedMessage {
        message_id: "msg-1".to_string(),
        content: "hello".to_string(),
        queued_at: chrono::Utc::now(),
        attachments: vec![],
        source: None,
        focus_path: None,
        thread_id: None,
        };
    queue_managers
        .submit_message(&agent, msg)
        .await
        .expect("submit should succeed");

    assert!(!enrolled.is_enrolled("plain-1").await);
    assert_eq!(enrolled.len().await, 0);
}

// === AppState tests ===

#[tokio::test]
async fn test_app_state_new_with_mock_succeeds() {
    let _env_guard = crate::plugin_paths::tests::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let mock = MockProcessSupervisor::new(vec![]);
    let state = state::AppState::new_with_mock(mock)
        .await
        .expect("AppState::new_with_mock should succeed");

    // Verify all fields are accessible
    let _bus = &state.event_bus;
    let _sup = &state.process_supervisor;
    let _norm = &state.normalizer_registry;
    let _cq = &state.command_queue;
    let _pers = &state.persistence;
    let _runner = &state.agent_runner;
    let _ir = &state.instance_registry;
    let _qm = &state.queue_managers;

    // Verify persistence layer created directories
    let data_root = &state.persistence.data_root;
    assert!(
        tokio::fs::metadata(data_root.agents_dir())
            .await
            .is_ok(),
        "agents/ directory should exist"
    );
    assert!(
        tokio::fs::metadata(data_root.messages_metadata_dir())
            .await
            .is_ok(),
        "messages/metadata/ directory should exist"
    );
    assert!(
        tokio::fs::metadata(data_root.messages_data_dir())
            .await
            .is_ok(),
        "messages/data/ directory should exist"
    );
}

// === Per-task output.txt tee ===

#[tokio::test]
async fn test_tasklist_agent_output_txt_written() {
    use ao_protocol::tasklist::{
        Task, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistOwner, TasklistStatus,
    };
    use agent_runner::RunScope;
    use ao_protocol::tasklist::TasklistScope;

    // Two text chunks — normalizer fuses each line into TextDelta + TextComplete.
    let chunk_a = "Hello from task";
    let chunk_b = " world.";
    let scenarios = vec![MockScenario {
        stdout_lines: vec![chunk_a.to_string(), chunk_b.to_string()],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 0,
    }];

    let (runner, _event_bus, _instance_registry, persistence, _tmp, _env_guard) =
        setup_test_runner(scenarios).await;

    let owner_agent_id = "owner-agent";
    let tl_id = "tl-out-test";
    let task_id = "task-1";

    // Build and persist an agent-owned tasklist with one task.
    let workspace_dir = persistence
        .data_root
        .agent_tasklist_workspace_dir(owner_agent_id, tl_id);
    let transcripts_dir = persistence
        .data_root
        .agent_tasklist_transcripts_dir(owner_agent_id, tl_id);
    let tl = Tasklist {
        id: tl_id.to_string(),
        owner: TasklistOwner::Agent {
            agent_id: owner_agent_id.to_string(),
        },
        team_id: None,
        title: "Output test tasklist".to_string(),
        description: String::new(),
        status: TasklistStatus::Active,
        groups: vec![TaskGroup {
            id: "g1".to_string(),
            mode: TaskGroupMode::Seq,
            tasks: vec![Task {
                id: task_id.to_string(),
                owner_agent_id: owner_agent_id.to_string(),
                prompt: "do work".to_string(),
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
                assignment: None,
                classifier_token: 0,
                dispatch_token: 0,
            }],
        }],
        workspace_dir: workspace_dir.to_string_lossy().to_string(),
        transcripts_dir: transcripts_dir.to_string_lossy().to_string(),
        created_at: chrono::Utc::now(),
        last_active_at: None,
        copilot_agent_id: None,
        last_opened_at: None,
        project_id: None,
        thread_id: None,
        };
    persistence
        .tasklists
        .create_for_agent(&tl)
        .await
        .expect("create agent-owned tasklist");

    // Persist the agent profile so the transcript store can locate it.
    persistence
        .agents
        .create(&make_test_agent(owner_agent_id))
        .await
        .expect("create agent");

    let agent = make_test_agent(owner_agent_id);
    let (complete_tx, mut complete_rx) = tokio::sync::mpsc::channel(1);

    runner
        .run_with_scope(
            &agent,
            "do work",
            &[],
            complete_tx,
            RunScope::Tasklist {
                scope: TasklistScope::Agent(owner_agent_id.to_string()),
                tasklist_id: tl_id.to_string(),
                task_id: task_id.to_string(),
            },
            None,
        )
        .await
        .expect("run_with_scope should succeed");

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        complete_rx.recv(),
    )
    .await
    .expect("run should complete in time")
    .expect("should receive RunComplete");

    // Assert output.txt was written and contains the teed text.
    let out_path = persistence
        .data_root
        .agent_tasklist_task_output_path(owner_agent_id, tl_id, task_id);
    let contents = tokio::fs::read_to_string(&out_path)
        .await
        .expect("output.txt should exist");
    assert!(
        contents.contains(chunk_a),
        "output.txt should contain first chunk, got: {:?}",
        contents,
    );

    // Assert the JSONL transcript exists at the new per-task path.
    let transcript_path = persistence
        .data_root
        .task_transcript_path(owner_agent_id, tl_id, task_id);
    assert!(
        tokio::fs::try_exists(&transcript_path).await.unwrap_or(false),
        "JSONL transcript should exist at new per-task path {}",
        transcript_path.display(),
    );
}

// === Back-compat transcript read falls through to legacy path ===

#[tokio::test]
async fn test_back_compat_transcript_read_falls_through_to_legacy() {
    use ao_protocol::tasklist::{
        Task, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistOwner, TasklistStatus,
    };
    use agent_runner::RunScope;
    use ao_protocol::tasklist::TasklistScope;

    let scenarios = vec![MockScenario {
        stdout_lines: vec!["new task output".to_string()],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 0,
    }];

    let (runner, _event_bus, _instance_registry, persistence, _tmp, _env_guard) =
        setup_test_runner(scenarios).await;

    let owner_agent_id = "agent-backcompat";
    let tl_id = "tl-backcompat";
    let task_id = "task-new";

    // Build and persist an agent-owned tasklist.
    let workspace_dir = persistence
        .data_root
        .agent_tasklist_workspace_dir(owner_agent_id, tl_id);
    let transcripts_dir = persistence
        .data_root
        .agent_tasklist_transcripts_dir(owner_agent_id, tl_id);
    let tl = Tasklist {
        id: tl_id.to_string(),
        owner: TasklistOwner::Agent {
            agent_id: owner_agent_id.to_string(),
        },
        team_id: None,
        title: "Back-compat test".to_string(),
        description: String::new(),
        status: TasklistStatus::Active,
        groups: vec![TaskGroup {
            id: "g1".to_string(),
            mode: TaskGroupMode::Seq,
            tasks: vec![Task {
                id: task_id.to_string(),
                owner_agent_id: owner_agent_id.to_string(),
                prompt: "do new work".to_string(),
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
                assignment: None,
                classifier_token: 0,
                dispatch_token: 0,
            }],
        }],
        workspace_dir: workspace_dir.to_string_lossy().to_string(),
        transcripts_dir: transcripts_dir.to_string_lossy().to_string(),
        created_at: chrono::Utc::now(),
        last_active_at: None,
        copilot_agent_id: None,
        last_opened_at: None,
        project_id: None,
        thread_id: None,
        };
    persistence
        .tasklists
        .create_for_agent(&tl)
        .await
        .expect("create agent-owned tasklist");

    persistence
        .agents
        .create(&make_test_agent(owner_agent_id))
        .await
        .expect("create agent");

    // Pre-seed a transcript at the LEGACY path.
    let legacy_path = persistence.data_root
        .agent_tasklist_transcript_path(owner_agent_id, tl_id, task_id);
    if let Some(parent) = legacy_path.parent() {
        tokio::fs::create_dir_all(parent).await.expect("create legacy transcripts dir");
    }
    let legacy_content = "{\"role\":\"user\",\"content\":\"legacy entry\"}\n";
    tokio::fs::write(&legacy_path, legacy_content).await.expect("seed legacy transcript");

    // Run the new task — write path goes to the NEW per-task location.
    let agent = make_test_agent(owner_agent_id);
    let (complete_tx, mut complete_rx) = tokio::sync::mpsc::channel(1);
    runner
        .run_with_scope(
            &agent,
            "do new work",
            &[],
            complete_tx,
            RunScope::Tasklist {
                scope: TasklistScope::Agent(owner_agent_id.to_string()),
                tasklist_id: tl_id.to_string(),
                task_id: task_id.to_string(),
            },
            None,
        )
        .await
        .expect("run_with_scope should succeed");

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        complete_rx.recv(),
    )
    .await
    .expect("run should complete in time")
    .expect("should receive RunComplete");

    // The new per-task transcript must have been written.
    let new_path = persistence.data_root
        .task_transcript_path(owner_agent_id, tl_id, task_id);
    assert!(
        tokio::fs::try_exists(&new_path).await.unwrap_or(false),
        "new per-task transcript must exist at {}",
        new_path.display(),
    );

    // The legacy file must be untouched — back-compat path is read-only.
    let legacy_after = tokio::fs::read_to_string(&legacy_path)
        .await
        .expect("legacy transcript should still exist");
    assert_eq!(
        legacy_after, legacy_content,
        "legacy transcript must be unchanged after new task run",
    );
}
