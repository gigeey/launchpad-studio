use std::sync::Arc;

use ao_normalizer::registry::NormalizerRegistry;
use ao_persistence::workflow_store::{TaskStore, WorkflowStore};
use ao_persistence::PersistenceLayer;
use ao_process::default_supervisor::DefaultProcessSupervisor;
use ao_process::mock::MockProcessSupervisor;
use ao_process::supervisor::ProcessSupervisor;
use ao_protocol::error::AoError;
use tokio::sync::{watch, Mutex, RwLock};
use tokio::task::JoinHandle;

use ao_engine_tools_core::{AgentProfileCacheInvalidator, ClassifierInFlight, Registry};
use ao_engine_tools_core::background_agents::{SubagentRegistry, SubagentSpawner};
use ao_engine_tools_engine::{
    register_all as register_engine_tools, AgentAuthor, Delegate, DelegateOutput, DelegateStop, SendEmail,
};
use ao_persistence::profiles::AgentProfileStore;
use ao_engine_tools_io::register_all as register_io_tools;
use ao_engine_tools_runner::background_agents::FileSidechainPersister;
use ao_engine_tools_runner::mcp::McpManager;
use ao_engine_tools_runner::prompt_bridge::FormBridgeRegistry;
use ao_engine_tools_provider_config::{ChannelSecretStore, McpServersConfig, McpTokenStore};
use crate::agent_runner::{
    CliAgentRunner, DefaultProviderFactory, NativeAgentRunner, ProfileAwareChildRunner,
    ProviderFactory, RunnerDispatcher, RunningAgents,
};
use crate::agent_cascade::AgentCascadeService;
use crate::artifact_task_status::ArtifactTaskStatusStore;
use crate::classifier_reconciler::ClassifierReconciler;
use crate::task_transcript_pruner::{PrunerConfig, TaskTranscriptPruner, TranscriptPrunerRunner};
use crate::task_classifier::TaskClassifier;
use crate::history::anchor::WindowAnchorRegistry;
use crate::agent_sleep_guard::AgentSleepGuardRunner;
use crate::command_queue::CommandQueue;
use crate::context_cache::ContextCache;
use crate::dispatch_watchdog::DispatchWatchdogRunner;
use crate::event_bus::EventBus;
use crate::instance_registry::InstanceRegistry;
use crate::mailbox_poller::{CopilotMailboxPoller, EnrolledCopilots};
use crate::mcp_session::McpSessionStore;
use crate::plugin_cache::PluginCache;
use crate::plugin_refresh::auto_update_tick_async;
use crate::queue_manager::QueueManagerRegistry;
use crate::agent_routing::AgentRoutingQueueManagerRegistry;
use crate::memory_promotion::MemoryPromotionJudge;
use crate::reflection_subscriber::ReflectionSubscriber;
use crate::skill_distillation::SkillDistiller;
use crate::schedule_runner::ScheduleRunner;
use crate::channels::discord::DiscordTransport;
use crate::channels::email::EmailTransport;
use crate::channels::slack::SlackTransport;
use crate::telegram::{ChannelBridge, TelegramClient, TelegramTransport};
use crate::task_feeder::TaskFeeder;
use crate::tasklist_service::TasklistService;
use crate::agent_snapshot_sync::{
    hydrate_agent_snapshot_fields, spawn_agent_snapshot_tasklist_sync,
};
use crate::sync_form_reaper::reap_orphaned_sync_forms;
use crate::tasklist_queue_manager::{TasklistQueueDispatcher, TasklistQueueManagerRegistry};
use crate::project_queue_manager::ProjectQueueManagerRegistry;
use crate::workflow_queue_manager::{self, WorkflowQueueHandle};
use crate::workflow_registry::WorkflowRegistry;
use crate::workflow_runner::WorkflowRunner;

/// Root dependency aggregate that wires all services together.
/// The server uses this as a single entry point for all dependencies.
pub struct AppState {
    pub event_bus: Arc<EventBus>,
    pub process_supervisor: Arc<dyn ProcessSupervisor>,
    pub normalizer_registry: Arc<NormalizerRegistry>,
    pub command_queue: Arc<CommandQueue>,
    pub persistence: Arc<PersistenceLayer>,
    pub agent_runner: Arc<CliAgentRunner>,
    /// Unified in-flight run registry keyed by agent_id.
    /// The cancel HTTP route fires tokens here to cancel active runs for
    /// both CLI and native runner paths.
    pub running_agents: Arc<RunningAgents>,
    pub instance_registry: Arc<InstanceRegistry>,
    pub queue_managers: Arc<QueueManagerRegistry>,
    pub project_queue_managers: Arc<ProjectQueueManagerRegistry>,
    pub tasklist_queue_managers: Arc<TasklistQueueManagerRegistry>,
    pub tasklist_queue_dispatcher: Arc<TasklistQueueDispatcher>,
    pub agent_routing_queue: Arc<AgentRoutingQueueManagerRegistry>,
    pub workflow_registry: Arc<RwLock<WorkflowRegistry>>,
    pub workflow_runner: Arc<WorkflowRunner>,
    pub workflow_queue: WorkflowQueueHandle,
    pub task_feeder: Arc<TaskFeeder>,
    /// Unified mutation point for tasklist operations (create, mutate, query).
    /// Used by HTTP route handlers and Todo* tools alike.
    pub tasklist_service: Arc<TasklistService>,
    /// Handle for firing an assignment immediately, shared by the
    /// `AssignmentTrigger` tool's `RunnerContext` wiring and the MCP HTTP
    /// route's per-request context. See `ao_engine_tools_core::AssignmentFireHandle`.
    pub assignment_fire: Arc<dyn ao_engine_tools_core::AssignmentFireHandle + Send + Sync>,
    /// Drop or send `()` to stop the schedule runner loop.
    pub schedule_runner_shutdown: watch::Sender<()>,
    /// Drop or send `()` to stop the channel bridge's reconcile loop and
    /// every live inbound task.
    pub telegram_bridge_shutdown: watch::Sender<()>,
    /// Handle to the running channel bridge. Exposed here (rather than only
    /// the shutdown sender above) so HTTP route handlers — token-delete and
    /// chat-unlink in `routes/telegram.rs` — can call
    /// `ChannelBridge::invalidate_thread`/`invalidate_thread_for_chat`
    /// directly on binding teardown, instead of waiting for the reconcile
    /// loop's next tick to notice.
    pub telegram_bridge: Arc<ChannelBridge>,
    /// `JoinHandle` for the channel bridge's reconcile-loop task — the same
    /// task that, once `telegram_bridge_shutdown` fires, releases every
    /// lease this process holds before it finishes. A
    /// graceful-shutdown signal handler must await this (not just send on
    /// the sender and move on) to actually confirm those releases happened
    /// before the process exits — otherwise a standby process has to wait
    /// out the full lease TTL instead of reclaiming immediately. `Mutex` +
    /// `Option` because a `JoinHandle` is consumed by awaiting it, and this
    /// is only ever taken once, by whichever shutdown path runs first.
    pub telegram_bridge_join_handle: Mutex<Option<JoinHandle<()>>>,
    /// Drop or send `()` to stop the agent-runner sleep guard loop.
    pub agent_sleep_guard_shutdown: watch::Sender<()>,
    /// Drop or send `()` to stop the tasklist dispatch watchdog loop.
    pub dispatch_watchdog_shutdown: watch::Sender<()>,
    /// Drop or send `()` to stop the agent-snapshot tasklist sync loop.
    pub agent_snapshot_sync_shutdown: watch::Sender<()>,
    /// Drop or send `()` to stop the co-pilot mailbox poller.
    pub copilot_mailbox_poller_shutdown: watch::Sender<()>,
    /// In-memory enrolled set of co-pilot agent ids. Maintained by the
    /// mailbox poller; read by the wake-on-deliver path.
    pub copilot_enrolled: Arc<EnrolledCopilots>,
    pub context_cache: Arc<ContextCache>,
    /// Distillation orchestrator, shared with the
    /// reflection subscriber's automatic cluster-detection pass. Exposed here
    /// so HTTP route handlers (the skill review surface's manual "promote one
    /// observation" action) can call [`SkillDistiller::generalize_single`]
    /// directly instead of duplicating the model-invocation seam.
    pub skill_distiller: Arc<SkillDistiller>,
    pub plugin_cache: Arc<PluginCache>,
    /// Process-global tool catalog — IO + engine + Delegate/DelegateOutput/DelegateStop.
    /// Exposed here so callers (e.g. tests) can inspect the populated set
    /// without downcasting through the dispatcher.
    pub tools_registry: Arc<Registry>,
    /// Per-agent live context map for the MCP HTTP route handler.
    pub mcp_sessions: Arc<McpSessionStore>,
    /// Owns the MCP server subprocesses for the process lifetime.
    /// Subprocess cleanup happens via `McpClientHandle` drop when `AppState` is dropped.
    pub mcp_manager: Arc<McpManager>,
    /// Shared runtime anchor registry — one per process, partitioned per scope key.
    /// Passed into both CliAgentRunner and NativeAgentRunner so all selector call
    /// sites share the same floor across turns (runtime-only, no cross-restart persistence).
    pub anchor_registry: Arc<WindowAnchorRegistry>,
    /// Drop or send `()` to stop the periodic classifier reconciler loop.
    pub classifier_reconciler_shutdown: watch::Sender<()>,
    /// Drop or send `()` to stop the task transcript pruner loop.
    pub transcript_pruner_shutdown: watch::Sender<()>,
    /// Cascade helper for agent deletion — scans address books and tasklists.
    pub cascade_service: AgentCascadeService,
    /// Process-wide classifier handle, plumbed into `RunnerContext` so the
    /// Todo* tools can spawn live (per-create) classifications instead of
    /// waiting for the periodic reconciler.
    pub task_classifier_handle: Arc<dyn ao_engine_tools_core::ClassifierHandle + Send + Sync>,
    /// Process-wide classifier dedup registry. Shared between the periodic
    /// reconciler and every event-driven spawn site so concurrent ticks can't
    /// re-spawn a task that an event-driven attempt is already classifying.
    pub classifier_in_flight: Arc<ClassifierInFlight>,
    /// Shared registry of per-agent live form bridges. The HTTP route handler
    /// for `POST /agents/{id}/form-answer` looks up the bridge here to deliver
    /// submitted form answers to waiting `AskUserQuestionWithForm` futures.
    pub form_bridge_registry: Arc<FormBridgeRegistry>,
    /// The same subagent spawner wired into the `Delegate` tool, exposed here
    /// so HTTP route handlers can drive it directly for server-initiated
    /// background agent runs (e.g. artifact regeneration) instead of the
    /// tool-call path. One process-wide instance — construction is identical
    /// either way, so there is exactly one seam for "spawn a background
    /// subagent" in this codebase.
    pub spawner: Arc<SubagentSpawner>,
    /// In-memory status of every `spawn_artifact_agent` run, keyed by
    /// `BackgroundAgentId::to_string()`. Populated by
    /// `crate::artifact_task_status::ArtifactTaskCompletionSink` and read by
    /// the artifact task-status HTTP route. Ephemeral by design, matching
    /// `BackgroundAgentRegistry`'s lifetime -- no disk persistence.
    pub artifact_task_status: Arc<ArtifactTaskStatusStore>,
}

