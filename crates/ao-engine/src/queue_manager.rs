use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono;
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing;
use uuid;

use ao_engine_tools_core::{SessionKind, DELEGATE_EXCERPT_CAP};
use ao_persistence::PersistenceLayer;
use ao_protocol::agent::{AgentId, AgentProfile};
use ao_protocol::assignment::AssignmentRunStatus;
use ao_protocol::error::AoError;
use ao_protocol::event::{AgentEventPayload, RunEndReason};
use ao_protocol::message::QueuedMessage;
use ao_protocol::scheduled_task::MessageSource;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

use crate::agent_runner::{AgentRunRequest, RunComplete, RunScope, RunnerDispatcher};
use crate::event_bus::EventBus;
use crate::instance_registry::{InstanceRegistry, InstanceRegistryGuard};
use crate::mailbox_poller::EnrolledCopilots;
use crate::prompt_sections::COPILOT_PROFILE_ID;

/// Snapshot of an in-flight `AssignmentRun` bound to a runner's pre-allocated
/// `run_id` (i.e. [`RunComplete::run_id`]). The pump inserts one on dispatch
/// whenever the queued message carries [`MessageSource::Assignment`]; the
/// completion branch and the runner-failure watcher both remove and consume
/// it to write the terminal status back to persistence.
///
/// Kept intentionally small — assignment metadata does not travel inside
/// `RunComplete` itself; this map is how the queue manager reconstructs the
/// association after the runner returns.
#[derive(Clone, Debug)]
struct AssignmentRunRef {
    assignment_id: String,
    run_id: String,
    #[allow(dead_code)]
    thread_id: Option<String>,
}

/// Shared handle type for the pump's `pre_run_id` → `AssignmentRunRef` map.
/// Wrapped in `Arc<Mutex<..>>` so the failure-path watcher spawned outside
/// the actor loop can also mutate it when a runner dies without emitting
/// [`RunComplete`].
type AssignmentRunTracker = Arc<Mutex<HashMap<String, AssignmentRunRef>>>;

/// Abstract surface for submitting a [`QueuedMessage`] to a target agent's
/// mailbox by id. Production: [`QueueManagerRegistry`] (resolves the agent
/// profile then submits via the per-agent queue manager). Tests substitute a
/// recording mock to observe dispatched payloads.
///
/// Why a trait: the agent_runner's parse-success path calls back into the
/// queue-manager submission path. Holding a concrete `Arc<QueueManagerRegistry>`
/// on `AgentRunner` would create a recursive `Send` inference cycle
/// (`run_with_scope` → submit_message → get_or_create → spawn(run) →
/// agent_runner.run → run_with_scope) that the compiler can't break. A
/// trait object erases the future type and breaks the cycle the same way
/// `TaskDispatcher` does for `TaskFeeder.dispatcher`.
#[async_trait]
pub trait NotificationDispatcher: Send + Sync {
    async fn submit_to_agent(
        &self,
        target_agent_id: &str,
        message: QueuedMessage,
    ) -> Result<(), AoError>;
}

#[async_trait]
impl NotificationDispatcher for QueueManagerRegistry {
    async fn submit_to_agent(
        &self,
        target_agent_id: &str,
        message: QueuedMessage,
    ) -> Result<(), AoError> {
        self.submit_message_to_agent_id(target_agent_id, message)
            .await
    }
}

/// Handle used to submit messages to an agent's queue manager.
#[derive(Clone)]
pub struct QueueManagerHandle {
    pub message_tx: mpsc::Sender<QueuedMessage>,
}

/// Registry of per-agent queue managers. Creates new ones lazily on first access.
pub struct QueueManagerRegistry {
    handles: Arc<RwLock<HashMap<AgentId, QueueManagerHandle>>>,
    /// Live queue depth per agent. Maintained by each [`AgentQueueManager`]
    /// after every queue mutation so `list_agents` can overlay an accurate
    /// `queue_depth` onto the snapshot at read time without ever persisting
    /// a runtime-only field. `0` (or "missing") = no queue manager / empty
    /// queue. Survives nothing across process restarts — that's the point.
    queue_depths: Arc<RwLock<HashMap<AgentId, u32>>>,
    /// Dispatcher picks the right AgentRunner impl per agent profile.
    dispatcher: Arc<RunnerDispatcher>,
    instance_registry: Arc<InstanceRegistry>,
    event_bus: Arc<EventBus>,
    persistence: Arc<PersistenceLayer>,
    /// Late-bound enrolled-set handle for the wake-on-deliver path.
    /// Set post-construction by `AppState` via [`set_enrolled_copilots`] so
    /// existing tests that construct a registry without spawning the mailbox
    /// poller (and therefore have no `EnrolledCopilots` to share) stay green —
    /// in that case the wake-on-deliver path is a silent no-op.
    enrolled_copilots: Arc<OnceLock<Arc<EnrolledCopilots>>>,
}

impl QueueManagerRegistry {
    pub fn new(
        dispatcher: Arc<RunnerDispatcher>,
        instance_registry: Arc<InstanceRegistry>,
        event_bus: Arc<EventBus>,
        persistence: Arc<PersistenceLayer>,
    ) -> Self {
        Self {
            handles: Arc::new(RwLock::new(HashMap::new())),
            queue_depths: Arc::new(RwLock::new(HashMap::new())),
            dispatcher,
            instance_registry,
            event_bus,
            persistence,
            enrolled_copilots: Arc::new(OnceLock::new()),
        }
    }

    /// Live queue depth for an agent (0 if no manager exists / queue empty).
    /// Read by the route-time snapshot overlay.
    pub async fn queue_depth_for(&self, agent_id: &AgentId) -> u32 {
        self.queue_depths
            .read()
            .await
            .get(agent_id)
            .copied()
            .unwrap_or(0)
    }

    /// Late-bind the shared `EnrolledCopilots` handle so [`submit_message`]
    /// can idempotently enroll a co-pilot agent on inbound delivery.
    /// Idempotent: a second call after the first is silently ignored.
    pub fn set_enrolled_copilots(&self, enrolled: Arc<EnrolledCopilots>) {
        let _ = self.enrolled_copilots.set(enrolled);
    }

    /// Get or create a queue manager for the given agent.
    /// If one doesn't exist, a new AgentQueueManager task is spawned.
    pub async fn get_or_create(&self, agent: &AgentProfile) -> QueueManagerHandle {
        // Check if handle exists
        {
            let handles = self.handles.read().await;
            if let Some(handle) = handles.get(&agent.id) {
                return handle.clone();
            }
        }

        // Create new queue manager
        let (message_tx, message_rx) = mpsc::channel::<QueuedMessage>(128);
        let (run_complete_tx, run_complete_rx) = mpsc::channel::<RunComplete>(1);

        let handle = QueueManagerHandle {
            message_tx: message_tx.clone(),
        };

        let queue_manager = AgentQueueManager {
            agent_id: agent.id.clone(),
            agent_profile: agent.clone(),
            queue: VecDeque::new(),
            message_rx,
            run_complete_tx,
            run_complete_rx,
            dispatcher: Arc::clone(&self.dispatcher),
            instance_registry: Arc::clone(&self.instance_registry),
            event_bus: Arc::clone(&self.event_bus),
            persistence: Arc::clone(&self.persistence),
            queue_depths: Arc::clone(&self.queue_depths),
            max_instances: agent.max_instances,
            heartbeat_interval: Duration::from_secs(5),
            interactive_runs: HashMap::new(),
            autonomous_thread_locks: HashMap::new(),
            assignment_runs_in_flight: Arc::new(Mutex::new(HashMap::new())),
        };

        // Spawn the queue manager main loop
        tokio::spawn(queue_manager.run());

        // Store handle
        {
            let mut handles = self.handles.write().await;
            handles.insert(agent.id.clone(), handle.clone());
        }

        handle
    }

    /// Remove an agent's queue manager, shutting it down gracefully.
    /// Dropping the handle closes the message channel, causing the `run()` loop to exit.
    /// No-op if the agent has no queue manager.
    pub async fn remove_agent(&self, agent_id: &AgentId) {
        let mut handles = self.handles.write().await;
        if handles.remove(agent_id).is_some() {
            tracing::debug!(agent_id = %agent_id, "Removed agent queue manager (sender dropped, loop will exit)");
        }
    }

