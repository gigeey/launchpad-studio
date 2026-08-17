use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use ao_engine_tools_core::SessionKind;
use ao_protocol::agent::{AgentId, AgentProfile};
use ao_protocol::attachment::Attachment;
use ao_protocol::error::AoError;

pub mod channel_gating;
pub mod shared;
pub mod system_prompt;
mod cli;
mod timeline_adapter;
mod native;
mod profile_child_runner;
#[allow(unused)]
mod tests;

pub use ao_protocol::agent::AgentRunnerMode;
pub use channel_gating::{compute_tool_admission, is_channel_bridge_thread, CHANNEL_BLOCKED_TOOLS};
pub use cli::CliAgentRunner;
pub use cli::{RunComplete, RunScope, WorkflowFollowup};
pub use timeline_adapter::TimelineAdapter;
pub use native::{NativeAgentRunner, DefaultProviderFactory, NativeChildRunner, ProviderFactory, BACKGROUND_AGENT_CAP};
pub use profile_child_runner::ProfileAwareChildRunner;

/// Transitional dispatch surface. This trait exists so NativeAgentRunner can
/// land alongside the existing CLI spawn path and be dogfooded side-by-side.
/// It retires once CLI-as-ProviderClient lands and both top-level
/// runners collapse into a single unified loop. Keep the surface minimal.
#[async_trait]
pub trait AgentRunner: Send + Sync {
    async fn run(&self, request: AgentRunRequest) -> Result<RunComplete, AoError>;
    fn mode(&self) -> AgentRunnerMode;
}

/// The fields the dispatcher hands across to a runner per request.
/// Deliberately minimal — only what the dispatcher actually needs.
pub struct AgentRunRequest {
    pub agent: AgentProfile,
    pub prompt: String,
    pub attachments: Vec<Attachment>,
    pub run_complete_tx: mpsc::Sender<RunComplete>,
    pub focus_path: Option<String>,
    pub scope: RunScope,
    pub thread_id: Option<String>,
    /// Whether a human operator is attending this session.
    ///
    /// Defaults to `Interactive`. Callers that dispatch unattended runs
    /// (scheduled tasks, tasklist workers, background subagents) set this
    /// to `Autonomous` so the runner enables the appropriate toolset,
    /// system-prompt section, drain priority, and permission resolution.
    pub session_kind: SessionKind,
    /// `run_id` the caller already allocated *and* registered in the
    /// [`InstanceRegistry`] before spawning the runner task. When `Some`,
    /// the runner adopts this id verbatim and skips its own
    /// `register_run` — leaving cleanup to the caller's RAII guard.
    ///
    /// Exists to close a race in the per-agent queue manager: with
    /// `max_instances = 1`, the queue's `pump()` loop checked
    /// `can_spawn()` (true), called `tokio::spawn`, then immediately
    /// re-checked `can_spawn()` *before* the just-spawned runner had a
    /// chance to await its own async `register_run`. The second check
    /// still observed zero active runs and over-spawned — producing
    /// concurrent runs under a `max_instances = 1` policy. Pre-allocating
    /// the id at the queue manager and registering synchronously before
    /// the spawn closes the window: by the time the next loop iteration
    /// re-checks `can_spawn()`, the slot is already booked.
    ///
    /// Callers who don't pre-register (direct test invocations, the
    /// tasklist dispatcher path, etc.) leave this `None` and the runner
    /// generates + registers a fresh `run_id` as before. Both paths
    /// converge on the same `InstanceRegistry` cleanup contract.
    pub pre_registered_run_id: Option<String>,

    // ── Delegation propagation fields ────────────────────────────────────────
    // All default to empty/None/false so existing top-level call sites are
    // byte-equivalent after adding `..Default::default()`.