// `has_active_run` and `queue_depth` are no longer persisted: routes overlay
// them at read time from `InstanceRegistry` and `QueueManagerRegistry`. With
// nothing to clean up at boot, the previous startup reconciliation step has
// been removed.

impl AppState {
    /// Create AppState with real DefaultProcessSupervisor for production use.
    pub async fn new() -> Result<Self, AoError> {
        let persistence = Arc::new(PersistenceLayer::init().await?);
        let event_bus = Arc::new(EventBus::new(1024));
        let process_supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(DefaultProcessSupervisor::new());
        let normalizer_registry = Arc::new(NormalizerRegistry::new());
        let command_queue = Arc::new(CommandQueue::new());
        let instance_registry = Arc::new(InstanceRegistry::new());

        let workflows_dir = persistence.data_root.root().join("workflows");
        let workflow_store = WorkflowStore::new(workflows_dir.clone());
        let workflow_registry = Arc::new(RwLock::new(
            WorkflowRegistry::new(workflow_store).await?,
        ));

        let task_dir = persistence.data_root.tasks_dir();
        let task_store = TaskStore::new(&task_dir);
        let workflow_store_for_runner = WorkflowStore::new(workflows_dir);
        let workflow_runner = Arc::new(WorkflowRunner::new(
            Arc::clone(&workflow_registry),
            task_store,
            workflow_store_for_runner,
            Arc::clone(&event_bus),
        ));

        // Create the workflow queue manager before agent_runner so
        // agent_runner can hold a queue handle for routing actions.
        // Spawning is deferred until after queue_managers is created.
        let (workflow_queue, mut wf_manager) = workflow_queue_manager::create_workflow_queue(
            Arc::clone(&workflow_runner),
            Arc::clone(&event_bus),
        );

        // Plumb the queue handle back into the runner so the
        // WorkflowAction* IoTools can notify the queue manager about
        // phase completions and skips — without this, Running tasks
        // never auto-advance after the agent finishes a phase.
        workflow_runner.set_workflow_queue(workflow_queue.clone()).await;

        let context_cache = Arc::new(ContextCache::new());
        let plugin_cache = Arc::new(PluginCache::new_empty());

        // Populate the plugin cache at startup so the first message turn
        // doesn't pay the disk walk. Non-fatal on failure (agents without
        // plugins are unaffected).
        if let Err(err) = plugin_cache.refresh().await {
            tracing::warn!("Initial plugin cache refresh failed: {err}");
        }

        // Shared in-flight run registry — both runners register here on entry.
        let running_agents = Arc::new(RunningAgents::new());

        // Shared anchor registry — runtime-only, partitioned per scope key.
        let anchor_registry = Arc::new(WindowAnchorRegistry::new());

        // Build the shared tool registry before either runner so both CliAgentRunner
        // (catalog injection into CLI system prompts) and NativeAgentRunner (API tool
        // dispatch) share the same Arc<Registry>.
        //
        // Construction order:
        // persister → child runner → spawner → registry → registry-population → runners.
        let sidechain_persister = FileSidechainPersister::new(
            ao_protocol::data_root::resolve_data_root_or_cwd(),
        );
        let subagent_registry = Arc::new(SubagentRegistry::new());
        // Create mcp_sessions here so NativeChildRunner can register child sessions.
        let mcp_sessions = Arc::new(McpSessionStore::new());
        // Shared with `native_runner` below so the main loop and the
        // in-process subagent path resolve provider/model through the exact
        // same `ProviderFactory` instance — one code path, not two.
        let provider_factory: Arc<dyn ProviderFactory> = Arc::new(DefaultProviderFactory);
        let profile_runner = Arc::new(ProfileAwareChildRunner::new(
            Some(Arc::clone(&mcp_sessions)),
            Arc::clone(&provider_factory),
        ));
        let child_runner = Arc::clone(&profile_runner)
            as Arc<dyn ao_engine_tools_core::background_agents::ChildRunner>;
        let spawner = Arc::new(
            SubagentSpawner::new(subagent_registry)
                .with_child_runner(child_runner)
                .with_sidechain_persister(sidechain_persister),
        );
        let artifact_task_status = Arc::new(ArtifactTaskStatusStore::new());

        let mut registry = Registry::new();
        register_io_tools(&mut registry);
        register_engine_tools(&mut registry);
        // DelegateOutput and DelegateStop poll/cancel async delegations. They
        // need no constructor injection — they interact with per-session state
        // through the RunnerContext at invocation time, not at registration
        // time. The spawner is wired into Delegate (below), which spawns the
        // async delegations these two tools then observe.
        registry.register_engine(Arc::new(DelegateOutput));
        registry.register_engine(Arc::new(DelegateStop));

        // Delegate requires runtime deps (spawner + agent profile store) that
        // are not available inside `register_all`. `register_all` installs a
        // stub Delegate so the catalog/deferred index includes the name; we
        // overwrite it here with a fully wired instance. Without this, a
        // Delegate call reaches the dispatcher and fails with
        // "Delegate requires a spawner and agent store (none configured in
        // this context)".
        let delegate_profile_store =
            Arc::new(AgentProfileStore::new(persistence.data_root.clone()));
        registry.register_io(Arc::new(Delegate::with_spawner_and_store(
            spawner.clone(),
            Arc::clone(&delegate_profile_store),
        )));

        // AgentAuthor needs the same runtime deps Delegate does (an agent
        // profile store), plus the snapshot store and a cache invalidator so
        // a self-edit (persona/special_instructions) takes effect on the
        // agent's next turn. `register_all` installs a stub that errors on
        // every op; overwrite it here with the fully wired instance, reusing
        // the profile store Arc already built for Delegate above.
        registry.register_engine(Arc::new(AgentAuthor::with_deps(
            Arc::clone(&delegate_profile_store),
            Arc::clone(&persistence.snapshots),
            Arc::clone(&context_cache) as Arc<dyn AgentProfileCacheInvalidator>,
        )));

        // SendEmail needs the same profile store plus a secret store for the
        // binding's SMTP password. `register_all` installs a stub that errors
        // clearly on every call; overwrite it here, reusing the profile store
        // Arc already built for Delegate above. If the secret store can't be
        // opened (data root unresolvable), leave the stub in place rather than
        // blocking AppState construction — SendEmail then errors clearly at
        // call time instead of silently doing nothing.
        match ChannelSecretStore::open() {
            Ok(store) => {
                registry.register_engine(Arc::new(SendEmail::with_deps(
                    Arc::clone(&delegate_profile_store),
                    Arc::new(store),
                )));
            }
            Err(e) => {
                tracing::warn!("failed to open channel secret store: {e}; SendEmail will error until this is resolved");
            }
        }

        // Load MCP servers and register their tools. Failures are isolated: a
        // bad server logs a warn but does not prevent AppState construction.
        let mcp_config = match McpServersConfig::load() {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("failed to load mcp_servers.toml: {e}; proceeding with no MCP servers");
                McpServersConfig { servers: vec![] }
            }
        };
        // Auth-aware construction: servers that answer the handshake with an
        // OAuth challenge (HTTP 401) are recorded as needs-auth and surface an
        // auth pseudo-tool via `register_into`, instead of being silently
        // dropped. If the token store cannot be opened, fall back to the
        // non-auth constructor so MCP startup never blocks AppState creation.
        let mcp_manager = match McpTokenStore::open() {
            Ok(token_store) => {
                McpManager::from_config_auth(&mcp_config, Arc::new(token_store)).await
            }
            Err(e) => {
                tracing::warn!(
                    "failed to open MCP token store: {e}; connecting MCP servers without auth support"
                );
                McpManager::from_config(&mcp_config).await
            }
        };
        let mcp_manager = Arc::new(mcp_manager.register_into(&mut registry).await);
        // Install the weak self-reference now that the manager is Arc-wrapped, so
        // auth pseudo-tools registered above can promote a server to Connected
        // when the agent-driven OAuth flow completes (keeps the UI badge in sync).
        mcp_manager.attach_self_reference();