    /// Cancel all active runs for the given agent via the instance registry.
    /// Cancellation is now primarily handled by `RunningAgents::cancel` in the
    /// HTTP route; this method is retained for any callers that go through the
    /// queue-manager path directly.
    pub async fn cancel_agent(&self, _agent_id: &AgentId) {
        // Cancellation is routed through RunningAgents.cancel in the HTTP
        // cancel route (messages.rs). No further action needed here.
    }

    /// Submit a message to an agent's queue manager.
    pub async fn submit_message(
        &self,
        agent: &AgentProfile,
        message: QueuedMessage,
    ) -> Result<(), ao_protocol::error::AoError> {
        // Wake-on-deliver. If the target is a co-pilot agent that is not
        // currently in the enrolled set (because its tasklist had gone
        // dormant), idempotently enroll it here so the existing pump path
        // dispatches the queued message without waiting for an external wake
        // signal. Direct enrollment via `EnrolledCopilots` is the alternative
        // to emitting a `TasklistWoke` event — picked here because it doesn't
        // require a per-message reverse-lookup of the bound tasklist and
        // doesn't pollute the SSE stream with a wake event for every
        // notification. Sleep transitions still fire normally because direct
        // enrollment doesn't update `last_active_at`/`last_opened_at` (no
        // infinite-wake loop).
        if let Some(enrolled) = self.enrolled_copilots.get() {
            wake_copilot_on_deliver(agent, enrolled).await;
        }

        let handle = self.get_or_create(agent).await;
        handle
            .message_tx
            .send(message)
            .await
            .map_err(|e| ao_protocol::error::AoError::Internal(format!("Queue send error: {}", e)))
    }

    /// Resolve an `AgentId` to its [`AgentProfile`] via the persistence layer
    /// and submit a message to the resulting queue manager. Returns
    /// `AoError::AgentNotFound` if no such agent exists. Used by the
    /// agent_runner's parse-success path to dispatch a `<task-item-notification>`
    /// QueuedMessage to a task's `remind_me` agent without the runner having
    /// to fetch the profile itself.
    pub async fn submit_message_to_agent_id(
        &self,
        agent_id: &str,
        message: QueuedMessage,
    ) -> Result<(), ao_protocol::error::AoError> {
        let agent = self
            .persistence
            .agents
            .get(agent_id)
            .await?
            .ok_or_else(|| ao_protocol::error::AoError::AgentNotFound(agent_id.to_string()))?;
        self.submit_message(&agent, message).await
    }
}

/// Per-agent queue manager that processes messages in order, respects instance caps,
/// and uses an event-driven pump pattern.
struct AgentQueueManager {
    agent_id: AgentId,
    agent_profile: AgentProfile,
    queue: VecDeque<QueuedMessage>,
    message_rx: mpsc::Receiver<QueuedMessage>,
    run_complete_tx: mpsc::Sender<RunComplete>,
    run_complete_rx: mpsc::Receiver<RunComplete>,
    dispatcher: Arc<RunnerDispatcher>,
    instance_registry: Arc<InstanceRegistry>,
    event_bus: Arc<EventBus>,
    persistence: Arc<PersistenceLayer>,
    /// Shared depth map back to the registry. Refreshed after every queue
    /// mutation so route-time overlays don't have to round-trip through this
    /// actor's mailbox.
    queue_depths: Arc<RwLock<HashMap<AgentId, u32>>>,
    max_instances: u32,
    heartbeat_interval: Duration,
    /// `run_id -> thread_id` for in-flight interactive turns. Serialization
    /// is scoped per thread, not per agent: a user typing in thread A must
    /// not see thread B's reply interleaved on the wire, but two different
    /// threads on the same agent are independent conversations and may run
    /// concurrently. `None` is itself a valid, distinct key — it consistently
    /// denotes the agent's default/main thread across the codebase, so it
    /// compares correctly against other `None`s and never collides with a
    /// real thread id. The pump consults this map before dispatching an
    /// interactive message: if some other in-flight run already holds the
    /// same thread_id, that message stays queued until the active run for
    /// its thread ends. Autonomous traffic (scheduled tasks, workflow
    /// follow-ups, assignments) is exempt from *this* gate — it never had a
    /// `serialize` opt-out concept to begin with — but see
    /// `autonomous_thread_locks` below for the guard that keeps it from
    /// racing another run on the same thread.
    interactive_runs: HashMap<String, Option<String>>,
    /// `run_id -> thread_id` for in-flight autonomous runs (assignments,
    /// scheduled tasks). Exists purely for transcript-safety, independent of
    /// `interactive_runs` and its `serialize` opt-out: since `Assignment`
    /// gained `Main`/`Dedicated` thread policies, an autonomous run can now
    /// target the same thread as a live interactive turn, another
    /// autonomous run (e.g. two `Main`-policy assignments, or a burst of
    /// webhook fires racing to reuse one `Dedicated` thread), or both.
    /// Nothing may write to a thread two runs are contending for at once, so
    /// `pump` unconditionally blocks any candidate message — interactive or
    /// autonomous — whose `thread_id` matches an entry here, with no
    /// `serialize`-style opt-out. `Fresh`-policy assignment runs are
    /// unaffected in practice: every fire mints a brand-new, never-seen-
    /// before thread id, so a collision here is structurally impossible for
    /// them. Populated for every dispatched autonomous message; cleared
    /// alongside `interactive_runs` on `RunComplete`.
    autonomous_thread_locks: HashMap<String, Option<String>>,
    /// Maps a runner's pre-allocated `run_id` to the [`AssignmentRunRef`]
    /// captured at dispatch. Entries land here only when the queued message
    /// carries [`MessageSource::Assignment`]. Removed on either the completion
    /// branch (writes `Succeeded` + `output_summary`) or the runner-failure
    /// watcher spawned around `runner.run` (writes `Failed` + `error`). Shared
    /// via `Arc<Mutex<..>>` because the failure watcher lives outside the
    /// actor loop and would otherwise not be able to clear the map.
    assignment_runs_in_flight: AssignmentRunTracker,
}

impl AgentQueueManager {
    /// Mirror the in-actor queue length into the registry-shared depth map.
    /// Cheap (one short-lived write lock) and keeps the runtime overlay
    /// truthful between heartbeats.
    async fn publish_depth(&self) {
        let depth = self.queue.len() as u32;
        let mut map = self.queue_depths.write().await;
        if depth == 0 {
            map.remove(&self.agent_id);
        } else {
            map.insert(self.agent_id.clone(), depth);
        }
    }
}

