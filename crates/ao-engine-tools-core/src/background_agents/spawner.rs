use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::Mutex;

use chrono::Utc;
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use tokio::sync::broadcast as broadcast_mod;

use ao_protocol::agent::AgentProfile;
use ao_protocol::data_root::resolve_data_root;

use super::child_runner::ChildRunner;
use super::definition::SubagentDefinition;
use super::handle::{RunnerEvent, TaskFinalReport, TaskFinalStatus};
use super::sidechain_persister::{NoopSidechainPersister, SidechainEventMeta, SidechainPersister};
use super::subagent_registry::SubagentRegistry;
use crate::background_agents::{BackgroundAgentHandle, BackgroundAgentId, BackgroundAgentRegistry};
use crate::context::{RunnerContext, UserEvent};
use crate::output::ToolOutput;
use crate::permissions::SessionKind;

/// Default maximum spawn depth.
///
/// A cap of 4 means the chain parent (depth 0) → child (1) → grandchild (2)
/// → great-grandchild (3) is permitted. An attempt to spawn from depth 3 —
/// which would place the child at depth 4 — is refused with
/// [`SpawnerError::DepthExceeded`].
pub const DEFAULT_DEPTH_CAP: usize = 4;

/// Maximum delegate chain length for the Delegate tool.
///
/// When `parent_ctx.delegate_chain.len() + 1 >= DELEGATE_DEPTH_CAP`, the
/// tool returns a non-recoverable error with the exact wording:
/// "Delegation chain limit reached (8 hops). Stopping here."
pub const DELEGATE_DEPTH_CAP: usize = 8;

/// Resolve the effective spawn depth cap for a given agent profile.
///
/// Returns `profile.max_delegation_depth` (converted to `usize`) when set,
/// otherwise falls back to [`DEFAULT_DEPTH_CAP`]. The absent case is the
/// global default — profile authors who want a tighter or looser bound set
/// `max_delegation_depth` explicitly.
pub fn effective_depth_cap(profile: &AgentProfile) -> usize {
    profile
        .max_delegation_depth
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_DEPTH_CAP)
}

/// Resolve the effective delegation chain depth cap for the Delegate tool.
///
/// Returns `profile.max_delegation_depth` (converted to `usize`) when set,
/// otherwise falls back to [`DELEGATE_DEPTH_CAP`] (8). Uses the Delegate
/// tool's higher default rather than [`DEFAULT_DEPTH_CAP`] (4) because
/// cross-profile delegation chains are typically shallower in practice but
/// need more headroom than in-process subagent trees.
pub fn effective_delegate_depth_cap(profile: &AgentProfile) -> usize {
    profile
        .max_delegation_depth
        .map(|n| n as usize)
        .unwrap_or(DELEGATE_DEPTH_CAP)
}

/// Errors returned by [`SubagentSpawner::check_guards`].
///
/// Every variant is distinct so callers can match by name rather than
/// inspecting string content. The mapping to [`ToolOutput::Error`]
/// `recoverable` flags is:
///
/// - [`ConcurrencyCapExceeded`] → `recoverable: true`  (model may retry)
/// - All other variants → `recoverable: false`
///
/// [`ConcurrencyCapExceeded`]: SpawnerError::ConcurrencyCapExceeded
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SpawnerError {
    /// The requested subagent type id is not registered in the catalog.
    #[error("unknown subagent type '{id}'")]
    UnknownSubagentType { id: String },

    /// Spawn refused: the child's depth would equal or exceed the depth cap.
    ///
    /// `depth` is the attempted child depth; `cap` is the configured limit.
    #[error("depth cap of {cap} exceeded (attempted child depth {depth})")]
    DepthExceeded { depth: usize, cap: usize },

    /// Spawn refused: `subagent_type` already appears in the spawn chain,
    /// indicating a name-recursion cycle.
    #[error("recursion detected: '{subagent_type}' already in spawn chain {chain:?}")]
    RecursionDetected {
        subagent_type: String,
        chain: Vec<String>,
    },

    /// Spawn refused: the per-parent concurrency cap is currently full.
    ///
    /// This is the only recoverable error variant — the model may retry after
    /// one of the in-flight children completes and is reaped.
    #[error("concurrency cap reached; retry after an in-flight agent completes")]
    ConcurrencyCapExceeded,
}

impl SpawnerError {
    /// Convert to a [`ToolOutput::Error`] with the correct `recoverable` flag.
    pub fn to_tool_output(&self) -> ToolOutput {
        let recoverable = matches!(self, SpawnerError::ConcurrencyCapExceeded);
        ToolOutput::error(self.to_string(), recoverable)
    }
}

/// Enforces depth, name-recursion, and concurrency guards before a child
/// runner is launched, constructs the child context, and launches the task.
///
/// Construct with a shared [`SubagentRegistry`] and optional
/// [`ChildRunner`](super::child_runner::ChildRunner) (required before calling
/// [`spawn`](Self::spawn)). The guard layer, context construction, and launch
/// are separate methods so each concern stays testable in isolation.
pub struct SubagentSpawner {
    subagent_registry: Arc<SubagentRegistry>,
    depth_cap: usize,
    child_runner: Option<Arc<dyn ChildRunner>>,
    sidechain_persister: Arc<dyn SidechainPersister>,
}

impl SubagentSpawner {
    /// Create a spawner backed by `subagent_registry` with the default depth
    /// cap of [`DEFAULT_DEPTH_CAP`]. Call [`with_child_runner`](Self::with_child_runner)
    /// before using [`spawn`](Self::spawn).
    pub fn new(subagent_registry: Arc<SubagentRegistry>) -> Self {
        Self {
            subagent_registry,
            depth_cap: DEFAULT_DEPTH_CAP,
            child_runner: None,
            sidechain_persister: Arc::new(NoopSidechainPersister),
        }
    }

