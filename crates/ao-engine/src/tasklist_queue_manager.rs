//! Per-tasklist queue manager (full Dispatch / Followup / Cancel
//! handling).
//!
//! This module provides per-tasklist queue management for tasklist
//! runs. It exists so tasklist task dispatches and their workflow followups
//! stay scoped to the tasklist's own transcript and event channel rather than
//! leaking onto the owner agent's personal transcript / channel — the bug
//! tracked by the parent project.
//!
//! Both the transcript file and the event channel are derived from
//! [`TasklistScope`], never from the owner's raw id: a team-owned tasklist
//! writes under `teams/` and emits on `team:{team_id}`, an agent-owned one
//! writes inside its own workspace under `tasks/agents/` and emits on the bare
//! agent id. See [`TasklistQueueManager::scope_id`] for why the distinction is
//! load-bearing rather than cosmetic.
//!
//! Lifecycle so far:
//! * Added the [`TasklistMessage`] enum, [`TasklistQueueManager`]
//!   struct, and a no-op run loop.
//! * Wires up the run loop and `pump` so Dispatch and Followup variants
//!   spawn an `agent_runner` run with `RunScope::Tasklist`, the `RunComplete`
//!   collector writes any `system_transcript` followups to the per-tasklist
//!   transcript file (not the agent's personal transcript) and emits
//!   `SystemMessage` events on the owner's channel (not the agent's personal
//!   channel), and any returned context-only followups are re-queued via
//!   `self_tx` as [`TasklistMessage::Followup`] so further pumps stay in
//!   tasklist scope. Cancel either calls `agent_runner.cancel_run` for an
//!   in-flight run or removes the task from the queue without spawning.
//! * Adds the [`TasklistQueueManagerRegistry`] (lazy per-tasklist manager
//!   creation) and the [`TasklistQueueDispatcher`]
//!   [`crate::task_feeder::TaskDispatcher`] adapter that resolves a
//!   tasklist's `workspace_dir` from [`PersistenceLayer`] before submitting
//!   a `Dispatch` message.
//! * Wires those into [`crate::state::AppState`] and cuts
//!   [`crate::task_feeder::TaskFeeder`] over to [`TasklistQueueDispatcher`].
//! * Deletes the `MessageSource::Tasklist` enum variant and the
//!   `QueueManagerDispatcher` it powered — tasklist tasks no longer enter
//!   the personal [`crate::queue_manager::AgentQueueManager`] at all.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::{mpsc, RwLock};
use tracing;
use uuid::Uuid;

use ao_persistence::PersistenceLayer;
use ao_protocol::agent::AgentId;
use ao_protocol::attachment::Attachment;
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::tasklist::{TaskId, TasklistId, TasklistOwner, TasklistScope, TaskStatus};
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

use crate::agent_runner::{CliAgentRunner, RunComplete, RunScope};
use crate::event_bus::EventBus;
use crate::sleep_guard::SleepGuard;
use crate::task_feeder::{TaskDispatcher, TaskFeeder};

/// Messages that can be submitted to a tasklist's queue manager.
///
/// Field shapes mirror the interview FR-2 design — `team_id` and
/// `tasklist_id` are *not* repeated on each message because the
/// [`TasklistQueueManager`] already owns them; only per-task context
/// (`task_id`, `owner_agent_id`, prompt/context, workspace) varies.
#[derive(Debug)]
pub(crate) enum TasklistMessage {
    /// Dispatch a tasklist task to its owner agent. Produced by
    /// `TasklistQueueDispatcher` on behalf of the
    /// [`crate::task_feeder::TaskFeeder`].
    Dispatch {
        task_id: TaskId,
        owner_agent_id: AgentId,
        prompt: String,
        /// Tasklist's shared workspace directory. The agent runner uses this
        /// as `focus_path`, so the task runs with `cwd` pointed at the
        /// tasklist workspace rather than the agent's default `working_dir`.
        workspace_dir: Option<String>,
        /// Team-scoped attachments bound to the task at append time. Forwarded
        /// to `run_with_scope` so `augment_prompt_with_attachments` can inject
        /// the file paths/templates into the prompt the owner agent sees.
        attachments: Vec<Attachment>,
    },
    /// Re-queue a workflow followup from a previous run. Produced by the
    /// `RunComplete` collector when the agent emits workflow followups; sent
    /// via `self_tx` so the next pump stays in tasklist scope.
    Followup {
        task_id: TaskId,
        owner_agent_id: AgentId,
        context: String,
        workspace_dir: Option<String>,
        attachments: Vec<Attachment>,
    },
    /// Cancel a task. If the task is in-flight, the active run is cancelled;
    /// if it's still queued, it's dropped from the queue without spawning.
    Cancel { task_id: TaskId },
}