impl AgentQueueManager {
    /// Main event loop using tokio::select! for message arrival, run completion, and heartbeat.
    async fn run(mut self) {
        let mut heartbeat = tokio::time::interval(self.heartbeat_interval);
        // Don't fire immediately on start
        heartbeat.tick().await;

        loop {
            tokio::select! {
                // Branch 1: New message arrives
                msg = self.message_rx.recv() => {
                    match msg {
                        Some(message) => {
                            tracing::debug!(
                                agent_id = %self.agent_id,
                                message_id = %message.message_id,
                                "Message received in queue"
                            );
                            self.queue.push_back(message);
                            self.publish_depth().await;
                            self.pump().await;
                        }
                        None => {
                            // Channel closed, all senders dropped — shutdown
                            tracing::debug!(agent_id = %self.agent_id, "Queue manager shutting down (channel closed)");
                            break;
                        }
                    }
                }
                // Branch 2: A run completes
                run_complete = self.run_complete_rx.recv() => {
                    match run_complete {
                        Some(rc) => {
                            // Clear the interactive lease (if held) before the
                            // pump runs — otherwise the just-completed
                            // interactive turn would keep blocking the next
                            // queued one. Also clear any autonomous
                            // thread-lock this run held (a no-op if the
                            // completed run was interactive).
                            self.interactive_runs.remove(&rc.run_id);
                            self.autonomous_thread_locks.remove(&rc.run_id);

                            // If this completion belongs to an in-flight
                            // AssignmentRun, transition the persisted row to
                            // its terminal status and drop the map entry.
                            // `RunComplete` is `Ok(..)` on every exit from the
                            // CLI runner's continuation loop, including a
                            // process-spawn failure (which breaks the loop
                            // with `end_reason: Error` rather than returning
                            // `Err`) — so success must be read from
                            // `end_reason`, not inferred from `Ok` alone.
                            let assignment_ref = self
                                .assignment_runs_in_flight
                                .lock()
                                .await
                                .remove(&rc.run_id);
                            if let Some(assignment_ref) = assignment_ref {
                                if rc.end_reason == RunEndReason::Completed {
                                    mark_assignment_run_succeeded(
                                        &self.persistence,
                                        &self.event_bus,
                                        &self.agent_id,
                                        &assignment_ref,
                                        &rc.output_text,
                                    )
                                    .await;
                                } else {
                                    let error_text = if rc.output_text.trim().is_empty() {
                                        format!("Run ended with {:?}", rc.end_reason)
                                    } else {
                                        format!(
                                            "Run ended with {:?}: {}",
                                            rc.end_reason,
                                            rc.output_text.trim()
                                        )
                                    };
                                    mark_assignment_run_failed(
                                        &self.persistence,
                                        &self.event_bus,
                                        &self.agent_id,
                                        &assignment_ref,
                                        error_text,
                                    )
                                    .await;
                                }
                            }

                            tracing::debug!(
                                agent_id = %self.agent_id,
                                run_id = %rc.run_id,
                                "Run completed, pumping queue"
                            );
                            // Queue workflow follow-up messages (next phase context)
                            for followup in rc.workflow_followups {
                                // Write system transcript entry if present (renders as centered bubble in UI)
                                if let Some(ref sys_text) = followup.system_transcript {
                                    let sys_entry = TranscriptEntry {
                                        ts: chrono::Utc::now(),
                                        role: TranscriptRole::System("system".to_string()),
                                        content: sys_text.clone(),
                                        event_type: "workflow_system".to_string(),
                                        metadata: None,
                                        hidden_from_user: false,
                                    };
                                    if let Err(e) = self
                                        .persistence
                                        .transcripts
                                        .append(&self.agent_id, &sys_entry)
                                        .await
                                    {
                                        tracing::error!(
                                            agent_id = %self.agent_id,
                                            "Failed to write workflow system transcript: {}",
                                            e
                                        );
                                    }
                                    // Emit SSE event so the UI can display the system bubble immediately
                                    self.event_bus
                                        .emit(
                                            &format!("system-{}", uuid::Uuid::new_v4()),
                                            &self.agent_id,
                                            None,
                                            AgentEventPayload::SystemMessage {
                                                text: sys_text.clone(),
                                                severity: None,
                                            },
                                        )
                                        .await;
                                }
                                let msg = QueuedMessage {
                                    message_id: format!("workflow-{}", uuid::Uuid::new_v4()),
                                    content: followup.context,
                                    queued_at: chrono::Utc::now(),
                                    attachments: vec![],
                                    source: None,
                                    focus_path: None,
                                    thread_id: None,
                                };
                                self.queue.push_back(msg);
                            }
                            self.publish_depth().await;
                            self.pump().await;
                        }
                        None => {
                            // Should not happen since we hold run_complete_tx
                            break;
                        }
                    }
                }
                // Branch 3: Heartbeat tick
                _ = heartbeat.tick() => {
                    let running = self.instance_registry.running_count(&self.agent_id).await;
                    tracing::debug!(
                        agent_id = %self.agent_id,
                        queue_depth = self.queue.len(),
                        running_count = running,
                        "Heartbeat"
                    );
                    // Safety net pump
                    self.pump().await;
                }
            }
        }
    }