    /// Override the depth cap.
    pub fn with_depth_cap(mut self, cap: usize) -> Self {
        self.depth_cap = cap;
        self
    }

    /// Shared handle to the subagent catalog this spawner resolves against.
    ///
    /// Callers (e.g. the Delegate tool) use this to enumerate the available
    /// built-in subagent types for dynamic descriptions and to validate a
    /// requested type before spawning, without holding their own copy.
    pub fn subagent_registry(&self) -> Arc<SubagentRegistry> {
        Arc::clone(&self.subagent_registry)
    }

    /// Set the [`ChildRunner`] used by [`spawn`](Self::spawn).
    ///
    /// The production runner passes a `SessionChildRunner` wrapping
    /// `run_session`; tests pass a scripted mock.
    pub fn with_child_runner(mut self, runner: Arc<dyn ChildRunner>) -> Self {
        self.child_runner = Some(runner);
        self
    }

    /// Set the [`SidechainPersister`] that records each child's events to disk.
    ///
    /// The default is [`NoopSidechainPersister`], which discards all events.
    /// Production code supplies a `FileSidechainPersister` (from
    /// `ao-engine-tools-runner`) that writes JSONL under
    /// `LAUNCHPAD_STUDIO_DATA_DIR`.
    pub fn with_sidechain_persister(mut self, persister: Arc<dyn SidechainPersister>) -> Self {
        self.sidechain_persister = persister;
        self
    }

    /// Check all guards before spawning a child of `parent_ctx` with the
    /// given `subagent_type`.
    ///
    /// # Guard order
    ///
    /// 1. **Unknown type** — `subagent_type` must exist in the registry.
    /// 2. **Depth cap** — `parent_ctx.depth + 1` must be less than `depth_cap`.
    /// 3. **Name recursion** — `subagent_type` must not already appear in
    ///    `parent_ctx.spawn_chain`.
    /// 4. **Concurrency cap** — `parent_ctx.background_agents.live_count()`
    ///    must be strictly below `parent_ctx.background_agents.cap()`.
    ///
    /// Returns `Ok(())` only when all guards pass.
    pub async fn check_guards(
        &self,
        parent_ctx: &RunnerContext,
        subagent_type: &str,
        child_profile: Option<&AgentProfile>,
    ) -> Result<(), SpawnerError> {
        // Guard 1: subagent type must be registered.
        if self.subagent_registry.lookup_by_id(subagent_type).is_err() {
            return Err(SpawnerError::UnknownSubagentType {
                id: subagent_type.to_string(),
            });
        }

        // Guard 2: depth cap — the child would live at parent.depth + 1.
        // Resolve against the child's profile when provided; fall back to the
        // spawner's configured cap (which itself defaults to DEFAULT_DEPTH_CAP).
        let resolved_cap = child_profile
            .map(effective_depth_cap)
            .unwrap_or(self.depth_cap);
        let next_depth = parent_ctx.depth + 1;
        if next_depth >= resolved_cap {
            return Err(SpawnerError::DepthExceeded {
                depth: next_depth,
                cap: resolved_cap,
            });
        }

        // Guard 3: name-recursion — same type cannot appear twice in the chain.
        if parent_ctx
            .spawn_chain
            .iter()
            .any(|name| name == subagent_type)
        {
            return Err(SpawnerError::RecursionDetected {
                subagent_type: subagent_type.to_string(),
                chain: parent_ctx.spawn_chain.clone(),
            });
        }

        // Guard 4: concurrency cap.
        let live = parent_ctx.background_agents.live_count().await;
        let cap = parent_ctx.background_agents.cap();
        if live >= cap {
            return Err(SpawnerError::ConcurrencyCapExceeded);
        }

        Ok(())
    }