/// Handle used to submit messages to a tasklist's queue manager.
#[derive(Clone)]
pub(crate) struct TasklistQueueManagerHandle {
    pub(crate) message_tx: mpsc::Sender<TasklistMessage>,
}

/// The SSE channel a tasklist's events belong on: `team:{team_id}` for
/// team-owned, the bare agent id for agent-owned. Matches the convention in
/// `task_feeder::owner_event_channel` and `TasklistService::event_agent_id`.
///
/// Deliberately takes the scope rather than the owner's raw id. An agent-owned
/// tasklist's raw id is its owner agent's id, and prefixing that with `team:`
/// yields a channel no subscriber matches — which is how tasklist errors and
/// workflow system messages for agent-owned tasklists were emitted into the
/// void rather than reaching the UI.
fn event_channel_for_scope(scope: &TasklistScope) -> String {
    match scope {
        TasklistScope::Team(team_id) => format!("team:{}", team_id),
        TasklistScope::Agent(agent_id) => agent_id.clone(),
    }
}

/// Where a run's `system_transcript` workflow followups are persisted.
///
/// Mirrors the runner's own `transcript_path_override` resolution in `cli.rs`,
/// so a followup lands in the same file as the run that produced it: team-owned
/// tasklists keep one file per agent under the team tree, agent-owned ones one
/// file per task inside the tasklist's own workspace.
///
/// Resolving this from the scope rather than from the owner's raw id is what
/// keeps agent-owned followups out of `teams/`. The id alone cannot say which
/// tree it belongs to, and treating it as a team id sent agent-owned followups
/// to `teams/{agent_id}/tasklists/...` — a file no reader ever opens.
fn followup_transcript_path(
    data_root: &ao_persistence::paths::DataRoot,
    scope: &TasklistScope,
    tasklist_id: &str,
    owner_agent_id: &str,
    task_id: &str,
) -> std::path::PathBuf {
    match scope {
        TasklistScope::Team(team_id) => {
            data_root.tasklist_agent_transcript_path(team_id, tasklist_id, owner_agent_id)
        }
        TasklistScope::Agent(scope_agent_id) => {
            data_root.task_transcript_path(scope_agent_id, tasklist_id, task_id)
        }
    }
}

/// Result delivered back to the manager loop after a spawned agent run
/// finishes. Carries the per-run metadata the manager needs to (a) clear its
/// `in_flight` entry and (b) re-queue any returned followups in scope.
struct RunFinished {
    run_id: String,
    task_id: TaskId,
    owner_agent_id: AgentId,
    workspace_dir: Option<String>,
    attachments: Vec<Attachment>,
    /// `None` only if the agent_runner's bridge channel closed without a
    /// `RunComplete` (process abort, panic). Logged but otherwise ignored.
    run_complete: Option<RunComplete>,
}

/// Per-tasklist queue manager.
///
/// Owns its own message queue, an mpsc receiver fed by external submitters,
/// and a `self_tx` clone of that sender that the `RunComplete` collector uses
/// to re-queue followups in scope. `in_flight` maps each running task to its
/// agent_runner `run_id` so [`TasklistMessage::Cancel`] can call
/// `agent_runner.cancel_run`.
pub(crate) struct TasklistQueueManager {
    /// Raw id of whoever owns this tasklist: the team id for team-owned, the
    /// owner agent's id for agent-owned. Used as the registry key and as a
    /// log field only — anything that needs to know *which* of the two it is
    /// must read [`Self::scope`], since the id alone cannot say. It was
    /// previously called `team_id`, which made an agent id read as a team id
    /// at every use site and produced two on-disk/on-the-wire bugs.
    pub(crate) scope_id: String,
    pub(crate) tasklist_id: TasklistId,
    /// Scope for spawning runs (Team or Agent). Replaces the hard-coded
    /// `TasklistScope::Team(..)` that was used previously.
    pub(crate) scope: TasklistScope,
    queue: VecDeque<TasklistMessage>,
    message_rx: mpsc::Receiver<TasklistMessage>,
    /// Clone of the manager's own message sender. Used by `on_run_finished`
    /// when re-queueing followups so they stay on the tasklist queue rather
    /// than re-entering [`crate::queue_manager::AgentQueueManager`].
    self_tx: mpsc::Sender<TasklistMessage>,
    /// Internal channel: spawned bridge tasks send `RunFinished` here when an
    /// agent run completes. Polled by [`Self::run`] alongside `message_rx`.
    run_finished_tx: mpsc::Sender<RunFinished>,
    run_finished_rx: mpsc::Receiver<RunFinished>,
    /// task_id → active `run_id` for in-flight runs. Populated by Dispatch /
    /// Followup, drained by `on_run_finished`, consulted by Cancel.
    in_flight: HashMap<TaskId, String>,
    agent_runner: Arc<CliAgentRunner>,
    persistence: Arc<PersistenceLayer>,
    event_bus: Arc<EventBus>,
    /// Holds the system awake while any tasklist task is queued or in-flight.
    /// Gated by the `prevent_sleep_during_tasklists` user preference and
    /// refreshed at the end of [`Self::pump`] / [`Self::on_run_finished`].
    sleep_guard: SleepGuard,
    /// Late-bound feeder reference used to call `on_task_terminal` when a
    /// spawn fails or the runner bridge closes without a RunComplete — the two
    /// paths that previously left a task permanently InProgress (zombie).
    /// Set via [`TasklistQueueManagerRegistry::set_task_feeder`] after the
    /// feeder is constructed (same deferred-init pattern as `CliAgentRunner`).
    task_feeder: Arc<OnceLock<Arc<TaskFeeder>>>,
}

