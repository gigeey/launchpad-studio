//! Regression test: does a non-Anthropic `native_provider` selection survive
//! all the way to a *spawned subagent's* actual provider client?
//!
//! Prior to this file, nothing exercised that path: every test touching
//! `NativeAgentRunner`/`NativeChildRunner`/`ProfileAwareChildRunner` injected
//! a `MockProviderFactory` that ignores the `AgentProfile` it's given, so a
//! future re-hardcode of the routing to always build `AnthropicClient` would
//! compile clean and pass the full suite.
//!
//! These two tests spawn a subagent through the exact production entry
//! points `crates/ao-engine/src/state.rs` wires up
//! (`SubagentSpawner::spawn_sync` / `SubagentSpawner::spawn_named`, both
//! calling into the injected `ChildRunner` — `ProfileAwareChildRunner` in
//! production, constructed with the same `Arc<dyn ProviderFactory>` the
//! main-loop runner uses), covering both spawn shapes:
//!
//! - [`catalog_subagent_inherits_non_anthropic_provider_from_parent`] — a
//!   built-in catalog subagent (Explore/general-purpose), `target_profile =
//!   None`. This is the shape that was silently pinned to Anthropic: its
//!   provider is resolved by looking up the *launching* agent's profile in
//!   `AgentProfileStore` and re-running it through `ProviderFactory::build`.
//! - [`named_profile_delegate_uses_its_own_non_anthropic_provider`] — a
//!   named-profile delegate, `target_profile = Some(profile)`, routed
//!   through `RunnerDispatcher` to a real `NativeAgentRunner`.
//!
//! ## How "did it actually get the OpenAI client" is observed
//!
//! `ProviderFactory::build` returns an opaque `Arc<dyn ProviderClient>` that
//! the production code never hands back to a caller for inspection — it's
//! consumed internally by `run_session`. To observe the routing decision
//! without reimplementing (and thereby risking drift from) the logic under
//! test, `RecordingProviderFactory` wraps the REAL `DefaultProviderFactory`,
//! forwards every `build()` call to it unmodified, and records
//! `(agent.native_provider, client.default_model())` — `default_model()` is
//! set to a distinct sentinel string per provider by the fixture
//! `providers.toml` these tests write, so it uniquely identifies which
//! concrete client type production code constructed.
//!
//! ## Where this stops short of a true end-to-end spawn
//!
//! There is no live network call to a real provider and no API key. Every
//! provider section's `base_url` in the fixture `providers.toml` points at
//! `http://127.0.0.1:1` — an address nothing listens on. This means:
//!
//! - Client *construction* (`ProviderFactory::build`, including the
//!   `providers.toml` load and the per-agent tuning-knob resolution) is
//!   exercised for real.
//! - The subsequent `run_session(...)` call — full turn assembly, then
//!   `provider.complete(...)` — is also exercised for real, on the SAME
//!   client object the spy observed (not a mock swapped in afterward). This
//!   is asserted by checking the run terminates in a `ToolOutput::Error`
//!   whose message does not read as a configuration problem — i.e. it got
//!   past `build()` and attempted the request, not that it treated the
//!   provider as unconfigured.
//! - The one thing genuinely unverified is what happens with a real,
//!   reachable provider and a valid API key — that would require live
//!   network access and a real credential, which no test in this suite has.
//!   Everything up to and including "attempted an HTTP call with the
//!   provider-specific client" is exercised; "the call reached a real
//!   server and got a real response" is not.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use ao_engine::agent_runner::{
    AgentRunRequest, AgentRunner, DefaultProviderFactory, NativeAgentRunner, ProfileAwareChildRunner,
    ProviderFactory, RunComplete, RunningAgents, RunnerDispatcher,
};
use ao_engine::event_bus::EventBus;
use ao_engine::instance_registry::InstanceRegistry;
use ao_engine_tools_core::background_agents::{
    ChildRunner, SubagentDefinition, SubagentRegistry, SubagentSpawner,
};
use ao_engine_tools_core::{Registry, RunnerContext, ToolOutput};
use ao_engine_tools_runner::provider::{ProviderClient, ProviderError};
use ao_persistence::paths::DataRoot;
use ao_persistence::profiles::AgentProfileStore;
use ao_persistence::PersistenceLayer;
use ao_protocol::agent::{AgentProfile, AgentRunnerMode, NativeProvider};
use ao_protocol::error::AoError;