    /// Try to dispatch queued messages while instance capacity is available.
    async fn pump(&mut self) {
        // Re-read agent profile from disk to pick up any edits
        let fresh_profile = match self.persistence.agents.get(&self.agent_id).await {
            Ok(Some(profile)) => {
                self.max_instances = profile.max_instances;
                profile
            }
            Ok(None) => {
                tracing::warn!(agent_id = %self.agent_id, "Agent not found during pump; using cached profile");
                self.agent_profile.clone()
            }
            Err(e) => {
                tracing::warn!(agent_id = %self.agent_id, "Failed to re-read profile during pump: {}; using cached", e);
                self.agent_profile.clone()
            }
        };

        while !self.queue.is_empty()
            && self
                .instance_registry
                .can_spawn(&self.agent_id, self.max_instances)
                .await
        {
            // Prioritize user messages over recurring scheduled messages.
            // One-shot scheduled messages (no recurring flag) keep FIFO order.
            // Look for the first non-recurring-schedule message; if none, take front.
            //
            // Interactive serialization: when an interactive turn is already
            // in flight for a given thread AND the agent profile requests
            // serialization, hold subsequent interactive messages for that
            // *same thread* so user-typed turns within one conversation run
            // one at a time. Different threads on the same agent are
            // independent conversations and are not blocked by each other.
            // Agents with serialize=false explicitly opt into concurrency
            // (governed solely by max_instances) and bypass this check.
            let idx = self
                .queue
                .iter()
                .position(|m| {
                    if is_recurring_schedule(m) {
                        return false;
                    }
                    if fresh_profile.serialize
                        && is_interactive_message(m)
                        && self
                            .interactive_runs
                            .values()
                            .any(|thread_id| *thread_id == m.thread_id)
                    {
                        return false;
                    }
                    // Cross-source thread-collision guard: unconditional,
                    // no `serialize` opt-out. Now that assignments can carry
                    // `Main`/`Dedicated` thread policies, a candidate message
                    // of *any* source must not dispatch while some other
                    // in-flight run — interactive or autonomous — already
                    // holds its exact thread_id (`None` included; that's the
                    // agent's default thread). Without this, a `Main`-policy
                    // assignment firing mid-conversation, two assignments
                    // sharing one thread, or a webhook burst reusing one
                    // `Dedicated` thread could race the same transcript file.
                    // `Fresh`-policy runs are never affected in practice —
                    // each mints a brand-new, never-before-seen thread id.
                    if self
                        .autonomous_thread_locks
                        .values()
                        .any(|thread_id| *thread_id == m.thread_id)
                    {
                        return false;
                    }
                    if !is_interactive_message(m)
                        && self
                            .interactive_runs
                            .values()
                            .any(|thread_id| *thread_id == m.thread_id)
                    {
                        return false;
                    }
                    true
                })
                .or_else(|| self.queue.iter().position(is_recurring_schedule));
            let Some(idx) = idx else {
                // Every queued message is blocked: either every remaining
                // message is interactive with its thread already occupied,
                // or a candidate's thread collides with an in-flight run of
                // any source — wait for those to complete.
                break;
            };
            let message = self.queue.remove(idx).unwrap();
            let is_interactive = is_interactive_message(&message);
            // Publish the new depth eagerly so any concurrent `list_agents`
            // call between dispatch and the runner registering with the
            // instance registry sees the post-pop length.
            self.publish_depth().await;

            // Emit MessageProcessingStarted event
            self.event_bus
                .emit(
                    &format!("queue-{}", self.agent_id),
                    &self.agent_id,
                    None,
                    AgentEventPayload::MessageProcessingStarted {
                        message_id: message.message_id.clone(),
                    },
                )
                .await;

            // Start the agent run. Tasklist tasks no longer flow through
            // this personal queue — they're dispatched via
            // `TasklistQueueDispatcher` -> `TasklistQueueManager`. Anything
            // landing here is a personal/scheduled/team-fanned-out message.
            //
            // When this agent is a tasklist co-pilot, prepend a
            // `<copilot-context>` block to the prompt. The original
            // `message.content` is left intact (transcripts already persisted
            // it via the route handler) — only `AgentRunner::run`'s prompt
            // input is augmented. Non-co-pilot agents short-circuit on the
            // template check inside `inject_copilot_context`.
            let prompt = crate::copilot_context::inject_copilot_context(
                &self.persistence,
                fresh_profile.template.as_deref(),
                &fresh_profile.id,
                &message.content,
            )
            .await;
            // Dispatch the run through the AgentRunner trait. The runner sends on
            // run_complete_tx when the run finishes; this queue manager's select!
            // loop receives from run_complete_rx as before.
            //
            // Pre-allocate the run_id and synchronously book the slot in
            // `InstanceRegistry` *before* the `tokio::spawn` below. Without
            // this, the next iteration of this `while` loop would re-check
            // `can_spawn` before the just-spawned runner had a chance to
            // await its own async `register_run` — observing the slot as
            // still free and over-spawning under `max_instances = 1`. The
            // runner adopts this id via `pre_registered_run_id` and skips
            // its own register. Cleanup is owned by the
            // `InstanceRegistryGuard` moved into the spawned task below,
            // so the slot frees on every exit path including panics and
            // runner-side early-return errors.
            let pre_run_id = uuid::Uuid::new_v4().to_string();
            self.instance_registry
                .register_run_with_thread(&self.agent_id, &pre_run_id, message.thread_id.clone())
                .await;
            let registry_guard = InstanceRegistryGuard::wrap_existing(
                Arc::clone(&self.instance_registry),
                self.agent_id.clone(),
                pre_run_id.clone(),
            );
            let session_kind = if matches!(
                message.source,
                Some(MessageSource::Schedule { .. }) | Some(MessageSource::Assignment { .. })
            ) {
                SessionKind::Autonomous
            } else {
                SessionKind::Interactive
            };
            // Book the interactive lease, or the autonomous thread lock,
            // *before* the spawn so a concurrent pump tick (heartbeat,
            // follow-up message) cannot dispatch a second run onto the same
            // thread in the same tick.
            if is_interactive {
                self.interactive_runs
                    .insert(pre_run_id.clone(), message.thread_id.clone());
            } else {
                self.autonomous_thread_locks
                    .insert(pre_run_id.clone(), message.thread_id.clone());
            }

            // Assignment lifecycle bookkeeping. When this message came from
            // an assignment trigger we:
            //   1. record the mapping `pre_run_id -> AssignmentRunRef` so
            //      completion / failure branches know which persisted row
            //      to write terminal status back to,
            //   2. transition the row from `Queued` to `Running` right now
            //      so the Assignments tab stops showing the run as pending
            //      the moment dispatch begins.
            // The `AssignmentRunRef` is also cloned into the outer runner
            // watcher (below) so it can write `Failed` if the runner errors
            // or panics without ever emitting `RunComplete`.
            let assignment_ref_for_failure = if let Some(MessageSource::Assignment {
                assignment_id,
                run_id,
                ..
            }) = &message.source
            {
                let assignment_ref = AssignmentRunRef {
                    assignment_id: assignment_id.clone(),
                    run_id: run_id.clone(),
                    thread_id: message.thread_id.clone(),
                };
                self.assignment_runs_in_flight
                    .lock()
                    .await
                    .insert(pre_run_id.clone(), assignment_ref.clone());
                mark_assignment_run_running(
                    &self.persistence,
                    &self.agent_id,
                    &assignment_ref,
                )
                .await;
                Some(assignment_ref)
            } else {
                None
            };

            let request = AgentRunRequest {
                agent: fresh_profile.clone(),
                prompt: prompt.clone(),
                attachments: message.attachments.clone(),
                run_complete_tx: self.run_complete_tx.clone(),
                focus_path: message.focus_path.clone(),
                scope: RunScope::Standalone,
                thread_id: message.thread_id.clone(),
                session_kind,
                pre_registered_run_id: Some(pre_run_id.clone()),
                ..Default::default()
            };
            let runner = self.dispatcher.pick(&self.agent_profile);
            let agent_id_for_log = self.agent_id.clone();
            let msg_id_for_log = message.message_id.clone();
            let event_bus_for_err = Arc::clone(&self.event_bus);

            // No snapshot mutation here. `has_active_run` and `queue_depth`
            // are derived at read time in the routes layer from the in-memory
            // [`InstanceRegistry`] and [`QueueManagerRegistry::queue_depth_for`]
            // — the same writers that own the underlying state. Persisting
            // these runtime fields used to require six separate cleanup
            // ladders (this site, dispatch-error, both runners' post-run
            // blocks, the boot sweep) and any missing reset wedged the
            // sidebar typing indicator until a new run completed.

            // Spawn the runner on its own task and watch the JoinHandle so a
            // panic inside `runner.run` surfaces as a user-visible
            // `Error` + `RunEnded(Error)` pair on the event bus. Before this
            // watcher landed, a panicking runner task died silently — the
            // queue manager observed it via "Run completed without result"
            // but the frontend never received `run_ended`, so the in-flight
            // chat bubble lingered until refresh.
            //
            // This covers the Native runner end-to-end (its whole turn runs
            // inside `runner.run`). For CLI runs, `runner.run` returns Ok
            // quickly after dispatching the runner's own inner spawn; the
            // inner spawn has its own dedicated panic watcher inside
            // `CliAgentRunner::run_with_scope`.
            let panic_agent_id = agent_id_for_log.clone();
            let panic_event_bus = Arc::clone(&event_bus_for_err);
            // Failure-path handles cloned for the outer runner watcher.
            // When the runner returns `Err` or panics without emitting
            // `RunComplete`, the completion branch will never fire, so the
            // watcher is the only place that can transition the mapped
            // AssignmentRun row to `Failed` and drop the tracker entry.
            let failure_assignment_ref = assignment_ref_for_failure.clone();
            let failure_pre_run_id = pre_run_id.clone();
            let failure_tracker = Arc::clone(&self.assignment_runs_in_flight);
            let failure_persistence = Arc::clone(&self.persistence);
            let failure_event_bus = Arc::clone(&event_bus_for_err);
            // Move the InstanceRegistry guard into the runner's task so
            // its Drop fires when the run completes (Ok, Err, or panic).
            // `runner.run` blocks until the inner workload sends on the
            // capture channel — see `CliAgentRunner::run` — so the guard
            // is held for the full lifetime of the run. The runner also
            // has its own `wrap_existing` deeper in `run_with_scope`, but
            // `unregister_run` is idempotent so the double-Drop is safe;
            // this outer guard is the load-bearing one because it covers
            // early-return errors *before* the runner's inner spawn.
            let inner = tokio::spawn(async move {
                let _registry_guard = registry_guard;
                runner.run(request).await
            });
            tokio::spawn(async move {
                match inner.await {
                    Ok(Ok(rc)) => {
                        tracing::debug!(
                            agent_id = %agent_id_for_log,
                            run_id = %rc.run_id,
                            message_id = %msg_id_for_log,
                            "Started run for queued message"
                        );
                    }
                    Ok(Err(e)) => {
                        tracing::error!(
                            agent_id = %agent_id_for_log,
                            message_id = %msg_id_for_log,
                            "Failed to start run: {}",
                            e
                        );
                        event_bus_for_err
                            .emit(
                                &format!("queue-{}", agent_id_for_log),
                                &agent_id_for_log,
                                None,
                                AgentEventPayload::Error {
                                    message: format!("Failed to start run: {}", e),
                                    recoverable: true,
                                },
                            )
                            .await;
                        // Assignment-run failure recovery. The runner exited
                        // with an error and no `RunComplete` was sent, so the
                        // completion branch of the actor loop will never
                        // clear this run. Drop the tracker entry and mark the
                        // AssignmentRun as `Failed` — otherwise the row would
                        // remain stuck in `Running` forever.
                        if failure_assignment_ref.is_some() {
                            let mapped =
                                failure_tracker.lock().await.remove(&failure_pre_run_id);
                            if let Some(assignment_ref) = mapped {
                                mark_assignment_run_failed(
                                    &failure_persistence,
                                    &failure_event_bus,
                                    &agent_id_for_log,
                                    &assignment_ref,
                                    format!("Runner error: {}", e),
                                )
                                .await;
                            }
                        }
                    }
                    Err(join_err) if join_err.is_panic() => {
                        let payload = join_err.into_panic();
                        let panic_msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                            (*s).to_string()
                        } else if let Some(s) = payload.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "non-string panic payload".to_string()
                        };
                        let user_msg = format!(
                            "Agent runner crashed mid-run: {}. The run was terminated. Try again, and if this repeats check the server logs for a stack trace.",
                            panic_msg
                        );
                        tracing::error!(
                            agent_id = %panic_agent_id,
                            panic = %panic_msg,
                            "Runner task panicked at queue-manager spawn level"
                        );
                        // Synthetic run_id — we don't have the one the runner
                        // would have generated, but the frontend keys on
                        // agent_id, so a unique-per-emit string is sufficient
                        // to keep EventBus's seq counter happy.
                        let synthetic_run_id = format!("queue-{}-panic-{}", panic_agent_id, uuid::Uuid::new_v4());
                        panic_event_bus
                            .emit(
                                &synthetic_run_id,
                                &panic_agent_id,
                                None,
                                AgentEventPayload::Error {
                                    message: user_msg,
                                    recoverable: false,
                                },
                            )
                            .await;
                        panic_event_bus
                            .emit(
                                &synthetic_run_id,
                                &panic_agent_id,
                                None,
                                AgentEventPayload::RunEnded {
                                    reason: RunEndReason::Error,
                                },
                            )
                            .await;
                        // InstanceRegistry cleanup happens via the
                        // `InstanceRegistryGuard` Drop inside the panicked
                        // task. No direct cleanup needed here.
                        // Assignment-run failure recovery mirrors the Err
                        // branch above: mark the row `Failed` so it isn't
                        // stranded in `Running` after a runner panic.
                        if failure_assignment_ref.is_some() {
                            let mapped =
                                failure_tracker.lock().await.remove(&failure_pre_run_id);
                            if let Some(assignment_ref) = mapped {
                                mark_assignment_run_failed(
                                    &failure_persistence,
                                    &failure_event_bus,
                                    &panic_agent_id,
                                    &assignment_ref,
                                    format!("Runner panic: {}", panic_msg),
                                )
                                .await;
                            }
                        }
                    }
                    Err(_) => {
                        // Task was cancelled at the runtime level (rare;
                        // normal cancellation flows through CancellationToken
                        // and yields an Ok above).
                    }
                }
            });
        }
    }
}