impl TasklistQueueManager {
    /// Construct a new manager. Returns the manager + a handle that submitters
    /// can clone freely. The manager itself is moved into a `tokio::spawn` by
    /// the registry.
    pub(crate) fn new(
        scope_id: String,
        tasklist_id: TasklistId,
        scope: TasklistScope,
        agent_runner: Arc<CliAgentRunner>,
        persistence: Arc<PersistenceLayer>,
        event_bus: Arc<EventBus>,
        task_feeder: Arc<OnceLock<Arc<TaskFeeder>>>,
    ) -> (Self, TasklistQueueManagerHandle) {
        let (message_tx, message_rx) = mpsc::channel::<TasklistMessage>(128);
        let (run_finished_tx, run_finished_rx) = mpsc::channel::<RunFinished>(64);
        let handle = TasklistQueueManagerHandle {
            message_tx: message_tx.clone(),
        };
        let manager = Self {
            scope_id,
            tasklist_id,
            scope,
            queue: VecDeque::new(),
            message_rx,
            self_tx: message_tx,
            run_finished_tx,
            run_finished_rx,
            in_flight: HashMap::new(),
            agent_runner,
            persistence,
            event_bus,
            sleep_guard: SleepGuard::new(1.0),
            task_feeder,
        };
        (manager, handle)
    }

    /// Derive a `TasklistOwner` from the manager's `scope` field.
    fn owner(&self) -> TasklistOwner {
        match &self.scope {
            TasklistScope::Team(team_id) => TasklistOwner::Team { team_id: team_id.clone() },
            TasklistScope::Agent(agent_id) => TasklistOwner::Agent { agent_id: agent_id.clone() },
        }
    }

    /// See [`event_channel_for_scope`].
    fn event_channel(&self) -> String {
        event_channel_for_scope(&self.scope)
    }

    /// Set a task's status to `Failed` on disk and notify the feeder so the
    /// tasklist can advance past it. Without this, a spawn failure or a bridge
    /// that closes without `RunComplete` leaves the task permanently `InProgress`
    /// (zombie) and the feeder's SEQ guard blocks all further dispatch.
    async fn mark_task_failed(&self, task_id: &TaskId) {
        let owner = self.owner();
        if let Err(e) = self
            .persistence
            .tasklists
            .set_task_status_by_owner(&owner, &self.tasklist_id, task_id, TaskStatus::Failed)
            .await
        {
            tracing::error!(
                scope_id = %self.scope_id,
                tasklist_id = %self.tasklist_id,
                task_id = %task_id,
                "mark_task_failed: could not set task status to Failed: {}",
                e
            );
        }
        if let Some(feeder) = self.task_feeder.get() {
            if let Err(e) = feeder.on_task_terminal(&owner, &self.tasklist_id, task_id).await {
                tracing::error!(
                    scope_id = %self.scope_id,
                    tasklist_id = %self.tasklist_id,
                    task_id = %task_id,
                    "mark_task_failed: on_task_terminal error: {}",
                    e
                );
            }
        } else {
            tracing::warn!(
                scope_id = %self.scope_id,
                tasklist_id = %self.tasklist_id,
                task_id = %task_id,
                "mark_task_failed: task_feeder not yet wired — feeder will not advance \
                 (watchdog will recover within 30s)"
            );
        }
    }

    /// True when there is any tasklist task either queued or in-flight.
    fn has_active_tasklist_work(&self) -> bool {
        !self.queue.is_empty() || !self.in_flight.is_empty()
    }