    /// Chain of agent IDs that delegated to reach this request, outermost first.
    /// Propagated into `RunnerContext.delegate_chain` so depth/cycle checks
    /// inside the run see the full ancestry, not just local depth-0.
    pub delegate_chain: Vec<String>,
    /// Chain of agent IDs that spawned (Task tool) to reach this request.
    pub spawn_chain: Vec<String>,
    /// Pre-computed delegation depth (= `delegate_chain.len()` at call time).
    /// Used directly when there is no live parent session to infer from.
    pub depth: usize,
    /// Session ID of the parent agent that triggered this delegation, if any.
    pub parent_session_id: Option<String>,
    /// Agent ID of the delegating parent, if any.
    pub parent_agent_id: Option<String>,
    /// Working directory of the delegating parent at delegation time, if any.
    pub parent_current_cwd: Option<String>,
    /// When `Some`, the runner wires this token into `RunHandle.cancel` instead
    /// of minting a fresh one, so a parent `DelegateStop` or cancel propagates
    /// to the child without requiring a separate bridge task.
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    /// When `true`, skip loading the agent's personal history before the run.
    /// Used for fresh delegations that must not resume the target's prior
    /// conversation — the child starts with an empty transcript.
    pub isolate_history: bool,
    /// When `Some`, every transcript entry this run produces is appended to
    /// this file instead of the agent's personal transcript. Delegation paths
    /// set this to the child's own sidechain file
    /// (`messages/data/<delegation_id>.jsonl`) so a delegated run — including
    /// a clone-parent delegate that shares the caller's agent id — never
    /// bleeds into the profile owner's chat history. When `None` and
    /// `isolate_history` is set, the runner skips personal-transcript
    /// persistence entirely rather than fall back to the personal file.
    pub transcript_override: Option<std::path::PathBuf>,
    /// When `Some`, all live event-bus emissions for this run use this channel
    /// id instead of the agent's own id (e.g. `delegate:<delegation_id>`), so
    /// a delegated child's streaming output does not render in the parent's
    /// or target profile's chat feed. Transcript persistence is unaffected.
    pub event_channel: Option<String>,
    /// When `true`, this run skips registering in the shared
    /// [`crate::instance_registry::InstanceRegistry`] under the agent's own
    /// key, so it never counts against that agent's `max_instances` slot and
    /// never lights up the sidebar "busy" overlay. Defaults to `false` for
    /// every existing caller — byte-equivalent behavior. Opt-in only for a
    /// background poll that must never contend with, or be mistaken for, the
    /// agent's own live turn (see `agent_watch::LiveAgentWatchDetector`).
    /// Honored in `CliAgentRunner::run` and `NativeAgentRunner::run`, the
    /// only two places that register a run in the instance registry.
    pub bypass_instance_cap: bool,
}

impl Default for AgentRunRequest {
    fn default() -> Self {
        use ao_protocol::agent::{
            AgentRunnerMode, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
        };
        let (tx, _) = mpsc::channel(1);
        Self {
            agent: AgentProfile {
                id: String::new(),
                name: String::new(),
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
                    no_output_timeout_ms: 30000,
                    file_capabilities: None,
                }),
                model: None,
                skills: vec![],
                system_prompt: None,
                tools: None,
                env: Default::default(),
                max_instances: 1,
                timeout_seconds: 300,
                working_dir: None,
                home_dir: None,
                serialize: false,
                workflows: None,
                template: None,
                runner_mode: AgentRunnerMode::Cli,
                enabled_plugins: Default::default(),
                enabled_launchpad_global_skills: None,
                enabled_launchpad_project_skills: Default::default(),
                owning_team_id: None,
                native_provider: None,
                thinking: None,
                delegates_to: vec![],
                persona: None,
                special_instructions: None,
                legacy_system_prompt: None,
                max_delegation_depth: None,
                channels: vec![],
                            max_output_tokens: None,
                max_context_tokens: None,
                reasoning_effort: None,
                max_turns: None,
},
            prompt: String::new(),
            attachments: vec![],
            run_complete_tx: tx,
            focus_path: None,
            scope: RunScope::Standalone,
            thread_id: None,
            session_kind: SessionKind::Autonomous,
            pre_registered_run_id: None,
            delegate_chain: vec![],
            spawn_chain: vec![],
            depth: 0,
            parent_session_id: None,
            parent_agent_id: None,
            parent_current_cwd: None,
            cancel: None,
            isolate_history: false,
            transcript_override: None,
            event_channel: None,
            bypass_instance_cap: false,
        }
    }
}