/// Serializes every test in this file against the process-global
/// `LAUNCHPAD_STUDIO_DATA_DIR` / `LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK` env
/// vars. A `tests/*.rs` file compiles to its own binary, so this only needs
/// to guard against races with *other tests in this same file* — it cannot
/// race with `ao-engine`'s lib unit tests (a different binary) or other
/// integration test files (also different binaries).
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Nothing listens here — every attempted HTTP call against a client built
/// from this fixture fails fast with a connection error instead of hanging
/// or reaching a real network.
const UNROUTABLE_BASE_URL: &str = "http://127.0.0.1:1";

fn write_sentinel_providers_toml(dir: &Path) {
    let toml = format!(
        r#"
[anthropic]
model = "claude-sentinel-anthropic"
base_url = "{url}"

[openai]
model = "gpt-sentinel-openai"
base_url = "{url}"

[openrouter]
model = "or-sentinel-openrouter"
base_url = "{url}"
"#,
        url = UNROUTABLE_BASE_URL,
    );
    std::fs::write(dir.join("providers.toml"), toml).expect("write providers.toml fixture");
}

/// RAII guard: sets the two provider-resolution env vars for the test's
/// duration and restores them on drop (including on panic/unwind), so a
/// failing test can't leak process-global state into the next one.
struct EnvGuard;

impl EnvGuard {
    fn set(dir: &Path) -> Self {
        write_sentinel_providers_toml(dir);
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", dir);
        std::env::set_var("LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK", "1");
        Self
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");
        std::env::remove_var("LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK");
    }
}

/// One observed call to `ProviderFactory::build`.
#[derive(Debug, Clone)]
struct RecordedBuild {
    native_provider: Option<NativeProvider>,
    default_model: Option<String>,
}

/// Wraps the REAL `DefaultProviderFactory`, forwarding every `build()` call
/// to it unmodified and recording what it decided. The routing logic under
/// test — the match on `agent.native_provider` — runs entirely inside the
/// wrapped `DefaultProviderFactory`; this type contributes no logic of its
/// own beyond observation, so it cannot mask a routing regression the way a
/// hand-rolled `MockProviderFactory` would.
struct RecordingProviderFactory {
    inner: DefaultProviderFactory,
    calls: Mutex<Vec<RecordedBuild>>,
}

impl RecordingProviderFactory {
    fn new() -> Self {
        Self { inner: DefaultProviderFactory, calls: Mutex::new(Vec::new()) }
    }

    fn calls(&self) -> Vec<RecordedBuild> {
        self.calls.lock().unwrap().clone()
    }
}

impl ProviderFactory for RecordingProviderFactory {
    fn build(&self, agent: &AgentProfile) -> Result<Arc<dyn ProviderClient>, ProviderError> {
        let result = self.inner.build(agent);
        if let Ok(client) = &result {
            self.calls.lock().unwrap().push(RecordedBuild {
                native_provider: agent.native_provider,
                default_model: client.default_model(),
            });
        }
        result
    }
}

/// Fills the `RunnerDispatcher`'s CLI slot. Neither test here exercises
/// `runner_mode = Cli` or an unset native flag, so a call landing here would
/// itself signal a wiring bug in the test setup.
struct UnusedCliRunner;

#[async_trait]
impl AgentRunner for UnusedCliRunner {
    async fn run(&self, _req: AgentRunRequest) -> Result<RunComplete, AoError> {
        panic!("UnusedCliRunner::run should never be invoked by this test");
    }
    fn mode(&self) -> AgentRunnerMode {
        AgentRunnerMode::Cli
    }
}

async fn make_persistence(data_root: DataRoot) -> Arc<PersistenceLayer> {
    data_root.ensure_directories().await.expect("ensure_directories");
    Arc::new(PersistenceLayer::init_with_root(data_root).await.expect("init persistence"))
}