        // Rebuild the deferred index so ToolSearch sees the full catalog
        // (including any MCP tools registered above).
        registry.build_deferred_index();

        let tool_names: Vec<String> = registry.list();
        tracing::debug!(
            count = tool_names.len(),
            tools = %tool_names.join(", "),
            "tools registry initialized"
        );

        let tools_registry = Arc::new(registry);

        // Append plugin-bundled MCP servers. Failures are isolated: a bad
        // plugin server logs a warn but does not block startup.
        for (plugin_name, entry) in crate::plugin_mcp::collect_all_plugin_mcp_entries() {
            let source = format!("plugin:{plugin_name}");
            if let Err(e) = mcp_manager
                .add_server(entry, Arc::clone(&tools_registry), source)
                .await
            {
                tracing::warn!("plugin {plugin_name}: failed to connect MCP server: {e}");
            }
        }

        // Classifier is constructed here (before the runners) so the same Arc
        // can be: (a) plumbed into NativeAgentRunner so it lands on every
        // RunnerContext built for native runs, (b) exposed on AppState so the
        // MCP route handler can do the same for CLI runs, and (c) handed to
        // the boot-sweep + cascade helpers further below. Without this
        // wiring, `ctx.classifier == None` everywhere in production and
        // `TodoCreate`'s live-classify spawn loop is unreachable, leaving
        // newly created agent-owned tasks stuck until the next boot sweep.
        let task_classifier = TaskClassifier::new(
            Arc::clone(&persistence),
            Arc::clone(&process_supervisor),
            Arc::clone(&normalizer_registry),
        );
        let task_classifier_handle: Arc<dyn ao_engine_tools_core::ClassifierHandle + Send + Sync> =
            Arc::new(task_classifier.clone());

        // Process-wide dedup registry for classifier spawns. Created here so
        // it can be: (a) plumbed into NativeAgentRunner → every RunnerContext
        // → the Todo* tool spawn sites, (b) exposed on AppState so the HTTP
        // routes (chat-input append_task / create_tasklist) and the periodic
        // reconciler share the same set. Without this dedup, a reconciler
        // tick fired while a tool's spawn is still retrying would spawn a
        // duplicate against the same task.
        let classifier_in_flight = Arc::new(ClassifierInFlight::new());

        // Reflection pass subscriber — binds the
        // trigger seam (anchor rotation / idle timeout / explicit archive,
        // wired in `history::select` and `ThreadStore::archive`) to the real
        // OBSERVE producer instead of the no-op default it defaults to. The
        // provider resolver defers to `build_reflection_provider` — the same
        // profile→client seam `build_quick_verification_engine` /
        // `build_thread_summarization_engine` already use — so this
        // subscriber never constructs a provider client of its own.
        //
        // Distillation shares that exact same seam — same
        // `build_reflection_provider` function, same persistence layer —
        // and is chained onto every trigger via `with_distiller` so a
        // repeated procedure gets a chance to generalize into a staged skill
        // immediately after the reflection pass that surfaced it, off the
        // user's turn.
        let skill_distiller = Arc::new(SkillDistiller::new(
            Arc::clone(&persistence),
            Arc::new(crate::build_reflection_provider),
        ));
        // The skill review HTTP surface's manual "promote one observation"
        // action needs its own handle on the distiller, independent of the
        // one the reflection subscriber consumes below.
        let skill_distiller_for_state = Arc::clone(&skill_distiller);
        // Promotion judge — shares the exact same
        // `build_reflection_provider` seam and persistence layer as the
        // reflection pass and distillation above (one execution-engine
        // seam, not a second). Chained via `with_promotion_judge` so it only
        // ever fires on `ReflectionTriggerReason::Archived`, never on anchor
        // rotation or idle timeout — see that builder's doc.
        let memory_promotion_judge = Arc::new(MemoryPromotionJudge::new(
            Arc::clone(&persistence),
            Arc::new(crate::build_reflection_provider),
        ));
        let reflection_subscriber = Arc::new(
            ReflectionSubscriber::new(Arc::clone(&persistence), Arc::new(crate::build_reflection_provider))
                .with_distiller(skill_distiller)
                .with_promotion_judge(memory_promotion_judge),
        );
        persistence
            .threads
            .set_reflection_subscriber(Arc::clone(&reflection_subscriber) as _);