/// Wake-on-deliver helper. Returns `true` if the call newly enrolled a
/// dormant co-pilot, `false` if the agent is not a co-pilot or was already
/// enrolled. Pulled out of [`QueueManagerRegistry::submit_message`] as a free
/// function so unit tests can exercise the gating logic without spinning up
/// the full registry + agent runner machinery.
pub(crate) async fn wake_copilot_on_deliver(
    agent: &AgentProfile,
    enrolled: &EnrolledCopilots,
) -> bool {
    if agent.template.as_deref() != Some(COPILOT_PROFILE_ID) {
        return false;
    }
    let added = enrolled.enroll(&agent.id).await;
    if added {
        tracing::debug!(
            agent_id = %agent.id,
            "Wake-on-deliver enrolled dormant co-pilot",
        );
    }
    added
}

/// Returns true if the message originated from a recurring scheduled task.
fn is_recurring_schedule(msg: &QueuedMessage) -> bool {
    matches!(
        msg.source,
        Some(MessageSource::Schedule {
            is_recurring: true,
            ..
        })
    )
}

/// Whether the message represents an interactive (human-attended) turn.
///
/// A turn is interactive unless its source identifies it as scheduled or
/// otherwise autonomous. The classification matches the `SessionKind` the
/// pump assigns when building the [`AgentRunRequest`] so the serialization
/// gate, the runner's permission policy, and the runtime pacing section all
/// agree on whether a human is watching.
fn is_interactive_message(msg: &QueuedMessage) -> bool {
    !matches!(
        msg.source,
        Some(MessageSource::Schedule { .. }) | Some(MessageSource::Assignment { .. })
    )
}

/// Load an AssignmentRun row and transition it to `Running` with a fresh
/// `started_ts`. Logs a warn on any failure — a missing row or a persistence
/// error must not abort dispatch. Called from the pump at the exact moment
/// a runner is spawned for an assignment-sourced message so the Assignments
/// tab reflects the transition immediately.
async fn mark_assignment_run_running(
    persistence: &Arc<PersistenceLayer>,
    agent_id: &AgentId,
    assignment_ref: &AssignmentRunRef,
) {
    match persistence
        .assignment_runs
        .get(&assignment_ref.assignment_id, &assignment_ref.run_id)
        .await
    {
        Ok(Some(mut run)) => {
            run.status = AssignmentRunStatus::Running;
            run.started_ts = Some(chrono::Utc::now());
            if let Err(e) = persistence
                .assignment_runs
                .update(&assignment_ref.assignment_id, &run)
                .await
            {
                tracing::warn!(
                    agent_id = %agent_id,
                    assignment_id = %assignment_ref.assignment_id,
                    run_id = %assignment_ref.run_id,
                    "Failed to persist AssignmentRun Running transition: {}",
                    e
                );
            }
        }
        Ok(None) => {
            tracing::warn!(
                agent_id = %agent_id,
                assignment_id = %assignment_ref.assignment_id,
                run_id = %assignment_ref.run_id,
                "AssignmentRun row missing when transitioning to Running"
            );
        }
        Err(e) => {
            tracing::warn!(
                agent_id = %agent_id,
                assignment_id = %assignment_ref.assignment_id,
                run_id = %assignment_ref.run_id,
                "Failed to load AssignmentRun for Running transition: {}",
                e
            );
        }
    }
}

/// Truncate a raw assistant output to the assignment `output_summary` cap and
/// return `None` when nothing remained after trimming. The reused
/// [`DELEGATE_EXCERPT_CAP`] applies the same 2000-char ceiling as delegate
/// completion summaries so the Assignments tab preview never balloons out
/// of a table row.
fn build_output_summary(output_text: &str) -> Option<String> {
    let trimmed = output_text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() > DELEGATE_EXCERPT_CAP {
        let mut truncated: String = trimmed.chars().take(DELEGATE_EXCERPT_CAP).collect();
        truncated.push('…');
        Some(truncated)
    } else {
        Some(trimmed.to_string())
    }
}

/// Transition an AssignmentRun row to `Succeeded`, populating `output_summary`
/// from the runner's final assistant text and stamping `finished_ts`. Emits a
/// `SystemMessage` on the per-assignment SSE channel so a live Assignments tab
/// refetches without waiting for a poll.
async fn mark_assignment_run_succeeded(
    persistence: &Arc<PersistenceLayer>,
    event_bus: &Arc<EventBus>,
    agent_id: &AgentId,
    assignment_ref: &AssignmentRunRef,
    output_text: &str,
) {
    let summary = build_output_summary(output_text);
    match persistence
        .assignment_runs
        .get(&assignment_ref.assignment_id, &assignment_ref.run_id)
        .await
    {
        Ok(Some(mut run)) => {
            run.status = AssignmentRunStatus::Succeeded;
            run.output_summary = summary;
            run.finished_ts = Some(chrono::Utc::now());
            if let Err(e) = persistence
                .assignment_runs
                .update(&assignment_ref.assignment_id, &run)
                .await
            {
                tracing::warn!(
                    agent_id = %agent_id,
                    assignment_id = %assignment_ref.assignment_id,
                    run_id = %assignment_ref.run_id,
                    "Failed to persist AssignmentRun Succeeded transition: {}",
                    e
                );
                return;
            }
        }
        Ok(None) => {
            tracing::warn!(
                agent_id = %agent_id,
                assignment_id = %assignment_ref.assignment_id,
                run_id = %assignment_ref.run_id,
                "AssignmentRun row missing when transitioning to Succeeded"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                agent_id = %agent_id,
                assignment_id = %assignment_ref.assignment_id,
                run_id = %assignment_ref.run_id,
                "Failed to load AssignmentRun for Succeeded transition: {}",
                e
            );
            return;
        }
    }
    event_bus
        .emit(
            &format!("assignment:{}", assignment_ref.assignment_id),
            agent_id,
            None,
            AgentEventPayload::SystemMessage {
                text: format!("Assignment run succeeded: {}", assignment_ref.run_id),
                severity: None,
            },
        )
        .await;
}