    /// Refresh the sleep guard based on the user preference and whether any
    /// tasklist task is queued or in-flight. Called at the end of every state
    /// transition (`pump`, `on_run_finished`).
    async fn refresh_sleep_guard(&mut self) {
        let prefs = self.persistence.preferences.get().await.ok().flatten();

        let enabled = prefs
            .as_ref()
            .map(|prefs| prefs.prevent_sleep_during_tasklists)
            .unwrap_or(true);
        self.sleep_guard.set_disabled(!enabled);

        let keep_display_awake = prefs.map(|prefs| prefs.keep_display_awake).unwrap_or(false);
        self.sleep_guard.set_keep_display_awake(keep_display_awake);

        self.sleep_guard.update_active(self.has_active_tasklist_work());
    }

    /// Main run loop. Handles inbound messages and run-completion signals
    /// in a `tokio::select!`. Exits when both the external sender and every
    /// internal bridge sender have been dropped.
    pub(crate) async fn run(mut self) {
        loop {
            tokio::select! {
                msg = self.message_rx.recv() => {
                    match msg {
                        Some(message) => {
                            self.queue.push_back(message);
                            self.pump().await;
                        }
                        None => {
                            tracing::debug!(
                                scope_id = %self.scope_id,
                                tasklist_id = %self.tasklist_id,
                                "Tasklist queue manager shutting down (channel closed)"
                            );
                            break;
                        }
                    }
                }
                finished = self.run_finished_rx.recv() => {
                    if let Some(result) = finished {
                        self.on_run_finished(result).await;
                        self.pump().await;
                    }
                    // run_finished_tx is held inside `self`, so the channel
                    // never returns None unless we drop ourselves — no need
                    // for a shutdown branch here.
                }
            }
        }
    }

    /// Drain the queue. Each variant is handled independently:
    /// Dispatch / Followup spawn an agent run; Cancel either cancels an
    /// in-flight run or drops the task from the queue.
    async fn pump(&mut self) {
        while let Some(message) = self.queue.pop_front() {
            match message {
                TasklistMessage::Dispatch {
                    task_id,
                    owner_agent_id,
                    prompt,
                    workspace_dir,
                    attachments,
                } => {
                    self.spawn_run(task_id, owner_agent_id, prompt, workspace_dir, attachments)
                        .await;
                }
                TasklistMessage::Followup {
                    task_id,
                    owner_agent_id,
                    context,
                    workspace_dir,
                    attachments,
                } => {
                    // Followup re-uses the same run_with_scope path: the
                    // agent_runner reads tasklist context from RunScope and
                    // appends to the tasklist transcript automatically.
                    self.spawn_run(task_id, owner_agent_id, context, workspace_dir, attachments)
                        .await;
                }
                TasklistMessage::Cancel { task_id } => {
                    self.handle_cancel(&task_id).await;
                }
            }
        }
        self.refresh_sleep_guard().await;
    }