        // mcp_sessions was created earlier (before child_runner) so NativeChildRunner
        // can register child sessions. Reuse the same Arc here.
        let agent_runner = Arc::new(
            CliAgentRunner::new(
                Arc::clone(&process_supervisor),
                Arc::clone(&normalizer_registry),
                Arc::clone(&event_bus),
                Arc::clone(&persistence),
                Arc::clone(&command_queue),
                Arc::clone(&instance_registry),
                Arc::clone(&running_agents),
                Arc::clone(&tools_registry),
            )
            .with_workflow_runner(Arc::clone(&workflow_runner))
            .with_workflow_registry(Arc::clone(&workflow_registry))
            .with_workflow_queue(workflow_queue.clone())
            .with_context_cache(Arc::clone(&context_cache))
            .with_plugin_cache(Arc::clone(&plugin_cache))
            .with_anchor_registry(Arc::clone(&anchor_registry))
            .with_mcp_sessions(Arc::clone(&mcp_sessions))
            .with_reflection_subscriber(Arc::clone(&reflection_subscriber) as _),
        );

        // Native (in-process API) runner — always constructed. Routed to by
        // `RunnerDispatcher::pick` for any agent with `runner_mode: Api`;
        // there is no other gate.
        let native_runner = Arc::new(
            NativeAgentRunner::new(
                Arc::clone(&event_bus),
                Arc::clone(&instance_registry),
                Arc::clone(&running_agents),
                Arc::clone(&provider_factory),
                Arc::clone(&tools_registry),
                Arc::clone(&persistence),
            )
            .with_workflow_runner(
                Arc::clone(&workflow_runner) as Arc<dyn ao_engine_tools_core::WorkflowRunnerHandle + Send + Sync>,
            )
            .with_anchor_registry(Arc::clone(&anchor_registry))
            .with_mcp_sessions(Arc::clone(&mcp_sessions))
            .with_mcp_manager(Arc::clone(&mcp_manager))
            .with_classifier(Arc::clone(&task_classifier_handle))
            .with_classifier_in_flight(Arc::clone(&classifier_in_flight))
            .with_reflection_subscriber(Arc::clone(&reflection_subscriber) as _),
        );
        let native_runner_ref = Arc::clone(&native_runner);
        let form_bridge_registry = Arc::clone(&native_runner.form_bridge_registry);

        let dispatcher = Arc::new(RunnerDispatcher::new(
            Arc::clone(&agent_runner),
            native_runner,
        ));

        // Late-bind the dispatcher into ProfileAwareChildRunner so named-profile
        // delegates can be routed through the appropriate runner (CLI or API).
        profile_runner.set_dispatcher(Arc::clone(&dispatcher));

        let queue_managers = Arc::new(QueueManagerRegistry::new(
            Arc::clone(&dispatcher),
            Arc::clone(&instance_registry),
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
        ));
        // Late-bind the registry into agent_runner so the parse-success path
        // for `<task-item-notification>` can dispatch to a task's `remind_me`
        // agent's mailbox via the same pipeline as user-typed messages. Cast
        // to `Arc<dyn NotificationDispatcher>` to break the Send-inference
        // cycle (see [`AgentRunner::set_notification_dispatcher`]).
        agent_runner.set_notification_dispatcher(
            Arc::clone(&queue_managers) as Arc<dyn crate::queue_manager::NotificationDispatcher>,
        );

        // Wire in the queue manager registry and transcript store so the workflow
        // queue manager can clean up synthetic phase agents and auto cold-start phases.
        wf_manager.set_queue_manager_registry(Arc::clone(&queue_managers));
        wf_manager.set_transcript_store(
            ao_persistence::transcript::TranscriptStore::new(persistence.data_root.clone()),
        );
        wf_manager.set_persistence(Arc::clone(&persistence));
        tokio::spawn(wf_manager.run());

        let project_queue_managers = Arc::new(ProjectQueueManagerRegistry::new(
            Arc::clone(&agent_runner),
            Arc::clone(&instance_registry),
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
        ));

        // Per-tasklist queue managers + their TaskDispatcher implementation.
        // The dispatcher is wired straight into TaskFeeder below so every
        // tasklist task flows through TasklistQueueManager (with
        // RunScope::Tasklist + tasklist transcript writes + team-channel
        // system bubbles), bypassing the personal AgentQueueManager entirely.
        let tasklist_queue_managers = Arc::new(TasklistQueueManagerRegistry::new(
            Arc::clone(&agent_runner),
            Arc::clone(&persistence),
            Arc::clone(&event_bus),
        ));
        let tasklist_queue_dispatcher = Arc::new(TasklistQueueDispatcher::new(
            Arc::clone(&tasklist_queue_managers),
            Arc::clone(&persistence),
        ));

        // TaskFeeder is wired with TasklistQueueDispatcher (the per-tasklist
        // path) and with persistence/tasklist-store for lookups. It's wired
        // into agent_runner post-hoc via set_task_feeder so agent_runner can
        // notify it when an agent emits `<task action="complete|fail">`.
        let task_feeder = Arc::new(
            TaskFeeder::new(
                Arc::new(ao_persistence::tasklist_store::TasklistStore::new(
                    persistence.data_root.clone(),
                )),
                Arc::clone(&tasklist_queue_dispatcher) as Arc<dyn crate::task_feeder::TaskDispatcher>,
            )
            .with_event_bus(Arc::clone(&event_bus))
            .with_instance_registry(Arc::clone(&instance_registry)),
        );
        agent_runner.set_task_feeder(Arc::clone(&task_feeder));
        tasklist_queue_managers.set_task_feeder(Arc::clone(&task_feeder));

        task_feeder.set_project_dispatcher(
            Arc::clone(&project_queue_managers) as Arc<dyn crate::task_feeder::ProjectDispatcher>,
        );
        task_feeder.set_thread_store(Arc::clone(&persistence.threads));

        // Agent routing queue: per-agent delegate classifier.
        // Handles unowned tasks in agent-owned tasklists using the owning
        // agent's delegates_to address book.
        let agent_routing_queue = Arc::new(AgentRoutingQueueManagerRegistry::new(
            Arc::clone(&persistence),
            Arc::clone(&process_supervisor),
            Arc::clone(&normalizer_registry),
            Arc::clone(&task_feeder),
        ));
        task_feeder.set_agent_routing_queue(
            Arc::clone(&agent_routing_queue) as Arc<dyn crate::agent_routing::AgentRoutingChannel>,
        );
        task_feeder.set_notification_dispatcher(
            Arc::clone(&queue_managers) as Arc<dyn crate::queue_manager::NotificationDispatcher>,
        );