    /// Construct a child [`RunnerContext`] for a subagent spawn.
    ///
    /// # Contract
    ///
    /// - `session_id`: freshly generated UUID (child gets its own session).
    /// - `agent_id`: set to `background_agent_id.to_string()`.
    /// - `depth`: `parent_ctx.depth + 1`.
    /// - `spawn_chain`: parent's chain extended by `definition.id`.
    /// - `cancel`: a fresh [`CancellationToken`] independent of the parent's.
    ///   The caller (spawner) stores the same token on the
    ///   [`BackgroundAgentHandle`](crate::background_agents::BackgroundAgentHandle)
    ///   so `DelegateStop` can fire it.
    /// - `registry`: filtered to only the tools named in
    ///   `definition.allowed_tools` (via [`Registry::filter_for`]).
    /// - `memory_loader`: Arc-cloned from the parent — same instance, no copy.
    /// - `system_prompt`: assembled as `parent_system_prompt + memory_blob +
    ///   definition.system_prompt_fragment` (non-empty parts joined with `"\n\n"`).
    /// - `background_agents`: a fresh [`BackgroundAgentRegistry`] for the
    ///   child's own grandchildren, with the same cap as the parent's registry.
    /// - All other fields (`cwd`, `permissions`, `todos`, `event_sink`,
    ///   `worktree_stack`, `prompt_bridge`) are Arc-cloned from the parent.
    ///
    /// # Caller responsibility
    ///
    /// Call [`check_guards`](Self::check_guards) before this method. If the
    /// guards pass, this method always succeeds.
    pub fn build_child_context(
        &self,
        parent_ctx: &RunnerContext,
        definition: &SubagentDefinition,
        background_agent_id: &BackgroundAgentId,
    ) -> RunnerContext {
        let child_session_id = uuid::Uuid::new_v4().to_string();
        let child_agent_id = background_agent_id.to_string();
        let child_cancel = CancellationToken::new();

        let mut child_spawn_chain = parent_ctx.spawn_chain.clone();
        child_spawn_chain.push(definition.id.clone());

        let mut child_delegate_chain = parent_ctx.delegate_chain.clone();
        child_delegate_chain.push(parent_ctx.agent_id.clone());

        // A `"*"` entry grants the child the parent's full registry rather than
        // a filtered subset — used by definitions that request the full tool
        // set (e.g. a skill running in fork mode).
        let child_registry: Arc<crate::registry::Registry> = if definition
            .allowed_tools
            .iter()
            .any(|t| t == super::definition::ALL_TOOLS_WILDCARD)
        {
            Arc::clone(&parent_ctx.registry)
        } else {
            Arc::new(parent_ctx.registry.filter_for(&definition.allowed_tools))
        };

        let memory_blob = parent_ctx.memory_loader.load_memory_blob();
        let child_system_prompt = assemble_system_prompt(
            parent_ctx.system_prompt.as_deref(),
            &memory_blob,
            &definition.system_prompt_fragment,
        );

        let grandchild_cap = parent_ctx.background_agents.cap();

        // Snapshot parent's cwd at delegation time for session memory layering.
        let parent_cwd_snapshot = parent_ctx.cwd.read().unwrap().clone();

        RunnerContext {
            session_id: child_session_id,
            agent_id: child_agent_id,
            depth: parent_ctx.depth + 1,
            cancel: child_cancel,
            registry: child_registry,
            cwd: parent_ctx.cwd.clone(),
            permissions: parent_ctx.permissions.clone(),
            todos: parent_ctx.todos.clone(),
            event_sink: parent_ctx.event_sink.clone(),
            worktree_stack: parent_ctx.worktree_stack.clone(),
            prompt_bridge: parent_ctx.prompt_bridge.clone(),
            form_bridge: parent_ctx.form_bridge.clone(),
            spawn_chain: child_spawn_chain,
            delegate_chain: child_delegate_chain,
            background_agents: Arc::new(BackgroundAgentRegistry::new(grandchild_cap)),
            background_processes: Arc::new(
                crate::background_processes::BackgroundProcessRegistry::new(
                    crate::context::DEFAULT_BACKGROUND_PROCESS_CAP,
                ),
            ),
            background_commands: Arc::new(
                crate::background_commands::BackgroundCommandRegistry::new(
                    crate::context::DEFAULT_BACKGROUND_COMMAND_CAP,
                ),
            ),
            memory_loader: parent_ctx.memory_loader.clone(),
            system_prompt: Some(child_system_prompt),
            runner_events: Arc::new(broadcast_mod::channel::<RunnerEvent>(256).0),
            skill_registry: parent_ctx.skill_registry.clone(),
            pending_user_messages: parent_ctx.pending_user_messages.clone(),
            skill_tool_filter: parent_ctx.skill_tool_filter.clone(),
            // Subagents drive their own draining loop, so they use the default
            // enqueue contract regardless of the parent's dispatch environment.
            inline_skill_via_tool_result: false,
            tool_admission: None,
            always_load_tools: parent_ctx.always_load_tools.clone(),
            activated_tools: parent_ctx.activated_tools.clone(),
            loaded_deferred_tools: parent_ctx.loaded_deferred_tools.clone(),
            telemetry: parent_ctx.telemetry.clone(),
            read_file_state: parent_ctx.read_file_state.clone(),
            workflow_runner: parent_ctx.workflow_runner.clone(),
            preferences: parent_ctx.preferences.clone(),
            assignment_store: parent_ctx.assignment_store.clone(),
            assignment_fire: parent_ctx.assignment_fire.clone(),
            agent_workflows: parent_ctx.agent_workflows.clone(),
            memory_store: parent_ctx.memory_store.clone(),
            artifact_store: parent_ctx.artifact_store.clone(),
            // Fresh, not inherited — the spawned agent's turn produces its
            // own message, distinct from the parent's.
            current_message_id: None,
            // Fresh, not inherited — this typed-definition spawn path carries
            // no artifact-regen mode signal (only `build_delegate_context`,
            // `spawn_artifact_agent`'s path, does).
            artifact_intent_source: None,
            transcript_store: parent_ctx.transcript_store.clone(),
            outcome_store: parent_ctx.outcome_store.clone(),
            reflection_staging: parent_ctx.reflection_staging.clone(),
            // Fresh, not inherited — see the field doc on `artifacts_used`.
            artifacts_used: Arc::new(Mutex::new(Vec::new())),
            window_floor_ts: None,
            recall_transcript_path: parent_ctx.recall_transcript_path.clone(),
            tasklist_service: parent_ctx.tasklist_service.clone(),
            classifier: parent_ctx.classifier.clone(),
            classifier_in_flight: parent_ctx.classifier_in_flight.clone(),
            agent_profile_store: parent_ctx.agent_profile_store.clone(),
            parent_session_id: Some(parent_ctx.session_id.clone()),
            parent_agent_id: Some(parent_ctx.agent_id.clone()),
            parent_current_cwd: Some(parent_cwd_snapshot),
            snapshot_store: parent_ctx.snapshot_store.clone(),
            kind: SessionKind::Autonomous,
            sleep_ran: Arc::new(AtomicBool::new(false)),
            // Not inherited: child contexts manage their own delegate
            // notifications if they spawn further delegates.
            delegate_completion_sink: None,
            project_id: parent_ctx.project_id.clone(),
            thread_id: parent_ctx.thread_id.clone(),
            thread_store: parent_ctx.thread_store.clone(),
            project_store: parent_ctx.project_store.clone(),
            verification_engine: parent_ctx.verification_engine.clone(),
            full_verification_engine: parent_ctx.full_verification_engine.clone(),
            thread_summarization_engine: parent_ctx.thread_summarization_engine.clone(),
        }
    }