    /// Look up the owner agent profile, start a `RunScope::Tasklist` run via
    /// the agent runner, and spawn a small bridge task that forwards the
    /// resulting `RunComplete` back to the manager's `run_finished_rx`.
    async fn spawn_run(
        &mut self,
        task_id: TaskId,
        owner_agent_id: AgentId,
        prompt: String,
        workspace_dir: Option<String>,
        attachments: Vec<Attachment>,
    ) {
        let agent_profile = match self.persistence.agents.get(&owner_agent_id).await {
            Ok(Some(profile)) => profile,
            Ok(None) => {
                tracing::error!(
                    scope_id = %self.scope_id,
                    tasklist_id = %self.tasklist_id,
                    task_id = %task_id,
                    owner_agent_id = %owner_agent_id,
                    "Owner agent not found — failing task so feeder can advance"
                );
                self.emit_task_error(format!(
                    "Tasklist task {} failed: owner agent {} not found",
                    task_id, owner_agent_id
                ))
                .await;
                self.mark_task_failed(&task_id).await;
                return;
            }
            Err(e) => {
                tracing::error!(
                    scope_id = %self.scope_id,
                    tasklist_id = %self.tasklist_id,
                    task_id = %task_id,
                    owner_agent_id = %owner_agent_id,
                    "Failed to load owner agent profile: {}",
                    e
                );
                self.emit_task_error(format!(
                    "Tasklist task {} failed: could not load owner agent {}: {}",
                    task_id, owner_agent_id, e
                ))
                .await;
                self.mark_task_failed(&task_id).await;
                return;
            }
        };

        let scope = RunScope::Tasklist {
            scope: self.scope.clone(),
            tasklist_id: self.tasklist_id.clone(),
            task_id: task_id.clone(),
        };

        // Per-run bridge: agent_runner sends one `RunComplete` here; the
        // bridge task below forwards it onto `run_finished_tx` so the manager
        // can process followups in its own loop (and clear `in_flight`).
        let (bridge_tx, mut bridge_rx) = mpsc::channel::<RunComplete>(1);
        let focus_path = workspace_dir.as_deref();

        match self
            .agent_runner
            .run_with_scope(&agent_profile, &prompt, &attachments, bridge_tx, scope, focus_path)
            .await
        {
            Ok(run_id) => {
                tracing::debug!(
                    scope_id = %self.scope_id,
                    tasklist_id = %self.tasklist_id,
                    task_id = %task_id,
                    owner_agent_id = %owner_agent_id,
                    run_id = %run_id,
                    "Tasklist run started"
                );
                self.in_flight.insert(task_id.clone(), run_id.clone());

                let run_finished_tx = self.run_finished_tx.clone();
                let task_id_clone = task_id;
                let owner_clone = owner_agent_id;
                let workspace_clone = workspace_dir;
                let attachments_clone = attachments;
                let run_id_clone = run_id;
                tokio::spawn(async move {
                    let run_complete = bridge_rx.recv().await;
                    let _ = run_finished_tx
                        .send(RunFinished {
                            run_id: run_id_clone,
                            task_id: task_id_clone,
                            owner_agent_id: owner_clone,
                            workspace_dir: workspace_clone,
                            attachments: attachments_clone,
                            run_complete,
                        })
                        .await;
                });
            }
            Err(e) => {
                tracing::error!(
                    scope_id = %self.scope_id,
                    tasklist_id = %self.tasklist_id,
                    task_id = %task_id,
                    owner_agent_id = %owner_agent_id,
                    "Failed to start tasklist run: {} — failing task so feeder can advance",
                    e
                );
                self.emit_task_error(format!(
                    "Tasklist task {} failed to start: {}",
                    task_id, e
                ))
                .await;
                self.mark_task_failed(&task_id).await;
            }
        }
    }

    /// Handle a `RunFinished` signal from a spawned bridge: drop the
    /// `in_flight` entry, persist any `system_transcript` followups to the
    /// per-tasklist transcript file, emit `SystemMessage` events on the
    /// owner's channel, and re-queue any context-bearing followups via `self_tx`
    /// so the next pump stays in tasklist scope.
    async fn on_run_finished(&mut self, finished: RunFinished) {
        // Only clear `in_flight` if the stored run_id still matches —
        // protects against a stale completion arriving after the same task
        // has been re-dispatched (Followup) with a fresh run_id.
        if self
            .in_flight
            .get(&finished.task_id)
            .map(|stored| stored == &finished.run_id)
            .unwrap_or(false)
        {
            self.in_flight.remove(&finished.task_id);
        }

        let Some(run_complete) = finished.run_complete else {
            tracing::error!(
                scope_id = %self.scope_id,
                tasklist_id = %self.tasklist_id,
                task_id = %finished.task_id,
                run_id = %finished.run_id,
                "Tasklist run bridge closed without RunComplete (runner crashed or was killed) \
                 — failing task so feeder can advance"
            );
            self.mark_task_failed(&finished.task_id).await;
            return;
        };

        let transcript_path = followup_transcript_path(
            &self.persistence.data_root,
            &self.scope,
            &self.tasklist_id,
            &finished.owner_agent_id,
            &finished.task_id,
        );
        let event_channel = self.event_channel();

        for followup in run_complete.workflow_followups {
            // Persist any system_transcript text to the *tasklist* transcript
            // file (not the agent's personal one), and emit a SystemMessage
            // event on the *team* channel (not the agent's personal channel).
            // This is the bug-fix core: previously this work happened in
            // AgentQueueManager keyed by &self.agent_id, leaking system
            // followups into personal scope.
            if let Some(ref text) = followup.system_transcript {
                let entry = TranscriptEntry {
                    ts: Utc::now(),
                    role: TranscriptRole::System("system".to_string()),
                    content: text.clone(),
                    event_type: "workflow_system".to_string(),
                    metadata: None,
                    hidden_from_user: false,
                };
                if let Err(e) = self
                    .persistence
                    .transcripts
                    .append_at(&transcript_path, &entry)
                    .await
                {
                    tracing::error!(
                        scope_id = %self.scope_id,
                        tasklist_id = %self.tasklist_id,
                        task_id = %finished.task_id,
                        owner_agent_id = %finished.owner_agent_id,
                        "Failed to write workflow_system entry to tasklist transcript: {}",
                        e
                    );
                }
                self.event_bus
                    .emit(
                        &format!("system-{}", Uuid::new_v4()),
                        &event_channel,
                        None,
                        AgentEventPayload::SystemMessage { text: text.clone(), severity: None },
                    )
                    .await;
            }

            // Re-queue the followup's context as a fresh tasklist message so
            // the next pump runs in scope. Bounded channel (128) is much
            // larger than the followups any single run produces.
            let _ = self
                .self_tx
                .send(TasklistMessage::Followup {
                    task_id: finished.task_id.clone(),
                    owner_agent_id: finished.owner_agent_id.clone(),
                    context: followup.context,
                    workspace_dir: finished.workspace_dir.clone(),
                    attachments: finished.attachments.clone(),
                })
                .await;
        }

        self.refresh_sleep_guard().await;
    }