        let tasklist_service = Arc::new(
            TasklistService::new(
                Arc::clone(&persistence),
                Arc::clone(&task_feeder),
                Arc::clone(&event_bus),
            )
            .with_instance_registry(Arc::clone(&instance_registry))
            .with_tasklist_queue_managers(Arc::clone(&tasklist_queue_managers)),
        );
        native_runner_ref.set_tasklist_service(
            Arc::clone(&tasklist_service) as Arc<dyn ao_engine_tools_core::TasklistServiceHandle + Send + Sync>,
        );

        // `assignment_fire` needs `queue_managers` (the `NotificationDispatcher`
        // that `fire_assignment` enqueues through), which doesn't exist until
        // after `native_runner` is built — same late-bind reason as
        // `tasklist_service` above.
        let assignment_fire: Arc<dyn ao_engine_tools_core::AssignmentFireHandle + Send + Sync> =
            Arc::new(crate::assignment_runner::ManualAssignmentFirer::new(
                Arc::clone(&persistence),
                Arc::clone(&queue_managers) as Arc<dyn crate::queue_manager::NotificationDispatcher>,
                Arc::clone(&event_bus),
            ));
        native_runner_ref.set_assignment_fire(Arc::clone(&assignment_fire));

        let dispatch_watchdog_shutdown =
            DispatchWatchdogRunner::new(Arc::clone(&task_feeder)).run();

        // Periodic classifier reconciler: every 30s, finds any agent-owned
        // task whose assignment is None and (re-)spawns `classify_with_retry`
        // through the shared in-flight dedup. Replaces the old 6-hour boot
        // sweep. First tick fires immediately at startup, so a fresh process
        // catches up on anything left orphaned by a crash mid-classification
        // or by a previous-tick retry budget exhausted.
        let svc_handle = Arc::clone(&tasklist_service)
            as Arc<dyn ao_engine_tools_core::TasklistServiceHandle + Send + Sync>;
        let classifier_reconciler_shutdown = ClassifierReconciler::new(
            Arc::clone(&task_classifier_handle),
            Arc::clone(&persistence),
            Arc::clone(&svc_handle),
            Arc::clone(&classifier_in_flight),
        )
        .run();
        let cascade_service = AgentCascadeService::new(
            Arc::clone(&persistence),
            Arc::clone(&task_feeder),
            task_classifier,
            Arc::clone(&svc_handle),
        );

        let transcript_pruner_shutdown = TranscriptPrunerRunner::new(
            TaskTranscriptPruner::new(),
            persistence.data_root.clone(),
            PrunerConfig::default(),
        )
        .run();

        let agent_snapshot_sync_shutdown = spawn_agent_snapshot_tasklist_sync(
            Arc::clone(&persistence),
            Arc::clone(&event_bus),
        );