    /// Spawn a child subagent session: run all guards, build the child context,
    /// create a broadcast event channel, launch via the stored
    /// [`ChildRunner`](super::child_runner::ChildRunner), insert the resulting
    /// handle into the parent's registry, and return the new
    /// [`BackgroundAgentId`] plus a [`broadcast::Receiver`] the caller can use
    /// to observe child events.
    ///
    /// # Guard order
    ///
    /// Same as [`check_guards`](Self::check_guards): unknown type → depth cap
    /// → name recursion → concurrency cap. The registry's [`insert`] is the
    /// authoritative cap check — a concurrent spawn may have raced past
    /// `check_guards`, in which case `insert` returns
    /// [`SpawnerError::ConcurrencyCapExceeded`].
    ///
    /// # Panics
    ///
    /// Panics if [`with_child_runner`](Self::with_child_runner) was never called.
    pub async fn spawn(
        &self,
        parent_ctx: &RunnerContext,
        subagent_type: &str,
        prompt: String,
    ) -> Result<(BackgroundAgentId, broadcast::Receiver<RunnerEvent>), SpawnerError> {
        // 1. All guards must pass before anything is allocated.
        self.check_guards(parent_ctx, subagent_type, None).await?;

        // 2. Re-look up definition — check_guards already verified it exists.
        let definition = self
            .subagent_registry
            .lookup_by_id(subagent_type)
            .expect("check_guards passed so definition must exist");

        // 3. Generate a fresh id — this becomes the child's agent_id.
        let bg_id = BackgroundAgentId::new();

        // 4. Build the child context.
        let child_ctx = self.build_child_context(parent_ctx, definition, &bg_id);
        let child_cancel = child_ctx.cancel.clone();
        let spawned_at = Utc::now();

        // 5. Broadcast channel (capacity 256 buffers a full turn's events).
        //    Drop the initial receiver; subscribe three explicit ones so the
        //    handle, the caller, and the sidechain-persistence sidecar all start
        //    at the same position.
        let (event_tx, _) = broadcast::channel::<RunnerEvent>(256);
        let handle_rx = event_tx.subscribe();
        let caller_rx = event_tx.subscribe();
        let persist_rx = event_tx.subscribe();

        // 6. Launch via the injected ChildRunner.
        let runner = self
            .child_runner
            .as_ref()
            .expect("ChildRunner must be set via with_child_runner before calling spawn");
        let join = runner.launch(child_ctx, prompt, bg_id.clone(), event_tx, None);

        // 7. Build the handle and insert into the parent's registry.
        let handle = BackgroundAgentHandle {
            id: bg_id.clone(),
            subagent_name: subagent_type.to_string(),
            spawned_at,
            cancel: child_cancel,
            events: handle_rx,
            join,
        };
        parent_ctx
            .background_agents
            .insert(handle)
            .await
            .map_err(|_| SpawnerError::ConcurrencyCapExceeded)?;

        // 8. Sidecar task: drain the child's event stream and persist each event.
        //    The loop ends when the broadcast channel closes (child task dropped
        //    its Sender), ensuring every event is persisted before the sidecar exits.
        let persister = self.sidechain_persister.clone();
        let persist_meta = SidechainEventMeta {
            background_agent_id: bg_id.clone(),
            parent_agent_id: parent_ctx.agent_id.clone(),
            subagent_type: subagent_type.to_string(),
            spawned_at,
        };
        tokio::spawn(async move {
            let mut rx = persist_rx;
            loop {
                match rx.recv().await {
                    Ok(event) => persister.persist_event(&persist_meta, &event).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        Ok((bg_id, caller_rx))
    }

    /// Spawn a child subagent synchronously: build the child context from the
    /// given definition, launch via the stored ChildRunner, wait for the child
    /// to complete, and return its last assistant text as a [`ToolOutput`].
    ///
    /// Unlike [`spawn`](Self::spawn), this method accepts a [`SubagentDefinition`]
    /// directly (no registry lookup), does not insert a live handle into the
    /// parent's background-agent registry, and blocks until the child finishes.
    ///
    /// # Panics
    ///
    /// Panics if [`with_child_runner`](Self::with_child_runner) was never called.
    pub async fn spawn_sync(
        &self,
        parent_ctx: &RunnerContext,
        definition: SubagentDefinition,
        prompt: String,
    ) -> ToolOutput {
        let next_depth = parent_ctx.depth + 1;
        if next_depth >= self.depth_cap {
            return SpawnerError::DepthExceeded {
                depth: next_depth,
                cap: self.depth_cap,
            }
            .to_tool_output();
        }

        let bg_id = BackgroundAgentId::new();
        let child_ctx = self.build_child_context(parent_ctx, &definition, &bg_id);
        let child_cancel = child_ctx.cancel.clone();
        let spawned_at = Utc::now();

        let (event_tx, _) = broadcast::channel::<RunnerEvent>(256);
        let persist_rx = event_tx.subscribe();
        let forward_rx = event_tx.subscribe();

        let runner = self
            .child_runner
            .as_ref()
            .expect("ChildRunner must be set via with_child_runner before calling spawn_sync");
        let mut join = runner.launch(child_ctx, prompt, bg_id.clone(), event_tx, None);

        let persister = self.sidechain_persister.clone();
        let persist_meta = SidechainEventMeta {
            background_agent_id: bg_id.clone(),
            parent_agent_id: parent_ctx.agent_id.clone(),
            subagent_type: definition.id.clone(),
            spawned_at,
        };
        tokio::spawn(async move {
            let mut rx = persist_rx;
            loop {
                match rx.recv().await {
                    Ok(event) => persister.persist_event(&persist_meta, &event).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Forward each child RunnerEvent to the parent's event sink so the UI
        // can display the forked skill's intermediate progress in real time.
        // The forwarding task terminates when the broadcast channel closes.
        let parent_sink = parent_ctx.event_sink.clone();
        tokio::spawn(async move {
            let mut rx = forward_rx;
            loop {
                match rx.recv().await {
                    Ok(RunnerEvent::AssistantText { text, .. }) => {
                        let _ = parent_sink.emit(UserEvent::Brief { content: text }).await;
                    }
                    Ok(RunnerEvent::ToolUse { tool_name, .. }) => {
                        let _ = parent_sink
                            .emit(UserEvent::Brief {
                                content: format!("[{}]", tool_name),
                            })
                            .await;
                    }
                    Ok(RunnerEvent::Failed { error, .. }) => {
                        let _ = parent_sink
                            .emit(UserEvent::Brief {
                                content: format!("[fork skill failed] {error}"),
                            })
                            .await;
                    }
                    Ok(RunnerEvent::Completed { .. })
                    | Ok(RunnerEvent::Cancelled { .. })
                    | Ok(RunnerEvent::AsyncLaunched { .. }) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let report = tokio::select! {
            result = &mut join => {
                match result {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => return ToolOutput::error(format!("subagent runner error: {e}"), false),
                    Err(e) => return ToolOutput::error(format!("subagent task panicked: {e}"), false),
                }
            }
            _ = parent_ctx.cancel.cancelled() => {
                child_cancel.cancel();
                return ToolOutput::error("fork skill cancelled by parent", true);
            }
        };

        match report.status {
            TaskFinalStatus::Completed => match report.final_assistant_text {
                Some(text) => ToolOutput::text(text),
                None => {
                    ToolOutput::error("fork skill completed without producing any output", false)
                }
            },
            TaskFinalStatus::Failed => {
                let msg = report
                    .error_message
                    .unwrap_or_else(|| "fork skill failed without an error message".to_string());
                ToolOutput::error(format!("fork skill failed: {msg}"), false)
            }
            TaskFinalStatus::Cancelled => {
                ToolOutput::error("fork skill was cancelled before completing", true)
            }
        }
    }

    /// Spawn a named delegate agent synchronously from an `AgentProfile`.
    ///
    /// Unlike [`spawn_sync`](Self::spawn_sync), this entry point bypasses the
    /// [`SubagentRegistry`] and uses a fully-resolved `AgentProfile` as the
    /// child specification. The child's `agent_id` is set to
    /// `target_profile.id` so `delegate_chain` cycle detection and telemetry
    /// use stable AgentProfile IDs rather than ephemeral background-agent UUIDs.
    ///
    /// # Guard order
    ///
    /// 1. **Depth cap** — resolved via [`effective_delegate_depth_cap`] against
    ///    the child's profile (falls back to [`DELEGATE_DEPTH_CAP`]). Returns a
    ///    non-recoverable error with the wording
    ///    "Delegation chain limit reached (N hops). Stopping here." where N is
    ///    the resolved cap.
    /// 2. No name-recursion check (cycles are allowed; only the
    ///    depth cap is a hard limit).
    ///
    /// # Panics
    ///
    /// Panics if [`with_child_runner`](Self::with_child_runner) was never called.
    pub async fn spawn_named(
        &self,
        parent_ctx: &RunnerContext,
        target_profile: &AgentProfile,
        directive: String,
        _share_context: bool,
    ) -> ToolOutput {
        // Depth cap — resolved from child profile, falls back to DELEGATE_DEPTH_CAP.
        let cap = effective_delegate_depth_cap(target_profile);
        if parent_ctx.delegate_chain.len() + 1 >= cap {
            return ToolOutput::error(
                format!("Delegation chain limit reached ({cap} hops). Stopping here."),
                false,
            );
        }

        let child_ctx = self.build_delegate_context(parent_ctx, target_profile);
        let child_cancel = child_ctx.cancel.clone();
        let bg_id = BackgroundAgentId::new();
        let spawned_at = Utc::now();

        let (event_tx, _) = broadcast::channel::<RunnerEvent>(256);
        let persist_rx = event_tx.subscribe();

        let runner = self
            .child_runner
            .as_ref()
            .expect("ChildRunner must be set via with_child_runner before calling spawn_named");
        let mut join = runner.launch(
            child_ctx,
            directive,
            bg_id.clone(),
            event_tx,
            Some(target_profile.clone()),
        );

        let persister = self.sidechain_persister.clone();
        let persist_meta = SidechainEventMeta {
            background_agent_id: bg_id.clone(),
            parent_agent_id: parent_ctx.agent_id.clone(),
            subagent_type: target_profile.id.clone(),
            spawned_at,
        };
        tokio::spawn(async move {
            let mut rx = persist_rx;
            loop {
                match rx.recv().await {
                    Ok(event) => persister.persist_event(&persist_meta, &event).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let report = tokio::select! {
            result = &mut join => {
                match result {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => return ToolOutput::error(format!("delegate runner error: {e}"), false),
                    Err(e) => return ToolOutput::error(format!("delegate task panicked: {e}"), false),
                }
            }
            _ = parent_ctx.cancel.cancelled() => {
                child_cancel.cancel();
                return ToolOutput::error("delegation cancelled by parent", true);
            }
        };

        match report.status {
            TaskFinalStatus::Completed => match report.final_assistant_text {
                Some(text) => ToolOutput::text(format_with_stats(
                    &text,
                    report.duration_ms,
                    report.num_turns,
                )),
                None => ToolOutput::error("delegate completed without producing any output", false),
            },
            TaskFinalStatus::Failed => {
                let msg = report
                    .error_message
                    .unwrap_or_else(|| "delegate failed without an error message".to_string());
                ToolOutput::error(format!("delegate failed: {msg}"), false)
            }
            TaskFinalStatus::Cancelled => {
                ToolOutput::error("delegation was cancelled before completing", true)
            }
        }
    }

    /// Shared implementation behind [`spawn_named_async`](Self::spawn_named_async)
    /// and [`spawn_named_async_id`](Self::spawn_named_async_id): runs the depth
    /// guard, builds and launches the child, registers it in the parent's
    /// background-agent registry, and wires up the completion-notification
    /// task. Returns the raw `(BackgroundAgentId, transcript_path)` pair on
    /// success. The two public methods differ only in how they present that
    /// pair to their caller — formatted as [`ToolOutput::text`] for an agent's
    /// tool-result channel, or as a plain value for a non-agent caller (e.g.
    /// an HTTP route that needs a `task_id` for a JSON response).
    async fn spawn_named_async_core(
        &self,
        parent_ctx: &RunnerContext,
        target_profile: &AgentProfile,
        directive: String,
        target_name: String,
    ) -> Result<(BackgroundAgentId, String), ToolOutput> {
        // Depth cap — same resolution as sync mode.
        let cap = effective_delegate_depth_cap(target_profile);
        if parent_ctx.delegate_chain.len() + 1 >= cap {
            return Err(ToolOutput::error(
                format!("Delegation chain limit reached ({cap} hops). Stopping here."),
                false,
            ));
        }

        let child_ctx = self.build_delegate_context(parent_ctx, target_profile);
        let child_cancel = child_ctx.cancel.clone();
        let bg_id = BackgroundAgentId::new();
        let delegation_id = bg_id.to_string();
        let spawned_at = Utc::now();

        let (event_tx, _) = broadcast::channel::<RunnerEvent>(256);
        let handle_rx = event_tx.subscribe();
        let persist_rx = event_tx.subscribe();

        let runner = self.child_runner.as_ref().expect(
            "ChildRunner must be set via with_child_runner before calling spawn_named_async",
        );
        let inner_join = runner.launch(
            child_ctx,
            directive,
            bg_id.clone(),
            event_tx,
            Some(target_profile.clone()),
        );

        // Sidecar: drain events into sidechain persistence.
        let persister = self.sidechain_persister.clone();
        let persist_meta = SidechainEventMeta {
            background_agent_id: bg_id.clone(),
            parent_agent_id: parent_ctx.agent_id.clone(),
            subagent_type: target_profile.id.clone(),
            spawned_at,
        };
        tokio::spawn(async move {
            let mut rx = persist_rx;
            loop {
                match rx.recv().await {
                    Ok(event) => persister.persist_event(&persist_meta, &event).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // JoinHandle is not Clone. Thread the report through a oneshot so the registry
        // handle and the notification task can both observe the same result.
        let (done_tx, done_rx) = oneshot::channel::<TaskFinalReport>();
        let join = tokio::spawn(async move {
            let report = match inner_join.await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => TaskFinalReport::failed(format!("runner error: {e}")),
                Err(e) => TaskFinalReport::failed(format!("runner panicked: {e}")),
            };
            let _ = done_tx.send(report.clone());
            Ok(report)
        });

        let handle = BackgroundAgentHandle {
            id: bg_id.clone(),
            subagent_name: target_name.clone(),
            spawned_at,
            cancel: child_cancel,
            events: handle_rx,
            join,
        };
        if parent_ctx.background_agents.insert(handle).await.is_err() {
            return Err(ToolOutput::error(
                "concurrency cap reached; retry after an in-flight delegation completes",
                true,
            ));
        }

        // Spawn marker. Persisted synchronously here — after the registry insert
        // has definitely succeeded, and before this function hands the caller an
        // id — so the transcript file exists from the moment the id is knowable.
        //
        // Without it the transcript is created lazily on the child's *first*
        // event, leaving a window in which a poller finds no file at all. That
        // window is not hypothetical: the registry is dropped at every parent
        // continuation step, so polls routinely land on the transcript rather
        // than the live handle.
        //
        // `AsyncLaunched` is deliberately reused rather than a new variant: it
        // already means "a background child was launched" and it maps to the
        // non-terminal `async_launched` event type, so transcript recovery can
        // never mistake this marker for an outcome.
        //
        // Failure handling is deliberate: the disk is a dependency of the
        // reporting path, and a delegate must never die because its logging
        // failed. `persist_event` is infallible by signature and logs its own
        // WARN (with the underlying io error) on ENOSPC or a bad path, so a
        // failed marker write degrades to a missing marker — visibly, in the
        // log — and never propagates. Running it on a separate task additionally
        // contains a panic from a third-party persister implementation, which is
        // the only remaining way this could take down a spawn that has already
        // registered its handle. Awaiting the task preserves the ordering
        // guarantee above.
        let marker_persister = self.sidechain_persister.clone();
        let marker_meta = SidechainEventMeta {
            background_agent_id: bg_id.clone(),
            parent_agent_id: parent_ctx.agent_id.clone(),
            subagent_type: target_profile.id.clone(),
            spawned_at,
        };
        let marker_event = RunnerEvent::AsyncLaunched {
            background_agent_id: bg_id.clone(),
            subagent_type: target_profile.id.clone(),
            parent_agent_id: parent_ctx.agent_id.clone(),
            spawned_at,
        };
        if let Err(e) = tokio::spawn(async move {
            marker_persister
                .persist_event(&marker_meta, &marker_event)
                .await;
        })
        .await
        {
            tracing::warn!(
                delegation_id = %delegation_id,
                "sidechain: spawn marker task did not complete ({e}); \
                 transcript will be created on the child's first event instead"
            );
        }

        // Bracket the background run with a "started" notification, mirroring
        // the completion notification below — async mode only (spawn_named,
        // the sync path, never calls this). Fires after the handle is
        // registered so DelegateOutput/DelegateStop can already see it by the
        // time the frontend learns the run began.
        if let Some(sink) = &parent_ctx.delegate_completion_sink {
            sink.notify_started(&target_name, &delegation_id, spawned_at)
                .await;
        }

        // Resolve transcript path now (before the spawn) so both the
        // notification task and the immediate return value share the same string.
        let transcript_path = resolve_data_root()
            .map(|r| {
                r.join("messages")
                    .join("data")
                    .join(format!("{}.jsonl", delegation_id))
                    .display()
                    .to_string()
            })
            .unwrap_or_default();

        // Completion-notification task. Fires once when the delegate reaches a
        // terminal state and dispatches to two notification channels:
        //
        // 1. `pending_user_messages` — for native-runner parents that drain the
        //    queue between turns. Always executed.
        // 2. `delegate_completion_sink` — for MCP-driven parents whose per-request
        //    `RunnerContext` is discarded at request boundary, making
        //    `pending_user_messages` invisible to subsequent requests. Executes
        //    only when the sink is wired (i.e. the MCP route injected it).
        let pending = parent_ctx.pending_user_messages.clone();
        let sink = parent_ctx.delegate_completion_sink.clone();
        let name = target_name.clone();
        let delegation_id_for_notification = delegation_id.clone();
        let transcript_path_for_notification = transcript_path.clone();
        tokio::spawn(async move {
            // Normalise done_rx: treat a dropped sender as cancellation.
            let report = match done_rx.await {
                Ok(r) => r,
                Err(_) => TaskFinalReport::cancelled(),
            };

            // Native-runner path: enqueue on pending_user_messages.
            let pending_msg = match report.status {
                TaskFinalStatus::Completed => {
                    let text = report.final_assistant_text.as_deref().unwrap_or_default();
                    format!("[delegate \"{}\" complete]\n{}", name, text)
                }
                TaskFinalStatus::Failed => {
                    let err = report.error_message.as_deref().unwrap_or_default();
                    format!("[delegate \"{}\" failed]\n{}", name, err)
                }
                TaskFinalStatus::Cancelled => format!("[delegate \"{}\" cancelled]", name),
            };
            pending.lock().unwrap().enqueue_low(pending_msg);

            // MCP-route path: dispatch via the durable-queue sink when present.
            if let Some(s) = sink {
                s.notify(
                    &name,
                    &delegation_id_for_notification,
                    &report,
                    &transcript_path_for_notification,
                )
                .await;
            }
        });

        Ok((bg_id, transcript_path))
    }

    /// Spawn a named delegate child in async (non-blocking) mode.
    ///
    /// Returns a [`ToolOutput::text`] immediately containing the `delegation_id`.
    /// When the child finishes, a notification is pushed to
    /// `parent_ctx.pending_user_messages`:
    /// - Completed: `[delegate "<target_name>" complete]\n<final_text>`
    /// - Failed: `[delegate "<target_name>" failed]\n<error_message>`
    /// - Cancelled or task panic: `[delegate "<target_name>" cancelled]`
    ///
    /// # Panics
    ///
    /// Panics if [`with_child_runner`](Self::with_child_runner) was never called.
    pub async fn spawn_named_async(
        &self,
        parent_ctx: &RunnerContext,
        target_profile: &AgentProfile,
        directive: String,
        _share_context: bool,
        target_name: String,
    ) -> ToolOutput {
        match self
            .spawn_named_async_core(parent_ctx, target_profile, directive, target_name.clone())
            .await
        {
            Ok((bg_id, transcript_path)) => {
                let delegation_id = bg_id.to_string();
                if transcript_path.is_empty() {
                    ToolOutput::text(format!(
                        "Delegated to {} in background (delegation_id={})\nPoll with DelegateOutput (supports wait_seconds; results survive restarts).",
                        target_name, delegation_id
                    ))
                } else {
                    ToolOutput::text(format!(
                        "Delegated to {} in background (delegation_id={})\ntranscript_path={}\nPoll with DelegateOutput (supports wait_seconds; results survive restarts).",
                        target_name, delegation_id, transcript_path
                    ))
                }
            }
            Err(output) => output,
        }
    }

    /// Spawn a named delegate child in async (non-blocking) mode and return
    /// its [`BackgroundAgentId`] directly, instead of the human-readable
    /// [`ToolOutput::text`] that [`spawn_named_async`](Self::spawn_named_async)
    /// produces for an agent's tool-result channel.
    ///
    /// For callers outside the tool-call loop — e.g. an HTTP route that needs
    /// a `task_id` value for a JSON response — parsing an id back out of that
    /// formatted string is unnecessary friction. This shares every guard,
    /// launch, sidechain-persistence, and completion-notification code path
    /// with `spawn_named_async` via
    /// [`spawn_named_async_core`](Self::spawn_named_async_core); only the
    /// return shape differs.
    ///
    /// # Panics
    ///
    /// Panics if [`with_child_runner`](Self::with_child_runner) was never called.
    pub async fn spawn_named_async_id(
        &self,
        parent_ctx: &RunnerContext,
        target_profile: &AgentProfile,
        directive: String,
        _share_context: bool,
        target_name: String,
    ) -> Result<BackgroundAgentId, ToolOutput> {
        self.spawn_named_async_core(parent_ctx, target_profile, directive, target_name)
            .await
            .map(|(bg_id, _transcript_path)| bg_id)
    }

    /// Build a child [`RunnerContext`] carrying only structural/metadata for a
    /// named delegate spawn.
    ///
    /// The child's `agent_id` is set to `target_profile.id` so the
    /// `delegate_chain` cycle telemetry keys on stable AgentProfile IDs.
    /// System-prompt composition and registry filtering are intentionally
    /// omitted here — the runner (ProfileAwareChildRunner) receives the full
    /// profile via the `target_profile` parameter of `ChildRunner::launch` and
    /// composes those from scratch inside `run()`.
    fn build_delegate_context(
        &self,
        parent_ctx: &RunnerContext,
        target_profile: &AgentProfile,
    ) -> RunnerContext {
        let child_session_id = uuid::Uuid::new_v4().to_string();
        let child_agent_id = target_profile.id.clone();
        let child_cancel = CancellationToken::new();

        let mut child_delegate_chain = parent_ctx.delegate_chain.clone();
        child_delegate_chain.push(parent_ctx.agent_id.clone());

        let grandchild_cap = parent_ctx.background_agents.cap();

        // Snapshot parent's cwd at delegation time for session memory layering.
        let parent_cwd_snapshot = parent_ctx.cwd.read().unwrap().clone();

        RunnerContext {
            session_id: child_session_id,
            agent_id: child_agent_id,
            depth: parent_ctx.depth + 1,
            cancel: child_cancel,
            registry: Arc::clone(&parent_ctx.registry),
            cwd: parent_ctx.cwd.clone(),
            permissions: parent_ctx.permissions.clone(),
            todos: parent_ctx.todos.clone(),
            event_sink: parent_ctx.event_sink.clone(),
            worktree_stack: parent_ctx.worktree_stack.clone(),
            prompt_bridge: parent_ctx.prompt_bridge.clone(),
            form_bridge: parent_ctx.form_bridge.clone(),
            spawn_chain: parent_ctx.spawn_chain.clone(),
            delegate_chain: child_delegate_chain,
            background_agents: Arc::new(BackgroundAgentRegistry::new(grandchild_cap)),
            background_processes: Arc::new(
                crate::background_processes::BackgroundProcessRegistry::new(
                    crate::context::DEFAULT_BACKGROUND_PROCESS_CAP,
                ),
            ),
            background_commands: Arc::new(
                crate::background_commands::BackgroundCommandRegistry::new(
                    crate::context::DEFAULT_BACKGROUND_COMMAND_CAP,
                ),
            ),
            memory_loader: parent_ctx.memory_loader.clone(),
            system_prompt: None,
            runner_events: Arc::new(broadcast_mod::channel::<RunnerEvent>(256).0),
            skill_registry: parent_ctx.skill_registry.clone(),
            pending_user_messages: parent_ctx.pending_user_messages.clone(),
            skill_tool_filter: parent_ctx.skill_tool_filter.clone(),
            inline_skill_via_tool_result: false,
            tool_admission: None,
            always_load_tools: parent_ctx.always_load_tools.clone(),
            activated_tools: parent_ctx.activated_tools.clone(),
            loaded_deferred_tools: parent_ctx.loaded_deferred_tools.clone(),
            telemetry: parent_ctx.telemetry.clone(),
            read_file_state: parent_ctx.read_file_state.clone(),
            workflow_runner: parent_ctx.workflow_runner.clone(),
            preferences: parent_ctx.preferences.clone(),
            assignment_store: parent_ctx.assignment_store.clone(),
            assignment_fire: parent_ctx.assignment_fire.clone(),
            agent_workflows: None,
            memory_store: parent_ctx.memory_store.clone(),
            artifact_store: parent_ctx.artifact_store.clone(),
            // Fresh, not inherited — the spawned agent's turn produces its
            // own message, distinct from the parent's.
            current_message_id: None,
            // Inherited: `spawn_artifact_agent` sets this on the synthetic
            // parent context it builds so the spawned regenerate/chat-adjust
            // subagent's `ArtifactWrite` call is tagged correctly — see that
            // function and `ArtifactAgentMode::intent_source`.
            artifact_intent_source: parent_ctx.artifact_intent_source,
            transcript_store: parent_ctx.transcript_store.clone(),
            outcome_store: parent_ctx.outcome_store.clone(),
            reflection_staging: parent_ctx.reflection_staging.clone(),
            // Fresh, not inherited — see the field doc on `artifacts_used`.
            artifacts_used: Arc::new(Mutex::new(Vec::new())),
            window_floor_ts: None,
            recall_transcript_path: parent_ctx.recall_transcript_path.clone(),
            tasklist_service: parent_ctx.tasklist_service.clone(),
            classifier: parent_ctx.classifier.clone(),
            classifier_in_flight: parent_ctx.classifier_in_flight.clone(),
            agent_profile_store: parent_ctx.agent_profile_store.clone(),
            parent_session_id: Some(parent_ctx.session_id.clone()),
            parent_agent_id: Some(parent_ctx.agent_id.clone()),
            parent_current_cwd: Some(parent_cwd_snapshot),
            snapshot_store: parent_ctx.snapshot_store.clone(),
            kind: SessionKind::Autonomous,
            sleep_ran: Arc::new(AtomicBool::new(false)),
            // Not inherited: child contexts manage their own delegate
            // notifications if they spawn further delegates.
            delegate_completion_sink: None,
            project_id: parent_ctx.project_id.clone(),
            thread_id: parent_ctx.thread_id.clone(),
            thread_store: parent_ctx.thread_store.clone(),
            project_store: parent_ctx.project_store.clone(),
            verification_engine: parent_ctx.verification_engine.clone(),
            full_verification_engine: parent_ctx.full_verification_engine.clone(),
            thread_summarization_engine: parent_ctx.thread_summarization_engine.clone(),
        }
    }
}

/// Append a compact stats line to `text` when duration or turn count is known.
///
/// Returns the text unchanged when neither stat is available (e.g. test mocks
/// that do not record timing). When at least one stat is present the line is
/// appended as `\n\n[stats: duration=Xms, turns=N]` so it reads as a clearly
/// separate annotation rather than content.
fn format_with_stats(text: &str, duration_ms: Option<u64>, num_turns: Option<u32>) -> String {
    match (duration_ms, num_turns) {
        (Some(d), Some(t)) => format!("{}\n\n[stats: duration={}ms, turns={}]", text, d, t),
        (Some(d), None) => format!("{}\n\n[stats: duration={}ms]", text, d),
        (None, Some(t)) => format!("{}\n\n[stats: turns={}]", text, t),
        (None, None) => text.to_string(),
    }
}

/// Assemble the child's resolved system prompt from its three parts.
///
/// Non-empty parts are joined with `"\n\n"`. Parts that are empty strings
/// are omitted so the result never starts or ends with a blank separator.
fn assemble_system_prompt(
    parent_system_prompt: Option<&str>,
    memory_blob: &str,
    fragment: &str,
) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(3);
    if let Some(p) = parent_system_prompt {
        if !p.is_empty() {
            parts.push(p);
        }
    }
    if !memory_blob.is_empty() {
        parts.push(memory_blob);
    }
    if !fragment.is_empty() {
        parts.push(fragment);
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests;
