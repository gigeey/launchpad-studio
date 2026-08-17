use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use ao_engine_tools_core::{
    FormBridge, NoopDenialTracker, PermissionMode, Registry, RunnerContext, SessionKind,
    TasklistServiceHandle, WorkflowRunnerHandle,
};
use ao_engine_tools_core::background_agents::{
    BackgroundAgentId, BackgroundAgentRegistry, ChildRunner, RunnerEvent, TaskFinalReport,
};
use ao_engine_tools_runner::{
    message::{ContentBlock, Message},
    prompt_bridge::{FormBridgeRegistry, LiveFormBridge, LivePermissionBridge, StubBridge, UserPromptBridge},
    provider::{
        resolve_max_context_tokens, resolve_max_output_tokens, resolve_model, resolve_reasoning_effort,
        ProviderClient, ProviderError,
    },
    query_loop::{run_session, RunnerConfig, SessionOutcome},
    hooks::config::load_runner_settings,
};
use ao_engine_tools_provider_anthropic::AnthropicClient;
use ao_engine_tools_provider_openai::OpenAIClient;
use ao_persistence::PersistenceLayer;
use tokio::sync::broadcast;
use ao_protocol::agent::{AgentId, AgentProfile, AgentRunnerMode};
use crate::history::anchor::WindowAnchorRegistry;
use ao_protocol::error::AoError;
use ao_protocol::event::{AgentEventPayload, RunEndReason};
use ao_protocol::outcome::ArtifactRef;
use ao_protocol::reflection_trigger::{NoopReflectionSubscriber, ReflectionTriggerSubscriber};

use crate::agent_runner::channel_gating::{compute_tool_admission, is_channel_bridge_thread, CHANNEL_BLOCKED_TOOLS};
use crate::agent_runner::{
    AgentRunRequest, AgentRunner, RunComplete, RunHandle, RunScope, RunningAgents, RunningAgentsGuard,
};
use crate::agent_runner::shared::augment_prompt_with_attachments;
use crate::agent_runner::TimelineAdapter;
use crate::event_bus::{EventBus, EventBusAgentSink};
use crate::instance_registry::{InstanceRegistry, InstanceRegistryGuard};
use crate::mcp_session::McpSessionStore;

/// Default cap for per-session background agents.
pub const BACKGROUND_AGENT_CAP: usize = 8;

/// Builds a `ProviderClient` for a given `AgentProfile`. Inspects the profile's
/// `native_provider` to choose which provider impl to instantiate and its
/// `model` to select which model that client sends. Both `NativeAgentRunner`
/// (main loop) and `NativeChildRunner` (in-process subagents) call the same
/// injected `Arc<dyn ProviderFactory>`, so a provider added here is picked up
/// by both without further changes.
pub trait ProviderFactory: Send + Sync {
    fn build(&self, agent: &AgentProfile) -> Result<Arc<dyn ProviderClient>, ProviderError>;
}

/// Default `ProviderFactory` that reads `providers.toml` and builds the
/// matching provider client. Routes to OpenAI when `agent.native_provider =
/// Some(NativeProvider::Openai)`, to OpenRouter (via the same OpenAI-compatible
/// client, pointed at OpenRouter's `providers.toml` section) when
/// `Some(NativeProvider::OpenRouter)`; falls back to Anthropic otherwise.
///
/// Also resolves the four tuning knobs each client sends — `model`,
/// `max_output_tokens`, `max_context_tokens`, `reasoning_effort` — through
/// the matching `resolve_*` function ([`resolve_model`],
/// [`resolve_max_output_tokens`], [`resolve_max_context_tokens`],
/// [`resolve_reasoning_effort`]). Each picks the per-agent override when
/// set, otherwise the provider's own loaded default (`providers.toml`'s
/// persisted value, or the provider crate's hardcoded fallback), and the
/// result is stamped onto the client via the matching `with_*` builder
/// method before it's handed back. This is the single place that
/// resolution happens for the native path — every caller of `build` (the
/// main loop and, via [`NativeChildRunner`], the subagent path) gets it for
/// free.
pub struct DefaultProviderFactory;

impl ProviderFactory for DefaultProviderFactory {
    fn build(&self, agent: &AgentProfile) -> Result<Arc<dyn ProviderClient>, ProviderError> {
        use ao_protocol::agent::NativeProvider;
        match agent.native_provider {
            Some(NativeProvider::Openai) => {
                let client = OpenAIClient::from_loaded_config()
                    .map_err(|e| ProviderError::NotConfigured(e.to_string()))?;
                let model = resolve_model(agent.model.clone(), &client);
                let max_output_tokens = resolve_max_output_tokens(agent.max_output_tokens, &client);
                let max_context_tokens = resolve_max_context_tokens(agent.max_context_tokens, &client);
                let reasoning_effort = resolve_reasoning_effort(agent.reasoning_effort, &client);
                let client = client
                    .with_model(model)
                    .with_max_output_tokens(max_output_tokens)
                    .with_max_context_tokens(max_context_tokens)
                    .with_reasoning_effort(reasoning_effort);
                Ok(Arc::new(client) as Arc<dyn ProviderClient>)
            }
            Some(NativeProvider::OpenRouter) => {
                let client = OpenAIClient::from_loaded_config_openrouter()
                    .map_err(|e| ProviderError::NotConfigured(e.to_string()))?;
                let model = resolve_model(agent.model.clone(), &client);
                let max_output_tokens = resolve_max_output_tokens(agent.max_output_tokens, &client);
                let max_context_tokens = resolve_max_context_tokens(agent.max_context_tokens, &client);
                let reasoning_effort = resolve_reasoning_effort(agent.reasoning_effort, &client);
                let client = client
                    .with_model(model)
                    .with_max_output_tokens(max_output_tokens)
                    .with_max_context_tokens(max_context_tokens)
                    .with_reasoning_effort(reasoning_effort);
                Ok(Arc::new(client) as Arc<dyn ProviderClient>)
            }
            _ => {
                let client = AnthropicClient::from_loaded_config()
                    .map_err(|e| ProviderError::NotConfigured(e.to_string()))?;
                let model = resolve_model(agent.model.clone(), &client);
                let max_output_tokens = resolve_max_output_tokens(agent.max_output_tokens, &client);
                let max_context_tokens = resolve_max_context_tokens(agent.max_context_tokens, &client);
                let reasoning_effort = resolve_reasoning_effort(agent.reasoning_effort, &client);
                let client = client
                    .with_model(model)
                    .with_max_output_tokens(max_output_tokens)
                    .with_max_context_tokens(max_context_tokens)
                    .with_reasoning_effort(reasoning_effort);
                Ok(Arc::new(client) as Arc<dyn ProviderClient>)
            }
        }
    }
}

/// A [`ChildRunner`] that drives the built-in-catalog-subagent path
/// (Explore, general-purpose, ...) in-process against the native (API)
/// runner. [`ProfileAwareChildRunner`] is this type's only production
/// constructor, and its `launch` intercepts `Some(profile)` — named-profile
/// delegates — routing those through `RunnerDispatcher::pick` instead of
/// calling here. [`ProfileAwareChildRunner`] owns all profile-based
/// routing, so `target_profile` below is expected to always be `None`.
///
/// A catalog subagent carries no `AgentProfile` of its own, so its provider
/// is resolved by inheriting the *launching* agent's `native_provider`:
/// `child_ctx.parent_agent_id` is looked up in the shared
/// `AgentProfileStore` and, when found, resolved through the same injected
/// [`ProviderFactory`] every other path uses — so a subagent never silently
/// falls back to a different provider than the one its launching agent has
/// configured. When no parent profile can be resolved (root-level sessions,
/// most test fixtures with no store configured), resolution falls through
/// to [`ProviderFactory`]'s own default (Anthropic — see
/// `DefaultProviderFactory::build`'s default arm).
///
/// Used by the process-global [`SubagentSpawner`] so the spawner can be
/// constructed at `AppState::new` without requiring a configured provider
/// client upfront. The provider is resolved fresh on each spawn so a
/// `providers.toml` update mid-session is picked up automatically.
///
/// `mcp_sessions` is optional: when `Some`, each child session is registered
/// in the store before launch and deregistered on exit. When `None`
/// (tests that don't need session tracking), registration is skipped.
///
/// [`SubagentSpawner`]: ao_engine_tools_core::background_agents::SubagentSpawner
/// [`ProfileAwareChildRunner`]: crate::agent_runner::ProfileAwareChildRunner
pub struct NativeChildRunner {
    pub mcp_sessions: Option<Arc<McpSessionStore>>,
    provider_factory: Arc<dyn ProviderFactory>,
}

impl NativeChildRunner {
    pub fn new(mcp_sessions: Option<Arc<McpSessionStore>>, provider_factory: Arc<dyn ProviderFactory>) -> Self {
        Self { mcp_sessions, provider_factory }
    }
}

/// Resolves the provider (and turn cap) for a subagent spawn that carries no
/// `target_profile` — the built-in catalog path (Explore, general-purpose,
/// ...). See the [`NativeChildRunner`] doc for the inheritance/fallback rule
/// this implements: prefer the launching agent's own resolved provider,
/// falling through to [`ProviderFactory`]'s bare default when no parent
/// profile is available. The returned `Option<u32>` is whichever profile's
/// `max_turns` backed that resolution — still unresolved against
/// [`ao_protocol::agent::DEFAULT_MAX_TURNS`], which is the caller's job.
async fn resolve_catalog_subagent_provider(
    provider_factory: &Arc<dyn ProviderFactory>,
    agent_profile_store: Option<&Arc<ao_persistence::profiles::AgentProfileStore>>,
    parent_agent_id: Option<&String>,
) -> Result<(Arc<dyn ProviderClient>, Option<u32>), ProviderError> {
    if let (Some(store), Some(parent_id)) = (agent_profile_store, parent_agent_id) {
        if let Ok(Some(parent_profile)) = store.get(parent_id).await {
            let provider = provider_factory.build(&parent_profile)?;
            return Ok((provider, parent_profile.max_turns));
        }
    }
    let fallback_profile = default_catalog_subagent_profile();
    let provider = provider_factory.build(&fallback_profile)?;
    Ok((provider, fallback_profile.max_turns))
}