        // Co-pilot mailbox poller: tracks which co-pilot agents are currently
        // "enrolled" (i.e. their tasklist is active or recently opened). The
        // set is rebuilt from disk on startup, kept in sync via the existing
        // `TasklistWoke` / `TasklistSlept` events, and swept periodically to
        // evict tasklists that have transitioned to sleep-eligible without an
        // external emitter.
        let copilot_poller =
            CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&event_bus));
        let copilot_enrolled = copilot_poller.enrolled();
        let copilot_mailbox_poller_shutdown = copilot_poller.run();
        // Share the enrolled set with the personal queue manager so
        // an inbound QueuedMessage (e.g. a `<task-item-notification>`)
        // addressed to a dormant co-pilot triggers wake-on-deliver enrollment
        // before dispatch.
        queue_managers.set_enrolled_copilots(Arc::clone(&copilot_enrolled));
        {
            let persistence = Arc::clone(&persistence);
            tokio::spawn(async move {
                hydrate_agent_snapshot_fields(persistence).await;
            });
        }
        // Reap `mode: "sync"` pending forms left over from before this
        // restart — a synchronous AskUserQuestionWithForm suspension cannot
        // survive a process restart (see `sync_form_reaper`'s module docs),
        // so any such form whose scope has no live run in this fresh
        // process is dead on arrival and must be marked orphaned rather
        // than shown to the user as still-answerable. Best-effort per-form,
        // same as the two hydration sweeps above: `sync_form_reaper` itself
        // logs real per-form context (agent id + form id) at `error` on
        // failure instead of swallowing it.
        {
            let persistence = Arc::clone(&persistence);
            let instance_registry = Arc::clone(&instance_registry);
            tokio::spawn(async move {
                reap_orphaned_sync_forms(persistence, instance_registry).await;
            });
        }

        // Startup scan: walk every Active tasklist (team + agent-owned) and
        // (a) recover any InProgress tasks whose runner is no longer alive
        //     (zombies from a previous server run), then
        // (b) poke `advance` so pending tasks are dispatched.
        // Best-effort: failures are logged per-tasklist and don't block startup.
        {
            let task_feeder = Arc::clone(&task_feeder);
            tokio::spawn(async move {
                match task_feeder.reconcile_zombies_on_start().await {
                    Ok(n) if n > 0 => tracing::info!(recovered = n, "Startup zombie reconcile"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("Startup zombie reconcile failed: {}", e),
                }
                if let Err(e) = task_feeder.advance_all_active().await {
                    tracing::warn!("Startup advance scan failed: {}", e);
                }
            });
        }

        let schedule_runner = ScheduleRunner::new(
            Arc::clone(&persistence),
            Arc::clone(&queue_managers),
            Arc::clone(&event_bus),
            Arc::clone(&mcp_manager),
            Arc::clone(&tools_registry),
            Arc::clone(&dispatcher),
        );
        let schedule_runner_shutdown = schedule_runner.run();

        let telegram_transport = Arc::new(TelegramTransport::new(Arc::new(TelegramClient::new())));
        let discord_transport = Arc::new(DiscordTransport::new());
        let email_transport = Arc::new(EmailTransport::new());
        let slack_transport = Arc::new(SlackTransport::new());
        let telegram_bridge = Arc::new(ChannelBridge::new(
            Arc::clone(&persistence),
            Arc::clone(&queue_managers),
            Arc::clone(&event_bus),
            telegram_transport,
            discord_transport,
            email_transport,
            slack_transport,
        ));
        let (telegram_bridge_shutdown, telegram_bridge_join_handle) = Arc::clone(&telegram_bridge).run();

        let agent_sleep_guard_shutdown = AgentSleepGuardRunner::new(
            Arc::clone(&persistence),
            Arc::clone(&instance_registry),
        )
        .run();

        // Fire the one-shot auto-update tick on startup so stale plugins
        // refresh in the background without blocking server start. On any
        // success the plugin cache is rebuilt so the first message turn
        // sees the new content.
        {
            let plugin_cache = Arc::clone(&plugin_cache);
            tokio::spawn(async move {
                match auto_update_tick_async().await {
                    Ok(outcome) => {
                        if outcome.succeeded > 0 {
                            tracing::info!(
                                attempted = outcome.attempted,
                                succeeded = outcome.succeeded,
                                failed = ?outcome.failed,
                                "startup plugin auto-update tick finished",
                            );
                            if let Err(err) = plugin_cache.refresh().await {
                                tracing::warn!(
                                    "plugin cache refresh after auto-update failed: {err}"
                                );
                            }
                        } else if outcome.attempted > 0 {
                            tracing::warn!(
                                attempted = outcome.attempted,
                                failed = ?outcome.failed,
                                "startup plugin auto-update tick: all refreshes failed",
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!("startup plugin auto-update tick failed: {err}");
                    }
                }
            });
        }

        Ok(Self {
            event_bus,
            process_supervisor,
            normalizer_registry,
            command_queue,
            persistence,
            agent_runner,
            running_agents,
            instance_registry,
            queue_managers,
            project_queue_managers,
            tasklist_queue_managers,
            tasklist_queue_dispatcher,
            agent_routing_queue,
            workflow_registry,
            workflow_runner,
            workflow_queue,
            task_feeder,
            tasklist_service,
            assignment_fire,
            schedule_runner_shutdown,
            telegram_bridge_shutdown,
            telegram_bridge,
            telegram_bridge_join_handle: Mutex::new(Some(telegram_bridge_join_handle)),
            agent_sleep_guard_shutdown,
            dispatch_watchdog_shutdown,
            agent_snapshot_sync_shutdown,
            copilot_mailbox_poller_shutdown,
            copilot_enrolled,
            context_cache,
            skill_distiller: skill_distiller_for_state,
            plugin_cache,
            tools_registry,
            mcp_sessions,
            mcp_manager: Arc::clone(&mcp_manager),
            anchor_registry,
            classifier_reconciler_shutdown,
            transcript_pruner_shutdown,
            cascade_service,
            task_classifier_handle,
            classifier_in_flight,
            form_bridge_registry,
            spawner,
            artifact_task_status,
        })
    }

    /// Create AppState with a MockProcessSupervisor for testing.
    pub async fn new_with_mock(mock: MockProcessSupervisor) -> Result<Self, AoError> {
        let persistence = Arc::new(PersistenceLayer::init().await?);
        let event_bus = Arc::new(EventBus::new(1024));
        let process_supervisor: Arc<dyn ProcessSupervisor> = Arc::new(mock);
        let normalizer_registry = Arc::new(NormalizerRegistry::new());
        let command_queue = Arc::new(CommandQueue::new());
        let instance_registry = Arc::new(InstanceRegistry::new());

        let workflows_dir = persistence.data_root.root().join("workflows");
        let workflow_store = WorkflowStore::new(workflows_dir.clone());
        let workflow_registry = Arc::new(RwLock::new(
            WorkflowRegistry::new(workflow_store).await?,
        ));

        let task_dir = persistence.data_root.tasks_dir();
        let task_store = TaskStore::new(&task_dir);
        let workflow_store_for_runner = WorkflowStore::new(workflows_dir);
        let workflow_runner = Arc::new(WorkflowRunner::new(
            Arc::clone(&workflow_registry),
            task_store,
            workflow_store_for_runner,
            Arc::clone(&event_bus),
        ));

        // Create the workflow queue manager before agent_runner so
        // agent_runner can hold a queue handle for routing actions.
        let (workflow_queue, mut wf_manager) = workflow_queue_manager::create_workflow_queue(
            Arc::clone(&workflow_runner),
            Arc::clone(&event_bus),
        );

        // Plumb the queue handle back into the runner so the
        // WorkflowAction* IoTools can notify the queue manager about
        // phase completions and skips.
        workflow_runner.set_workflow_queue(workflow_queue.clone()).await;

        let context_cache = Arc::new(ContextCache::new());
        let plugin_cache = Arc::new(PluginCache::new_empty());

        if let Err(err) = plugin_cache.refresh().await {
            tracing::warn!("Initial plugin cache refresh failed (mock): {err}");
        }

        let running_agents = Arc::new(RunningAgents::new());

        // Shared anchor registry for the mock path (same shape as production).
        let anchor_registry = Arc::new(WindowAnchorRegistry::new());

        let sidechain_persister_mock = FileSidechainPersister::new(
            ao_protocol::data_root::resolve_data_root_or_cwd(),
        );
        let subagent_registry_mock = Arc::new(SubagentRegistry::new());
        let mcp_sessions = Arc::new(McpSessionStore::new());
        // Shared with `native_runner` below — see the matching comment in `new()`.
        let provider_factory_mock: Arc<dyn ProviderFactory> = Arc::new(DefaultProviderFactory);
        let profile_runner_mock = Arc::new(ProfileAwareChildRunner::new(
            Some(Arc::clone(&mcp_sessions)),
            Arc::clone(&provider_factory_mock),
        ));
        let child_runner_mock = Arc::clone(&profile_runner_mock)
            as Arc<dyn ao_engine_tools_core::background_agents::ChildRunner>;
        let spawner_mock = Arc::new(
            SubagentSpawner::new(subagent_registry_mock)
                .with_child_runner(child_runner_mock)
                .with_sidechain_persister(sidechain_persister_mock),
        );
        let artifact_task_status_mock = Arc::new(ArtifactTaskStatusStore::new());

        let mut registry_mock = Registry::new();
        register_io_tools(&mut registry_mock);
        register_engine_tools(&mut registry_mock);
        registry_mock.register_engine(Arc::new(DelegateOutput));
        registry_mock.register_engine(Arc::new(DelegateStop));

        // Delegate runtime wiring — mirrors the production path. See the
        // analogous block in `AppState::new` for the full rationale.
        let delegate_profile_store_mock =
            Arc::new(AgentProfileStore::new(persistence.data_root.clone()));
        registry_mock.register_io(Arc::new(Delegate::with_spawner_and_store(
            spawner_mock.clone(),
            Arc::clone(&delegate_profile_store_mock),
        )));

        // AgentAuthor runtime wiring — mirrors the production path. See the
        // analogous block in `AppState::new` for the full rationale.
        registry_mock.register_engine(Arc::new(AgentAuthor::with_deps(
            Arc::clone(&delegate_profile_store_mock),
            Arc::clone(&persistence.snapshots),
            Arc::clone(&context_cache) as Arc<dyn AgentProfileCacheInvalidator>,
        )));

        // SendEmail runtime wiring — mirrors the production path. See the
        // analogous block in `AppState::new` for the full rationale.
        match ChannelSecretStore::open() {
            Ok(store) => {
                registry_mock.register_engine(Arc::new(SendEmail::with_deps(
                    Arc::clone(&delegate_profile_store_mock),
                    Arc::new(store),
                )));
            }
            Err(e) => {
                tracing::warn!("failed to open channel secret store (mock): {e}; SendEmail will error until this is resolved");
            }
        }

        // Load MCP servers (missing file returns empty config — safe in tests).
        let mcp_config_mock = match McpServersConfig::load() {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("failed to load mcp_servers.toml (mock): {e}; proceeding with no MCP servers");
                McpServersConfig { servers: vec![] }
            }
        };
        // Mirror the live path's auth-aware construction (see `AppState::new`)
        // so mock-backed sessions exercise the same needs-auth surface.
        let mcp_manager_mock = match McpTokenStore::open() {
            Ok(token_store) => {
                McpManager::from_config_auth(&mcp_config_mock, Arc::new(token_store)).await
            }
            Err(e) => {
                tracing::warn!(
                    "failed to open MCP token store (mock): {e}; connecting MCP servers without auth support"
                );
                McpManager::from_config(&mcp_config_mock).await
            }
        };
        let mcp_manager_mock = Arc::new(mcp_manager_mock.register_into(&mut registry_mock).await);
        mcp_manager_mock.attach_self_reference();

        registry_mock.build_deferred_index();

        let tools_registry = Arc::new(registry_mock);

        for (plugin_name, entry) in crate::plugin_mcp::collect_all_plugin_mcp_entries() {
            let source = format!("plugin:{plugin_name}");
            if let Err(e) = mcp_manager_mock
                .add_server(entry, Arc::clone(&tools_registry), source)
                .await
            {
                tracing::warn!("plugin {plugin_name} (mock): failed to connect MCP server: {e}");
            }
        }

        // Classifier — same construction order as the production path so the
        // mock-backed test harness exercises the same wiring (live trigger +
        // boot sweep share one instance).
        let task_classifier_mock = TaskClassifier::new(
            Arc::clone(&persistence),
            Arc::clone(&process_supervisor),
            Arc::clone(&normalizer_registry),
        );
        let task_classifier_handle: Arc<dyn ao_engine_tools_core::ClassifierHandle + Send + Sync> =
            Arc::new(task_classifier_mock.clone());

        // Shared in-flight dedup — mirrors the production layout so the
        // mock-backed harness exercises the same wiring.
        let classifier_in_flight = Arc::new(ClassifierInFlight::new());

        // Reflection pass subscriber — see the
        // production `new()` constructor for the full rationale; mirrored
        // here so mock-backed tests exercise the same wiring, including
        // distillation chained via `with_distiller`.
        let skill_distiller_mock = Arc::new(SkillDistiller::new(
            Arc::clone(&persistence),
            Arc::new(crate::build_reflection_provider),
        ));
        let skill_distiller_for_state_mock = Arc::clone(&skill_distiller_mock);
        let memory_promotion_judge_mock = Arc::new(MemoryPromotionJudge::new(
            Arc::clone(&persistence),
            Arc::new(crate::build_reflection_provider),
        ));
        let reflection_subscriber = Arc::new(
            ReflectionSubscriber::new(Arc::clone(&persistence), Arc::new(crate::build_reflection_provider))
                .with_distiller(skill_distiller_mock)
                .with_promotion_judge(memory_promotion_judge_mock),
        );
        persistence
            .threads
            .set_reflection_subscriber(Arc::clone(&reflection_subscriber) as _);

        // mcp_sessions was created earlier (before child_runner_mock) so
        // NativeChildRunner can register child sessions. Reuse the same Arc.
        let agent_runner = Arc::new(
            CliAgentRunner::new(
                Arc::clone(&process_supervisor),
                Arc::clone(&normalizer_registry),
                Arc::clone(&event_bus),
                Arc::clone(&persistence),
                Arc::clone(&command_queue),
                Arc::clone(&instance_registry),
                Arc::clone(&running_agents),
                Arc::clone(&tools_registry),
            )
            .with_workflow_runner(Arc::clone(&workflow_runner))
            .with_workflow_registry(Arc::clone(&workflow_registry))
            .with_workflow_queue(workflow_queue.clone())
            .with_context_cache(Arc::clone(&context_cache))
            .with_plugin_cache(Arc::clone(&plugin_cache))
            .with_anchor_registry(Arc::clone(&anchor_registry))
            .with_mcp_sessions(Arc::clone(&mcp_sessions))
            .with_reflection_subscriber(Arc::clone(&reflection_subscriber) as _),
        );

        let native_runner = Arc::new(
            NativeAgentRunner::new(
                Arc::clone(&event_bus),
                Arc::clone(&instance_registry),
                Arc::clone(&running_agents),
                Arc::clone(&provider_factory_mock),
                Arc::clone(&tools_registry),
                Arc::clone(&persistence),
            )
            .with_workflow_runner(
                Arc::clone(&workflow_runner) as Arc<dyn ao_engine_tools_core::WorkflowRunnerHandle + Send + Sync>,
            )
            .with_anchor_registry(Arc::clone(&anchor_registry))
            .with_mcp_sessions(Arc::clone(&mcp_sessions))
            .with_mcp_manager(Arc::clone(&mcp_manager_mock))
            .with_classifier(Arc::clone(&task_classifier_handle))
            .with_classifier_in_flight(Arc::clone(&classifier_in_flight))
            .with_reflection_subscriber(Arc::clone(&reflection_subscriber) as _),
        );
        let native_runner_ref = Arc::clone(&native_runner);
        let form_bridge_registry_mock = Arc::clone(&native_runner.form_bridge_registry);

        let dispatcher = Arc::new(RunnerDispatcher::new(
            Arc::clone(&agent_runner),
            native_runner,
        ));

        // Late-bind the dispatcher into ProfileAwareChildRunner (mock path mirrors production).
        profile_runner_mock.set_dispatcher(Arc::clone(&dispatcher));

        let queue_managers = Arc::new(QueueManagerRegistry::new(
            Arc::clone(&dispatcher),
            Arc::clone(&instance_registry),
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
        ));
        agent_runner.set_notification_dispatcher(
            Arc::clone(&queue_managers) as Arc<dyn crate::queue_manager::NotificationDispatcher>,
        );

        wf_manager.set_queue_manager_registry(Arc::clone(&queue_managers));
        wf_manager.set_transcript_store(
            ao_persistence::transcript::TranscriptStore::new(persistence.data_root.clone()),
        );
        wf_manager.set_persistence(Arc::clone(&persistence));
        tokio::spawn(wf_manager.run());

        let project_queue_managers = Arc::new(ProjectQueueManagerRegistry::new(
            Arc::clone(&agent_runner),
            Arc::clone(&instance_registry),
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
        ));

        // Per-tasklist queue managers + their TaskDispatcher implementation
        // (mirror of the production wiring above). Stored on AppState so the
        // mock-backed test harness can submit/observe through the same path
        // production uses.
        let tasklist_queue_managers = Arc::new(TasklistQueueManagerRegistry::new(
            Arc::clone(&agent_runner),
            Arc::clone(&persistence),
            Arc::clone(&event_bus),
        ));
        let tasklist_queue_dispatcher = Arc::new(TasklistQueueDispatcher::new(
            Arc::clone(&tasklist_queue_managers),
            Arc::clone(&persistence),
        ));

        let task_feeder = Arc::new(
            TaskFeeder::new(
                Arc::new(ao_persistence::tasklist_store::TasklistStore::new(
                    persistence.data_root.clone(),
                )),
                Arc::clone(&tasklist_queue_dispatcher) as Arc<dyn crate::task_feeder::TaskDispatcher>,
            )
            .with_event_bus(Arc::clone(&event_bus))
            .with_instance_registry(Arc::clone(&instance_registry)),
        );
        agent_runner.set_task_feeder(Arc::clone(&task_feeder));
        tasklist_queue_managers.set_task_feeder(Arc::clone(&task_feeder));

        task_feeder.set_project_dispatcher(
            Arc::clone(&project_queue_managers) as Arc<dyn crate::task_feeder::ProjectDispatcher>,
        );
        task_feeder.set_thread_store(Arc::clone(&persistence.threads));

        let agent_routing_queue = Arc::new(AgentRoutingQueueManagerRegistry::new(
            Arc::clone(&persistence),
            Arc::clone(&process_supervisor),
            Arc::clone(&normalizer_registry),
            Arc::clone(&task_feeder),
        ));
        task_feeder.set_agent_routing_queue(
            Arc::clone(&agent_routing_queue) as Arc<dyn crate::agent_routing::AgentRoutingChannel>,
        );
        task_feeder.set_notification_dispatcher(
            Arc::clone(&queue_managers) as Arc<dyn crate::queue_manager::NotificationDispatcher>,
        );

        let tasklist_service = Arc::new(
            TasklistService::new(
                Arc::clone(&persistence),
                Arc::clone(&task_feeder),
                Arc::clone(&event_bus),
            )
            .with_instance_registry(Arc::clone(&instance_registry))
            .with_tasklist_queue_managers(Arc::clone(&tasklist_queue_managers)),
        );
        native_runner_ref.set_tasklist_service(
            Arc::clone(&tasklist_service) as Arc<dyn ao_engine_tools_core::TasklistServiceHandle + Send + Sync>,
        );

        // `assignment_fire` needs `queue_managers` (the `NotificationDispatcher`
        // that `fire_assignment` enqueues through), which doesn't exist until
        // after `native_runner` is built — same late-bind reason as
        // `tasklist_service` above.
        let assignment_fire: Arc<dyn ao_engine_tools_core::AssignmentFireHandle + Send + Sync> =
            Arc::new(crate::assignment_runner::ManualAssignmentFirer::new(
                Arc::clone(&persistence),
                Arc::clone(&queue_managers) as Arc<dyn crate::queue_manager::NotificationDispatcher>,
                Arc::clone(&event_bus),
            ));
        native_runner_ref.set_assignment_fire(Arc::clone(&assignment_fire));

        let dispatch_watchdog_shutdown =
            DispatchWatchdogRunner::new(Arc::clone(&task_feeder)).run();

        // Classifier reconciler for the mock path — mirrors the production
        // wiring so mock-backed integration tests exercise the same loop.
        let svc_handle_mock = Arc::clone(&tasklist_service)
            as Arc<dyn ao_engine_tools_core::TasklistServiceHandle + Send + Sync>;
        let classifier_reconciler_shutdown = ClassifierReconciler::new(
            Arc::clone(&task_classifier_handle),
            Arc::clone(&persistence),
            Arc::clone(&svc_handle_mock),
            Arc::clone(&classifier_in_flight),
        )
        .run();
        let cascade_service_mock = AgentCascadeService::new(
            Arc::clone(&persistence),
            Arc::clone(&task_feeder),
            task_classifier_mock,
            Arc::clone(&svc_handle_mock),
        );

        let transcript_pruner_shutdown = TranscriptPrunerRunner::new(
            TaskTranscriptPruner::new(),
            persistence.data_root.clone(),
            PrunerConfig::default(),
        )
        .run();

        let agent_snapshot_sync_shutdown = spawn_agent_snapshot_tasklist_sync(
            Arc::clone(&persistence),
            Arc::clone(&event_bus),
        );

        // Co-pilot mailbox poller: tracks which co-pilot agents are currently
        // "enrolled" (i.e. their tasklist is active or recently opened). The
        // set is rebuilt from disk on startup, kept in sync via the existing
        // `TasklistWoke` / `TasklistSlept` events, and swept periodically to
        // evict tasklists that have transitioned to sleep-eligible without an
        // external emitter.
        let copilot_poller =
            CopilotMailboxPoller::new(Arc::clone(&persistence), Arc::clone(&event_bus));
        let copilot_enrolled = copilot_poller.enrolled();
        let copilot_mailbox_poller_shutdown = copilot_poller.run();
        // Share the enrolled set with the personal queue manager so
        // an inbound QueuedMessage (e.g. a `<task-item-notification>`)
        // addressed to a dormant co-pilot triggers wake-on-deliver enrollment
        // before dispatch.
        queue_managers.set_enrolled_copilots(Arc::clone(&copilot_enrolled));
        {
            let persistence = Arc::clone(&persistence);
            tokio::spawn(async move {
                hydrate_agent_snapshot_fields(persistence).await;
            });
        }
        // Reap `mode: "sync"` pending forms left over from before this
        // restart — a synchronous AskUserQuestionWithForm suspension cannot
        // survive a process restart (see `sync_form_reaper`'s module docs),
        // so any such form whose scope has no live run in this fresh
        // process is dead on arrival and must be marked orphaned rather
        // than shown to the user as still-answerable. Best-effort per-form,
        // same as the two hydration sweeps above: `sync_form_reaper` itself
        // logs real per-form context (agent id + form id) at `error` on
        // failure instead of swallowing it.
        {
            let persistence = Arc::clone(&persistence);
            let instance_registry = Arc::clone(&instance_registry);
            tokio::spawn(async move {
                reap_orphaned_sync_forms(persistence, instance_registry).await;
            });
        }

        let schedule_runner = ScheduleRunner::new(
            Arc::clone(&persistence),
            Arc::clone(&queue_managers),
            Arc::clone(&event_bus),
            Arc::clone(&mcp_manager_mock),
            Arc::clone(&tools_registry),
            Arc::clone(&dispatcher),
        );
        let schedule_runner_shutdown = schedule_runner.run();

        let telegram_transport = Arc::new(TelegramTransport::new(Arc::new(TelegramClient::new())));
        let discord_transport = Arc::new(DiscordTransport::new());
        let email_transport = Arc::new(EmailTransport::new());
        let slack_transport = Arc::new(SlackTransport::new());
        let telegram_bridge = Arc::new(ChannelBridge::new(
            Arc::clone(&persistence),
            Arc::clone(&queue_managers),
            Arc::clone(&event_bus),
            telegram_transport,
            discord_transport,
            email_transport,
            slack_transport,
        ));
        let (telegram_bridge_shutdown, telegram_bridge_join_handle) = Arc::clone(&telegram_bridge).run();

        let agent_sleep_guard_shutdown = AgentSleepGuardRunner::new(
            Arc::clone(&persistence),
            Arc::clone(&instance_registry),
        )
        .run();

        // Fire the one-shot auto-update tick on startup so stale plugins
        // refresh in the background without blocking server start. On any
        // success the plugin cache is rebuilt so the first message turn
        // sees the new content.
        {
            let plugin_cache = Arc::clone(&plugin_cache);
            tokio::spawn(async move {
                match auto_update_tick_async().await {
                    Ok(outcome) => {
                        if outcome.succeeded > 0 {
                            tracing::info!(
                                attempted = outcome.attempted,
                                succeeded = outcome.succeeded,
                                failed = ?outcome.failed,
                                "startup plugin auto-update tick finished",
                            );
                            if let Err(err) = plugin_cache.refresh().await {
                                tracing::warn!(
                                    "plugin cache refresh after auto-update failed: {err}"
                                );
                            }
                        } else if outcome.attempted > 0 {
                            tracing::warn!(
                                attempted = outcome.attempted,
                                failed = ?outcome.failed,
                                "startup plugin auto-update tick: all refreshes failed",
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!("startup plugin auto-update tick failed: {err}");
                    }
                }
            });
        }

        Ok(Self {
            event_bus,
            process_supervisor,
            normalizer_registry,
            command_queue,
            persistence,
            agent_runner,
            running_agents,
            instance_registry,
            queue_managers,
            project_queue_managers,
            tasklist_queue_managers,
            tasklist_queue_dispatcher,
            agent_routing_queue,
            workflow_registry,
            workflow_runner,
            workflow_queue,
            task_feeder,
            tasklist_service,
            assignment_fire,
            schedule_runner_shutdown,
            telegram_bridge_shutdown,
            telegram_bridge,
            telegram_bridge_join_handle: Mutex::new(Some(telegram_bridge_join_handle)),
            agent_sleep_guard_shutdown,
            dispatch_watchdog_shutdown,
            agent_snapshot_sync_shutdown,
            copilot_mailbox_poller_shutdown,
            copilot_enrolled,
            context_cache,
            skill_distiller: skill_distiller_for_state_mock,
            plugin_cache,
            tools_registry,
            mcp_sessions,
            mcp_manager: Arc::clone(&mcp_manager_mock),
            anchor_registry,
            classifier_reconciler_shutdown,
            transcript_pruner_shutdown,
            cascade_service: cascade_service_mock,
            task_classifier_handle,
            classifier_in_flight,
            form_bridge_registry: form_bridge_registry_mock,
            spawner: spawner_mock,
            artifact_task_status: artifact_task_status_mock,
        })
    }
}