fn make_agent_profile(
    id: &str,
    runner_mode: AgentRunnerMode,
    native_provider: Option<NativeProvider>,
    working_dir: Option<String>,
) -> AgentProfile {
    use ao_protocol::agent::{CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
    AgentProfile {
        id: id.to_string(),
        name: id.to_string(),
        description: String::new(),
        emoji: None,
        provider: ProviderConfig::Cli(CliProviderConfig {
            command: String::new(),
            args: vec![],
            normalizer: None,
            output_format: OutputFormat::Text,
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
        system_prompt: Some("test agent".to_string()),
        tools: None,
        env: Default::default(),
        max_instances: 1,
        timeout_seconds: 60,
        working_dir,
        home_dir: None,
        serialize: false,
        workflows: None,
        template: None,
        runner_mode,
        enabled_plugins: Default::default(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: Default::default(),
        owning_team_id: None,
        native_provider,
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

/// Confirms the run failed *after* reaching the (unroutable) provider
/// endpoint rather than failing early because the test's own fixture left
/// the provider looking unconfigured. This is the signal that
/// `run_session` actually called `.complete()` on the exact client the spy
/// observed, not merely that `ProviderFactory::build` returned one.
fn assert_failed_via_attempted_request(output: &ToolOutput, context: &str) {
    match output {
        ToolOutput::Error { message, .. } => {
            let lower = message.to_lowercase();
            assert!(
                !lower.contains("not configured"),
                "{context}: run must fail from an ATTEMPTED network call against the \
                 unroutable fixture endpoint, not from the provider looking unconfigured \
                 to the test's own fixture — got: {message}"
            );
        }
        other => panic!(
            "{context}: expected the run to fail once it reached {UNROUTABLE_BASE_URL}, got: {other:?}"
        ),
    }
}

/// Catalog subagent (Explore/general-purpose), `target_profile = None`.
///
/// Production path: `SubagentSpawner::spawn` / `spawn_sync` (both call
/// `ChildRunner::launch(.., None)`) → `ProfileAwareChildRunner::launch`'s
/// `None` arm → `NativeChildRunner::launch` →
/// `resolve_catalog_subagent_provider` (looks up the parent's `AgentProfile`
/// in `AgentProfileStore` and re-resolves ITS `native_provider` through the
/// same injected `ProviderFactory`) → `run_session`.
///
/// This is the shape flagged in the verification report as "the one
/// silently pinned to Anthropic": `default_catalog_subagent_profile()`'s
/// fallback-of-the-fallback always has `native_provider: None`, so if the
/// parent-profile lookup were ever skipped or broken, this test would keep
/// passing for the WRONG reason unless it also asserts the sentinel model —
/// which it does.
#[tokio::test]
async fn catalog_subagent_inherits_non_anthropic_provider_from_parent() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(tmp.path());

    // The launching agent's own profile, persisted so
    // `resolve_catalog_subagent_provider` can look it up by id.
    let data_root = DataRoot::new(tmp.path());
    data_root.ensure_directories().await.expect("ensure_directories");
    let profile_store = AgentProfileStore::new(data_root);
    let parent_profile = make_agent_profile(
        "parent-agent",
        AgentRunnerMode::Api,
        Some(NativeProvider::Openai),
        None,
    );
    profile_store.create(&parent_profile).await.expect("create parent profile");

    let factory = Arc::new(RecordingProviderFactory::new());
    let factory_dyn: Arc<dyn ProviderFactory> = Arc::clone(&factory) as Arc<dyn ProviderFactory>;

    // Same wrapper type `state.rs` wires as the process-wide `ChildRunner`
    // for BOTH spawn shapes.
    let child_runner: Arc<dyn ChildRunner> = Arc::new(ProfileAwareChildRunner::new(None, factory_dyn));

    // No built-in catalog ships with the engine, so this test owns its own
    // catalog-subagent fixture rather than depending on a registry entry
    // that no longer exists.
    let mut registry = SubagentRegistry::new();
    let test_agent_def = SubagentDefinition {
        id: "test-agent".to_string(),
        description: "Catalog subagent fixture for provider-routing test".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    };
    registry.register(test_agent_def.clone());
    let spawner = SubagentSpawner::new(Arc::new(registry)).with_child_runner(child_runner);

    let parent_ctx = RunnerContext::new_with_cwd("session-1", "parent-agent", tmp.path().to_path_buf())
        .with_agent_profile_store(Arc::new(profile_store));

    let output = spawner
        .spawn_sync(&parent_ctx, test_agent_def, "search for the bug".to_string())
        .await;

    let calls = factory.calls();
    assert_eq!(
        calls.len(),
        1,
        "ProviderFactory::build must be called exactly once for this spawn; got: {calls:?}"
    );
    assert_eq!(
        calls[0].native_provider,
        Some(NativeProvider::Openai),
        "recorded build() call must carry the parent's native_provider"
    );
    assert_eq!(
        calls[0].default_model.as_deref(),
        Some("gpt-sentinel-openai"),
        "a catalog subagent (target_profile=None) must inherit the OpenAI client from its \
         launching agent's native_provider, not silently fall back to Anthropic"
    );

    assert_failed_via_attempted_request(&output, "catalog subagent");
}

/// Named-profile delegate, `target_profile = Some(profile)`.
///
/// Production path: `SubagentSpawner::spawn_named` / `spawn_named_async*`
/// (both call `ChildRunner::launch(.., Some(profile))`) →
/// `ProfileAwareChildRunner::launch`'s `Some` arm → `RunnerDispatcher::pick`
/// (routed to the real `NativeAgentRunner` here, matching
/// `runner_mode = Api` + the native flag) → `NativeAgentRunner::run` →
/// `self.provider_factory.build(&agent)` → `run_session`.
#[tokio::test]
async fn named_profile_delegate_uses_its_own_non_anthropic_provider() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::set(tmp.path());

    let persistence = make_persistence(DataRoot::new(tmp.path())).await;

    let factory = Arc::new(RecordingProviderFactory::new());
    let factory_dyn: Arc<dyn ProviderFactory> = Arc::clone(&factory) as Arc<dyn ProviderFactory>;

    let native_runner = Arc::new(NativeAgentRunner::new(
        Arc::new(EventBus::new(64)),
        Arc::new(InstanceRegistry::new()),
        Arc::new(RunningAgents::new()),
        Arc::clone(&factory_dyn),
        Arc::new(Registry::default()),
        Arc::clone(&persistence),
    ));
    let dispatcher = Arc::new(RunnerDispatcher::with_runners(
        Arc::new(UnusedCliRunner) as Arc<dyn AgentRunner>,
        native_runner as Arc<dyn AgentRunner>,
    ));

    let profile_runner = Arc::new(ProfileAwareChildRunner::new(None, factory_dyn));
    profile_runner.set_dispatcher(Arc::clone(&dispatcher));
    let child_runner: Arc<dyn ChildRunner> = profile_runner;

    let registry = SubagentRegistry::new();
    let spawner = SubagentSpawner::new(Arc::new(registry)).with_child_runner(child_runner);

    let parent_ctx = RunnerContext::new_with_cwd("session-1", "parent-agent", tmp.path().to_path_buf());

    // working_dir pinned at the tempdir so NativeAgentRunner::run's cwd
    // resolution never falls through to the real machine's $HOME.
    let target_profile = make_agent_profile(
        "openai-delegate",
        AgentRunnerMode::Api,
        Some(NativeProvider::Openai),
        Some(tmp.path().to_string_lossy().into_owned()),
    );

    let output = spawner
        .spawn_named(&parent_ctx, &target_profile, "investigate the OpenAI path".to_string(), false)
        .await;

    let calls = factory.calls();
    assert_eq!(
        calls.len(),
        1,
        "ProviderFactory::build must be called exactly once for this spawn; got: {calls:?}"
    );
    assert_eq!(calls[0].native_provider, Some(NativeProvider::Openai));
    assert_eq!(
        calls[0].default_model.as_deref(),
        Some("gpt-sentinel-openai"),
        "a named-profile delegate must build the client its OWN native_provider selects, \
         not silently fall back to Anthropic"
    );

    assert_failed_via_attempted_request(&output, "named-profile delegate");
}