/// Transition an AssignmentRun row to `Failed`, recording the underlying
/// error text and stamping `finished_ts`. Invoked only from the outer
/// runner-failure watcher — the completion branch owns the success path.
/// Emits a `SystemMessage` on the per-assignment SSE channel for parity
/// with the success emit.
async fn mark_assignment_run_failed(
    persistence: &Arc<PersistenceLayer>,
    event_bus: &Arc<EventBus>,
    agent_id: &AgentId,
    assignment_ref: &AssignmentRunRef,
    error_text: String,
) {
    match persistence
        .assignment_runs
        .get(&assignment_ref.assignment_id, &assignment_ref.run_id)
        .await
    {
        Ok(Some(mut run)) => {
            run.status = AssignmentRunStatus::Failed;
            run.error = Some(error_text.clone());
            run.finished_ts = Some(chrono::Utc::now());
            if let Err(e) = persistence
                .assignment_runs
                .update(&assignment_ref.assignment_id, &run)
                .await
            {
                tracing::warn!(
                    agent_id = %agent_id,
                    assignment_id = %assignment_ref.assignment_id,
                    run_id = %assignment_ref.run_id,
                    "Failed to persist AssignmentRun Failed transition: {}",
                    e
                );
                return;
            }
        }
        Ok(None) => {
            tracing::warn!(
                agent_id = %agent_id,
                assignment_id = %assignment_ref.assignment_id,
                run_id = %assignment_ref.run_id,
                "AssignmentRun row missing when transitioning to Failed"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                agent_id = %agent_id,
                assignment_id = %assignment_ref.assignment_id,
                run_id = %assignment_ref.run_id,
                "Failed to load AssignmentRun for Failed transition: {}",
                e
            );
            return;
        }
    }
    event_bus
        .emit(
            &format!("assignment:{}", assignment_ref.assignment_id),
            agent_id,
            None,
            AgentEventPayload::SystemMessage {
                text: format!("Assignment run failed: {}", assignment_ref.run_id),
                severity: None,
            },
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use ao_protocol::agent::{
        CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };

    // ---------------------------------------------------------------------------
    // Helpers shared by the assignment lifecycle end-to-end test
    // ---------------------------------------------------------------------------

    async fn make_e2e_persistence() -> (Arc<ao_persistence::PersistenceLayer>, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
        let p = ao_persistence::PersistenceLayer::init_with_root(data_root)
            .await
            .expect("init persistence");
        (Arc::new(p), tmp)
    }

    // ---------------------------------------------------------------------------
    // End-to-end test: AssignmentRun lifecycle through the production pump
    // ---------------------------------------------------------------------------

    /// Verify that a real assignment-sourced `QueuedMessage` pumped through the
    /// production `QueueManagerRegistry` + `AgentQueueManager` path advances the
    /// persisted `AssignmentRun` row from `Queued` → `Running` → `Succeeded`
    /// and captures `output_summary`. This test is the acceptance gate for the
    /// lifecycle write-back fix: it must FAIL against any revision that only
    /// pokes `assignment_runs.update` directly in `#[cfg(test)]` code and
    /// PASS only when the production pump owns the transitions.
    #[tokio::test]
    async fn assignment_run_reaches_succeeded_through_production_pump() {
        use std::time::Duration;

        use async_trait::async_trait;
        use chrono::Utc;

        use ao_protocol::assignment::{
            Assignment, AssignmentRunStatus, AssignmentTrigger, AssignmentTriggerKind, OutputMode,
        };

        use ao_protocol::agent::AgentRunnerMode;

        use crate::agent_runner::{AgentRunRequest, AgentRunner, RunComplete, RunnerDispatcher};
        use crate::assignment_runner::fire_assignment;

        // Stub runner: takes whatever pre_run_id the pump pre-allocated,
        // sends RunComplete on the queue manager's channel, and returns the
        // same value so the outer watcher also sees a success.
        struct StubRunner {
            output: String,
        }

        #[async_trait]
        impl AgentRunner for StubRunner {
            fn mode(&self) -> AgentRunnerMode {
                AgentRunnerMode::Cli
            }

            async fn run(&self, req: AgentRunRequest) -> Result<RunComplete, AoError> {
                let run_id = req
                    .pre_registered_run_id
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let rc = RunComplete {
                    run_id: run_id.clone(),
                    output_text: self.output.clone(),
                    workflow_followups: vec![],
                    end_reason: RunEndReason::Completed,
                };
                // Notify the queue manager's completion branch (the production path).
                let _ = req.run_complete_tx.send(rc.clone()).await;
                Ok(rc)
            }
        }

        // --- Setup ---
        let (persistence, _tmp) = make_e2e_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let instance_registry = Arc::new(crate::instance_registry::InstanceRegistry::new());

        let stub = Arc::new(StubRunner {
            output: "assignment-run-output".to_string(),
        });
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(
            Arc::clone(&stub) as Arc<dyn AgentRunner>,
            Arc::clone(&stub) as Arc<dyn AgentRunner>,
        ));

        let registry = Arc::new(QueueManagerRegistry::new(
            dispatcher,
            Arc::clone(&instance_registry),
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
        ));

        // Agent uses Cli mode (default) so the dispatcher picks the stub runner.
        let agent = make_test_agent("agent-pump-e2e", None);
        persistence.agents.create(&agent).await.unwrap();

        let now = Utc::now();
        let assignment = Assignment {
            id: "assign-pump-e2e".to_string(),
            agent_id: "agent-pump-e2e".to_string(),
            name: "Pump E2E".to_string(),
            instruction: "produce a test output".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::Cron {
                cron_expr: "* * * * *".to_string(),
                is_recurring: true,
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: ao_protocol::assignment::AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: Some(now),
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        };
        persistence.assignments.add(assignment.clone()).await.unwrap();

        // Fire the assignment. This creates the Queued row in persistence and
        // submits a MessageSource::Assignment-tagged QueuedMessage to the
        // production QueueManagerRegistry.
        let registry_dispatcher =
            Arc::clone(&registry) as Arc<dyn NotificationDispatcher>;
        let queued_run = fire_assignment(
            &persistence,
            &registry_dispatcher,
            &event_bus,
            &assignment,
            AssignmentTriggerKind::Cron,
            None,
            None,
            None,
        )
        .await
        .expect("fire_assignment must succeed");

        assert_eq!(
            queued_run.status,
            AssignmentRunStatus::Queued,
            "fire_assignment returns Queued status"
        );

        let assignment_id = queued_run.assignment_id.clone();
        let run_id = queued_run.id.clone();

        // Poll until the pump transitions the run past Queued/Running.
        let final_run = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let r = persistence
                    .assignment_runs
                    .get(&assignment_id, &run_id)
                    .await
                    .expect("persistence get must not error")
                    .expect("run row must exist");
                match r.status {
                    AssignmentRunStatus::Queued | AssignmentRunStatus::Running => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    _ => return r,
                }
            }
        })
        .await
        .expect("assignment run must reach a terminal state within 5 seconds");

        // --- Assertions ---
        assert_eq!(
            final_run.status,
            AssignmentRunStatus::Succeeded,
            "pump must write Succeeded through the production completion branch"
        );
        assert!(
            final_run.output_summary.is_some(),
            "output_summary must be populated when the run succeeds"
        );
        assert!(
            final_run
                .output_summary
                .as_deref()
                .unwrap()
                .contains("assignment-run-output"),
            "output_summary must contain the runner's output text; got: {:?}",
            final_run.output_summary
        );
        assert!(
            final_run.finished_ts.is_some(),
            "finished_ts must be stamped on the Succeeded row"
        );
        assert!(
            final_run.started_ts.is_some(),
            "started_ts must be stamped when the run transitions to Running"
        );
    }

    /// Verify that when a runner returns `Err` (simulating a startup failure or
    /// process error), the AssignmentRun row is transitioned to `Failed` — the
    /// run does NOT remain stuck in `Running` forever.
    #[tokio::test]
    async fn assignment_run_reaches_failed_when_runner_errors() {
        use std::time::Duration;

        use async_trait::async_trait;
        use chrono::Utc;

        use ao_protocol::assignment::{
            Assignment, AssignmentRunStatus, AssignmentTrigger, AssignmentTriggerKind, OutputMode,
        };

        use ao_protocol::agent::AgentRunnerMode;

        use crate::agent_runner::{AgentRunRequest, AgentRunner, RunComplete, RunnerDispatcher};
        use crate::assignment_runner::fire_assignment;

        struct FailingRunner;

        #[async_trait]
        impl AgentRunner for FailingRunner {
            fn mode(&self) -> AgentRunnerMode {
                AgentRunnerMode::Cli
            }

            async fn run(&self, _req: AgentRunRequest) -> Result<RunComplete, AoError> {
                Err(AoError::Internal("deliberate runner error in test".to_string()))
            }
        }

        let (persistence, _tmp) = make_e2e_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let instance_registry = Arc::new(crate::instance_registry::InstanceRegistry::new());

        let failing = Arc::new(FailingRunner);
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(
            Arc::clone(&failing) as Arc<dyn AgentRunner>,
            Arc::clone(&failing) as Arc<dyn AgentRunner>,
        ));

        let registry = Arc::new(QueueManagerRegistry::new(
            dispatcher,
            Arc::clone(&instance_registry),
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
        ));

        let agent = make_test_agent("agent-fail-e2e", None);
        persistence.agents.create(&agent).await.unwrap();

        let now = Utc::now();
        let assignment = Assignment {
            id: "assign-fail-e2e".to_string(),
            agent_id: "agent-fail-e2e".to_string(),
            name: "Fail E2E".to_string(),
            instruction: "this will error".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::Cron {
                cron_expr: "* * * * *".to_string(),
                is_recurring: true,
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: ao_protocol::assignment::AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: Some(now),
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        };
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let registry_dispatcher = Arc::clone(&registry) as Arc<dyn NotificationDispatcher>;
        let queued_run = fire_assignment(
            &persistence,
            &registry_dispatcher,
            &event_bus,
            &assignment,
            AssignmentTriggerKind::Cron,
            None,
            None,
            None,
        )
        .await
        .expect("fire_assignment must succeed");

        let assignment_id = queued_run.assignment_id.clone();
        let run_id = queued_run.id.clone();

        let final_run = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let r = persistence
                    .assignment_runs
                    .get(&assignment_id, &run_id)
                    .await
                    .expect("persistence get must not error")
                    .expect("run row must exist");
                match r.status {
                    AssignmentRunStatus::Queued | AssignmentRunStatus::Running => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    _ => return r,
                }
            }
        })
        .await
        .expect("failed run must reach a terminal state within 5 seconds");

        assert_eq!(
            final_run.status,
            AssignmentRunStatus::Failed,
            "runner error must drive the run to Failed, not leave it stuck in Running"
        );
        assert!(
            final_run.finished_ts.is_some(),
            "finished_ts must be set even on Failed runs"
        );
        assert!(
            final_run.error.is_some(),
            "error field must be populated on Failed runs"
        );
    }

    /// Verify that interactive serialization is scoped per thread, not per
    /// agent: two different threads on the same `serialize: true` agent must
    /// run concurrently, while a second message on the *same* thread as an
    /// in-flight run stays queued until that run completes.
    #[tokio::test]
    async fn interactive_serialization_is_scoped_per_thread() {
        use std::collections::VecDeque as StdVecDeque;
        use std::time::Duration;

        use async_trait::async_trait;
        use chrono::Utc;
        use tokio::sync::oneshot;

        use ao_protocol::agent::AgentRunnerMode;

        use crate::agent_runner::{AgentRunRequest, AgentRunner, RunComplete, RunnerDispatcher};

        // A runner that records the thread_id of every dispatched run (in
        // dispatch order) and then blocks until the test explicitly releases
        // it via a per-call oneshot gate, so the test can observe exactly
        // which runs are in flight at each point without racing real work.
        struct GatedRunner {
            dispatched: Arc<Mutex<Vec<Option<String>>>>,
            gates: Arc<Mutex<StdVecDeque<oneshot::Sender<()>>>>,
        }

        #[async_trait]
        impl AgentRunner for GatedRunner {
            fn mode(&self) -> AgentRunnerMode {
                AgentRunnerMode::Cli
            }

            async fn run(&self, req: AgentRunRequest) -> Result<RunComplete, AoError> {
                let (tx, rx) = oneshot::channel();
                self.dispatched.lock().await.push(req.thread_id.clone());
                self.gates.lock().await.push_back(tx);
                let _ = rx.await;

                let run_id = req
                    .pre_registered_run_id
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let rc = RunComplete {
                    run_id: run_id.clone(),
                    output_text: "ok".to_string(),
                    workflow_followups: vec![],
                    end_reason: RunEndReason::Completed,
                };
                let _ = req.run_complete_tx.send(rc.clone()).await;
                Ok(rc)
            }
        }

        async fn wait_for_len(
            dispatched: &Arc<Mutex<Vec<Option<String>>>>,
            expected: usize,
        ) -> Vec<Option<String>> {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let snapshot = dispatched.lock().await.clone();
                    if snapshot.len() >= expected {
                        return snapshot;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("dispatch count did not reach expected length in time")
        }

        let (persistence, _tmp) = make_e2e_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let instance_registry = Arc::new(crate::instance_registry::InstanceRegistry::new());

        let dispatched = Arc::new(Mutex::new(Vec::new()));
        let gates = Arc::new(Mutex::new(StdVecDeque::new()));
        let runner = Arc::new(GatedRunner {
            dispatched: Arc::clone(&dispatched),
            gates: Arc::clone(&gates),
        });
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(
            Arc::clone(&runner) as Arc<dyn AgentRunner>,
            Arc::clone(&runner) as Arc<dyn AgentRunner>,
        ));

        let registry = Arc::new(QueueManagerRegistry::new(
            dispatcher,
            Arc::clone(&instance_registry),
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
        ));

        // serialize: true (from make_test_agent) with enough max_instances
        // headroom that only the thread-scoped gate — not the instance cap —
        // is under test here.
        let mut agent = make_test_agent("agent-thread-serialize", None);
        agent.max_instances = 4;
        persistence.agents.create(&agent).await.unwrap();

        let mk_message = |content: &str, thread_id: Option<&str>| QueuedMessage {
            message_id: uuid::Uuid::new_v4().to_string(),
            content: content.to_string(),
            queued_at: Utc::now(),
            attachments: vec![],
            source: None,
            focus_path: None,
            thread_id: thread_id.map(str::to_string),
        };

        // 1. First message on thread A dispatches immediately.
        registry
            .submit_message(&agent, mk_message("a1", Some("thread-a")))
            .await
            .unwrap();
        let after_a1 = wait_for_len(&dispatched, 1).await;
        assert_eq!(after_a1, vec![Some("thread-a".to_string())]);

        // 2. A second message on the SAME thread stays queued while the
        // first is in flight — the thread-scoped serialization gate holds it.
        registry
            .submit_message(&agent, mk_message("a2", Some("thread-a")))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            dispatched.lock().await.len(),
            1,
            "second message on thread-a must stay queued behind the in-flight thread-a run"
        );

        // 3. A message on a DIFFERENT thread dispatches concurrently — it is
        // not blocked by thread-a's in-flight run.
        registry
            .submit_message(&agent, mk_message("b1", Some("thread-b")))
            .await
            .unwrap();
        let after_b1 = wait_for_len(&dispatched, 2).await;
        assert_eq!(
            after_b1,
            vec![Some("thread-a".to_string()), Some("thread-b".to_string())],
            "thread-b's message must dispatch without waiting on thread-a"
        );

        // 4. Release thread-a's first run. Its queued second message should
        // now dispatch, proving the gate cleared specifically because
        // thread-a's own run completed (not merely because some run did).
        {
            let mut g = gates.lock().await;
            let tx = g.pop_front().expect("thread-a run1 gate must exist");
            let _ = tx.send(());
        }
        let after_a2 = wait_for_len(&dispatched, 3).await;
        assert_eq!(
            after_a2,
            vec![
                Some("thread-a".to_string()),
                Some("thread-b".to_string()),
                Some("thread-a".to_string()),
            ],
            "thread-a's second message must dispatch only after thread-a's first run completes"
        );

        // Cleanup: release any remaining gates so no task is left blocked.
        {
            let mut g = gates.lock().await;
            while let Some(tx) = g.pop_front() {
                let _ = tx.send(());
            }
        }
    }

    /// Covers the correctness gap introduced by `Assignment`'s `Main`/
    /// `Dedicated` thread policies: before those existed, an assignment's
    /// thread_id was always freshly minted and could never collide with
    /// anything in flight, so no guard was needed against
    /// assignment-vs-interactive or assignment-vs-assignment races on the
    /// same thread. Exercises all three new collision directions plus one
    /// non-collision control, reusing the `GatedRunner` harness above.
    #[tokio::test]
    async fn cross_source_thread_collision_guard_blocks_regardless_of_source() {
        use std::collections::VecDeque as StdVecDeque;
        use std::time::Duration;

        use async_trait::async_trait;
        use chrono::Utc;
        use tokio::sync::oneshot;

        use ao_protocol::agent::AgentRunnerMode;

        use crate::agent_runner::{AgentRunRequest, AgentRunner, RunComplete, RunnerDispatcher};

        struct GatedRunner {
            dispatched: Arc<Mutex<Vec<Option<String>>>>,
            gates: Arc<Mutex<StdVecDeque<oneshot::Sender<()>>>>,
        }

        #[async_trait]
        impl AgentRunner for GatedRunner {
            fn mode(&self) -> AgentRunnerMode {
                AgentRunnerMode::Cli
            }

            async fn run(&self, req: AgentRunRequest) -> Result<RunComplete, AoError> {
                let (tx, rx) = oneshot::channel();
                self.dispatched.lock().await.push(req.thread_id.clone());
                self.gates.lock().await.push_back(tx);
                let _ = rx.await;

                let run_id = req
                    .pre_registered_run_id
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let rc = RunComplete {
                    run_id: run_id.clone(),
                    output_text: "ok".to_string(),
                    workflow_followups: vec![],
                    end_reason: RunEndReason::Completed,
                };
                let _ = req.run_complete_tx.send(rc.clone()).await;
                Ok(rc)
            }
        }

        async fn wait_for_len(
            dispatched: &Arc<Mutex<Vec<Option<String>>>>,
            expected: usize,
        ) -> Vec<Option<String>> {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let snapshot = dispatched.lock().await.clone();
                    if snapshot.len() >= expected {
                        return snapshot;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("dispatch count did not reach expected length in time")
        }

        async fn release_one(gates: &Arc<Mutex<StdVecDeque<oneshot::Sender<()>>>>) {
            let mut g = gates.lock().await;
            let tx = g.pop_front().expect("a gate must exist to release");
            let _ = tx.send(());
        }

        let (persistence, _tmp) = make_e2e_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let instance_registry = Arc::new(crate::instance_registry::InstanceRegistry::new());

        let dispatched = Arc::new(Mutex::new(Vec::new()));
        let gates = Arc::new(Mutex::new(StdVecDeque::new()));
        let runner = Arc::new(GatedRunner {
            dispatched: Arc::clone(&dispatched),
            gates: Arc::clone(&gates),
        });
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(
            Arc::clone(&runner) as Arc<dyn AgentRunner>,
            Arc::clone(&runner) as Arc<dyn AgentRunner>,
        ));

        let registry = Arc::new(QueueManagerRegistry::new(
            dispatcher,
            Arc::clone(&instance_registry),
            Arc::clone(&event_bus),
            Arc::clone(&persistence),
        ));

        // Plenty of instance headroom so only the thread-collision guard —
        // never max_instances — explains any queued message in this test.
        let mut agent = make_test_agent("agent-cross-source-collision", None);
        agent.max_instances = 4;
        persistence.agents.create(&agent).await.unwrap();

        let mk_interactive = |content: &str, thread_id: Option<&str>| QueuedMessage {
            message_id: uuid::Uuid::new_v4().to_string(),
            content: content.to_string(),
            queued_at: Utc::now(),
            attachments: vec![],
            source: None,
            focus_path: None,
            thread_id: thread_id.map(str::to_string),
        };
        let mk_assignment = |content: &str, thread_id: Option<&str>, run_id: &str| QueuedMessage {
            message_id: uuid::Uuid::new_v4().to_string(),
            content: content.to_string(),
            queued_at: Utc::now(),
            attachments: vec![],
            source: Some(MessageSource::Assignment {
                assignment_id: "assign-collision-test".to_string(),
                run_id: run_id.to_string(),
                trigger_kind: "cron".to_string(),
            }),
            focus_path: None,
            thread_id: thread_id.map(str::to_string),
        };

        // (a) An assignment run is in flight on "shared". A later INTERACTIVE
        // message for the same thread must stay queued — this is exactly the
        // "Main-policy assignment fires mid-conversation" scenario.
        registry
            .submit_message(&agent, mk_assignment("assignment-1", Some("shared"), "run-1"))
            .await
            .unwrap();
        wait_for_len(&dispatched, 1).await;

        registry
            .submit_message(&agent, mk_interactive("user-1", Some("shared")))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            dispatched.lock().await.len(),
            1,
            "an interactive message must not dispatch onto a thread an in-flight assignment run holds"
        );

        // (b) A second, different assignment message for "shared" must also
        // stay queued — two assignments (or a webhook burst) must never race
        // the same thread.
        registry
            .submit_message(&agent, mk_assignment("assignment-2", Some("shared"), "run-2"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            dispatched.lock().await.len(),
            1,
            "a second assignment message must not dispatch onto a thread another in-flight assignment run holds"
        );

        // Control: a message on an unrelated thread is never blocked by any
        // of the above.
        registry
            .submit_message(&agent, mk_interactive("user-other", Some("unrelated")))
            .await
            .unwrap();
        let after_unrelated = wait_for_len(&dispatched, 2).await;
        assert_eq!(
            after_unrelated,
            vec![Some("shared".to_string()), Some("unrelated".to_string())],
            "a message on a different thread must dispatch without waiting"
        );

        // Release "shared"'s in-flight assignment run. Exactly one of its two
        // queued same-thread messages may now dispatch.
        release_one(&gates).await;
        let after_release = wait_for_len(&dispatched, 3).await;
        assert_eq!(after_release[2], Some("shared".to_string()));

        // (c) Symmetric direction: with an INTERACTIVE run now in flight on
        // "shared", a fresh assignment-sourced message for "shared" must also
        // stay queued.
        registry
            .submit_message(&agent, mk_assignment("assignment-3", Some("shared"), "run-3"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            dispatched.lock().await.len(),
            3,
            "an assignment message must not dispatch onto a thread an in-flight interactive run holds"
        );

        // Cleanup: release any remaining gates so no task is left blocked.
        {
            let mut g = gates.lock().await;
            while let Some(tx) = g.pop_front() {
                let _ = tx.send(());
            }
        }
    }

    fn make_test_agent(id: &str, template: Option<&str>) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Test Agent {}", id),
            description: String::new(),
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
                no_output_timeout_ms: 30_000,
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
            template: template.map(str::to_string),
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

    #[tokio::test]
    async fn wake_on_deliver_enrolls_copilot_agent() {
        // An inbound delivery to a dormant co-pilot adds it to the
        // enrolled set so the next pump tick processes the queued message.
        let enrolled = EnrolledCopilots::new();
        let agent = make_test_agent("copilot-A", Some(COPILOT_PROFILE_ID));

        assert!(!enrolled.is_enrolled("copilot-A").await);
        let added = wake_copilot_on_deliver(&agent, &enrolled).await;
        assert!(added, "first delivery should newly enroll the co-pilot");
        assert!(enrolled.is_enrolled("copilot-A").await);
    }

    #[tokio::test]
    async fn wake_on_deliver_is_idempotent_for_already_enrolled_copilot() {
        // Repeat deliveries to an already-enrolled co-pilot are
        // no-ops at the enrollment layer (returns false). The message itself
        // is still delivered by `submit_message` regardless.
        let enrolled = EnrolledCopilots::new();
        let agent = make_test_agent("copilot-A", Some(COPILOT_PROFILE_ID));

        wake_copilot_on_deliver(&agent, &enrolled).await;
        let second = wake_copilot_on_deliver(&agent, &enrolled).await;
        let third = wake_copilot_on_deliver(&agent, &enrolled).await;

        assert!(!second, "repeat delivery should not re-add");
        assert!(!third, "repeat delivery should not re-add");
        assert_eq!(enrolled.len().await, 1, "enrolled set stays singular");
    }

    #[tokio::test]
    async fn wake_on_deliver_skips_non_copilot_agents() {
        // No infinite-wake loop, and no non-co-pilot side effects:
        // a delivery to a regular agent (no template / different template)
        // must NOT enroll it — the enrolled set is co-pilot-only state.
        let enrolled = EnrolledCopilots::new();

        let plain = make_test_agent("plain-1", None);
        let templated = make_test_agent("other-1", Some("not-a-copilot"));

        assert!(!wake_copilot_on_deliver(&plain, &enrolled).await);
        assert!(!wake_copilot_on_deliver(&templated, &enrolled).await);
        assert_eq!(enrolled.len().await, 0);
        assert!(!enrolled.is_enrolled("plain-1").await);
        assert!(!enrolled.is_enrolled("other-1").await);
    }
}