/// Unique per-registration identifier minted by `RunningAgents::insert`.
/// Distinct from the runner's `run_id` (which is internal to the agent run)
/// so we can hand the right handle back to the RAII guard without having to
/// plumb the runner's run_id all the way up to the trait entry point.
pub type RunRegistrationId = String;

/// Per-run handle stored in `RunningAgents`.
///
/// `agent_id` is carried in the value rather than the map key so that two
/// concurrent runs under the same agent identity (e.g. a parent agent and a
/// tasklist subtask that share that agent's profile) can coexist as separate
/// entries — the previous agent-id-keyed map would overwrite one with the
/// other and leave the parent uncancellable once the subtask's RAII guard
/// removed the surviving entry.
///
/// `thread_id` scopes the handle to the UI conversation thread it belongs to
/// (`None` for the default/no-thread conversation, and for non-threaded
/// contexts like scheduled or autonomous runs). It is propagated verbatim
/// from `AgentRunRequest.thread_id`, which delegate/subtask spawns already
/// inherit from their parent context — so a cancel scoped to `(agent_id,
/// thread_id)` still sweeps subtasks spawned within that thread, without
/// also hitting an unrelated concurrent thread for the same agent. See
/// `RunningAgents::cancel`.
#[derive(Clone)]
pub struct RunHandle {
    pub agent_id: AgentId,
    pub thread_id: Option<String>,
    pub cancel: CancellationToken,
    pub runner_mode: AgentRunnerMode,
    pub started_at: DateTime<Utc>,
}

/// In-flight run registry. Keyed by a unique registration id so multiple
/// concurrent runs sharing an `agent_id` (parent + subtask, coordinator
/// self-dispatch, etc.) all retain their own cancellation token.
///
/// `cancel(&agent_id, thread_id)` is exact-match on both fields, not a
/// fan-out across every run sharing `agent_id`: a user stopping the thread
/// they're looking at must not tear down an unrelated concurrent thread for
/// the same agent. Cascading to spawned subtasks still works without a
/// fan-out, via two independent mechanisms: (1) delegated children reuse
/// their parent's `CancellationToken` directly (see `AgentRunRequest.cancel`
/// and its use in `cli.rs`/`native.rs`), and (2) same-agent subtasks inherit
/// their parent's `thread_id`, so the exact-match still covers them.
pub struct RunningAgents {
    inner: DashMap<RunRegistrationId, RunHandle>,
}

impl RunningAgents {
    pub fn new() -> Self {
        Self {
            inner: DashMap::new(),
        }
    }

    /// Register a new run. Returns the registration id the caller must hand
    /// to `RunningAgentsGuard` (and to `remove` if doing manual cleanup) so
    /// the right entry is dropped on exit, even if other runs for the same
    /// agent are still in flight.
    pub fn insert(&self, handle: RunHandle) -> RunRegistrationId {
        let reg_id = Uuid::new_v4().to_string();
        self.inner.insert(reg_id.clone(), handle);
        reg_id
    }

    /// Remove a previously inserted entry by its registration id.
    pub fn remove(&self, reg_id: &RunRegistrationId) -> Option<RunHandle> {
        self.inner.remove(reg_id).map(|(_, h)| h)
    }

    /// Fire the cancellation token for every active run that exactly matches
    /// `agent_id` AND `thread_id`. Returns true if at least one handle was
    /// found and fired, false otherwise.
    ///
    /// Exact-match, not a fan-out (see struct doc): stopping the thread a
    /// user is looking at must not cancel a different concurrent thread for
    /// the same agent. `thread_id: None` matches only handles that are
    /// themselves on the default/no-thread conversation — it is not a
    /// wildcard. Subtask/delegation cascade still works without fanning out
    /// across threads: inherited `thread_id` keeps same-thread subtasks in
    /// the exact-match, and shared-token propagation covers the rest.
    pub fn cancel(&self, agent_id: &AgentId, thread_id: Option<&str>) -> bool {
        let mut fired = false;
        for entry in self.inner.iter() {
            let handle = entry.value();
            if &handle.agent_id == agent_id && handle.thread_id.as_deref() == thread_id {
                handle.cancel.cancel();
                fired = true;
            }
        }
        fired
    }

