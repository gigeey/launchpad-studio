use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing;

use ao_persistence::PersistenceLayer;
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::workflow::{PhaseStatus, TaskStatus};

use crate::event_bus::EventBus;
use crate::queue_manager::QueueManagerRegistry;
use crate::sleep_guard::SleepGuard;
use crate::workflow_runner::WorkflowRunner;

/// Messages sent to the WorkflowQueueManager to control task execution.
#[derive(Debug, Clone)]
pub enum WfQueueMsg {
    /// Start executing a task (enters ready queue).
    StartTask { task_id: String },
    /// A phase completed — advance to the next phase.
    PhaseCompleted { task_id: String, phase_id: String },
    /// A phase failed — mark the task as failed.
    PhaseFailed {
        task_id: String,
        phase_id: String,
        error: String,
    },
    /// Resume a paused task — clear paused phase status and re-queue.
    ResumeTask { task_id: String },
}

/// Info about a currently running task (executing a folder phase). Stored in
/// `running_tasks` keyed by task id, so the id is not repeated in the value.
#[derive(Debug, Clone)]
struct RunningTaskInfo {
    working_directory: Option<String>,
}

/// Handle used to send messages to the WorkflowQueueManager.
#[derive(Clone)]
pub struct WorkflowQueueHandle {
    pub tx: mpsc::Sender<WfQueueMsg>,
}

impl WorkflowQueueHandle {
    pub async fn send(&self, msg: WfQueueMsg) -> Result<(), ao_protocol::error::AoError> {
        self.tx.send(msg).await.map_err(|e| {
            ao_protocol::error::AoError::Internal(format!("Workflow queue send error: {}", e))
        })
    }
}

/// Queue-based workflow task executor.
///
/// Manages a ready queue and a backoff queue. Tasks in the ready queue are
/// executed immediately unless their working directory conflicts with a
/// currently running task, in which case they are moved to the backoff queue.
/// A heartbeat timer periodically promotes tasks from backoff back to ready.
pub struct WorkflowQueueManager {
    ready_queue: VecDeque<String>,
    backoff_queue: VecDeque<String>,
    running_tasks: HashMap<String, RunningTaskInfo>,
    task_rx: mpsc::Receiver<WfQueueMsg>,
    internal_tx: mpsc::Sender<WfQueueMsg>,
    workflow_runner: Arc<WorkflowRunner>,
    event_bus: Arc<EventBus>,
    /// Optional reference to the per-agent queue manager registry.
    /// Used to clean up synthetic phase agent queue managers when phases/tasks finish.
    queue_manager_registry: Option<Arc<QueueManagerRegistry>>,
    /// Optional transcript store for writing cold-start entries to the agent transcript.
    transcript_store: Option<ao_persistence::transcript::TranscriptStore>,
    /// Optional persistence handle used to read the sleep guard preference.
    persistence: Option<Arc<PersistenceLayer>>,
    /// Holds the system awake while any workflow task is active.
    sleep_guard: SleepGuard,
}