    /// Cancel either cancels an in-flight run or drops a pending message
    /// from the queue (whichever applies).
    async fn handle_cancel(&mut self, task_id: &TaskId) {
        if let Some(run_id) = self.in_flight.remove(task_id) {
            let sent = self.agent_runner.cancel_run(&run_id).await;
            tracing::info!(
                scope_id = %self.scope_id,
                tasklist_id = %self.tasklist_id,
                task_id = %task_id,
                run_id = %run_id,
                sent,
                "Cancelled in-flight tasklist run"
            );
            return;
        }

        let before = self.queue.len();
        self.queue.retain(|m| match m {
            TasklistMessage::Dispatch { task_id: t, .. }
            | TasklistMessage::Followup { task_id: t, .. } => t != task_id,
            TasklistMessage::Cancel { .. } => true,
        });
        let removed = before - self.queue.len();
        if removed > 0 {
            tracing::info!(
                scope_id = %self.scope_id,
                tasklist_id = %self.tasklist_id,
                task_id = %task_id,
                removed,
                "Removed pending tasklist message(s) from queue"
            );
        } else {
            tracing::debug!(
                scope_id = %self.scope_id,
                tasklist_id = %self.tasklist_id,
                task_id = %task_id,
                "Cancel: no in-flight run or pending message found"
            );
        }
    }

    /// Emit a recoverable error on the owner's event channel so the UI can
    /// surface failures that prevent a tasklist task from starting.
    async fn emit_task_error(&self, message: String) {
        self.event_bus
            .emit(
                &format!("tasklist-error-{}", Uuid::new_v4()),
                &self.event_channel(),
                None,
                AgentEventPayload::Error {
                    message,
                    recoverable: true,
                },
            )
            .await;
    }
}

/// Registry of per-tasklist queue managers. A manager is
/// created lazily on the first [`Self::submit`] for that tasklist and reused
/// for subsequent submits. [`Self::remove_tasklist`] drops the registry's
/// handle so the manager can shut down on tasklist archive / delete.
pub struct TasklistQueueManagerRegistry {
    handles: Arc<RwLock<HashMap<TasklistId, TasklistQueueManagerHandle>>>,
    agent_runner: Arc<CliAgentRunner>,
    persistence: Arc<PersistenceLayer>,
    event_bus: Arc<EventBus>,
    /// Shared with every `TasklistQueueManager` spawned by this registry so
    /// they can call `feeder.on_task_terminal` when a spawn fails or a bridge
    /// closes without `RunComplete`. Bound post-construction via
    /// `set_task_feeder` (same deferred-init pattern as `CliAgentRunner`).
    task_feeder: Arc<OnceLock<Arc<TaskFeeder>>>,
}