    /// One entry per distinct active agent identity. If an agent has
    /// multiple concurrent runs, reports the runner mode of one of them
    /// (they share a profile, so the mode is the same in practice).
    pub fn list_modes(&self) -> Vec<(AgentId, AgentRunnerMode)> {
        let mut by_agent: std::collections::HashMap<AgentId, AgentRunnerMode> =
            std::collections::HashMap::new();
        for entry in self.inner.iter() {
            by_agent
                .entry(entry.value().agent_id.clone())
                .or_insert(entry.value().runner_mode);
        }
        by_agent.into_iter().collect()
    }
}

impl Default for RunningAgents {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard that removes a specific run's entry from `RunningAgents` on
/// drop. The guard holds the unique `RunRegistrationId` returned by
/// `insert`, not the agent id — that's what keeps a parent's entry alive
/// when its subtask finishes (and vice versa).
pub struct RunningAgentsGuard {
    agents: Arc<RunningAgents>,
    reg_id: RunRegistrationId,
}

impl RunningAgentsGuard {
    pub fn new(agents: Arc<RunningAgents>, reg_id: RunRegistrationId) -> Self {
        Self { agents, reg_id }
    }
}

impl Drop for RunningAgentsGuard {
    fn drop(&mut self) {
        self.agents.remove(&self.reg_id);
    }
}

/// Picks the right `AgentRunner` impl per request based solely on
/// `agent.runner_mode`. Both runners are always constructed at startup (per
/// deliberate); `runner_mode` alone decides which one a given agent's
/// runs are handed to — there is no other gate.
pub struct RunnerDispatcher {
    cli: Arc<dyn AgentRunner>,
    native: Arc<dyn AgentRunner>,
}

impl RunnerDispatcher {
    /// Production constructor.
    pub fn new(cli: Arc<CliAgentRunner>, native: Arc<NativeAgentRunner>) -> Self {
        Self {
            cli: cli as Arc<dyn AgentRunner>,
            native: native as Arc<dyn AgentRunner>,
        }
    }

    /// Test constructor: builds a dispatcher directly from trait objects so
    /// tests can inject stub `AgentRunner` impls instead of the concrete
    /// `CliAgentRunner`/`NativeAgentRunner` types `new()` requires.
    pub fn with_runners(cli: Arc<dyn AgentRunner>, native: Arc<dyn AgentRunner>) -> Self {
        Self { cli, native }
    }

    /// Pick the runner for this agent: `runner_mode` decides,
    /// no fallback.
    pub fn pick(&self, agent: &AgentProfile) -> Arc<dyn AgentRunner> {
        match agent.runner_mode {
            AgentRunnerMode::Cli => Arc::clone(&self.cli),
            AgentRunnerMode::Api => Arc::clone(&self.native),
        }
    }
}

// Re-export shared pure helpers for downstream consumers that import from
// crate::agent_runner (e.g. tests in lib.rs that use build_memory_block).
pub use shared::{
    augment_prompt_with_attachments, build_memory_block, build_workflow_block,
};

#[cfg(test)]
mod dispatcher_tests {
    use super::*;

    struct TestRunner {
        mode: AgentRunnerMode,
    }

    #[async_trait]
    impl AgentRunner for TestRunner {
        async fn run(&self, _req: AgentRunRequest) -> Result<RunComplete, AoError> {
            unimplemented!("test-only stub")
        }
        fn mode(&self) -> AgentRunnerMode {
            self.mode
        }
    }

    fn cli_runner() -> Arc<dyn AgentRunner> {
        Arc::new(TestRunner { mode: AgentRunnerMode::Cli })
    }

    fn api_runner() -> Arc<dyn AgentRunner> {
        Arc::new(TestRunner { mode: AgentRunnerMode::Api })
    }