impl WorkflowQueueManager {
    pub fn new(
        task_rx: mpsc::Receiver<WfQueueMsg>,
        internal_tx: mpsc::Sender<WfQueueMsg>,
        workflow_runner: Arc<WorkflowRunner>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            ready_queue: VecDeque::new(),
            backoff_queue: VecDeque::new(),
            running_tasks: HashMap::new(),
            task_rx,
            internal_tx,
            workflow_runner,
            event_bus,
            queue_manager_registry: None,
            transcript_store: None,
            persistence: None,
            sleep_guard: SleepGuard::new(1.0),
        }
    }

    /// Set the persistence handle so the queue manager can read the
    /// `prevent_sleep_during_workflows` preference.
    /// Must be called before `run()`.
    pub fn set_persistence(&mut self, persistence: Arc<PersistenceLayer>) {
        self.persistence = Some(persistence);
    }

    /// True when any workflow task is running, ready, or in backoff.
    fn has_active_workflows(&self) -> bool {
        !self.running_tasks.is_empty()
            || !self.ready_queue.is_empty()
            || !self.backoff_queue.is_empty()
    }

    /// Refresh the sleep guard based on the user preference and whether any
    /// workflow task is active.
    async fn refresh_sleep_guard(&mut self) {
        // `enabled` defaults to false when there's no persistence handle at
        // all (guard can't be meaningfully driven), but true when there is a
        // handle and the preference just failed to load or isn't set yet.
        let (enabled, keep_display_awake) = match self.persistence {
            Some(ref p) => {
                let prefs = p.preferences.get().await.ok().flatten();
                (
                    prefs.as_ref().map(|prefs| prefs.prevent_sleep_during_workflows).unwrap_or(true),
                    prefs.map(|prefs| prefs.keep_display_awake).unwrap_or(false),
                )
            }
            None => (false, false),
        };
        self.sleep_guard.set_disabled(!enabled);
        self.sleep_guard.set_keep_display_awake(keep_display_awake);

        self.sleep_guard.update_active(self.has_active_workflows());
    }

    /// Set the per-agent queue manager registry for cleanup of synthetic phase agents.
    /// Must be called before `run()`.
    pub fn set_queue_manager_registry(&mut self, registry: Arc<QueueManagerRegistry>) {
        self.queue_manager_registry = Some(registry);
    }

    /// Set the transcript store for writing cold-start entries.
    /// Must be called before `run()`.
    pub fn set_transcript_store(&mut self, store: ao_persistence::transcript::TranscriptStore) {
        self.transcript_store = Some(store);
    }

    /// Remove the synthetic agent queue manager for a completed/failed phase.
    async fn cleanup_phase_agent(&self, task_id: &str, phase_id: &str) {
        if let Some(ref registry) = self.queue_manager_registry {
            let agent_id = format!("task:{}:phase:{}", task_id, phase_id);
            registry.remove_agent(&agent_id).await;
        }
    }

    /// Remove all synthetic agent queue managers for every phase in a task.
    async fn cleanup_all_phase_agents(&self, task_id: &str) {
        if let Some(ref registry) = self.queue_manager_registry {
            if let Ok(snapshot) = self.workflow_runner.get_task_state(task_id).await {
                for phase_id in snapshot.phases.keys() {
                    let agent_id = format!("task:{}:phase:{}", task_id, phase_id);
                    registry.remove_agent(&agent_id).await;
                }
            }
        }
    }

    /// Backend-driven cold start for an agent phase.
    /// Builds the synthetic agent profile, writes the cold-start transcript entry,
    /// and submits the initial message to the queue manager — all without frontend involvement.
    async fn cold_start_agent_phase(
        &self,
        task_id: &str,
        phase_id: &str,
        phase_name: &str,
        context: &str,
        working_dir: Option<&str>,
        workflow_id: &str,
    ) {
        let registry = match self.queue_manager_registry {
            Some(ref r) => r,
            None => {
                tracing::warn!(
                    task_id = %task_id,
                    phase_id = %phase_id,
                    "Cannot cold-start agent phase: queue manager registry not set"
                );
                return;
            }
        };

        let agent = build_phase_agent(task_id, phase_id, context, working_dir, workflow_id);

        let message_id = uuid::Uuid::new_v4().to_string();
        let cold_start_content = format!(
            "Begin working on phase '{}'. Follow the system prompt instructions. \
             If this phase requires user interaction (like an interview), start by \
             greeting the user and asking your first question.",
            phase_name
        );

        // Write cold-start transcript entry to the phase message log
        let system_entry = ao_protocol::transcript::TranscriptEntry {
            ts: chrono::Utc::now(),
            role: ao_protocol::transcript::TranscriptRole::System("system".to_string()),
            content: cold_start_content.clone(),
            event_type: "cold_start".to_string(),
            metadata: Some({
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "message_id".to_string(),
                    serde_json::Value::String(message_id.clone()),
                );
                m
            }),
            hidden_from_user: false,
        };

        if let Err(e) = self
            .workflow_runner
            .append_phase_message(task_id, phase_id, &system_entry)
            .await
        {
            tracing::error!(
                task_id = %task_id,
                phase_id = %phase_id,
                "Failed to write cold-start phase message: {}",
                e
            );
            return;
        }

        // Also write to the agent transcript store so get_phase_messages can read it
        if let Some(ref store) = self.transcript_store {
            let agent_id = phase_agent_id(task_id, phase_id);
            if let Err(e) = store.append(&agent_id, &system_entry).await {
                tracing::error!(
                    task_id = %task_id,
                    phase_id = %phase_id,
                    "Failed to write cold-start agent transcript: {}",
                    e
                );
                return;
            }
        }

        // Submit the cold-start message to the queue manager
        let queued = ao_protocol::message::QueuedMessage {
            message_id: message_id.clone(),
            content: cold_start_content,
            queued_at: chrono::Utc::now(),
            attachments: vec![],
            source: None,
            focus_path: None,
            thread_id: None,
        };

        if let Err(e) = registry.submit_message(&agent, queued).await {
            tracing::error!(
                task_id = %task_id,
                phase_id = %phase_id,
                "Failed to submit cold-start message to queue manager: {}",
                e
            );
        } else {
            tracing::info!(
                task_id = %task_id,
                phase_id = %phase_id,
                "Backend cold-started agent phase"
            );
        }
    }

    /// Recover tasks that were Running when the process crashed.
    /// Re-queues them into the ready queue for retry.
    pub async fn recover_running_tasks(&mut self) {
        let task_ids = match self.workflow_runner.list_task_ids().await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!("Failed to list tasks for crash recovery: {}", e);
                return;
            }
        };

        for task_id in task_ids {
            match self.workflow_runner.get_task_state(&task_id).await {
                Ok(snapshot) => {
                    if snapshot.status == TaskStatus::Running {
                        tracing::info!(
                            task_id = %task_id,
                            "Recovering running task from crash"
                        );
                        self.ready_queue.push_back(task_id);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        "Failed to read task snapshot during recovery: {}",
                        e
                    );
                }
            }
        }

        if !self.ready_queue.is_empty() {
            tracing::info!(
                count = self.ready_queue.len(),
                "Recovered running tasks into ready queue"
            );
        }
    }

    /// Main event loop.
    pub async fn run(mut self) {
        // Crash recovery: re-queue any tasks that were Running
        self.recover_running_tasks().await;

        // Pump after recovery
        self.pump().await;

        let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
        // Don't fire immediately
        heartbeat.tick().await;

        loop {
            tokio::select! {
                msg = self.task_rx.recv() => {
                    match msg {
                        Some(WfQueueMsg::StartTask { task_id }) => {
                            tracing::debug!(task_id = %task_id, "StartTask received");
                            self.ready_queue.push_back(task_id);
                            self.pump().await;
                        }
                        Some(WfQueueMsg::PhaseCompleted { task_id, phase_id }) => {
                            tracing::debug!(
                                task_id = %task_id,
                                phase_id = %phase_id,
                                "PhaseCompleted received"
                            );
                            self.running_tasks.remove(&task_id);
                            self.cleanup_phase_agent(&task_id, &phase_id).await;
                            // Only advance if the task is in Running state.
                            // Pending tasks may have phases pre-filled by the agent
                            // but should not auto-advance until explicitly started.
                            let should_advance = match self.workflow_runner.get_task_state(&task_id).await {
                                Ok(snapshot) => snapshot.status == TaskStatus::Running,
                                Err(_) => false,
                            };
                            if should_advance {
                                self.ready_queue.push_back(task_id);
                                self.pump().await;
                            } else {
                                tracing::debug!(
                                    task_id = %task_id,
                                    "Task not running — skipping auto-advance after phase completion"
                                );
                            }
                        }
                        Some(WfQueueMsg::PhaseFailed { task_id, phase_id, error }) => {
                            tracing::error!(
                                task_id = %task_id,
                                phase_id = %phase_id,
                                error = %error,
                                "PhaseFailed received"
                            );
                            self.running_tasks.remove(&task_id);
                            self.cleanup_phase_agent(&task_id, &phase_id).await;
                            // Mark task as failed
                            self.mark_task_failed(&task_id).await;
                        }
                        Some(WfQueueMsg::ResumeTask { task_id }) => {
                            tracing::debug!(task_id = %task_id, "ResumeTask received");

                            // Check if this is a stopped task (vs paused)
                            let is_stopped = match self.workflow_runner.get_task_state(&task_id).await {
                                Ok(snap) => matches!(snap.status, ao_protocol::workflow::TaskStatus::Stopped),
                                Err(_) => false,
                            };

                            if is_stopped {
                                // For stopped tasks: clear the stopped phase so it replays from scratch
                                match self.workflow_runner.clear_stopped_phases(&task_id).await {
                                    Ok(_) => {
                                        // Emit WorkflowTaskStarted so frontend knows the task is running again
                                        self.workflow_runner.emit_task_started(&task_id).await;
                                        self.ready_queue.push_back(task_id);
                                        self.pump().await;
                                    }
                                    Err(e) => {
                                        tracing::error!(task_id = %task_id, "Failed to clear stopped phases: {}", e);
                                    }
                                }
                            } else {
                                // For pause-type phases, mark them as completed instead
                                // of removing them, so get_next_phase advances past them.
                                if let Err(e) = self.complete_paused_gate_phases(&task_id).await {
                                    tracing::error!(task_id = %task_id, "Failed to handle resume: {}", e);
                                } else {
                                    match self.workflow_runner.clear_paused_phases(&task_id).await {
                                        Ok(true) | Ok(false) => {
                                            self.ready_queue.push_back(task_id);
                                            self.pump().await;
                                        }
                                        Err(e) => {
                                            tracing::error!(task_id = %task_id, "Failed to clear paused phases: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                        None => {
                            tracing::debug!("Workflow queue manager shutting down (channel closed)");
                            break;
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    tracing::debug!(
                        ready = self.ready_queue.len(),
                        backoff = self.backoff_queue.len(),
                        running = self.running_tasks.len(),
                        "Workflow queue heartbeat"
                    );
                    self.promote_backoff();
                    self.pump().await;
                }
            }
        }
    }

    /// Try to dispatch tasks from the ready queue.
    async fn pump(&mut self) {
        let mut requeue = VecDeque::new();

        while let Some(task_id) = self.ready_queue.pop_front() {
            // Get the next phase for this task
            let next_phase = match self.workflow_runner.get_next_phase(&task_id).await {
                Ok(Some(phase)) => phase,
                Ok(None) => {
                    // All phases done — mark task completed
                    self.mark_task_completed(&task_id).await;
                    continue;
                }
                Err(e) => {
                    tracing::error!(
                        task_id = %task_id,
                        error = %e,
                        "Failed to get next phase"
                    );
                    self.mark_task_failed(&task_id).await;
                    continue;
                }
            };

            // Get snapshot for working directory info
            let snapshot = match self.workflow_runner.get_task_state(&task_id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(task_id = %task_id, "Failed to read snapshot: {}", e);
                    self.mark_task_failed(&task_id).await;
                    continue;
                }
            };

            let workflow_id = snapshot.workflow.clone();

            // Check if all declared inputs are available before executing
            if !next_phase.inputs.is_empty() {
                match self
                    .workflow_runner
                    .check_inputs_available(&task_id, &next_phase)
                    .await
                {
                    Ok(missing) if !missing.is_empty() => {
                        let reason = format!(
                            "Missing required inputs: {}",
                            missing.join(", ")
                        );
                        tracing::warn!(
                            task_id = %task_id,
                            phase_id = %next_phase.id,
                            reason = %reason,
                            "Pausing phase — inputs not available"
                        );
                        if let Err(e) = self
                            .workflow_runner
                            .pause_phase(&task_id, &next_phase.id, &reason)
                            .await
                        {
                            tracing::error!(
                                task_id = %task_id,
                                "Failed to pause phase: {}",
                                e
                            );
                        }
                        continue;
                    }
                    Err(e) => {
                        tracing::error!(
                            task_id = %task_id,
                            "Failed to check inputs: {}",
                            e
                        );
                        self.mark_task_failed(&task_id).await;
                        continue;
                    }
                    _ => {} // All inputs available, proceed
                }
            }

            // Check if this phase should pause
            let phase_type = next_phase.phase_type.unwrap_or(
                if self.workflow_runner.is_folder_phase(&workflow_id, &next_phase).await {
                    ao_protocol::workflow::PhaseType::Folder
                } else {
                    ao_protocol::workflow::PhaseType::Prompt
                }
            );

            if phase_type == ao_protocol::workflow::PhaseType::Pause
                || phase_type == ao_protocol::workflow::PhaseType::Input
            {
                // Pause phases require explicit user approval to advance
                let reason = format!("Phase '{}' requires approval to continue", next_phase.name);
                tracing::info!(
                    task_id = %task_id,
                    phase_id = %next_phase.id,
                    "Pause phase — waiting for user approval"
                );
                if let Err(e) = self.workflow_runner.start_phase(&task_id, &next_phase.id).await {
                    tracing::error!(task_id = %task_id, "Failed to start pause phase: {}", e);
                }
                if let Err(e) = self.workflow_runner.pause_phase(&task_id, &next_phase.id, &reason).await {
                    tracing::error!(task_id = %task_id, "Failed to pause phase: {}", e);
                }
                self.event_bus
                    .emit(
                        &task_id,
                        &format!("workflow:{}", workflow_id),
                        None,
                        AgentEventPayload::PhasePaused {
                            task_id: task_id.clone(),
                            phase_id: next_phase.id.clone(),
                            reason: reason.clone(),
                        },
                    )
                    .await;
                continue;
            }

            // Check if the previous phase had auto_advance: false
            if !next_phase.auto_advance {
                // This phase opted out of auto-advance — pause before it starts
                let reason = format!("Phase '{}' requires manual approval (auto_advance: false)", next_phase.name);
                tracing::info!(
                    task_id = %task_id,
                    phase_id = %next_phase.id,
                    "Auto-advance disabled — pausing before phase"
                );
                if let Err(e) = self.workflow_runner.start_phase(&task_id, &next_phase.id).await {
                    tracing::error!(task_id = %task_id, "Failed to start phase: {}", e);
                }
                if let Err(e) = self.workflow_runner.pause_phase(&task_id, &next_phase.id, &reason).await {
                    tracing::error!(task_id = %task_id, "Failed to pause phase: {}", e);
                }
                self.event_bus
                    .emit(
                        &task_id,
                        &format!("workflow:{}", workflow_id),
                        None,
                        AgentEventPayload::PhasePaused {
                            task_id: task_id.clone(),
                            phase_id: next_phase.id.clone(),
                            reason: reason.clone(),
                        },
                    )
                    .await;
                continue;
            }

            // Check if this is a folder phase
            if phase_type == ao_protocol::workflow::PhaseType::Folder {
                // Check for directory conflict
                if self.has_directory_conflict(&snapshot.working_directory) {
                    tracing::debug!(
                        task_id = %task_id,
                        "Directory conflict — moving to backoff queue"
                    );
                    self.backoff_queue.push_back(task_id);
                    continue;
                }

                // Emit PhaseStarted is handled inside execute_folder_phase
                let info = RunningTaskInfo {
                    working_directory: snapshot.working_directory.clone(),
                };
                self.running_tasks.insert(task_id.clone(), info);

                // Spawn folder phase execution
                let runner = Arc::clone(&self.workflow_runner);
                let internal_tx = self.internal_tx.clone();
                let phase = next_phase.clone();
                let tid = task_id.clone();

                tokio::spawn(async move {
                    match runner.execute_folder_phase(&tid, &phase).await {
                        Ok(()) => {
                            let _ = internal_tx
                                .send(WfQueueMsg::PhaseCompleted {
                                    task_id: tid.clone(),
                                    phase_id: phase.id.clone(),
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = internal_tx
                                .send(WfQueueMsg::PhaseFailed {
                                    task_id: tid.clone(),
                                    phase_id: phase.id.clone(),
                                    error: e.to_string(),
                                })
                                .await;
                        }
                    }
                });
            } else {
                // Agent phase — mark as Running (agent runner will handle completion
                // via PhaseCompleted message from process_workflow_action)
                let info = RunningTaskInfo {
                    working_directory: snapshot.working_directory.clone(),
                };
                self.running_tasks.insert(task_id.clone(), info);

                // Build context for the agent phase
                match self
                    .workflow_runner
                    .build_phase_context(&task_id, &next_phase)
                    .await
                {
                    Ok(context) => {
                        // Persist the phase as running in the snapshot
                        if let Err(e) = self
                            .workflow_runner
                            .start_phase(&task_id, &next_phase.id)
                            .await
                        {
                            tracing::error!(
                                task_id = %task_id,
                                phase_id = %next_phase.id,
                                "Failed to mark phase as running: {}",
                                e
                            );
                        }

                        // Emit PhaseStarted for the agent phase
                        self.event_bus
                            .emit(
                                &task_id,
                                &format!("workflow:{}", workflow_id),
                                None,
                                AgentEventPayload::PhaseStarted {
                                    task_id: task_id.clone(),
                                    phase_id: next_phase.id.clone(),
                                    phase_name: next_phase.name.clone(),
                                },
                            )
                            .await;

                        // Auto cold-start the agent phase from the backend.
                        // This submits the initial message so the agent begins
                        // working immediately without waiting for the frontend.
                        self.cold_start_agent_phase(
                            &task_id,
                            &next_phase.id,
                            &next_phase.name,
                            &context,
                            snapshot.working_directory.as_deref(),
                            &workflow_id,
                        )
                        .await;
                    }
                    Err(e) => {
                        tracing::error!(
                            task_id = %task_id,
                            phase_id = %next_phase.id,
                            "Failed to build phase context: {}",
                            e
                        );
                        self.running_tasks.remove(&task_id);
                        self.mark_task_failed(&task_id).await;
                    }
                }
            }
        }

        // Put back any tasks that couldn't be dispatched
        while let Some(task_id) = requeue.pop_front() {
            self.ready_queue.push_back(task_id);
        }

        self.refresh_sleep_guard().await;
    }

    /// Check if any running task shares the same working directory.
    fn has_directory_conflict(&self, working_directory: &Option<String>) -> bool {
        let Some(ref wd) = working_directory else {
            return false; // No working directory = no conflict
        };

        self.running_tasks
            .values()
            .any(|info| info.working_directory.as_ref() == Some(wd))
    }

    /// Move tasks from backoff queue to ready queue when their working
    /// directory is no longer in use by running tasks.
    fn promote_backoff(&mut self) {
        while let Some(task_id) = self.backoff_queue.pop_front() {
            // Move all backoff tasks to ready and let pump() re-check for conflicts.
            self.ready_queue.push_back(task_id);
        }
    }

    /// Mark a task as completed (all phases done).
    /// For pause/gate-type phases that are paused, mark them as completed
    /// so get_next_phase advances past them on resume.
    async fn complete_paused_gate_phases(&self, task_id: &str) -> Result<(), AoError> {
        let mut snapshot = self.workflow_runner.get_task_state(task_id).await?;
        let registry = self.workflow_runner.workflow_registry().read().await;

        if let Some(definition) = registry.get_definition(&snapshot.workflow) {
            let paused_ids: Vec<String> = snapshot
                .phases
                .iter()
                .filter(|(_, state)| matches!(state.status, PhaseStatus::Paused))
                .map(|(id, _)| id.clone())
                .collect();

            for phase_id in &paused_ids {
                let is_gate = definition
                    .phases
                    .iter()
                    .find(|p| &p.id == phase_id)
                    .map(|p| {
                        p.phase_type == Some(ao_protocol::workflow::PhaseType::Pause)
                            || p.phase_type == Some(ao_protocol::workflow::PhaseType::Input)
                    })
                    .unwrap_or(false);

                if is_gate {
                    if let Some(state) = snapshot.phases.get_mut(phase_id) {
                        state.status = PhaseStatus::Completed;
                        state.completed_at = Some(chrono::Utc::now());
                    }
                    tracing::info!(
                        task_id = %task_id,
                        phase_id = %phase_id,
                        "Completed pause/gate phase on resume"
                    );
                }
            }
        }
        drop(registry);

        self.workflow_runner
            .write_task_snapshot(task_id, &snapshot)
            .await?;
        Ok(())
    }

    async fn mark_task_completed(&mut self, task_id: &str) {
        // Clean up all synthetic phase agent queue managers for this task
        self.cleanup_all_phase_agents(task_id).await;

        match self.workflow_runner.get_task_state(task_id).await {
            Ok(mut snapshot) => {
                if snapshot.status == TaskStatus::Running {
                    snapshot.status = TaskStatus::Completed;
                    if let Err(e) = self
                        .workflow_runner
                        .write_task_snapshot(task_id, &snapshot)
                        .await
                    {
                        tracing::error!(task_id = %task_id, "Failed to write completed status: {}", e);
                    }
                }
            }
            Err(e) => {
                tracing::error!(task_id = %task_id, "Failed to read snapshot for completion: {}", e);
            }
        }
    }

    /// Mark a task as failed.
    async fn mark_task_failed(&mut self, task_id: &str) {
        // Clean up all synthetic phase agent queue managers for this task
        self.cleanup_all_phase_agents(task_id).await;

        match self.workflow_runner.get_task_state(task_id).await {
            Ok(mut snapshot) => {
                snapshot.status = TaskStatus::Failed;
                if let Err(e) = self
                    .workflow_runner
                    .write_task_snapshot(task_id, &snapshot)
                    .await
                {
                    tracing::error!(task_id = %task_id, "Failed to write failed status: {}", e);
                }

                self.event_bus
                    .emit(
                        task_id,
                        &format!("workflow:{}", snapshot.workflow),
                        None,
                        AgentEventPayload::WorkflowTaskFailed {
                            task_id: task_id.to_string(),
                            error: "Task execution failed".to_string(),
                        },
                    )
                    .await;
            }
            Err(e) => {
                tracing::error!(task_id = %task_id, "Failed to read snapshot for failure: {}", e);
            }
        }
    }
}

/// Synthetic agent ID for a task phase.
pub fn phase_agent_id(task_id: &str, phase_id: &str) -> String {
    format!("task:{}:phase:{}", task_id, phase_id)
}

/// Build a synthetic AgentProfile for running a phase's prompt via the
/// existing agent runner / queue manager infrastructure.
pub fn build_phase_agent(
    task_id: &str,
    phase_id: &str,
    system_prompt: &str,
    working_dir: Option<&str>,
    workflow_id: &str,
) -> ao_protocol::agent::AgentProfile {
    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig, WorkflowBinding,
    };

    AgentProfile {
        id: phase_agent_id(task_id, phase_id),
        name: format!("Phase: {}", phase_id),
        description: format!("Synthetic agent for task {} phase {}", task_id, phase_id),
        emoji: None,
        provider: ProviderConfig::Cli(CliProviderConfig {
            command: "claude".to_string(),
            args: vec![
                "--print".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "--include-partial-messages".to_string(),
            ],
            normalizer: Some("claude-code".to_string()),
            output_format: OutputFormat::StreamJson,
            input_mode: InputMode::Arg,
            model_arg: Some("--model".to_string()),
            model_aliases: Default::default(),
            system_prompt_arg: Some("--system-prompt".to_string()),
            session_arg: None,
            resume_args: vec!["--resume".to_string()],
            session_id_fields: vec!["session_id".to_string()],
            clear_env: false,
            no_output_timeout_ms: 120_000,
            file_capabilities: None,
        }),
        model: None,
        skills: vec![],
        system_prompt: Some(system_prompt.to_string()),
        tools: None,
        env: Default::default(),
        max_instances: 1,
        timeout_seconds: 600,
        working_dir: working_dir.map(|s| s.to_string()),
        home_dir: None,
        serialize: true,
        workflows: Some(WorkflowBinding::List(vec![workflow_id.to_string()])),
        template: None,
        runner_mode: Default::default(),
        enabled_plugins: std::collections::HashMap::new(),
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

/// Create a WorkflowQueueManager and its handle. Returns (handle, manager).
pub fn create_workflow_queue(
    workflow_runner: Arc<WorkflowRunner>,
    event_bus: Arc<EventBus>,
) -> (WorkflowQueueHandle, WorkflowQueueManager) {
    let (tx, rx) = mpsc::channel::<WfQueueMsg>(128);
    let internal_tx = tx.clone();

    let handle = WorkflowQueueHandle { tx };
    let manager = WorkflowQueueManager::new(rx, internal_tx, workflow_runner, event_bus);

    (handle, manager)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use ao_persistence::workflow_store::{TaskStore, WorkflowStore};
    use ao_protocol::workflow::{PhaseDefinition, TaskStatus, WorkflowDefinition};
    use tokio::sync::RwLock;

    use crate::event_bus::EventBus;
    use crate::workflow_registry::WorkflowRegistry;
    use crate::workflow_runner::WorkflowRunner;

    /// Create a test workflow YAML in a temp dir and return (WorkflowRunner, temp_dir).
    async fn setup_test_runner(
        workflow_id: &str,
        phases: Vec<PhaseDefinition>,
    ) -> (Arc<WorkflowRunner>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let workflows_dir = tmp.path().join("workflows");
        let tasks_dir = tmp.path().join("tasks");
        tokio::fs::create_dir_all(&workflows_dir).await.unwrap();
        tokio::fs::create_dir_all(&tasks_dir).await.unwrap();

        // Write workflow.yaml
        let wf_dir = workflows_dir.join(workflow_id);
        tokio::fs::create_dir_all(&wf_dir).await.unwrap();

        let definition = WorkflowDefinition {
            id: workflow_id.to_string(),
            name: format!("Test Workflow {}", workflow_id),
            version: Some("1.0".to_string()),
            description: Some("Test".to_string()),
            phases,
        };
        let yaml = serde_yaml::to_string(&definition).unwrap();
        tokio::fs::write(wf_dir.join("workflow.yaml"), &yaml)
            .await
            .unwrap();

        // Write a dummy phase prompt file
        tokio::fs::write(wf_dir.join("phase1.md"), "Do phase 1")
            .await
            .unwrap();
        tokio::fs::write(wf_dir.join("phase2.md"), "Do phase 2")
            .await
            .unwrap();

        let workflow_store = WorkflowStore::new(workflows_dir.clone());
        let registry = Arc::new(RwLock::new(
            WorkflowRegistry::new(workflow_store).await.unwrap(),
        ));
        let task_store = TaskStore::new(&tasks_dir);
        let workflow_store_for_runner = WorkflowStore::new(workflows_dir);
        let event_bus = Arc::new(EventBus::new(64));

        let runner = Arc::new(WorkflowRunner::new(
            registry,
            task_store,
            workflow_store_for_runner,
            event_bus,
        ));

        (runner, tmp)
    }

    fn make_file_phase(id: &str, path: &str) -> PhaseDefinition {
        PhaseDefinition {
            id: id.to_string(),
            name: format!("Phase {}", id),
            intent: Some(format!("Intent for {}", id)),
            path: path.to_string(),
            phase_type: None,
            auto_advance: true,
            schema: None,
            inputs: vec![],
            outputs: vec![],
            fields: vec![],
        }
    }

    #[tokio::test]
    async fn test_start_task_enters_ready_queue() {
        let phases = vec![
            make_file_phase("p1", "phase1.md"),
            make_file_phase("p2", "phase2.md"),
        ];
        let (runner, _tmp) = setup_test_runner("wf-test", phases).await;

        // Create a task
        let task_id = runner
            .create_task("wf-test", "Test Project", None, None)
            .await
            .unwrap();
        runner.start_task(&task_id).await.unwrap();

        let event_bus = Arc::new(EventBus::new(64));
        let (handle, manager) = create_workflow_queue(Arc::clone(&runner), event_bus);

        // Send StartTask
        handle
            .send(WfQueueMsg::StartTask {
                task_id: task_id.clone(),
            })
            .await
            .unwrap();

        // Verify the message was sent (channel didn't error)
        // We can't easily verify internal queue state without running the loop,
        // so we verify the handle works correctly.
        assert!(handle
            .send(WfQueueMsg::StartTask {
                task_id: "another-task".to_string(),
            })
            .await
            .is_ok());

        drop(manager);
    }

    #[tokio::test]
    async fn test_directory_conflict_detection() {
        let phases = vec![make_file_phase("p1", "phase1.md")];
        let (runner, _tmp) = setup_test_runner("wf-conflict", phases).await;
        let event_bus = Arc::new(EventBus::new(64));

        let (_, mut manager) = create_workflow_queue(Arc::clone(&runner), event_bus);

        // No conflict when no running tasks
        assert!(!manager.has_directory_conflict(&Some("/path/a".to_string())));

        // No conflict with None working directory
        assert!(!manager.has_directory_conflict(&None));

        // Add a running task with a specific directory
        manager.running_tasks.insert(
            "task-1".to_string(),
            RunningTaskInfo {
                working_directory: Some("/path/a".to_string()),
            },
        );

        // Conflict with same directory
        assert!(manager.has_directory_conflict(&Some("/path/a".to_string())));

        // No conflict with different directory
        assert!(!manager.has_directory_conflict(&Some("/path/b".to_string())));

        // No conflict with None
        assert!(!manager.has_directory_conflict(&None));
    }

    #[tokio::test]
    async fn test_backoff_promotion() {
        let phases = vec![make_file_phase("p1", "phase1.md")];
        let (runner, _tmp) = setup_test_runner("wf-backoff", phases).await;
        let event_bus = Arc::new(EventBus::new(64));

        let (_, mut manager) = create_workflow_queue(Arc::clone(&runner), event_bus);

        // Add tasks to backoff queue
        manager
            .backoff_queue
            .push_back("task-a".to_string());
        manager
            .backoff_queue
            .push_back("task-b".to_string());

        assert_eq!(manager.ready_queue.len(), 0);
        assert_eq!(manager.backoff_queue.len(), 2);

        // Promote — should move all to ready queue
        manager.promote_backoff();

        assert_eq!(manager.ready_queue.len(), 2);
        assert_eq!(manager.backoff_queue.len(), 0);
        assert_eq!(manager.ready_queue[0], "task-a");
        assert_eq!(manager.ready_queue[1], "task-b");
    }

    #[tokio::test]
    async fn test_crash_recovery_requeues_running_tasks() {
        let phases = vec![
            make_file_phase("p1", "phase1.md"),
            make_file_phase("p2", "phase2.md"),
        ];
        let (runner, _tmp) = setup_test_runner("wf-recovery", phases).await;

        // Create a task and set it to Running status
        let task_id = runner
            .create_task("wf-recovery", "Recovery Test", None, None)
            .await
            .unwrap();
        runner.start_task(&task_id).await.unwrap();

        // Verify it's Running
        let snapshot = runner.get_task_state(&task_id).await.unwrap();
        assert_eq!(snapshot.status, TaskStatus::Running);

        let event_bus = Arc::new(EventBus::new(64));
        let (_, mut manager) = create_workflow_queue(Arc::clone(&runner), event_bus);

        assert_eq!(manager.ready_queue.len(), 0);

        // Run crash recovery
        manager.recover_running_tasks().await;

        // The running task should be in the ready queue
        assert_eq!(manager.ready_queue.len(), 1);
        assert_eq!(manager.ready_queue[0], task_id);
    }

    #[tokio::test]
    async fn test_handle_send_works() {
        let phases = vec![make_file_phase("p1", "phase1.md")];
        let (runner, _tmp) = setup_test_runner("wf-handle", phases).await;
        let event_bus = Arc::new(EventBus::new(64));

        let (handle, _manager) = create_workflow_queue(Arc::clone(&runner), event_bus);

        // Sending should succeed
        assert!(handle
            .send(WfQueueMsg::StartTask {
                task_id: "test-123".to_string()
            })
            .await
            .is_ok());

        assert!(handle
            .send(WfQueueMsg::PhaseCompleted {
                task_id: "test-123".to_string(),
                phase_id: "p1".to_string()
            })
            .await
            .is_ok());

        assert!(handle
            .send(WfQueueMsg::PhaseFailed {
                task_id: "test-123".to_string(),
                phase_id: "p1".to_string(),
                error: "something broke".to_string()
            })
            .await
            .is_ok());
    }
}