/// Placeholder `AgentProfile` used only to route the catalog-subagent
/// fallback-of-the-fallback through [`ProviderFactory::build`] when no
/// parent profile is available. Every tuning field is `None`/empty so
/// `ProviderFactory` resolves its own defaults; `provider` is never read by
/// `DefaultProviderFactory::build` (it only reads `native_provider`,
/// `model`, and the token/reasoning overrides), so its value here is
/// arbitrary.
fn default_catalog_subagent_profile() -> ao_protocol::agent::AgentProfile {
    use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
    AgentProfile {
        id: "__native_child_runner_default".to_string(),
        name: "default".to_string(),
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
        system_prompt: None,
        tools: None,
        env: Default::default(),
        max_instances: 1,
        timeout_seconds: 60,
        max_turns: None,
        working_dir: None,
        home_dir: None,
        serialize: false,
        workflows: None,
        template: None,
        runner_mode: Default::default(),
        enabled_plugins: Default::default(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: Default::default(),
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
    }
}

impl ChildRunner for NativeChildRunner {
    fn launch(
        &self,
        mut child_ctx: RunnerContext,
        initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
        // `target_profile` is part of the `ChildRunner` trait contract, but
        // `ProfileAwareChildRunner` — this type's only production
        // constructor — intercepts `Some(profile)` before it ever reaches
        // here (see the struct doc above), so this should always be `None`.
        // A debug assertion surfaces a wiring regression loudly in tests
        // instead of silently mis-resolving the provider in release builds.
        debug_assert!(
            target_profile.is_none(),
            "NativeChildRunner::launch received Some(profile); ProfileAwareChildRunner \
             should have intercepted it and routed it through RunnerDispatcher::pick instead"
        );

        // Register child session in McpSessionStore and share its cwd Arc with
        // child_ctx so Bash-cd writes propagate to the session entry.
        let session_guard = if let Some(sessions) = self.mcp_sessions.as_ref() {
            let parent_info = child_ctx.parent_session_id.as_ref().map(|pid| {
                use crate::mcp_session::ParentSessionInfo;
                ParentSessionInfo {
                    session_id: pid.clone(),
                    agent_id: child_ctx.parent_agent_id.clone().unwrap_or_default(),
                    current_cwd: child_ctx
                        .parent_current_cwd
                        .clone()
                        .unwrap_or_else(|| child_ctx.cwd.read().unwrap().clone()),
                }
            });
            let cwd_snapshot = child_ctx.cwd.read().unwrap().clone();
            let child_session_id = child_ctx.session_id.clone();
            let child_agent_id = child_ctx.agent_id.clone();
            if let Ok(sess) = sessions.register_session(
                child_session_id.clone(),
                child_agent_id,
                cwd_snapshot,
                parent_info,
            ) {
                // Rebind child_ctx.cwd to session.cwd so they share the same Arc.
                child_ctx = child_ctx.with_cwd_arc(Arc::clone(&sess.cwd));
                Some(NativeMcpSessionGuard {
                    sessions: Arc::clone(sessions),
                    session_id: child_session_id,
                })
            } else {
                None
            }
        } else {
            None
        };

        let cwd_snapshot = child_ctx.cwd.read().unwrap().clone();
        let settings = load_runner_settings(&cwd_snapshot).unwrap_or_default();
        let provider_factory = Arc::clone(&self.provider_factory);
        let agent_profile_store = child_ctx.agent_profile_store.clone();
        let parent_agent_id = child_ctx.parent_agent_id.clone();
        let bg_id = background_agent_id;

        tokio::spawn(async move {
            let _guard = session_guard;

            // Captured before `child_ctx` moves into `run_session` below —
            // `CancellationToken::clone()` is a cheap handle to the same
            // underlying shared state, so checking `is_cancelled()` on this
            // clone after the run still reflects whether the run's *own*
            // cancellation token actually fired. That's what distinguishes a
            // genuine cancel from the turn cap below: the query-loop's
            // turn-cap exit also reports `cancelled: true` on its
            // `SessionOutcome` (see `query_loop::run_session`'s turn-cap
            // check), but never touches this token.
            let cancel_token = child_ctx.cancel.clone();

            let (provider, parent_max_turns) = match resolve_catalog_subagent_provider(
                &provider_factory,
                agent_profile_store.as_ref(),
                parent_agent_id.as_ref(),
            )
            .await
            {
                Ok(pair) => pair,
                Err(e) => {
                    let msg = format!("provider not configured for subagent: {e}");
                    tracing::error!(background_agent_id = %bg_id, "subagent launch failed: {msg}");
                    let _ = event_tx.send(RunnerEvent::Failed {
                        background_agent_id: bg_id,
                        error: msg.clone(),
                    });
                    return Ok(TaskFinalReport::failed(msg));
                }
            };
            let max_turns = parent_max_turns.unwrap_or(ao_protocol::agent::DEFAULT_MAX_TURNS);

            let config = RunnerConfig {
                provider,
                bridge: Arc::new(StubBridge),
                denial_tracker: Arc::new(NoopDenialTracker),
                settings,
                mode: PermissionMode::default(),
                kind: SessionKind::Autonomous,
                auto_approve: vec![],
                system_prompt: child_ctx.system_prompt.clone(),
                event_sink: None,
                // Subagents inherit the parent agent's reasoning posture only
                // implicitly — there's no per-subagent profile here, so the
                // safe default is to leave it unset and let the resolved
                // provider's own "no extended thinking" default apply. A
                // future change can read a thinking config off the
                // SubagentDefinition if needed.
                thinking: None,
                max_turns: Some(max_turns as usize),
            };

            let initial_messages = vec![Message::User {
                content: vec![ContentBlock::Text { text: initial_prompt }],
            }];
            match run_session(initial_messages, child_ctx, config).await {
                Ok(outcome) => {
                    let report = if outcome.cancelled && !cancel_token.is_cancelled() {
                        // `cancelled: true` with the run's own token never
                        // fired can only mean the turn cap tripped — surface
                        // it as a named failure (not a bare "cancelled") so
                        // the limit is visible wherever this subagent's
                        // result is read, instead of looking like a plain
                        // user-initiated stop.
                        let msg = format!(
                            "Subagent stopped after reaching its configured turn limit of {max_turns} turns"
                        );
                        tracing::warn!(
                            background_agent_id = %bg_id,
                            max_turns,
                            "subagent run hit its turn cap"
                        );
                        let _ = event_tx.send(RunnerEvent::Failed {
                            background_agent_id: bg_id,
                            error: msg.clone(),
                        });
                        TaskFinalReport::failed(msg)
                    } else if outcome.cancelled {
                        let _ =
                            event_tx.send(RunnerEvent::Cancelled { background_agent_id: bg_id });
                        TaskFinalReport::cancelled()
                    } else {
                        let text = (!outcome.final_assistant_text.is_empty())
                            .then_some(outcome.final_assistant_text);
                        let _ =
                            event_tx.send(RunnerEvent::Completed { background_agent_id: bg_id });
                        TaskFinalReport::completed(text)
                    };
                    Ok(report)
                }
                Err(e) => {
                    let msg = e.to_string();
                    tracing::error!(
                        background_agent_id = %bg_id,
                        "subagent run failed: {msg}"
                    );
                    let _ = event_tx.send(RunnerEvent::Failed {
                        background_agent_id: bg_id,
                        error: msg.clone(),
                    });
                    Ok(TaskFinalReport::failed(msg))
                }
            }
        })
    }
}


/// RAII guard that deregisters a native session from McpSessionStore on drop.
/// Ensures cleanup on normal exit, early return, and panic unwind.
struct NativeMcpSessionGuard {
    sessions: Arc<McpSessionStore>,
    session_id: String,
}

impl Drop for NativeMcpSessionGuard {
    fn drop(&mut self) {
        self.sessions.remove(&self.session_id);
    }
}

/// RAII guard that deregisters a per-run form bridge from the registry on drop
/// and cancels any outstanding ask_form futures. Ensures cleanup on normal
/// exit, early return, and panic unwind.
struct FormBridgeGuard {
    registry: Arc<FormBridgeRegistry>,
    agent_id: String,
    bridge: Arc<LiveFormBridge>,
}

impl Drop for FormBridgeGuard {
    fn drop(&mut self) {
        self.registry.deregister(&self.agent_id, &self.bridge);
        self.bridge.cancel_pending();
    }
}

/// In-process API runner that drives `run_session` directly against a provider
/// client, emitting the same `AgentEventPayload` stream as `CliAgentRunner`
/// via the `TimelineAdapter`.
pub struct NativeAgentRunner {
    pub event_bus: Arc<EventBus>,
    pub instance_registry: Arc<InstanceRegistry>,
    pub running_agents: Arc<RunningAgents>,
    pub provider_factory: Arc<dyn ProviderFactory>,
    pub tools_registry: Arc<Registry>,
    pub background_agent_cap: usize,
    pub persistence: Arc<PersistenceLayer>,
    pub workflow_runner: Option<Arc<dyn WorkflowRunnerHandle + Send + Sync>>,
    tasklist_service: Arc<std::sync::OnceLock<Arc<dyn TasklistServiceHandle + Send + Sync>>>,
    /// Optional handle to the MCP manager. When set, `extend_skill_registry` is
    /// called at session startup to surface MCP server prompts as inline skills.
    mcp_manager: Option<Arc<ao_engine_tools_runner::mcp::McpManager>>,
    /// Late-bound handle for `AssignmentTrigger`'s fire-now capability.
    ///
    /// A `OnceLock` because it depends on the queue-manager registry, which is
    /// constructed after this runner (see `AppState::new`'s dispatcher/queue
    /// bootstrapping order) — mirrors the `tasklist_service` late-bind below.
    assignment_fire: Arc<std::sync::OnceLock<Arc<dyn ao_engine_tools_core::AssignmentFireHandle + Send + Sync>>>,
    /// Optional handle to the task classifier — when set, Todo* tools invoked
    /// from this runner can spawn background classifications at task-create
    /// time instead of waiting for the periodic reconciler. Plumbed through to
    /// `RunnerContext` at session start.
    classifier: Option<Arc<dyn ao_engine_tools_core::ClassifierHandle + Send + Sync>>,
    /// Process-wide classifier dedup set, shared with the reconciler so an
    /// event-driven spawn from one of this runner's Todo* tool calls cannot
    /// collide with a concurrent reconciler tick on the same task.
    classifier_in_flight: Option<Arc<ao_engine_tools_core::ClassifierInFlight>>,
    /// Runtime anchor registry for cache-floor stability.
    pub anchor_registry: Arc<WindowAnchorRegistry>,
    /// Reflection-trigger subscriber for the OBSERVE pass. Defaults to a no-op; late-bound to a real subscriber via
    /// [`Self::with_reflection_subscriber`] once the reflection pass exists.
    pub reflection_subscriber: Arc<dyn ReflectionTriggerSubscriber>,
    /// Session store shared with the MCP route handler; each run registers its
    /// session_id here so Bash-cwd tracking and memory layering work.
    pub mcp_sessions: Arc<McpSessionStore>,
    /// Per-agent registry of live form bridges. The HTTP `POST /agents/{id}/form-answer`
    /// route looks up the bridge here to deliver submitted form answers.
    pub form_bridge_registry: Arc<FormBridgeRegistry>,
}

impl NativeAgentRunner {
    pub fn new(
        event_bus: Arc<EventBus>,
        instance_registry: Arc<InstanceRegistry>,
        running_agents: Arc<RunningAgents>,
        provider_factory: Arc<dyn ProviderFactory>,
        tools_registry: Arc<Registry>,
        persistence: Arc<PersistenceLayer>,
    ) -> Self {
        Self {
            event_bus,
            instance_registry,
            running_agents,
            provider_factory,
            tools_registry,
            background_agent_cap: BACKGROUND_AGENT_CAP,
            persistence,
            workflow_runner: None,
            tasklist_service: Arc::new(std::sync::OnceLock::new()),
            classifier: None,
            classifier_in_flight: None,
            anchor_registry: Arc::new(WindowAnchorRegistry::new()),
            reflection_subscriber: Arc::new(NoopReflectionSubscriber),
            mcp_sessions: Arc::new(McpSessionStore::new()),
            form_bridge_registry: Arc::new(FormBridgeRegistry::new()),
            mcp_manager: None,
            assignment_fire: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Replace the anchor registry (used by AppState to share one registry across runners).
    pub fn with_anchor_registry(mut self, registry: Arc<WindowAnchorRegistry>) -> Self {
        self.anchor_registry = registry;
        self
    }

    /// Supply a reflection-trigger subscriber (used by AppState once the
    /// reflection pass exists to receive `select`'s anchor-rotation and
    /// idle-timeout triggers).
    pub fn with_reflection_subscriber(
        mut self,
        subscriber: Arc<dyn ReflectionTriggerSubscriber>,
    ) -> Self {
        self.reflection_subscriber = subscriber;
        self
    }

    /// Supply the process-global MCP manager so each session's skill registry
    /// is extended with prompt-sourced inline skills at startup.
    pub fn with_mcp_manager(
        mut self,
        manager: Arc<ao_engine_tools_runner::mcp::McpManager>,
    ) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    /// Share the McpSessionStore with this runner so each native session is tracked
    /// for Bash-cwd updates and memory-layering.
    pub fn with_mcp_sessions(mut self, sessions: Arc<McpSessionStore>) -> Self {
        self.mcp_sessions = sessions;
        self
    }

    pub fn with_workflow_runner(
        mut self,
        runner: Arc<dyn WorkflowRunnerHandle + Send + Sync>,
    ) -> Self {
        self.workflow_runner = Some(runner);
        self
    }

    /// Plumb a classifier handle through to every `RunnerContext` this runner
    /// builds. Without this, `ctx.classifier` is `None` and the Todo* tools'
    /// background-classify spawn path is unreachable in production — newly
    /// created agent-owned tasks then wait for the periodic reconciler instead
    /// of being routed within seconds of `TodoCreate`.
    pub fn with_classifier(
        mut self,
        classifier: Arc<dyn ao_engine_tools_core::ClassifierHandle + Send + Sync>,
    ) -> Self {
        self.classifier = Some(classifier);
        self
    }

    /// Plumb the process-wide classifier dedup set into every `RunnerContext`
    /// this runner builds. Required for tools' spawn sites to coordinate with
    /// the periodic reconciler.
    pub fn with_classifier_in_flight(
        mut self,
        in_flight: Arc<ao_engine_tools_core::ClassifierInFlight>,
    ) -> Self {
        self.classifier_in_flight = Some(in_flight);
        self
    }

    /// Late-bind the tasklist service handle (called from AppState after creating TasklistService).
    pub fn set_tasklist_service(&self, service: Arc<dyn TasklistServiceHandle + Send + Sync>) {
        let _ = self.tasklist_service.set(service);
    }

    /// Late-bind the assignment-fire handle (called from `AppState::new` once
    /// the queue-manager registry exists — see the field doc on
    /// `assignment_fire`).
    pub fn set_assignment_fire(
        &self,
        handle: Arc<dyn ao_engine_tools_core::AssignmentFireHandle + Send + Sync>,
    ) {
        let _ = self.assignment_fire.set(handle);
    }
}

#[async_trait]
impl AgentRunner for NativeAgentRunner {
    async fn run(&self, request: AgentRunRequest) -> Result<RunComplete, AoError> {
        let AgentRunRequest {
            agent,
            prompt,
            attachments,
            run_complete_tx,
            focus_path,
            scope,
            thread_id,
            session_kind,
            pre_registered_run_id,
            delegate_chain,
            spawn_chain,
            depth,
            parent_session_id,
            parent_agent_id,
            parent_current_cwd,
            cancel: request_cancel,
            isolate_history,
            transcript_override,
            event_channel,
            bypass_instance_cap,
        } = request;

        let agent_id: AgentId = agent.id.clone();
        // Channel all live event-bus emissions use. Delegated children supply
        // a dedicated channel (e.g. `delegate:<delegation_id>`) so their
        // streaming output never renders in an agent's chat feed — critical
        // for clone-parent delegates, whose agent_id IS the parent's.
        let event_agent_id: AgentId = event_channel.clone().unwrap_or_else(|| agent_id.clone());
        // Adopt the caller's pre-allocated run_id if provided, else mint
        // a fresh one. The pre-allocated path is taken by the per-agent
        // queue manager to close a TOCTOU window in its `can_spawn`
        // check — see `AgentRunRequest::pre_registered_run_id`.
        let caller_pre_registered = pre_registered_run_id.is_some();
        let run_id = pre_registered_run_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        // Determine cwd early so we can register the session before the run.
        // (The same value is used again below when building RunnerContext, and
        // it is what project-scope memory keys off further down.)
        //
        // Precedence mirrors the CLI runner's effective_cwd resolution
        // (see `CliAgentRunner`): focus_path > agent.working_dir > home dir.
        // `focus_path` carries the caller's actual target directory for this
        // run (delegation, tasklist workspace, assignment working directory,
        // etc.) and must win over the agent's static `working_dir`. Dropping
        // it here previously left `session_cwd` — and everything derived from
        // it, including project-key resolution below — falling back to the
        // agent's working_dir or process cwd ($HOME in most launch contexts),
        // so Project-scope memory silently keyed off the wrong project.
        let session_cwd = focus_path
            .as_deref()
            .map(PathBuf::from)
            .or_else(|| agent.working_dir.as_deref().map(PathBuf::from))
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")));

        // Register this invocation in the shared McpSessionStore so Bash-cwd
        // tracking and memory-scope layering can access our session.
        // Deregistered on every exit path via NativeMcpSessionGuard::drop.
        // Capture the session Arc so we can share its cwd with RunnerContext.
        let session = self.mcp_sessions.register_session(
            run_id.clone(),
            agent_id.clone(),
            session_cwd.clone(),
            None,
        ).ok();
        let _mcp_session_guard = NativeMcpSessionGuard {
            sessions: Arc::clone(&self.mcp_sessions),
            session_id: run_id.clone(),
        };

        // Register run handle and RAII guard for cleanup on every exit path.
        // The registry returns a unique per-registration id so a parent run
        // and any subtasks that share this agent_id retain independent
        // entries — none of them clobbers the others on insert.
        let handle = RunHandle {
            agent_id: agent_id.clone(),
            thread_id: thread_id.clone(),
            cancel: request_cancel.unwrap_or_else(tokio_util::sync::CancellationToken::new),
            runner_mode: AgentRunnerMode::Api,
            started_at: Utc::now(),
        };
        let reg_id = self.running_agents.insert(handle.clone());
        let _guard = RunningAgentsGuard::new(Arc::clone(&self.running_agents), reg_id);
        // RAII handle for the InstanceRegistry overlay. Drop fires on every
        // exit path — normal return, early `Err(_)`, and panic unwind — so the
        // sidebar `has_active_run` overlay clears unconditionally. Previously
        // the manual `unregister_run` pair around `run_session` could leak the
        // registration if anything in between panicked.
        //
        // Three constructors, one cleanup contract:
        // - `bypass_instance_cap` (see `AgentRunRequest::bypass_instance_cap`):
        //   never register at all, so this run can't occupy — or be mistaken
        //   in the UI for — the agent's own slot. `wrap_existing` on a key
        //   that was never inserted is a harmless no-op on Drop.
        // - When the caller pre-registered (queue manager path), wrap the
        //   existing entry so this guard takes over `unregister_run` on Drop
        //   without double-booking the slot.
        // - Otherwise (direct test invocations, etc.), register fresh.
        let _instance_guard = if bypass_instance_cap {
            InstanceRegistryGuard::wrap_existing(
                Arc::clone(&self.instance_registry),
                agent_id.clone(),
                run_id.clone(),
            )
        } else if caller_pre_registered {
            InstanceRegistryGuard::wrap_existing(
                Arc::clone(&self.instance_registry),
                agent_id.clone(),
                run_id.clone(),
            )
        } else {
            InstanceRegistryGuard::register_with_thread(
                Arc::clone(&self.instance_registry),
                agent_id.clone(),
                run_id.clone(),
                thread_id.clone(),
            )
            .await
        };

        tracing::info!(
            agent_id = %agent_id,
            run_id = %run_id,
            provider = ?agent.native_provider,
            prompt_len = prompt.len(),
            attachments = attachments.len(),
            "native agent run starting"
        );

        // Emit RunStarted eagerly so the UI shows a typing indicator immediately.
        self.event_bus
            .emit(&run_id, &event_agent_id, thread_id.clone(), AgentEventPayload::RunStarted)
            .await;

        // Build the provider client — fail fast if not configured.
        let provider = match self.provider_factory.build(&agent) {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("Provider not configured: {}", e);
                self.event_bus
                    .emit(
                        &run_id,
                        &event_agent_id,
                        thread_id.clone(),
                        AgentEventPayload::Error {
                            message: msg.clone(),
                            recoverable: false,
                        },
                    )
                    .await;
                self.event_bus
                    .emit(
                        &run_id,
                        &event_agent_id,
                        thread_id.clone(),
                        AgentEventPayload::RunEnded {
                            reason: RunEndReason::Error,
                        },
                    )
                    .await;
                // `_instance_guard` drops on the return below — cleans the
                // registry overlay; no manual unregister needed here.
                return Err(AoError::Provider(msg));
            }
        };

        // Resolve the chat thread this turn targets. `None` and any default-kind
        // row keep the back-compat agent-keyed transcript path so single-thread
        // agents stay byte-equivalent. Fresh and branch threads carry their own
        // transcript path and (for branches) a `history_floor_ts` lifted from
        // the branch source; those are threaded into `HistorySource`,
        // `RunnerContext`, and the timeline-adapter's write override below.
        let thread_metadata: Option<ao_protocol::thread::Thread> = match thread_id.as_deref() {
            Some(id) => self
                .persistence
                .threads
                .get(id)
                .await
                .ok()
                .flatten()
                .filter(|t| t.kind != ao_protocol::thread::ThreadKind::Default),
            None => None,
        };

        // Conditionally extend the base tool registry with per-run,
        // thread-eligibility-gated tools. Cloning `Registry` is cheap
        // (Arc-shared tool instances); mirrors the Autonomous-only `Sleep`
        // extension in `query_loop::init_session_context`, just decided here
        // instead since the thread-store lookups this needs are already done
        // by this function.
        //
        // - `RenameThread`: only when the acting thread is eligible
        //   (personal, non-default, not yet named — see
        //   `Thread::offers_rename_tool`).
        // - `ListThreads`/`SummarizeThread`: only when this agent has more
        //   than one thread at all — with a single thread there is nothing
        //   else to list or cross-reference, so the pair would be dead
        //   weight in every turn's tool array.
        let offers_rename_tool = thread_metadata
            .as_ref()
            .map(|t| t.offers_rename_tool())
            .unwrap_or(false);
        let agent_threads = self
            .persistence
            .threads
            .list_for_agent(&agent_id)
            .await
            .unwrap_or_default();
        let offers_cross_thread_tools = agent_threads.len() > 1;

        let effective_tools_registry: Arc<Registry> =
            if offers_rename_tool || offers_cross_thread_tools {
                let mut extended = (*self.tools_registry).clone();
                if offers_rename_tool {
                    ao_engine_tools_engine::rename_thread::register(&mut extended);
                }
                if offers_cross_thread_tools {
                    ao_engine_tools_engine::list_threads::register(&mut extended);
                    ao_engine_tools_engine::summarize_thread::register(&mut extended);
                }
                Arc::new(extended)
            } else {
                Arc::clone(&self.tools_registry)
            };

        let thread_transcript_override: Option<PathBuf> = thread_metadata
            .as_ref()
            .map(|t| PathBuf::from(&t.transcript_path));
        let thread_window_floor: Option<chrono::DateTime<Utc>> =
            thread_metadata.as_ref().and_then(|t| t.history_floor_ts);
        // For a branch thread, RecallHistory should pull pre-branch context
        // from the SOURCE thread's transcript — the branch's own file holds
        // only post-floor turns. For a fresh thread, RecallHistory still
        // operates on the thread's own file so the agent can re-read its
        // earlier turns past the live window.
        let thread_recall_override: Option<PathBuf> = if let Some(ref t) = thread_metadata {
            match &t.branch_source {
                Some(bs) => self
                    .persistence
                    .threads
                    .get(&bs.source_thread_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|src| PathBuf::from(src.transcript_path))
                    .or_else(|| Some(PathBuf::from(&t.transcript_path))),
                None => Some(PathBuf::from(&t.transcript_path)),
            }
        } else {
            None
        };

        // Load prior transcript entries and translate to the messages array.
        // Per-agent AnchorKey: Personal(agent_id) — coordinator for Standalone API agents.
        // Per-thread agents widen the key to `AgentThread(agent_id, path)` so
        // anchor state is partitioned per thread (see `history::anchor`).
        // Delegated children with `isolate_history` skip the load so they start with a clean slate.
        let history_select_source = match thread_transcript_override.as_ref() {
            Some(path) => {
                // For branch threads: pass the source transcript path so select()
                // grafts pre-fork source history into the initial context window
                // (TRUE FORK — agent reasons over inherited history without a tool call).
                let is_branch = thread_metadata
                    .as_ref()
                    .and_then(|t| t.branch_source.as_ref())
                    .is_some();
                crate::history::HistorySource::PersonalThread {
                    agent_id: agent_id.clone(),
                    transcript_path: path.clone(),
                    branch_source_path: if is_branch { thread_recall_override.clone() } else { None },
                    history_floor_ts: thread_window_floor,
                }
            }
            None => crate::history::HistorySource::Personal { agent_id: agent_id.clone() },
        };
        let (history_entries, anchor_signal) = if isolate_history {
            (vec![], None)
        } else {
            crate::history::select(
                &self.persistence,
                crate::history::HistorySelectInput {
                    source: history_select_source,
                    current_message_already_persisted: true,
                    now: Utc::now(),
                    config: crate::context::ContextConfig::default(),
                    anchor_registry: Some(Arc::clone(&self.anchor_registry)),
                    reflection_subscriber: Some(Arc::clone(&self.reflection_subscriber)),
                },
            )
            .await
        };
        if let Some(signal) = anchor_signal {
            // RunnerContext is rebuilt fresh each turn, so DeferredIndex is already
            // clean. Log the rotation for observability.
            tracing::debug!(
                agent_id = %agent_id,
                signal = ?signal,
                "history::select anchor signal"
            );
        }
        // Anthropic signatures are bound to both the model and the API key.
        // Pass both so reconstruction drops any reasoning block that would 400:
        // model mismatch (cheaper-model delegate path) or key rotation (TB-1).
        let current_key_fingerprint = provider.key_fingerprint();
        // Resolve None → the provider's configured default so default-model
        // agents can replay their reasoning blocks on resume. Without this, both
        // the authoring tag and the current_model passed to to_messages would be
        // None and every block would be dropped even when the model is unchanged.
        let resolved_model = agent.model.clone().or_else(|| provider.default_model());
        let mut messages = crate::history::to_messages(
            &history_entries,
            resolved_model.as_deref(),
            current_key_fingerprint.as_deref(),
        );

        // Build the augmented user prompt and append it as the current turn.
        let augmented = augment_prompt_with_attachments(&prompt, &attachments, None);
        messages.push(Message::User {
            content: vec![ContentBlock::Text { text: augmented }],
        });

        // Use the cwd already resolved above at session-registration time.
        let cwd = session_cwd.clone();

        // Fetch workflow summaries for compose_system_prompt().
        // Fetches summaries via the trait handle so no direct registry dependency is needed.
        let workflow_summaries = {
            use ao_protocol::agent::WorkflowBinding;
            match (&agent.workflows, &self.workflow_runner) {
                (None, _) | (Some(WorkflowBinding::None), _) | (_, None) => vec![],
                (Some(binding), Some(runner)) => {
                    let ids = match binding {
                        WorkflowBinding::All => None,
                        WorkflowBinding::List(ids) => Some(ids.as_slice()),
                        WorkflowBinding::None => unreachable!(),
                    };
                    runner.get_workflow_summaries(ids).await
                }
            }
        };

        // Load all inputs for compose_system_prompt() concurrently.
        let user_prefs = self.persistence.preferences.get().await
            .unwrap_or(None)
            .unwrap_or_default();
        // TODO(memory-usage): these three fetches are this turn's real
        // "surfaced" memory set — the entries actually injected into the
        // system prompt below. Bumping
        // `ao_engine_tools_core::memory_usage::increment` once per entry here
        // would be a correct but naive per-turn cost (a full sidecar
        // read-modify-write per entry, for every entry in every scope, every
        // turn); that should be batched into a single read-modify-write per
        // scope (or moved off the critical path) before wiring it in. Scope
        // paths: `self.persistence.data_root
        // .memory_agent_path(&agent_id)` / `.memory_global_path()` /
        // `.memory_project_path(&hash)` (the last needs `hash` captured
        // below, alongside `canonical_key`).
        let agent_memories = self.persistence.memory.list(&agent_id).await
            .unwrap_or_default();
        let global_memories = self.persistence.memory.list_global().await
            .unwrap_or_default();
        // Current-thread ephemeral working memory (see `MemoryScope::Thread`).
        // No thread id (e.g. a delegate run with no thread context) means no
        // thread-scope tier exists for this turn, so this is simply empty —
        // not an error path, unlike the durable scopes above.
        let thread_memories = match thread_id.as_deref() {
            Some(tid) => self.persistence.memory.list_thread(tid).await.unwrap_or_default(),
            None => vec![],
        };
        let (project_memories, resolved_project_key) = {
            match ao_persistence::project_key::resolve_project_key(&cwd).await {
                Ok(canonical_key) => {
                    let hash = ao_persistence::project_key::hash_project_key(&canonical_key);
                    let _ = ao_persistence::project_key::update_projects_index(
                        &self.persistence.data_root,
                        &hash,
                        &canonical_key,
                    ).await;
                    let memories = self.persistence.memory.list_project(&hash).await.unwrap_or_default();
                    (memories, Some(canonical_key))
                }
                Err(e) => {
                    tracing::warn!(agent_id = %agent_id, "Failed to resolve project key for memory loading: {}", e);
                    (vec![], None)
                }
            }
        };
        let agent_home_path =
            crate::instructions::resolve_agent_home_dir(&agent, &self.persistence.data_root);
        let (workspace_ctx, mut agent_home_ctx) = tokio::join!(
            crate::system_prompt_composer::loader::load_workspace_context(&cwd),
            crate::system_prompt_composer::loader::load_agent_home_context(&agent_home_path),
        );

        // Build the skill registry from the same pools `RunSkill` resolves
        // against (user pool + enabled plugins, plus the MCP overlay) and render
        // its listing into the agent-home context. This is the load-bearing
        // fix: without it the composed "# Studio Skills" block was driven by
        // the agent-home `skills/` directory alone — empty for pool/plugin
        // agents — so the model was never told its enabled skills existed even
        // though dispatch could resolve them. The same Arc is
        // reused below for `with_skill_registry`, so the advertised listing and
        // the dispatch registry are guaranteed identical (native = no CLI
        // precedence directive; there is no competing external binary here).
        let skill_registry = crate::agent_context::build_skill_registry(
            self.persistence.data_root.root(),
            &agent,
            self.mcp_manager.as_deref(),
        );
        agent_home_ctx.skills_block =
            crate::agent_context::render_studio_skills_block(&skill_registry, false);

        // Extract project scope for wiring into RunnerContext below.
        let run_project_id: Option<String> = if let RunScope::Project { ref project_id } = scope {
            Some(project_id.clone())
        } else {
            None
        };

        // Determine session kind before composing the system prompt so the
        // pacing section can be appended for autonomous sessions.
        let effective_kind = if matches!(scope, RunScope::Tasklist { .. }) {
            SessionKind::Autonomous
        } else {
            session_kind
        };

        // Compose the canonical system prompt from pure-data inputs.
        let date_str = Utc::now().format("%Y-%m-%d").to_string();
        let base_prompt = crate::system_prompt_composer::compose_system_prompt(
            &agent,
            &user_prefs,
            &workspace_ctx,
            &agent_home_ctx,
            &agent_memories,
            &project_memories,
            &global_memories,
            &workflow_summaries,
            &agent.delegates_to,
            &date_str,
            resolved_project_key.as_deref(),
        );
        // Append the current thread's ephemeral working memory as its own
        // "[Thread Notes]" block, kept out of compose_system_prompt() itself
        // so its existing Agent/Project/Global rendering stays untouched.
        // `None` (including "no active thread") appends nothing.
        let base_prompt = match crate::system_prompt_composer::build_thread_notes_section(&thread_memories) {
            Some(block) => format!("{}\n\n{}", base_prompt, block),
            None => base_prompt,
        };
        // Append the autonomous-pacing section for sessions where no human is
        // watching. Gives every autonomous run (scheduled task, background agent)
        // the same guidance the tasklist preamble has always carried.
        let system_prompt = Some(if effective_kind == SessionKind::Autonomous {
            format!("{}\n\n{}", base_prompt, crate::tasklist_runtime::autonomous_pacing_section())
        } else {
            base_prompt
        });

        // Project-scoped runs append the project context block (goal/spec plus
        // the status-dependent role section) after the composed prompt — same
        // contract as the CLI runner. It must happen post-compose: the
        // composer rebuilds the system prompt from persona/special_instructions
        // and discards the profile's legacy `system_prompt` field, so per-run
        // context stuffed into the profile would be silently dropped.
        let system_prompt = if let Some(ref project_id) = run_project_id {
            crate::project_context::append_project_context(
                &self.persistence.projects,
                project_id,
                system_prompt,
            )
            .await
        } else {
            system_prompt
        };

        // Observability: prompt-size metrics tagged with the same
        // (agent_id, run_id) the cache-usage log uses, so a single tail can
        // pair "what we sent" with "how the provider billed it". Useful for
        // diagnosing first-byte latency that scales with prompt size, and for
        // confirming the cached prefix is stable across turns.
        let system_prompt_chars = system_prompt.as_deref().map(str::len).unwrap_or(0);
        let history_chars: usize = messages
            .iter()
            .map(|m| match m {
                Message::User { content } | Message::Assistant { content } => content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => text.len(),
                        // Tool blocks are tiny next to text; approximated as the
                        // serialized JSON length. Good enough for relative sizing.
                        _ => serde_json::to_string(b).map(|s| s.len()).unwrap_or(0),
                    })
                    .sum::<usize>(),
                Message::System { content } => content.len(),
                Message::ToolResult { content, .. } => content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => text.len(),
                        _ => serde_json::to_string(b).map(|s| s.len()).unwrap_or(0),
                    })
                    .sum::<usize>(),
            })
            .sum();
        tracing::info!(
            target: "ao_engine::request",
            agent_id = %agent_id,
            run_id = %run_id,
            provider = ?agent.native_provider,
            system_prompt_chars = system_prompt_chars,
            history_messages = messages.len(),
            history_chars = history_chars,
            "request prepared",
        );

        // Load settings from the agent's working directory (falls back to defaults if missing).
        let settings = load_runner_settings(&cwd).unwrap_or_default();

        // Compute the agent-level admission gate from the profile's ToolsConfig.
        // This gates which tools the model may see at all; it is orthogonal to the
        // load-policy resolution that decides eager-vs-deferred presentation.
        // On a channel-bridge turn (e.g. Telegram), also fold in the
        // channel-blocked tools — a UI-form tool has no channel-side surface
        // to render on, so it must never reach the model there. Slack has no
        // `bridge_thread_id` to reverse-look-up from (one thread per
        // conversation, not per binding — see `ChannelBridgeOrigin`'s
        // docstring), so also fetch the thread's own recorded origin, if any.
        let thread_channel_origin = match thread_id.as_deref() {
            Some(tid) => self
                .persistence
                .threads
                .get(tid)
                .await
                .ok()
                .flatten()
                .and_then(|t| t.channel_origin),
            None => None,
        };
        let on_channel_bridge =
            is_channel_bridge_thread(&agent, thread_id.as_deref(), thread_channel_origin.as_ref());
        let extra_deny: &[&str] = if on_channel_bridge { CHANNEL_BLOCKED_TOOLS } else { &[] };
        let tool_admission =
            compute_tool_admission(agent.tools.as_ref(), &effective_tools_registry, extra_deny);

        // Build a fresh timeline adapter that translates runner session events to EventBus events.
        // Wire in the persistence layer so inline transcript writes happen during the run.
        //
        // Delegated children (isolate_history) must never write into the
        // profile owner's personal transcript — for clone-parent delegates the
        // agent_id IS the parent's, so an ungated attach would splice the
        // child's turns directly into the parent's chat history. When the
        // caller supplied a sidechain transcript path, route writes there;
        // with no path, drop persistence for this run entirely (the spawner's
        // sidechain persister still records the terminal event).
        let base_adapter = TimelineAdapter::new(
            run_id.clone(),
            agent_id.clone(),
            thread_id.clone(),
            Arc::clone(&self.event_bus),
        )
        .with_model(resolved_model.clone())
        .with_key_fingerprint(current_key_fingerprint.clone())
        .with_skill_registry(Arc::clone(&skill_registry))
        .with_event_channel(event_channel.clone());
        // Choose the destination this run's transcript writes go to:
        // 1. An explicit delegation/tasklist override always wins (it carries
        //    semantics this layer can't infer).
        // 2. Otherwise a non-default thread routes writes to its own JSONL file
        //    so each thread of an agent stays partitioned on disk.
        // 3. Otherwise writes land in the agent's pre-thread transcript file
        //    via `append_for_run`'s `None`-branch — back-compat preserved.
        let runtime_transcript_override =
            transcript_override.clone().or_else(|| thread_transcript_override.clone());
        let adapter = Arc::new(if isolate_history && runtime_transcript_override.is_none() {
            base_adapter
        } else {
            base_adapter.with_persistence(
                Arc::clone(&self.persistence),
                runtime_transcript_override.clone(),
            )
        });

        // `skill_registry` was built above (before the system prompt was
        // composed) so the advertised "# Studio Skills" listing and the
        // dispatch registry come from one and the same load — neither half can
        // drift out of sync. Reusing it here wires `RunSkill`/`SkillRegister`
        // to resolve against exactly the skills the model was told it has.

        // Build the runner context; pre-set the admission gate so the query loop
        // filters each turn's tool array to the agent's permitted set. This is
        // independent of the load-policy resolution that picks eager vs deferred.
        let mut runner_ctx = RunnerContext::new_with_cwd(run_id.clone(), agent_id.clone(), cwd)
            .with_registry(Arc::clone(&effective_tools_registry))
            .with_thread_store(Arc::clone(&self.persistence.threads))
            .with_skill_registry(skill_registry)
            .with_background_agents(Arc::new(BackgroundAgentRegistry::new(
                self.background_agent_cap,
            )))
            .with_tool_admission(tool_admission)
            .with_assignment_store(Arc::clone(&self.persistence.assignments))
            .with_preferences(Arc::new(ao_persistence::preferences::UserPreferencesStore::new(
                self.persistence.data_root.clone(),
            )))
            .with_transcript_store(Arc::new(ao_persistence::transcript::TranscriptStore::new(
                self.persistence.data_root.clone(),
            )))
            .with_outcome_store(Arc::new(ao_persistence::outcome::OutcomeStore::new(
                self.persistence.data_root.clone(),
            )))
            .with_snapshot_store(Arc::clone(&self.persistence.snapshots))
            .with_memory_store(Arc::clone(&self.persistence.memory))
            .with_artifact_store(Arc::clone(&self.persistence.artifacts))
            .with_reflection_staging(Arc::clone(&self.persistence.reflection_staging));

        // Bind ctx.cwd to the same Arc as the session entry so Bash-cd writes
        // (and EnterWorktree/ExitWorktree writes) propagate to session.cwd.
        if let Some(sess) = &session {
            runner_ctx = runner_ctx.with_cwd_arc(Arc::clone(&sess.cwd));
        }

        // Set the window floor from the oldest visible entry in the loaded history.
        // For branch threads the grafted combined slice (source + own entries) means
        // history_entries.first() is the oldest grafted source entry in the window,
        // so RecallHistory correctly surfaces pre-window source history via the
        // source recall path pinned below. Single-thread agents use the same path.
        if let Some(ts) = history_entries.first().map(|e| e.ts) {
            runner_ctx = runner_ctx.with_window_floor_ts(ts);
        }
        if let Some(path) = thread_recall_override.clone() {
            runner_ctx = runner_ctx.with_recall_transcript_path(path);
        }

        // Only bother building the summarization engine when `SummarizeThread`
        // was actually registered above — it's otherwise unreachable, and
        // building it may resolve a provider client for nothing.
        if offers_cross_thread_tools {
            if let Some(engine) = crate::build_thread_summarization_engine(&agent) {
                runner_ctx = runner_ctx.with_thread_summarization_engine(engine);
            }
        }
        if let Some(wf_binding) = agent.workflows.clone() {
            runner_ctx = runner_ctx.with_agent_workflows(wf_binding);
        }
        if let Some(wf_runner) = self.workflow_runner.clone() {
            runner_ctx = runner_ctx.with_workflow_runner(wf_runner);
        }
        if let Some(ts) = self.tasklist_service.get() {
            runner_ctx = runner_ctx.with_tasklist_service(Arc::clone(ts));
        }
        if let Some(af) = self.assignment_fire.get() {
            runner_ctx = runner_ctx.with_assignment_fire(Arc::clone(af));
        }
        if let Some(classifier) = self.classifier.as_ref() {
            runner_ctx = runner_ctx.with_classifier(Arc::clone(classifier));
        }
        if let Some(in_flight) = self.classifier_in_flight.as_ref() {
            runner_ctx = runner_ctx.with_classifier_in_flight(Arc::clone(in_flight));
        }
        // Lets Todo* tools resolve an `owner` value (agent_id or address-book
        // display name) to a canonical agent_id at task-creation/update time,
        // the same lookup `Delegate.target` performs.
        runner_ctx = runner_ctx.with_agent_profile_store(Arc::new(
            ao_persistence::profiles::AgentProfileStore::new(self.persistence.data_root.clone()),
        ));

        // Propagate delegation metadata so depth/cycle caps apply correctly
        // for delegates-of-delegates. Without this, every run starts at depth=0
        // with empty chains and grandchild delegates never hit the cap.
        runner_ctx = runner_ctx
            .with_depth(depth)
            .with_delegate_chain(delegate_chain)
            .with_spawn_chain(spawn_chain);
        if let (Some(sess_id), Some(ag_id), Some(cwd_str)) = (
            parent_session_id,
            parent_agent_id,
            parent_current_cwd,
        ) {
            runner_ctx = runner_ctx.with_parent_session_info(
                sess_id,
                ag_id,
                std::path::PathBuf::from(cwd_str),
            );
        }

        // Wire project scope: TodoCreate (and other project tools) read ctx.project_id
        // to stamp new tasklists and gate project-only operations.
        if let Some(ref pid) = run_project_id {
            let project_store = Arc::new(ao_persistence::projects::ProjectStore::new(
                self.persistence.data_root.clone(),
            ));
            runner_ctx = runner_ctx
                .with_project(pid.clone())
                .with_project_store(project_store);
        }

        // Wire thread scope: TodoCreate/Delegate read ctx.thread_id to tag
        // completion events and persisted transcript markers with the thread
        // that was active when the tool call happened, instead of always
        // falling back to the agent's default-thread transcript.
        if let Some(tid) = thread_id.clone() {
            runner_ctx = runner_ctx.with_thread(tid);
        }

        // Wire the cancel token from the run handle into the runner context.
        runner_ctx.cancel = handle.cancel.clone();

        // Record this turn's surfaced memory set (the same three fetches that
        // fed `compose_system_prompt` above) so the query loop's end-of-turn
        // `OutcomeRecord` can be joined back to specific memory entries later
        // (self-improvement outcome tracking). Usage-counter bumps for these same
        // entries remain TODO(memory-usage) — see the note above `agent_memories`.
        runner_ctx.record_artifacts_used(
            agent_memories
                .iter()
                .chain(project_memories.iter())
                .chain(global_memories.iter())
                .map(|entry| ArtifactRef::memory(entry.id.clone())),
        );

        // Create a per-run LiveFormBridge backed by the same event sink as the
        // timeline adapter. Register it in the shared registry so the HTTP route
        // handler can deliver submitted form answers. Deregistered + cancelled in
        // the finally-equivalent block below run_session.
        // Keyed by the event channel, not the raw agent_id: a clone-parent
        // delegate shares the parent's agent_id, and registering under it
        // would clobber the parent's live bridge (and deregister it on drop).
        let form_event_sink: Arc<dyn ao_engine_tools_core::EventSink + Send + Sync> =
            Arc::new(EventBusAgentSink {
                bus: Arc::clone(&self.event_bus),
                agent_id: event_agent_id.clone(),
                thread_id: thread_id.clone(),
            });
        // A channel-bridge turn has no UI to render a form on, so `ask_form`
        // must fail fast with `NoOperator` instead of suspending on an
        // answer nothing can ever deliver — same signal already computed
        // above for the tool-admission gate (`on_channel_bridge`).
        // Same scope-key convention the async form path uses (see
        // `ao_engine_tools_core::form_events::wire_posted_async_form`): the
        // agent's own snapshot slot, or `project_{id}` for a project-scoped
        // run. Lets a sync form's persisted `pending_forms` entry land in
        // exactly the same place `GET /agents` already reads for async ones.
        let form_scope_key: String = run_project_id
            .as_deref()
            .map(|pid| format!("project_{}", pid))
            .unwrap_or_else(|| agent_id.clone());
        let form_bridge = Arc::new(
            if on_channel_bridge {
                LiveFormBridge::new_non_interactive(form_event_sink)
            } else {
                LiveFormBridge::new(form_event_sink)
            }
            .with_persistence(
                Arc::clone(&self.persistence.snapshots),
                Arc::new(ao_persistence::transcript::TranscriptStore::new(
                    self.persistence.data_root.clone(),
                )),
                form_scope_key,
                thread_id.clone(),
            ),
        );
        self.form_bridge_registry
            .register(&event_agent_id, Arc::clone(&form_bridge));
        runner_ctx = runner_ctx.with_form_bridge(form_bridge.clone());
        // Deregisters + cancels pending ask_form futures on every exit path.
        let _form_bridge_guard = FormBridgeGuard {
            registry: Arc::clone(&self.form_bridge_registry),
            agent_id: event_agent_id.to_string(),
            bridge: Arc::clone(&form_bridge),
        };

        // For interactive sessions wire a form-based permission bridge so the
        // operator can approve or deny tool calls through the UI. Autonomous
        // sessions keep StubBridge — no human is present to answer a dialog,
        // and evaluate_permission already auto-denies Ask decisions for those.
        let perm_bridge: Arc<dyn UserPromptBridge> = if effective_kind == SessionKind::Interactive {
            Arc::new(LivePermissionBridge::new(
                Arc::clone(&form_bridge),
                handle.cancel.clone(),
            ))
        } else {
            Arc::new(StubBridge)
        };

        // Build the runner config. `thinking` is plumbed verbatim from the
        // agent profile so the request builder can opt the API path into
        // extended thinking; absent → provider default (no thinking).
        //
        // `agent.max_turns` is `timeout_seconds`'s sibling safety rail —
        // bounding the number of model-completion turns this run may take
        // instead of wall-clock time. `None` (profile never set it) resolves
        // to `DEFAULT_MAX_TURNS`, matching how `RunnerConfig::max_turns`
        // itself already documents "no cap" as `None`, never as this
        // resolved value — the query loop's turn-cap check
        // (`query_loop::run_session`) is what actually stops the loop; see
        // the `cancelled && !handle.cancel.is_cancelled()` branch below for
        // how a cap trip is told apart from a genuine user cancel.
        let max_turns = agent.max_turns.unwrap_or(ao_protocol::agent::DEFAULT_MAX_TURNS);
        let config = RunnerConfig {
            provider,
            bridge: perm_bridge,
            denial_tracker: Arc::new(NoopDenialTracker),
            settings,
            mode: PermissionMode::default(),
            kind: effective_kind,
            auto_approve: vec![],
            system_prompt,
            event_sink: Some(adapter.clone()),
            thinking: agent.thinking.clone(),
            max_turns: Some(max_turns as usize),
        };

        // Run the session to completion. The `Instant` captures wall-clock
        // duration from "we've finished assembling the request" through
        // "the provider stream is fully drained" — i.e. exactly the window
        // a user perceives as "thinking…". Pair with `request prepared`
        // above to attribute latency: large `system_prompt_chars` + slow
        // round-trip → first-byte cost; small prompt + slow round-trip →
        // network / model queue.
        //
        // `agent.timeout_seconds` is the same engine-level wall-clock budget
        // the CLI runner feeds to its process-supervisor watchdog
        // (`agent_runner::cli`'s `bg_timeout_ms = agent.timeout_seconds *
        // 1000`); the API path has no subprocess to supervise, so
        // `tokio::time::timeout` is the equivalent backstop here. Dropping
        // the `run_session` future on expiry stops polling it — any
        // in-flight provider request it held is cancelled at its next await
        // point, same as the CLI watchdog killing the child process.
        let session_started_at = std::time::Instant::now();
        let run_timeout = std::time::Duration::from_secs(agent.timeout_seconds);
        let outcome = match tokio::time::timeout(run_timeout, run_session(messages, runner_ctx, config)).await {
            Ok(result) => result,
            Err(_elapsed) => {
                let session_elapsed_ms = session_started_at.elapsed().as_millis() as u64;
                tracing::warn!(
                    target: "ao_engine::request",
                    agent_id = %agent_id,
                    run_id = %run_id,
                    elapsed_ms = session_elapsed_ms,
                    timeout_seconds = agent.timeout_seconds,
                    "native agent run exceeded its configured timeout",
                );
                // Persist whatever streamed text the adapter had already
                // buffered before the deadline hit, so a long partial answer
                // isn't silently discarded even though the turn itself is
                // cut short.
                adapter.flush_text();
                adapter.persist_pending().await;
                let msg = format!(
                    "Agent run exceeded its configured timeout of {}s",
                    agent.timeout_seconds
                );
                self.event_bus
                    .emit(
                        &run_id,
                        &event_agent_id,
                        thread_id.clone(),
                        AgentEventPayload::Error {
                            message: msg.clone(),
                            recoverable: false,
                        },
                    )
                    .await;
                self.event_bus
                    .emit(
                        &run_id,
                        &event_agent_id,
                        thread_id.clone(),
                        AgentEventPayload::RunEnded {
                            reason: RunEndReason::TimedOut,
                        },
                    )
                    .await;
                // `_instance_guard` drops on the return below, clearing the
                // registry overlay. No `RunComplete` is sent on this path —
                // mirrors the provider-not-configured early return above,
                // which the queue manager already treats as a completed (if
                // failed) dispatch: it synthesizes its own visible error and
                // clears any tracked assignment run when `run()` returns
                // `Err` without a `RunComplete`.
                return Err(AoError::Provider(msg));
            }
        };
        let session_elapsed_ms = session_started_at.elapsed().as_millis() as u64;
        tracing::info!(
            target: "ao_engine::request",
            agent_id = %agent_id,
            run_id = %run_id,
            elapsed_ms = session_elapsed_ms,
            ok = outcome.is_ok(),
            "request completed",
        );

        // Flush any trailing text before emitting RunEnded, then persist all
        // accumulated transcript entries inline.
        adapter.flush_text();
        adapter.persist_pending().await;

        let (end_reason, output_text) = match &outcome {
            // `cancelled: true` with the run's own cancellation token never
            // having fired can only mean the turn cap tripped —
            // `query_loop::run_session`'s turn-cap check (see its doc on
            // `RunnerConfig::max_turns`) reports the same `cancelled: true`
            // shape a genuine cancel does, but a genuine cancel always fires
            // `handle.cancel` first (it's the same token threaded into
            // `runner_ctx.cancel` above). Surfaced as its own named terminal
            // event — folding it into plain `Cancelled` would read as an
            // intentional stop nobody actually asked for.
            Ok(SessionOutcome { cancelled: true, turns, final_assistant_text, .. })
                if !handle.cancel.is_cancelled() =>
            {
                tracing::warn!(
                    agent_id = %agent_id,
                    run_id = %run_id,
                    turns = turns,
                    max_turns,
                    "native agent run stopped at its configured turn limit"
                );
                let msg = format!(
                    "Agent run stopped after reaching its configured turn limit of {max_turns} turns"
                );
                self.event_bus
                    .emit(
                        &run_id,
                        &event_agent_id,
                        thread_id.clone(),
                        AgentEventPayload::Error {
                            message: msg.clone(),
                            recoverable: false,
                        },
                    )
                    .await;
                (RunEndReason::TurnLimitReached, final_assistant_text.clone())
            }
            Ok(SessionOutcome { cancelled: true, turns, final_assistant_text, .. }) => {
                tracing::info!(
                    agent_id = %agent_id,
                    run_id = %run_id,
                    turns = turns,
                    "native agent run cancelled"
                );
                (RunEndReason::Cancelled, final_assistant_text.clone())
            }
            Ok(SessionOutcome { turns, final_assistant_text, .. }) => {
                tracing::info!(
                    agent_id = %agent_id,
                    run_id = %run_id,
                    turns = turns,
                    output_len = final_assistant_text.len(),
                    "native agent run completed"
                );
                (RunEndReason::Completed, final_assistant_text.clone())
            }
            Err(e) => {
                tracing::error!(
                    agent_id = %agent_id,
                    run_id = %run_id,
                    error = %e,
                    "native agent run failed"
                );
                (RunEndReason::Error, String::new())
            }
        };

        self.event_bus
            .emit(
                &run_id,
                &event_agent_id,
                thread_id.clone(),
                AgentEventPayload::RunEnded { reason: end_reason },
            )
            .await;

        // `_instance_guard` drops at function exit — Drop spawns the async
        // `unregister_run` so the registry overlay clears. `has_active_run`
        // is overlaid at read time from the instance registry; no snapshot
        // mutation needed here.

        if let Err(e) = &outcome {
            return Err(AoError::Provider(e.to_string()));
        }

        let run_complete = RunComplete {
            run_id,
            output_text,
            workflow_followups: vec![],
            end_reason,
        };

        // Notify the queue manager via the completion channel (best-effort).
        let _ = run_complete_tx.send(run_complete.clone()).await;

        Ok(run_complete)
    }

    fn mode(&self) -> AgentRunnerMode {
        AgentRunnerMode::Api
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};
    use ao_engine_tools_core::Registry;
    use ao_persistence::paths::DataRoot;
    use ao_protocol::agent::{AgentProfile, AgentRunnerMode};

    use crate::agent_runner::{AgentRunRequest, RunScope, RunningAgents};
    use crate::event_bus::EventBus;
    use crate::instance_registry::InstanceRegistry;
    use ao_protocol::event::AgentEventPayload;

    struct MockProviderFactory {
        client: Arc<MockProviderClient>,
    }

    impl ProviderFactory for MockProviderFactory {
        fn build(&self, _agent: &AgentProfile) -> Result<Arc<dyn ProviderClient>, ProviderError> {
            Ok(Arc::clone(&self.client) as Arc<dyn ProviderClient>)
        }
    }

    async fn make_test_persistence() -> Arc<PersistenceLayer> {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let data_root = DataRoot::new(tmp.path());
        data_root.ensure_directories().await.expect("ensure_directories");
        let p = PersistenceLayer::init_with_root(data_root).await.expect("init persistence");
        // Keep tmp alive by leaking it (test process is short-lived).
        std::mem::forget(tmp);
        Arc::new(p)
    }

    fn make_agent() -> AgentProfile {
        use ao_protocol::agent::{CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
        AgentProfile {
            id: "test-agent".to_string(),
            name: "Test".to_string(),
            description: "".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "claude".to_string(),
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
            system_prompt: Some("You are a test agent.".to_string()),
            tools: None,
            env: Default::default(),
            max_instances: 1,
            timeout_seconds: 60,
            max_turns: None,
            working_dir: None,
            home_dir: None,
            serialize: false,
            workflows: None,
            template: None,
            runner_mode: AgentRunnerMode::Api,
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
}
    }

    #[tokio::test]
    async fn mock_provider_one_turn_run_emits_expected_payload_sequence() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        let mock_client = Arc::new(MockProviderClient::new(vec![vec![
            CompletionEvent::AssistantText("hello".to_string()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ]]));

        let factory = Arc::new(MockProviderFactory { client: Arc::clone(&mock_client) });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        let runner = NativeAgentRunner::new(
            Arc::clone(&bus),
            instance_registry,
            running_agents,
            factory,
            registry,
            persistence,
        );

        let (tx, _rx) = mpsc::channel(4);
        let agent = make_agent();
        let request = AgentRunRequest {
            agent,
            prompt: "say hello".to_string(),
            attachments: vec![],
            run_complete_tx: tx,
            focus_path: None,
            scope: RunScope::Standalone,
            thread_id: None,
            session_kind: SessionKind::Interactive,
            pre_registered_run_id: None,
            ..Default::default()
        };

        let result = runner.run(request).await;
        assert!(result.is_ok(), "run should succeed: {:?}", result);
        let run_complete = result.unwrap();
        assert_eq!(run_complete.output_text, "hello");

        // Drain all events from the bus.
        let mut payloads = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(e) => payloads.push(e.payload),
                Err(_) => break,
            }
        }

        // Expected sequence: RunStarted, TextDelta("hello"), TextComplete("hello"), RunEnded(Completed)
        assert!(
            payloads.iter().any(|p| matches!(p, AgentEventPayload::RunStarted)),
            "missing RunStarted"
        );
        assert!(
            payloads.iter().any(|p| matches!(p, AgentEventPayload::TextDelta { text } if text == "hello")),
            "missing TextDelta"
        );
        assert!(
            payloads.iter().any(|p| matches!(p, AgentEventPayload::TextComplete { text } if text == "hello")),
            "missing TextComplete"
        );
        assert!(
            payloads.iter().any(|p| matches!(p, AgentEventPayload::RunEnded { reason: RunEndReason::Completed })),
            "missing RunEnded(Completed)"
        );
    }

    /// Thread-scope memory (`MemoryScope::Thread`) must inject only the
    /// *current* thread's entries into the system prompt — never another
    /// thread's — and the block must be clearly delimited from the durable
    /// Agent/Project/Global sections.
    #[tokio::test]
    async fn thread_scope_memory_injects_current_thread_only() {
        use ao_protocol::memory::MemorySource;

        let bus = Arc::new(EventBus::new(64));
        let mock_client = Arc::new(MockProviderClient::new(vec![vec![
            CompletionEvent::AssistantText("hello".to_string()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ]]));
        let factory = Arc::new(MockProviderFactory { client: Arc::clone(&mock_client) });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        persistence
            .memory
            .add_thread("thread-a", "thread-a's own working note", MemorySource::Agent)
            .await
            .expect("add thread-a memory");
        persistence
            .memory
            .add_thread("thread-b", "thread-b's unrelated working note", MemorySource::Agent)
            .await
            .expect("add thread-b memory");

        let runner = NativeAgentRunner::new(
            bus,
            instance_registry,
            running_agents,
            factory,
            registry,
            persistence,
        );

        let (tx, _rx) = mpsc::channel(4);
        let request = AgentRunRequest {
            agent: make_agent(),
            prompt: "say hello".to_string(),
            run_complete_tx: tx,
            scope: RunScope::Standalone,
            thread_id: Some("thread-a".to_string()),
            session_kind: SessionKind::Interactive,
            ..Default::default()
        };

        let result = runner.run(request).await;
        assert!(result.is_ok(), "run should succeed: {:?}", result);

        let system_prompt = mock_client.last_system_prompt().expect("system prompt must be set");
        assert!(
            system_prompt.contains("[Thread Notes]"),
            "system prompt must contain a distinct Thread Notes section: {system_prompt}"
        );
        assert!(
            system_prompt.contains("thread-a's own working note"),
            "current thread's own entry must be injected: {system_prompt}"
        );
        assert!(
            !system_prompt.contains("thread-b's unrelated working note"),
            "a different thread's entry must NOT be injected: {system_prompt}"
        );
    }

    /// A run with no active thread id must inject nothing for thread scope,
    /// and must not error — thread memory is opportunistic context, not a
    /// required input.
    #[tokio::test]
    async fn thread_scope_memory_absent_thread_id_injects_nothing() {
        let bus = Arc::new(EventBus::new(64));
        let mock_client = Arc::new(MockProviderClient::new(vec![vec![
            CompletionEvent::AssistantText("hello".to_string()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ]]));
        let factory = Arc::new(MockProviderFactory { client: Arc::clone(&mock_client) });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        let runner = NativeAgentRunner::new(
            bus,
            instance_registry,
            running_agents,
            factory,
            registry,
            persistence,
        );

        let (tx, _rx) = mpsc::channel(4);
        let request = AgentRunRequest {
            agent: make_agent(),
            prompt: "say hello".to_string(),
            run_complete_tx: tx,
            scope: RunScope::Standalone,
            thread_id: None,
            session_kind: SessionKind::Interactive,
            ..Default::default()
        };

        let result = runner.run(request).await;
        assert!(result.is_ok(), "run with no thread id must succeed, not error: {:?}", result);

        let system_prompt = mock_client.last_system_prompt().expect("system prompt must be set");
        assert!(
            !system_prompt.contains("[Thread Notes]"),
            "no thread id means no Thread Notes section should be injected: {system_prompt}"
        );
    }

    /// Regression test for the native runner dropping `focus_path` when it
    /// resolved the run's cwd (and, downstream, its project-memory key).
    /// `focus_path` must outrank `agent.working_dir` — exactly the CLI
    /// runner's `focus_path > agent.working_dir > home dir` precedence — so a
    /// delegated/tasklist run keys Project-scope memory off its actual target
    /// directory rather than the agent's static working_dir (or, with no
    /// working_dir set, process cwd/$HOME).
    #[tokio::test]
    async fn project_scope_memory_keys_off_focus_path_not_working_dir() {
        use ao_protocol::memory::MemorySource;

        let bus = Arc::new(EventBus::new(64));
        let mock_client = Arc::new(MockProviderClient::new(vec![vec![
            CompletionEvent::AssistantText("hello".to_string()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ]]));
        let factory = Arc::new(MockProviderFactory { client: Arc::clone(&mock_client) });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        // Two distinct directories: `focus_dir` is the run's actual target
        // (what a delegation/tasklist/assignment would pass as `focus_path`);
        // `working_dir` is the agent's unrelated static working directory.
        // Before the fix, `focus_path` was discarded entirely and the native
        // runner's cwd — and therefore its project-memory key — resolved to
        // `working_dir` regardless.
        let focus_dir = tempfile::tempdir().expect("focus tempdir");
        let working_dir = tempfile::tempdir().expect("working_dir tempdir");

        let focus_key = ao_persistence::project_key::resolve_project_key(focus_dir.path())
            .await
            .expect("resolve focus_dir project key");
        let focus_hash = ao_persistence::project_key::hash_project_key(&focus_key);
        let working_key = ao_persistence::project_key::resolve_project_key(working_dir.path())
            .await
            .expect("resolve working_dir project key");
        let working_hash = ao_persistence::project_key::hash_project_key(&working_key);

        persistence
            .memory
            .add_project(&focus_hash, "focus_path project fact", MemorySource::Manual)
            .await
            .expect("add project memory keyed to focus_dir");
        persistence
            .memory
            .add_project(&working_hash, "working_dir decoy fact", MemorySource::Manual)
            .await
            .expect("add project memory keyed to working_dir");

        let runner = NativeAgentRunner::new(
            bus,
            instance_registry,
            running_agents,
            factory,
            registry,
            persistence,
        );

        let (tx, _rx) = mpsc::channel(4);
        let mut agent = make_agent();
        agent.working_dir = Some(working_dir.path().to_string_lossy().into_owned());
        let request = AgentRunRequest {
            agent,
            prompt: "say hello".to_string(),
            run_complete_tx: tx,
            focus_path: Some(focus_dir.path().to_string_lossy().into_owned()),
            scope: RunScope::Standalone,
            thread_id: None,
            session_kind: SessionKind::Interactive,
            ..Default::default()
        };

        let result = runner.run(request).await;
        assert!(result.is_ok(), "run should succeed: {:?}", result);

        let system_prompt = mock_client.last_system_prompt().expect("system prompt must be set");
        assert!(
            system_prompt.contains("focus_path project fact"),
            "project memory keyed to focus_path's directory must be injected: {system_prompt}"
        );
        assert!(
            !system_prompt.contains("working_dir decoy fact"),
            "project memory keyed to agent.working_dir must NOT be injected when focus_path is set: {system_prompt}"
        );
    }

    /// A delegated child run (isolate_history + transcript_override +
    /// event_channel) must leave the agent's personal transcript untouched,
    /// write its turns to the override file, and emit every live event on the
    /// delegate channel — never on the agent's own channel. This is the guard
    /// against clone-parent delegates splicing their output into the parent's
    /// chat history.
    #[tokio::test]
    async fn isolated_run_routes_transcript_and_events_to_sidechain() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        let mock_client = Arc::new(MockProviderClient::new(vec![vec![
            CompletionEvent::AssistantText("child says hi".to_string()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ]]));

        let factory = Arc::new(MockProviderFactory { client: mock_client });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        let runner = NativeAgentRunner::new(
            Arc::clone(&bus),
            instance_registry,
            running_agents,
            factory,
            registry,
            Arc::clone(&persistence),
        );

        let override_path = persistence
            .data_root
            .root()
            .join("messages")
            .join("data")
            .join("bg-test.jsonl");

        let (tx, _rx2) = mpsc::channel(4);
        let request = AgentRunRequest {
            agent: make_agent(),
            prompt: "do the thing".to_string(),
            run_complete_tx: tx,
            session_kind: SessionKind::Autonomous,
            isolate_history: true,
            transcript_override: Some(override_path.clone()),
            event_channel: Some("delegate:bg-test".to_string()),
            ..Default::default()
        };

        let result = runner.run(request).await;
        assert!(result.is_ok(), "run should succeed: {:?}", result);

        // Personal transcript must stay empty.
        let personal = persistence
            .transcripts
            .read_recent("test-agent", 10)
            .await
            .unwrap_or_default();
        assert!(
            personal.is_empty(),
            "personal transcript must not receive child entries; got {} entries",
            personal.len()
        );

        // The override file must hold the child's response.
        let sidechain = persistence
            .transcripts
            .read_recent_for_run("test-agent", Some(override_path.as_path()), 10)
            .await
            .expect("read sidechain transcript");
        assert!(
            sidechain.iter().any(|e| e.content.contains("child says hi")),
            "sidechain transcript must contain the child's response; got {} entries",
            sidechain.len()
        );

        // Every live event must ride the delegate channel.
        let mut saw_delegate_channel = false;
        loop {
            match rx.try_recv() {
                Ok(e) => {
                    assert_ne!(
                        e.agent_id, "test-agent",
                        "no live event may emit on the agent's own channel (payload: {:?})",
                        e.payload
                    );
                    if e.agent_id == "delegate:bg-test" {
                        saw_delegate_channel = true;
                    }
                }
                Err(_) => break,
            }
        }
        assert!(saw_delegate_channel, "events must emit on the delegate channel");
    }

    /// Without a transcript_override, an isolated run must not write
    /// anywhere — CLI-parity floor: skip persistence rather than fall back
    /// to the personal file.
    #[tokio::test]
    async fn isolated_run_without_override_skips_personal_transcript() {
        let bus = Arc::new(EventBus::new(64));

        let mock_client = Arc::new(MockProviderClient::new(vec![vec![
            CompletionEvent::AssistantText("quiet child".to_string()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ]]));

        let factory = Arc::new(MockProviderFactory { client: mock_client });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        let runner = NativeAgentRunner::new(
            Arc::clone(&bus),
            instance_registry,
            running_agents,
            factory,
            registry,
            Arc::clone(&persistence),
        );

        let (tx, _rx2) = mpsc::channel(4);
        let request = AgentRunRequest {
            agent: make_agent(),
            prompt: "do the thing".to_string(),
            run_complete_tx: tx,
            session_kind: SessionKind::Autonomous,
            isolate_history: true,
            ..Default::default()
        };

        let result = runner.run(request).await;
        assert!(result.is_ok(), "run should succeed: {:?}", result);

        let personal = persistence
            .transcripts
            .read_recent("test-agent", 10)
            .await
            .unwrap_or_default();
        assert!(
            personal.is_empty(),
            "isolated run without override must not write to the personal transcript; got {} entries",
            personal.len()
        );
    }

    // ─── DefaultProviderFactory::build routing (regression guard) ──────────
    //
    // These call the REAL `DefaultProviderFactory::build` — no
    // `MockProviderFactory` — so they exercise the actual match on
    // `agent.native_provider` this module documents at the top of the file.
    // A future change that re-hardcodes any of the three arms to always
    // build `AnthropicClient` (or drops the `Openai`/`OpenRouter` arms
    // entirely) will make one of these fail: each asserts on the
    // constructed client's `default_model()`, which is only equal to the
    // sentinel value from the matching `providers.toml` section when the
    // right concrete client type was actually built.
    //
    // These do NOT prove the routing survives all the way to a spawned
    // subagent — see `crates/ao-engine/tests/subagent_provider_routing.rs`
    // for that (load-bearing) layer.

    /// Writes a `providers.toml` with a distinct, greppable `model` string
    /// per provider so a test can tell which concrete client got built from
    /// nothing but `ProviderClient::default_model()`. `base_url` points at
    /// an address nothing listens on (`http://127.0.0.1:1`) — irrelevant
    /// here since these tests never call `.complete()`, but keeps the
    /// fixture safe to reuse if a future test extends it into one that does.
    fn write_sentinel_providers_toml(dir: &std::path::Path) {
        let toml = r#"
[anthropic]
model = "claude-sentinel-anthropic"
base_url = "http://127.0.0.1:1"

[openai]
model = "gpt-sentinel-openai"
base_url = "http://127.0.0.1:1"

[openrouter]
model = "or-sentinel-openrouter"
base_url = "http://127.0.0.1:1"
"#;
        std::fs::write(dir.join("providers.toml"), toml).expect("write providers.toml fixture");
    }

    /// Points `LAUNCHPAD_STUDIO_DATA_DIR` at a fresh tempdir carrying the
    /// sentinel `providers.toml` and forces the file-backed secret vault so
    /// `DefaultProviderFactory::build`'s `ProviderConfig::load()` /
    /// `SecretVault::open()` calls never touch the real data root or OS
    /// keychain. Returns the tempdir (kept alive by the caller) plus a
    /// guard that restores both env vars on drop. Must be called while
    /// holding `crate::plugin_paths::tests::ENV_LOCK` — see that lock's doc
    /// for why this crate has exactly one env-var lock.
    struct ProviderEnvGuard;
    impl Drop for ProviderEnvGuard {
        fn drop(&mut self) {
            std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");
            std::env::remove_var("LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK");
        }
    }
    fn set_up_provider_env(dir: &std::path::Path) -> ProviderEnvGuard {
        write_sentinel_providers_toml(dir);
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", dir);
        std::env::set_var("LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK", "1");
        ProviderEnvGuard
    }

    #[test]
    fn default_provider_factory_routes_absent_native_provider_to_anthropic() {
        let _lock = crate::plugin_paths::tests::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = set_up_provider_env(tmp.path());

        let mut agent = make_agent();
        agent.native_provider = None;
        let client = DefaultProviderFactory.build(&agent).expect("build must succeed");
        assert_eq!(
            client.default_model().as_deref(),
            Some("claude-sentinel-anthropic"),
            "absent native_provider must fall back to the Anthropic client (documented default)"
        );
    }

    #[test]
    fn default_provider_factory_routes_explicit_anthropic() {
        let _lock = crate::plugin_paths::tests::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = set_up_provider_env(tmp.path());

        let mut agent = make_agent();
        agent.native_provider = Some(ao_protocol::agent::NativeProvider::Anthropic);
        let client = DefaultProviderFactory.build(&agent).expect("build must succeed");
        assert_eq!(
            client.default_model().as_deref(),
            Some("claude-sentinel-anthropic"),
            "explicit NativeProvider::Anthropic must build the Anthropic client"
        );
    }

    #[test]
    fn default_provider_factory_routes_openai() {
        let _lock = crate::plugin_paths::tests::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = set_up_provider_env(tmp.path());

        let mut agent = make_agent();
        agent.native_provider = Some(ao_protocol::agent::NativeProvider::Openai);
        let client = DefaultProviderFactory.build(&agent).expect("build must succeed");
        assert_eq!(
            client.default_model().as_deref(),
            Some("gpt-sentinel-openai"),
            "NativeProvider::Openai must build the OpenAI client, not silently fall back to Anthropic"
        );
    }

    #[test]
    fn default_provider_factory_routes_openrouter() {
        let _lock = crate::plugin_paths::tests::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = set_up_provider_env(tmp.path());

        let mut agent = make_agent();
        agent.native_provider = Some(ao_protocol::agent::NativeProvider::OpenRouter);
        let client = DefaultProviderFactory.build(&agent).expect("build must succeed");
        assert_eq!(
            client.default_model().as_deref(),
            Some("or-sentinel-openrouter"),
            "NativeProvider::OpenRouter must build the OpenRouter-configured client, not silently fall back to Anthropic"
        );
    }

    // ── agent.timeout_seconds enforcement ───────────────────────────────

    /// A provider whose `complete()` call blocks for a configurable delay
    /// before ever producing a stream. Stands in for a hung or
    /// pathologically looping upstream call so the timeout wrapper in
    /// `NativeAgentRunner::run` can be exercised deterministically without
    /// depending on a real provider.
    struct SlowProviderClient {
        delay: std::time::Duration,
        normalizer: ao_engine_tools_runner::message::normalizer::MockNormalizer,
    }

    impl SlowProviderClient {
        fn new(delay: std::time::Duration) -> Self {
            Self {
                delay,
                normalizer: ao_engine_tools_runner::message::normalizer::MockNormalizer,
            }
        }
    }

    #[async_trait]
    impl ProviderClient for SlowProviderClient {
        async fn complete(
            &self,
            _request: ao_engine_tools_runner::provider::CompletionRequest,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<ao_engine_tools_runner::provider::CompletionStream, ProviderError> {
            tokio::time::sleep(self.delay).await;
            // Never actually reached by the timeout tests below — the outer
            // `tokio::time::timeout` in `NativeAgentRunner::run` cuts the run
            // off well before `self.delay` elapses.
            Err(ProviderError::ScriptExhausted)
        }

        fn message_normalizer(&self) -> &dyn ao_engine_tools_runner::message::MessageNormalizer {
            &self.normalizer
        }
    }

    struct SlowProviderFactory {
        delay: std::time::Duration,
    }

    impl ProviderFactory for SlowProviderFactory {
        fn build(&self, _agent: &AgentProfile) -> Result<Arc<dyn ProviderClient>, ProviderError> {
            Ok(Arc::new(SlowProviderClient::new(self.delay)) as Arc<dyn ProviderClient>)
        }
    }

    /// (a) A native run whose provider call outlives `agent.timeout_seconds`
    /// is cut off at the configured budget instead of running unbounded:
    /// `run()` returns `Err`, and the event bus carries a visible `Error`
    /// naming the timeout and its configured value, followed by
    /// `RunEnded { reason: TimedOut }` — the same class of user-visible
    /// outcome the CLI runner's process-supervisor watchdog produces on
    /// expiry.
    #[tokio::test]
    async fn run_exceeding_timeout_seconds_is_cut_off_and_emits_error() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        // The provider call takes far longer than the configured timeout;
        // if enforcement regresses, this test would hang for 30s instead of
        // failing fast.
        let factory = Arc::new(SlowProviderFactory { delay: std::time::Duration::from_secs(30) });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        let runner = NativeAgentRunner::new(
            Arc::clone(&bus),
            instance_registry,
            running_agents,
            factory,
            registry,
            persistence,
        );

        let mut agent = make_agent();
        agent.timeout_seconds = 1;

        let (tx, _rx) = mpsc::channel(4);
        let request = AgentRunRequest {
            agent,
            prompt: "loop forever".to_string(),
            attachments: vec![],
            run_complete_tx: tx,
            focus_path: None,
            scope: RunScope::Standalone,
            thread_id: None,
            session_kind: SessionKind::Interactive,
            pre_registered_run_id: None,
            ..Default::default()
        };

        let started = std::time::Instant::now();
        let result = runner.run(request).await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "run exceeding timeout_seconds must return Err, got {:?}", result);
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "run must be cut off at the configured timeout, not the provider's full delay; elapsed={:?}",
            elapsed
        );

        let mut payloads = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(e) => payloads.push(e.payload),
                Err(_) => break,
            }
        }

        let error_message = payloads.iter().find_map(|p| match p {
            AgentEventPayload::Error { message, recoverable } if !recoverable => Some(message.clone()),
            _ => None,
        });
        let message = error_message.expect("missing AgentEventPayload::Error on timeout");
        assert!(
            message.contains("timeout") && message.contains('1'),
            "timeout error message must name the timeout and its configured value; got: {:?}",
            message
        );
        assert!(
            payloads.iter().any(|p| matches!(p, AgentEventPayload::RunEnded { reason: RunEndReason::TimedOut })),
            "missing RunEnded(TimedOut)"
        );
    }

    /// (b) A run that finishes comfortably inside `timeout_seconds` is
    /// unaffected by the timeout wrapper: same success path, same output,
    /// no `Error`/`TimedOut` events.
    #[tokio::test]
    async fn run_finishing_inside_timeout_budget_is_unaffected() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        let mock_client = Arc::new(MockProviderClient::new(vec![vec![
            CompletionEvent::AssistantText("hello".to_string()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ]]));
        let factory = Arc::new(MockProviderFactory { client: Arc::clone(&mock_client) });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        let runner = NativeAgentRunner::new(
            Arc::clone(&bus),
            instance_registry,
            running_agents,
            factory,
            registry,
            persistence,
        );

        let mut agent = make_agent();
        agent.timeout_seconds = 5;

        let (tx, _rx) = mpsc::channel(4);
        let request = AgentRunRequest {
            agent,
            prompt: "say hello".to_string(),
            attachments: vec![],
            run_complete_tx: tx,
            focus_path: None,
            scope: RunScope::Standalone,
            thread_id: None,
            session_kind: SessionKind::Interactive,
            pre_registered_run_id: None,
            ..Default::default()
        };

        let result = runner.run(request).await;
        assert!(result.is_ok(), "run finishing inside budget must succeed: {:?}", result);
        assert_eq!(result.unwrap().output_text, "hello");

        let mut payloads = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(e) => payloads.push(e.payload),
                Err(_) => break,
            }
        }
        assert!(
            !payloads.iter().any(|p| matches!(p, AgentEventPayload::Error { .. })),
            "a run finishing inside budget must not emit an Error event"
        );
        assert!(
            payloads.iter().any(|p| matches!(p, AgentEventPayload::RunEnded { reason: RunEndReason::Completed })),
            "missing RunEnded(Completed)"
        );
    }

    /// (c) A profile carrying `timeout_seconds: 300` — the value
    /// `ao_protocol::agent`'s `#[serde(default = "default_timeout_seconds")]`
    /// resolves an omitted field to — behaves exactly as it did before this
    /// runner read the field at all: the timeout wrapper introduces no
    /// observable change for ordinary, non-overridden profiles.
    #[tokio::test]
    async fn default_timeout_seconds_profile_behaves_as_before() {
        let bus = Arc::new(EventBus::new(64));

        let mock_client = Arc::new(MockProviderClient::new(vec![vec![
            CompletionEvent::AssistantText("hello".to_string()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ]]));
        let factory = Arc::new(MockProviderFactory { client: Arc::clone(&mock_client) });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        let runner = NativeAgentRunner::new(
            bus,
            instance_registry,
            running_agents,
            factory,
            registry,
            persistence,
        );

        let mut agent = make_agent();
        agent.timeout_seconds = 300;

        let (tx, _rx) = mpsc::channel(4);
        let request = AgentRunRequest {
            agent,
            prompt: "say hello".to_string(),
            attachments: vec![],
            run_complete_tx: tx,
            focus_path: None,
            scope: RunScope::Standalone,
            thread_id: None,
            session_kind: SessionKind::Interactive,
            pre_registered_run_id: None,
            ..Default::default()
        };

        let result = runner.run(request).await;
        assert!(result.is_ok(), "default-timeout run must succeed exactly as before: {:?}", result);
        assert_eq!(result.unwrap().output_text, "hello");
    }

    // ── agent.max_turns enforcement ─────────────────────────────────────

    /// (a) A native run whose model keeps calling tools past
    /// `agent.max_turns` is force-stopped at the configured cap instead of
    /// running unbounded: `run()` still returns `Ok` (the query loop exits
    /// cleanly, it never gets externally aborted the way the timeout wrapper
    /// does), but the event bus carries a visible `Error` naming the limit
    /// and its configured value, followed by `RunEnded { reason:
    /// TurnLimitReached }` — never a bare `Cancelled`, which would read as
    /// an intentional stop nobody asked for.
    #[tokio::test]
    async fn run_hitting_max_turns_cap_terminates_and_emits_visible_terminal_event() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        // Two turns, each calling a tool that doesn't exist in the (empty)
        // test registry — the loop treats an unknown tool as a soft
        // tool_result error and keeps going, so nothing here ever lets the
        // model naturally stop on its own. With `max_turns: Some(2)` the cap
        // check trips right after the 2nd turn completes, before a 3rd
        // provider call would ever be made — this mock is scripted with
        // exactly 2 turns so a regression that fails to cap would surface as
        // `ProviderError::ScriptExhausted` instead of silently looping.
        let mock_client = Arc::new(MockProviderClient::new(vec![
            vec![
                CompletionEvent::ToolUse {
                    id: "call-1".to_string(),
                    name: "nonexistent_tool".to_string(),
                    input: serde_json::json!({}),
                },
                CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
            ],
            vec![
                CompletionEvent::ToolUse {
                    id: "call-2".to_string(),
                    name: "nonexistent_tool".to_string(),
                    input: serde_json::json!({}),
                },
                CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
            ],
        ]));
        let factory = Arc::new(MockProviderFactory { client: Arc::clone(&mock_client) });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        let runner = NativeAgentRunner::new(
            Arc::clone(&bus),
            instance_registry,
            running_agents,
            factory,
            registry,
            persistence,
        );

        let mut agent = make_agent();
        agent.max_turns = Some(2);

        let (tx, _rx) = mpsc::channel(4);
        let request = AgentRunRequest {
            agent,
            prompt: "keep calling tools forever".to_string(),
            attachments: vec![],
            run_complete_tx: tx,
            focus_path: None,
            scope: RunScope::Standalone,
            thread_id: None,
            session_kind: SessionKind::Interactive,
            pre_registered_run_id: None,
            ..Default::default()
        };

        let result = runner.run(request).await;
        assert!(result.is_ok(), "a turn-cap stop is a clean exit, not an Err: {:?}", result);
        assert_eq!(result.unwrap().end_reason, RunEndReason::TurnLimitReached);

        let mut payloads = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(e) => payloads.push(e.payload),
                Err(_) => break,
            }
        }

        let error_message = payloads.iter().find_map(|p| match p {
            AgentEventPayload::Error { message, recoverable } if !recoverable => Some(message.clone()),
            _ => None,
        });
        let message = error_message.expect("missing AgentEventPayload::Error on turn-cap trip");
        assert!(
            message.contains("turn limit") && message.contains('2'),
            "turn-limit error message must name the limit and its configured value; got: {:?}",
            message
        );
        assert!(
            payloads.iter().any(|p| matches!(p, AgentEventPayload::RunEnded { reason: RunEndReason::TurnLimitReached })),
            "missing RunEnded(TurnLimitReached)"
        );
        assert!(
            !payloads.iter().any(|p| matches!(p, AgentEventPayload::RunEnded { reason: RunEndReason::Cancelled })),
            "a turn-cap trip must never be reported as a plain Cancelled"
        );
    }

    /// (b) A run that naturally completes in fewer turns than `max_turns`
    /// is unaffected by the cap: same success path, same output, no
    /// `Error`/`TurnLimitReached` events.
    #[tokio::test]
    async fn run_finishing_under_max_turns_is_unaffected() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        let mock_client = Arc::new(MockProviderClient::new(vec![vec![
            CompletionEvent::AssistantText("hello".to_string()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ]]));
        let factory = Arc::new(MockProviderFactory { client: Arc::clone(&mock_client) });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        let runner = NativeAgentRunner::new(
            Arc::clone(&bus),
            instance_registry,
            running_agents,
            factory,
            registry,
            persistence,
        );

        let mut agent = make_agent();
        agent.max_turns = Some(5);

        let (tx, _rx) = mpsc::channel(4);
        let request = AgentRunRequest {
            agent,
            prompt: "say hello".to_string(),
            attachments: vec![],
            run_complete_tx: tx,
            focus_path: None,
            scope: RunScope::Standalone,
            thread_id: None,
            session_kind: SessionKind::Interactive,
            pre_registered_run_id: None,
            ..Default::default()
        };

        let result = runner.run(request).await;
        assert!(result.is_ok(), "run finishing under the cap must succeed: {:?}", result);
        let run_complete = result.unwrap();
        assert_eq!(run_complete.output_text, "hello");
        assert_eq!(run_complete.end_reason, RunEndReason::Completed);

        let mut payloads = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(e) => payloads.push(e.payload),
                Err(_) => break,
            }
        }
        assert!(
            !payloads.iter().any(|p| matches!(p, AgentEventPayload::Error { .. })),
            "a run finishing under the cap must not emit an Error event"
        );
        assert!(
            payloads.iter().any(|p| matches!(p, AgentEventPayload::RunEnded { reason: RunEndReason::Completed })),
            "missing RunEnded(Completed)"
        );
    }

    /// (c) A profile that never sets `max_turns` is still capped — enforced
    /// through the real `agent.max_turns.unwrap_or(DEFAULT_MAX_TURNS)`
    /// resolution in `run()`, not just asserted against the constant in
    /// isolation. Scripts exactly [`ao_protocol::agent::DEFAULT_MAX_TURNS`]
    /// turns, same trick as test (a): if a regression stopped resolving the
    /// fallback (e.g. left the cap unset), the mock would run out of
    /// scripted turns and fail with `ProviderError::ScriptExhausted` instead
    /// of cleanly tripping `TurnLimitReached`.
    #[tokio::test]
    async fn run_hitting_default_max_turns_cap_when_profile_leaves_it_unset() {
        // A generous capacity, not 64 like the sibling tests above — this
        // run emits several events per turn across DEFAULT_MAX_TURNS turns,
        // and a broadcast channel too small to hold all of them would make
        // the receiver lag and drop the very payloads this test inspects
        // (a silent-drop failure mode, not a real defect in the runner).
        let bus = Arc::new(EventBus::new(4096));
        let mut rx = bus.subscribe();

        let default_cap = ao_protocol::agent::DEFAULT_MAX_TURNS as usize;
        let scripted_turns: Vec<Vec<CompletionEvent>> = (0..default_cap)
            .map(|i| {
                vec![
                    CompletionEvent::ToolUse {
                        id: format!("call-{i}"),
                        name: "nonexistent_tool".to_string(),
                        input: serde_json::json!({}),
                    },
                    CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
                ]
            })
            .collect();
        let mock_client = Arc::new(MockProviderClient::new(scripted_turns));
        let factory = Arc::new(MockProviderFactory { client: Arc::clone(&mock_client) });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        let runner = NativeAgentRunner::new(
            Arc::clone(&bus),
            instance_registry,
            running_agents,
            factory,
            registry,
            persistence,
        );

        // `make_agent()` already leaves `max_turns: None` — asserted here so
        // this test still means what it says if that default ever changes.
        let agent = make_agent();
        assert_eq!(agent.max_turns, None, "this test only proves something if the profile leaves max_turns unset");

        let (tx, _rx) = mpsc::channel(4);
        let request = AgentRunRequest {
            agent,
            prompt: "keep calling tools forever".to_string(),
            attachments: vec![],
            run_complete_tx: tx,
            focus_path: None,
            scope: RunScope::Standalone,
            thread_id: None,
            session_kind: SessionKind::Interactive,
            pre_registered_run_id: None,
            ..Default::default()
        };

        let result = runner.run(request).await;
        assert!(result.is_ok(), "a turn-cap stop is a clean exit, not an Err: {:?}", result);
        assert_eq!(result.unwrap().end_reason, RunEndReason::TurnLimitReached);

        let mut payloads = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(e) => payloads.push(e.payload),
                Err(_) => break,
            }
        }

        let error_message = payloads.iter().find_map(|p| match p {
            AgentEventPayload::Error { message, recoverable } if !recoverable => Some(message.clone()),
            _ => None,
        });
        let message = error_message.expect("missing AgentEventPayload::Error on turn-cap trip");
        assert!(
            message.contains("turn limit") && message.contains(&default_cap.to_string()),
            "turn-limit error message must name the fallback default's value ({default_cap}); got: {:?}",
            message
        );
    }

    /// (d) An explicit per-profile `max_turns` always wins over
    /// [`ao_protocol::agent::DEFAULT_MAX_TURNS`] — set it below the default
    /// and confirm the run caps at the smaller explicit value rather than
    /// drifting up to the fallback.
    #[tokio::test]
    async fn explicit_max_turns_overrides_default_even_when_smaller() {
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        let explicit_cap: u32 = 3;
        assert!(
            (explicit_cap as usize) < ao_protocol::agent::DEFAULT_MAX_TURNS as usize,
            "this test only proves an override happened if the explicit cap is below the default"
        );
        let scripted_turns: Vec<Vec<CompletionEvent>> = (0..explicit_cap)
            .map(|i| {
                vec![
                    CompletionEvent::ToolUse {
                        id: format!("call-{i}"),
                        name: "nonexistent_tool".to_string(),
                        input: serde_json::json!({}),
                    },
                    CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
                ]
            })
            .collect();
        let mock_client = Arc::new(MockProviderClient::new(scripted_turns));
        let factory = Arc::new(MockProviderFactory { client: Arc::clone(&mock_client) });
        let running_agents = Arc::new(RunningAgents::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let registry = Arc::new(Registry::default());
        let persistence = make_test_persistence().await;

        let runner = NativeAgentRunner::new(
            Arc::clone(&bus),
            instance_registry,
            running_agents,
            factory,
            registry,
            persistence,
        );

        let mut agent = make_agent();
        agent.max_turns = Some(explicit_cap);

        let (tx, _rx) = mpsc::channel(4);
        let request = AgentRunRequest {
            agent,
            prompt: "keep calling tools forever".to_string(),
            attachments: vec![],
            run_complete_tx: tx,
            focus_path: None,
            scope: RunScope::Standalone,
            thread_id: None,
            session_kind: SessionKind::Interactive,
            pre_registered_run_id: None,
            ..Default::default()
        };

        let result = runner.run(request).await;
        assert!(result.is_ok(), "a turn-cap stop is a clean exit, not an Err: {:?}", result);
        assert_eq!(result.unwrap().end_reason, RunEndReason::TurnLimitReached);

        let mut payloads = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(e) => payloads.push(e.payload),
                Err(_) => break,
            }
        }
        let error_message = payloads.iter().find_map(|p| match p {
            AgentEventPayload::Error { message, recoverable } if !recoverable => Some(message.clone()),
            _ => None,
        });
        let message = error_message.expect("missing AgentEventPayload::Error on turn-cap trip");
        assert!(
            message.contains("turn limit") && message.contains(&explicit_cap.to_string()),
            "turn-limit error message must name the explicit override ({explicit_cap}), not the default; got: {:?}",
            message
        );
    }
}