    fn make_agent(runner_mode: AgentRunnerMode) -> AgentProfile {
        use ao_protocol::agent::{CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
        AgentProfile {
            id: "test".to_string(),
            name: "test".to_string(),
            description: "".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
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
                no_output_timeout_ms: 30000,
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
            runner_mode,
            enabled_plugins: Default::default(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            max_turns: None,
}
    }

    #[test]
    fn dispatcher_picks_cli_for_cli_mode() {
        let cli = cli_runner();
        let native = api_runner();
        let d = RunnerDispatcher::with_runners(Arc::clone(&cli), Arc::clone(&native));
        let runner = d.pick(&make_agent(AgentRunnerMode::Cli));
        assert_eq!(runner.mode(), AgentRunnerMode::Cli);
    }

    #[test]
    fn dispatcher_picks_native_for_api_mode() {
        let cli = cli_runner();
        let native = api_runner();
        let d = RunnerDispatcher::with_runners(Arc::clone(&cli), Arc::clone(&native));
        let runner = d.pick(&make_agent(AgentRunnerMode::Api));
        assert_eq!(runner.mode(), AgentRunnerMode::Api);
    }

    /// Reachability guard: proves an `Api`-mode agent actually reaches
    /// `NativeAgentRunner` through the same construction path production
    /// uses — `RunnerDispatcher::new()` fed real `CliAgentRunner` and
    /// `NativeAgentRunner` instances — with no env var of any kind set.
    /// `RunnerDispatcher::new()` used to read a feature-flag env var here;
    /// a unit test built via the test-only `with_runners()` constructor
    /// would keep passing even if that flag check silently came back, since
    /// `with_runners()` never touches the environment. Exercising the real
    /// `new()` is what would have caught that class of bug, and is what
    /// keeps it caught.
    #[tokio::test]
    async fn dispatcher_new_routes_api_mode_to_native_runner_with_no_env_var_set() {
        use ao_engine_tools_core::Registry;
        use ao_normalizer::registry::NormalizerRegistry;
        use ao_persistence::paths::DataRoot;
        use ao_persistence::PersistenceLayer;
        use ao_process::mock::MockProcessSupervisor;
        use ao_process::supervisor::ProcessSupervisor;
        use crate::command_queue::CommandQueue;
        use crate::event_bus::EventBus;
        use crate::instance_registry::InstanceRegistry;

        // Deliberately no env var of any kind is set here — this test's
        // entire purpose is proving `pick()` no longer consults one.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let data_root = DataRoot::new(tmp.path());
        data_root.ensure_directories().await.expect("ensure_directories");
        let persistence = Arc::new(
            PersistenceLayer::init_with_root(data_root).await.expect("init persistence"),
        );

        let event_bus = Arc::new(EventBus::new(64));
        let supervisor: Arc<dyn ProcessSupervisor> = Arc::new(MockProcessSupervisor::new(vec![]));
        let normalizer_registry = Arc::new(NormalizerRegistry::new());
        let command_queue = Arc::new(CommandQueue::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let running_agents = Arc::new(RunningAgents::new());
        let tools_registry = Arc::new(Registry::new());

        let cli_runner = Arc::new(CliAgentRunner::new(
            supervisor,
            normalizer_registry,
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
            command_queue,
            Arc::clone(&instance_registry),
            Arc::clone(&running_agents),
            Arc::clone(&tools_registry),
        ));

        let native_runner = Arc::new(NativeAgentRunner::new(
            event_bus,
            instance_registry,
            running_agents,
            Arc::new(DefaultProviderFactory),
            tools_registry,
            persistence,
        ));

        // The production constructor — no test-only flag injection.
        let dispatcher = RunnerDispatcher::new(cli_runner, native_runner);

        let runner = dispatcher.pick(&make_agent(AgentRunnerMode::Api));
        assert_eq!(
            runner.mode(),
            AgentRunnerMode::Api,
            "an Api-mode agent must reach NativeAgentRunner through RunnerDispatcher::new() \
             with no env var set"
        );
    }

    #[test]
    fn running_agents_insert_remove_round_trip() {
        let ra = RunningAgents::new();
        let token = CancellationToken::new();
        let reg_id = ra.insert(RunHandle {
            agent_id: "agent-1".to_string(),
            thread_id: None,
            cancel: token,
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        });
        let removed = ra.remove(&reg_id);
        assert!(removed.is_some());
        // After removal, cancel returns false.
        assert!(!ra.cancel(&"agent-1".to_string(), None));
    }

    #[test]
    fn running_agents_cancel_fires_token() {
        let ra = RunningAgents::new();
        let token = CancellationToken::new();
        let child = token.child_token();
        ra.insert(RunHandle {
            agent_id: "agent-2".to_string(),
            thread_id: None,
            cancel: token,
            runner_mode: AgentRunnerMode::Api,
            started_at: Utc::now(),
        });
        let fired = ra.cancel(&"agent-2".to_string(), None);
        assert!(fired);
        assert!(child.is_cancelled());
    }

    #[test]
    fn running_agents_guard_cleans_on_drop() {
        let ra = Arc::new(RunningAgents::new());
        let token = CancellationToken::new();
        let reg_id = ra.insert(RunHandle {
            agent_id: "agent-3".to_string(),
            thread_id: None,
            cancel: token,
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        });
        {
            let _guard = RunningAgentsGuard::new(Arc::clone(&ra), reg_id);
        } // guard drops here
        assert!(!ra.cancel(&"agent-3".to_string(), None));
    }

    #[test]
    fn running_agents_guard_cleans_on_panic() {
        let ra = Arc::new(RunningAgents::new());
        let token = CancellationToken::new();
        let reg_id = ra.insert(RunHandle {
            agent_id: "agent-4".to_string(),
            thread_id: None,
            cancel: token,
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        });
        let ra_clone = Arc::clone(&ra);
        // DashMap uses UnsafeCell which is not RefUnwindSafe; assert safety
        // explicitly — the map is not mutated concurrently during this test.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = RunningAgentsGuard::new(Arc::clone(&ra_clone), reg_id);
            panic!("intentional panic for guard test");
        }));
        // Guard must have dropped despite the panic.
        assert!(!ra.cancel(&"agent-4".to_string(), None));
    }