impl TasklistQueueManagerRegistry {
    pub fn new(
        agent_runner: Arc<CliAgentRunner>,
        persistence: Arc<PersistenceLayer>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            handles: Arc::new(RwLock::new(HashMap::new())),
            agent_runner,
            persistence,
            event_bus,
            task_feeder: Arc::new(OnceLock::new()),
        }
    }

    /// Late-bind the [`TaskFeeder`] so newly-created [`TasklistQueueManager`]
    /// instances can call `on_task_terminal` on spawn failures and bridge
    /// closes. Idempotent — a second call is silently ignored.
    pub fn set_task_feeder(&self, feeder: Arc<TaskFeeder>) {
        let _ = self.task_feeder.set(feeder);
    }

    /// Get-or-create the per-tasklist handle. On miss, constructs a new
    /// [`TasklistQueueManager`] and spawns its run loop.
    async fn get_or_create(
        &self,
        scope_id: &str,
        tasklist_id: &TasklistId,
        scope: TasklistScope,
    ) -> TasklistQueueManagerHandle {
        let mut handles = self.handles.write().await;
        if let Some(handle) = handles.get(tasklist_id) {
            return handle.clone();
        }
        let (manager, handle) = TasklistQueueManager::new(
            scope_id.to_string(),
            tasklist_id.clone(),
            scope,
            Arc::clone(&self.agent_runner),
            Arc::clone(&self.persistence),
            Arc::clone(&self.event_bus),
            Arc::clone(&self.task_feeder),
        );
        let tasklist_id_log = tasklist_id.clone();
        tokio::spawn(async move {
            manager.run().await;
            tracing::debug!(
                tasklist_id = %tasklist_id_log,
                "TasklistQueueManager run loop exited (channel closed)"
            );
        });
        handles.insert(tasklist_id.clone(), handle.clone());
        handle
    }

    /// Submit a [`TasklistMessage`] to the per-tasklist queue manager,
    /// lazy-creating it on first message. Returns
    /// [`AoError::Internal`] if the bounded send fails (the manager task has
    /// already exited).
    pub(crate) async fn submit(
        &self,
        scope_id: &str,
        tasklist_id: &TasklistId,
        scope: TasklistScope,
        message: TasklistMessage,
    ) -> Result<(), AoError> {
        let handle = self.get_or_create(scope_id, tasklist_id, scope).await;
        handle
            .message_tx
            .send(message)
            .await
            .map_err(|e| AoError::Internal(format!("Tasklist queue send error: {}", e)))
    }

    /// Send a [`TasklistMessage::Cancel`] to the queue manager for
    /// `tasklist_id`, but only if a manager is already running. Does nothing
    /// when there is no live handle (avoids creating a manager just to cancel
    /// a task that was never dispatched through it).
    ///
    /// Called by the cancel and stop-task paths in `TasklistService` so that
    /// the in-flight CLI subprocess is killed and the run's cancellation token
    /// fires, rather than the agent running to completion after user-initiated
    /// cancellation.
    pub async fn cancel_task_if_running(&self, tasklist_id: &str, task_id: &str) {
        let handles = self.handles.read().await;
        if let Some(handle) = handles.get(tasklist_id) {
            let _ = handle
                .message_tx
                .send(TasklistMessage::Cancel {
                    task_id: task_id.to_string(),
                })
                .await;
        }
    }

    /// Drop the registry's handle for a tasklist. The background manager task
    /// will wind down once its inbound channel closes — note that
    /// [`TasklistQueueManager`] currently keeps a `self_tx` clone for
    /// in-scope followup re-queue, so the channel only fully closes
    /// after the manager itself is dropped. For archive/delete this is fine:
    /// no new external messages arrive, the manager drains followups, and on
    /// the next process restart it isn't reconstructed.
    pub async fn remove_tasklist(&self, tasklist_id: &str) {
        let removed = self.handles.write().await.remove(tasklist_id);
        if removed.is_some() {
            tracing::info!(
                tasklist_id = %tasklist_id,
                "Removed tasklist queue manager handle"
            );
        }
    }
}

/// Production [`TaskDispatcher`] backed by [`TasklistQueueManagerRegistry`].
/// Resolves the tasklist's `workspace_dir` from [`PersistenceLayer`] (so the
/// agent runner uses it as `focus_path` and runs the task with `cwd` pointed
/// at the tasklist workspace) and submits a [`TasklistMessage::Dispatch`] to
/// the registry. This is the only production [`TaskDispatcher`] —
/// [`crate::task_feeder::TaskFeeder`] and `dispatch_watchdog` flow tasklist
/// tasks through here, never through the personal
/// [`crate::queue_manager::AgentQueueManager`].
pub struct TasklistQueueDispatcher {
    registry: Arc<TasklistQueueManagerRegistry>,
    persistence: Arc<PersistenceLayer>,
}

impl TasklistQueueDispatcher {
    pub fn new(
        registry: Arc<TasklistQueueManagerRegistry>,
        persistence: Arc<PersistenceLayer>,
    ) -> Self {
        Self {
            registry,
            persistence,
        }
    }
}