    #[test]
    fn running_agents_keeps_concurrent_runs_for_same_agent_id_independent() {
        // Regression: a tasklist subtask runs under the same agent_id as its
        // parent. The old map (keyed by agent_id) overwrote the parent's
        // handle on subtask insert, and the subtask's RAII guard then removed
        // the surviving entry — stranding the parent without a cancel
        // pathway. The new key (per-registration id) keeps both alive.
        let ra = Arc::new(RunningAgents::new());

        let parent_token = CancellationToken::new();
        let parent_child = parent_token.child_token();
        let parent_reg = ra.insert(RunHandle {
            agent_id: "shared-agent".to_string(),
            thread_id: None,
            cancel: parent_token,
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        });

        let subtask_token = CancellationToken::new();
        let subtask_child = subtask_token.child_token();
        let subtask_reg = ra.insert(RunHandle {
            agent_id: "shared-agent".to_string(),
            thread_id: None,
            cancel: subtask_token,
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        });

        assert_ne!(parent_reg, subtask_reg);

        // Subtask completes — its RAII guard removes only its own entry.
        ra.remove(&subtask_reg);

        // Cancelling the agent must still reach the parent, which is the
        // exact case the old keying broke.
        let fired = ra.cancel(&"shared-agent".to_string(), None);
        assert!(fired);
        assert!(parent_child.is_cancelled());
        // The subtask's token was created independently and was never
        // signalled (its handle was already gone before cancel ran).
        assert!(!subtask_child.is_cancelled());
    }

    #[test]
    fn running_agents_cancel_reaches_all_runs_sharing_agent_and_thread() {
        // Sibling case: parent + in-flight subtask share an agent_id AND a
        // thread_id (subtasks inherit their parent's thread_id — see
        // spawner.rs). A user-initiated cancel of that thread should reach
        // both so neither half of the dispatch tree is left orphaned.
        let ra = RunningAgents::new();

        let t1 = CancellationToken::new();
        let c1 = t1.child_token();
        ra.insert(RunHandle {
            agent_id: "shared-agent".to_string(),
            thread_id: Some("thread-a".to_string()),
            cancel: t1,
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        });

        let t2 = CancellationToken::new();
        let c2 = t2.child_token();
        ra.insert(RunHandle {
            agent_id: "shared-agent".to_string(),
            thread_id: Some("thread-a".to_string()),
            cancel: t2,
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        });

        // A different agent in flight — must NOT be cancelled.
        let t_other = CancellationToken::new();
        let c_other = t_other.child_token();
        ra.insert(RunHandle {
            agent_id: "other-agent".to_string(),
            thread_id: Some("thread-a".to_string()),
            cancel: t_other,
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        });

        assert!(ra.cancel(&"shared-agent".to_string(), Some("thread-a")));
        assert!(c1.is_cancelled());
        assert!(c2.is_cancelled());
        assert!(!c_other.is_cancelled());
    }

    #[test]
    fn running_agents_cancel_does_not_cross_threads() {
        // Regression for the reported bug: the same agent has two
        // concurrent runs on two different threads. Cancelling one thread
        // must not touch the other.
        let ra = RunningAgents::new();

        let t_a = CancellationToken::new();
        let c_a = t_a.child_token();
        ra.insert(RunHandle {
            agent_id: "agent-1".to_string(),
            thread_id: Some("thread-a".to_string()),
            cancel: t_a,
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        });

        let t_b = CancellationToken::new();
        let c_b = t_b.child_token();
        ra.insert(RunHandle {
            agent_id: "agent-1".to_string(),
            thread_id: Some("thread-b".to_string()),
            cancel: t_b,
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        });

        assert!(ra.cancel(&"agent-1".to_string(), Some("thread-a")));
        assert!(c_a.is_cancelled());
        assert!(!c_b.is_cancelled());
    }

    #[test]
    fn running_agents_cancel_default_thread_is_not_a_wildcard() {
        // `thread_id: None` (the default/no-thread conversation) must be
        // exact-matched, not treated as "cancel every thread for this
        // agent". Otherwise stopping the default thread — the most common
        // case — would still reproduce the reported cross-thread bug.
        let ra = RunningAgents::new();

        let t_default = CancellationToken::new();
        let c_default = t_default.child_token();
        ra.insert(RunHandle {
            agent_id: "agent-1".to_string(),
            thread_id: None,
            cancel: t_default,
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        });

        let t_named = CancellationToken::new();
        let c_named = t_named.child_token();
        ra.insert(RunHandle {
            agent_id: "agent-1".to_string(),
            thread_id: Some("thread-a".to_string()),
            cancel: t_named,
            runner_mode: AgentRunnerMode::Cli,
            started_at: Utc::now(),
        });

        assert!(ra.cancel(&"agent-1".to_string(), None));
        assert!(c_default.is_cancelled());
        assert!(!c_named.is_cancelled());
    }

    #[test]
    fn running_agents_list_modes_dedupes_by_agent_id() {
        // Two concurrent runs for the same agent_id should surface once in
        // list_modes — the UI signal it feeds is "this agent has activity",
        // not "this agent has N activities".
        let ra = RunningAgents::new();
        for _ in 0..2 {
            ra.insert(RunHandle {
                agent_id: "agent-x".to_string(),
                thread_id: None,
                cancel: CancellationToken::new(),
                runner_mode: AgentRunnerMode::Cli,
                started_at: Utc::now(),
            });
        }
        ra.insert(RunHandle {
            agent_id: "agent-y".to_string(),
            thread_id: None,
            cancel: CancellationToken::new(),
            runner_mode: AgentRunnerMode::Api,
            started_at: Utc::now(),
        });
        let modes = ra.list_modes();
        assert_eq!(modes.len(), 2);
        let mut by_agent: std::collections::HashMap<_, _> = modes.into_iter().collect();
        assert_eq!(by_agent.remove("agent-x"), Some(AgentRunnerMode::Cli));
        assert_eq!(by_agent.remove("agent-y"), Some(AgentRunnerMode::Api));
    }
}