#[async_trait]
impl TaskDispatcher for TasklistQueueDispatcher {
    async fn dispatch_task(
        &self,
        owner_agent_id: &AgentId,
        prompt: String,
        owner: &TasklistOwner,
        tasklist_id: &TasklistId,
        task_id: &TaskId,
    ) -> Result<(), AoError> {
        let scope = match owner {
            TasklistOwner::Team { team_id } => TasklistScope::Team(team_id.clone()),
            TasklistOwner::Agent { agent_id } => TasklistScope::Agent(agent_id.clone()),
        };
        // The owner's raw id — the registry key, and the manager's `scope_id`.
        // Deliberately NOT the SSE channel: that is `team:{id}` for team
        // owners and the bare id for agent owners, and the manager derives it
        // from `scope` via `event_channel()`. Conflating the two is what sent
        // agent-owned tasklist events to a channel nothing subscribes to.
        let scope_id = match owner {
            TasklistOwner::Team { team_id } => team_id.clone(),
            TasklistOwner::Agent { agent_id } => agent_id.clone(),
        };

        // Resolve the tasklist's shared workspace dir AND the task's bound
        // attachments before submitting. Missing tasklist = dispatch with
        // empty defaults (the manager's spawn_run logs and emits a task
        // error); we intentionally don't fail the dispatch here — that would
        // break the feeder's per-tasklist progression.
        let (workspace_dir, attachments) = self
            .persistence
            .tasklists
            .get_by_owner(owner, tasklist_id)
            .await?
            .map(|tl| {
                let attachments = tl
                    .groups
                    .iter()
                    .flat_map(|g| g.tasks.iter())
                    .find(|t| t.id == *task_id)
                    .map(|t| t.attachments.clone())
                    .unwrap_or_default();
                (Some(tl.workspace_dir), attachments)
            })
            .unwrap_or((None, Vec::new()));

        tracing::info!(
            scope_id = %scope_id,
            tasklist_id = %tasklist_id,
            task_id = %task_id,
            owner_agent_id = %owner_agent_id,
            attachment_count = attachments.len(),
            "TasklistQueueDispatcher: submitting Dispatch to TasklistQueueManager"
        );

        for att in &attachments {
            let exists = tokio::fs::try_exists(&att.file_path).await.unwrap_or(false);
            tracing::info!(
                scope_id = %scope_id,
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                attachment_id = %att.id,
                file_path = %att.file_path,
                exists,
                "TasklistQueueDispatcher: attachment existence check"
            );
        }

        self.registry
            .submit(
                &scope_id,
                tasklist_id,
                scope,
                TasklistMessage::Dispatch {
                    task_id: task_id.clone(),
                    owner_agent_id: owner_agent_id.clone(),
                    prompt,
                    workspace_dir,
                    attachments,
                },
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_persistence::paths::DataRoot;

    /// Agent-owned tasklist events must reach the owner agent's own channel.
    /// Prefixing the raw scope id with `team:` — which is what the manager did
    /// while the field was called `team_id` — produced `team:{agent_id}`, a
    /// channel the frontend's `channel.team` matcher accepts but no view
    /// subscribes to, so `Error` and `SystemMessage` events were dropped.
    #[test]
    fn event_channel_is_the_bare_agent_id_for_agent_owned() {
        assert_eq!(
            event_channel_for_scope(&TasklistScope::Agent("agent-1".into())),
            "agent-1"
        );
        assert_eq!(
            event_channel_for_scope(&TasklistScope::Team("team-1".into())),
            "team:team-1"
        );
    }

    /// Workflow followups from an agent-owned tasklist must be written inside
    /// that tasklist's own workspace, never under the legacy `teams/` subtree.
    #[test]
    fn followup_transcript_for_agent_owned_stays_out_of_the_teams_subtree() {
        let root = DataRoot::new(std::path::Path::new("/tmp/does-not-need-to-exist"));

        let agent_path = followup_transcript_path(
            &root,
            &TasklistScope::Agent("agent-1".into()),
            "tl-1",
            "agent-1",
            "task-9",
        );
        assert!(
            !agent_path.starts_with(root.teams_dir()),
            "agent-owned followup resolved into the legacy team tree: {}",
            agent_path.display()
        );
        assert_eq!(
            agent_path,
            root.task_transcript_path("agent-1", "tl-1", "task-9"),
            "must match the path the runner itself writes this run's transcript to"
        );

        // Team-owned keeps its existing per-agent file under teams/.
        let team_path = followup_transcript_path(
            &root,
            &TasklistScope::Team("team-1".into()),
            "tl-1",
            "agent-1",
            "task-9",
        );
        assert_eq!(
            team_path,
            root.tasklist_agent_transcript_path("team-1", "tl-1", "agent-1")
        );
        assert!(team_path.starts_with(root.teams_dir()));
    }

    /// The two are distinct decisions off the same scope. A single "scope id"
    /// string cannot serve both, which is the conflation the fix removed.
    #[test]
    fn event_channel_and_transcript_path_disagree_for_agent_owned() {
        let root = DataRoot::new(std::path::Path::new("/tmp/does-not-need-to-exist"));
        let scope = TasklistScope::Agent("agent-1".into());

        let channel = event_channel_for_scope(&scope);
        let path = followup_transcript_path(&root, &scope, "tl-1", "agent-1", "task-9");

        assert!(!channel.starts_with("team:"));
        assert!(path.to_string_lossy().contains("tasks/agents/agent-1"));
    }
}
