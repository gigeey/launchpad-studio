use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use ao_engine_tools_core::terminal_report::{CancelOutcome, TerminalWatcherGuard};
use ao_engine_tools_core::{
    ResumeOutcome, StartOutcome, StartOutcomeKind, TasklistServiceHandle, ZombieReport,
};
use ao_persistence::progress_log::{append_progress_block, ProgressBlock};
use ao_persistence::PersistenceLayer;
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::tasklist::{
    Task, TaskAssignment, TaskComment, TaskCommentAuthorKind, TaskGroup, TaskGroupMode, TaskStatus,
    Tasklist, TasklistOwner, TasklistStatus,
};

use crate::event_bus::EventBus;
use crate::instance_registry::InstanceRegistry;
use crate::task_feeder::TaskFeeder;
use crate::tasklist_lifecycle;
use crate::tasklist_queue_manager::TasklistQueueManagerRegistry;

/// Unified mutation point for tasklist operations.
/// Used by HTTP route handlers (team scope) and Todo* tools (agent scope) alike.
pub struct TasklistService {
    persistence: Arc<PersistenceLayer>,
    feeder: Arc<TaskFeeder>,
    event_bus: Arc<EventBus>,
    instance_registry: Option<Arc<InstanceRegistry>>,
    /// When set, cancel and stop-task paths forward kill signals to in-flight
    /// runs via the queue manager so the CLI subprocess is actually terminated.
    tasklist_queue_managers: Option<Arc<TasklistQueueManagerRegistry>>,
}

impl TasklistService {
    pub fn new(
        persistence: Arc<PersistenceLayer>,
        feeder: Arc<TaskFeeder>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            persistence,
            feeder,
            event_bus,
            instance_registry: None,
            tasklist_queue_managers: None,
        }
    }

    pub fn with_instance_registry(mut self, registry: Arc<InstanceRegistry>) -> Self {
        self.instance_registry = Some(registry);
        self
    }

    pub fn with_tasklist_queue_managers(mut self, qm: Arc<TasklistQueueManagerRegistry>) -> Self {
        self.tasklist_queue_managers = Some(qm);
        self
    }

    fn event_agent_id(owner: &TasklistOwner) -> String {
        match owner {
            TasklistOwner::Team { team_id } => format!("team:{}", team_id),
            TasklistOwner::Agent { agent_id } => agent_id.clone(),
        }
    }

    /// Create a new tasklist for the given owner.
    /// Team path: validates team exists, creates under teams/{team_id}/tasklists/.
    /// Agent path: validates agent exists, creates under tasks/agents/{agent_id}/tasklists/.
    pub async fn create(
        &self,
        owner: TasklistOwner,
        title: String,
        description: String,
        groups: Vec<TaskGroup>,
        allow_empty: bool,
    ) -> Result<Tasklist, AoError> {
        if title.trim().is_empty() {
            return Err(AoError::ValidationError("Tasklist title is required".into()));
        }
        if groups.is_empty() && !allow_empty {
            return Err(AoError::ValidationError(
                "Tasklist must have at least one group".into(),
            ));
        }

        match &owner {
            // `TasklistOwner::Team` is retained only so tasklists already on
            // disk still deserialize (the variant is `#[serde(tag = "kind")]`
            // and persisted). Nothing can create one any more — there is no
            // team store left to validate against — so creation is refused
            // outright rather than validated.
            TasklistOwner::Team { team_id } => {
                return Err(AoError::ValidationError(format!(
                    "Team-scoped tasklists are no longer supported (team '{team_id}')"
                )));
            }
            TasklistOwner::Agent { agent_id } => {
                self.persistence
                    .agents
                    .get(agent_id)
                    .await?
                    .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;
            }
        }

        let tasklist_id = Uuid::new_v4().to_string();

        let (workspace_dir, transcripts_dir) = match &owner {
            TasklistOwner::Team { team_id } => (
                self.persistence
                    .data_root
                    .tasklist_workspace_dir(team_id, &tasklist_id),
                self.persistence
                    .data_root
                    .tasklist_transcripts_dir(team_id, &tasklist_id),
            ),
            TasklistOwner::Agent { agent_id } => (
                self.persistence
                    .data_root
                    .agent_tasklist_workspace_dir(agent_id, &tasklist_id),
                self.persistence
                    .data_root
                    .agent_tasklist_transcripts_dir(agent_id, &tasklist_id),
            ),
        };

        let initial_status = if groups.is_empty() {
            TasklistStatus::Paused
        } else {
            TasklistStatus::Active
        };

        let team_id_compat = match &owner {
            TasklistOwner::Team { team_id } => Some(team_id.clone()),
            TasklistOwner::Agent { .. } => None,
        };

        let tasklist = Tasklist {
            id: tasklist_id.clone(),
            owner: owner.clone(),
            team_id: team_id_compat,
            title,
            description,
            status: initial_status,
            groups,
            workspace_dir: workspace_dir.to_string_lossy().to_string(),
            transcripts_dir: transcripts_dir.to_string_lossy().to_string(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        match &owner {
            TasklistOwner::Team { .. } => {
                self.persistence.tasklists.create(&tasklist).await?;
            }
            TasklistOwner::Agent { agent_id } => {
                self.persistence.tasklists.create_for_agent(&tasklist).await?;
                if initial_status == TasklistStatus::Active {
                    if let Some(reg) = self.instance_registry.as_ref() {
                        reg.mark_has_active_tasklist(agent_id).await;
                    }
                }
            }
        }

        // Announce the new tasklist on the owner's event channel for BOTH owner
        // kinds. Team tasklists land on `team:{team_id}`; agent-owned tasklists
        // land on the owning agent's channel. Without this the frontend has no
        // deterministic "tasklist created" signal to hydrate the Todo panel for
        // agent-owned lists — it would only ever populate by racing the
        // mount-time refetch, so a list could finish before the panel showed it.
        // The `owner` field is the canonical routing key; the legacy `team_id`
        // string stays empty for agent owners (subscribers filter on `owner`).
        {
            let event_channel = Self::event_agent_id(&owner);
            let synth_run_id = format!("tasklist:{}", tasklist_id);
            let team_id_for_event = match &owner {
                TasklistOwner::Team { team_id } => team_id.clone(),
                TasklistOwner::Agent { .. } => String::new(),
            };
            self.event_bus
                .emit(
                    &synth_run_id,
                    &event_channel,
                    None,
                    AgentEventPayload::TasklistCreated {
                        team_id: team_id_for_event,
                        tasklist_id: tasklist_id.clone(),
                        owner: Some(owner.clone()),
                        // project_id is not yet stamped at this point for
                        // project tasklists (create_for_project stamps it
                        // after this returns). The frontend defensive check in
                        // applyTasklistCreated handles the race by inspecting
                        // the fetched tasklist's project_id field.
                        project_id: None,
                    },
                )
                .await;
        }

        if let Err(e) = self.feeder.advance(&tasklist).await {
            tracing::warn!(
                tasklist_id = %tasklist_id,
                "tasklist_service::create: initial advance failed: {}",
                e
            );
        }

        Ok(tasklist)
    }

    /// Get the active (non-terminal) tasklist for the given owner.
    pub async fn active(&self, owner: &TasklistOwner) -> Result<Option<Tasklist>, AoError> {
        match owner {
            TasklistOwner::Team { team_id } => {
                self.persistence.tasklists.find_active(team_id).await
            }
            TasklistOwner::Agent { agent_id } => {
                self.persistence.tasklists.active_for_agent(agent_id).await
            }
        }
    }

    /// List all tasklists for an owner (newest-first).
    pub async fn list(&self, owner: &TasklistOwner) -> Result<Vec<Tasklist>, AoError> {
        match owner {
            TasklistOwner::Team { team_id } => self.persistence.tasklists.list(team_id).await,
            TasklistOwner::Agent { agent_id } => {
                self.persistence.tasklists.list_for_agent(agent_id).await
            }
        }
    }

    /// Get a specific tasklist by ID.
    pub async fn get(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
    ) -> Result<Option<Tasklist>, AoError> {
        match owner {
            TasklistOwner::Team { team_id } => {
                self.persistence.tasklists.get(team_id, tasklist_id).await
            }
            TasklistOwner::Agent { agent_id } => {
                self.persistence
                    .tasklists
                    .get_for_agent(agent_id, tasklist_id)
                    .await
            }
        }
    }

    /// Append a task to an existing tasklist.
    /// Handles attachment resolution, copilot remind_me stamping, auto-resume
    /// for terminal tasklists, SSE events, feeder advance, and coordinator routing
    /// for unowned tasks (Team path only).
    pub async fn add_tasks(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        prompt: String,
        owner_agent_id: Option<String>,
        expected_outputs: Vec<String>,
        mode: TaskGroupMode,
        attachment_ids: Vec<String>,
    ) -> Result<Tasklist, AoError> {
        if prompt.trim().is_empty() {
            return Err(AoError::ValidationError("Task prompt is required".into()));
        }

        let new_task_id = Uuid::new_v4().to_string();
        let new_group_id = Uuid::new_v4().to_string();
        let owner_agent_id = owner_agent_id.unwrap_or_default();
        let owner_was_unset = owner_agent_id.is_empty();

        let mut expected_outputs = expected_outputs;
        ao_protocol::tasklist::prefix_expected_outputs(&new_task_id, &mut expected_outputs);

        let attachments = if attachment_ids.is_empty() {
            Vec::new()
        } else {
            let asset_key = match owner {
                TasklistOwner::Team { team_id } => format!("team_{}", team_id),
                TasklistOwner::Agent { agent_id } => format!("agent_{}", agent_id),
            };
            let all = self.persistence.assets.list_files(&asset_key).await?;
            let id_set: std::collections::HashSet<&str> =
                attachment_ids.iter().map(|s| s.as_str()).collect();
            all.into_iter()
                .filter(|a| id_set.contains(a.id.as_str()))
                .collect()
        };

        let copilot_agent_id: Option<String> = match owner {
            TasklistOwner::Team { team_id } => {
                self.persistence
                    .tasklists
                    .get(team_id, tasklist_id)
                    .await?
                    .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.to_string()))?
                    .copilot_agent_id
            }
            TasklistOwner::Agent { .. } => None,
        };

        tracing::debug!(
            tasklist_id = %tasklist_id,
            task_id = %new_task_id,
            remind_me_set = copilot_agent_id.is_some(),
            target = copilot_agent_id.as_deref().unwrap_or("(none)"),
            "TasklistService::add_tasks: stamped remind_me",
        );

        let mut new_task = Task {
            id: new_task_id.clone(),
            owner_agent_id,
            prompt,
            expected_outputs,
            status: TaskStatus::Pending,
            group_id: String::new(),
            attempt_count: 0,
            error_log: Vec::new(),
            comments: Vec::new(),
            attachments,
            remind_me: copilot_agent_id,
            parse_failed: false,
            notification_parse_retry_count: 0,
            assignment: None,
            classifier_token: 0,
            dispatch_token: 0,
        };

        let mut revived_to_paused = false;
        let mut revived_to_active = false;

        const AUTO_RESUME_WINDOW: chrono::Duration = chrono::Duration::minutes(8);

        let team_active_slot_taken = match owner {
            TasklistOwner::Team { team_id } => self
                .persistence
                .tasklists
                .find_active(team_id)
                .await?
                .map(|other| other.id != tasklist_id)
                .unwrap_or(false),
            TasklistOwner::Agent { .. } => false,
        };

        let updated = match owner {
            TasklistOwner::Team { team_id } => {
                self.persistence
                    .tasklists
                    .mutate(team_id, tasklist_id, |tl| {
                        let last_matches =
                            tl.groups.last().map(|g| g.mode == mode).unwrap_or(false);
                        if last_matches {
                            let group = tl.groups.last_mut().expect("checked above");
                            new_task.group_id = group.id.clone();
                            group.tasks.push(new_task.clone());
                        } else {
                            new_task.group_id = new_group_id.clone();
                            tl.groups.push(TaskGroup {
                                id: new_group_id.clone(),
                                mode,
                                tasks: vec![new_task.clone()],
                            });
                        }
                        match tl.status {
                            TasklistStatus::Completed => {
                                let within_window = tl
                                    .last_active_at
                                    .map(|t| {
                                        Utc::now().signed_duration_since(t) < AUTO_RESUME_WINDOW
                                    })
                                    .unwrap_or(false);
                                if within_window && !team_active_slot_taken {
                                    tl.status = TasklistStatus::Active;
                                    revived_to_active = true;
                                } else {
                                    tl.status = TasklistStatus::Paused;
                                    revived_to_paused = true;
                                }
                            }
                            TasklistStatus::Failed | TasklistStatus::Cancelled => {
                                tl.status = TasklistStatus::Paused;
                                revived_to_paused = true;
                            }
                            _ => {}
                        }
                        Ok(())
                    })
                    .await?
            }
            TasklistOwner::Agent { agent_id } => {
                self.persistence
                    .tasklists
                    .mutate_for_agent(agent_id, tasklist_id, |tl| {
                        let last_matches =
                            tl.groups.last().map(|g| g.mode == mode).unwrap_or(false);
                        if last_matches {
                            let group = tl.groups.last_mut().expect("checked above");
                            new_task.group_id = group.id.clone();
                            group.tasks.push(new_task.clone());
                        } else {
                            new_task.group_id = new_group_id.clone();
                            tl.groups.push(TaskGroup {
                                id: new_group_id.clone(),
                                mode,
                                tasks: vec![new_task.clone()],
                            });
                        }
                        Ok(())
                    })
                    .await?
            }
        };

        if let TasklistOwner::Team { team_id } = owner {
            let event_agent_id = Self::event_agent_id(owner);
            let synth_run_id = format!("tasklist:{}", tasklist_id);

            if revived_to_paused || revived_to_active {
                let status_str = if revived_to_active { "active" } else { "paused" };
                self.event_bus
                    .emit(
                        &synth_run_id,
                        &event_agent_id,
                        None,
                        AgentEventPayload::TasklistStatusChanged {
                            team_id: team_id.clone(),
                            tasklist_id: tasklist_id.to_string(),
                            status: status_str.to_string(),
                            owner: Some(owner.clone()),
                            project_id: None,
                        },
                    )
                    .await;
            }
            self.event_bus
                .emit(
                    &synth_run_id,
                    &event_agent_id,
                    None,
                    AgentEventPayload::TasklistTaskAdded {
                        team_id: team_id.clone(),
                        tasklist_id: tasklist_id.to_string(),
                        task_id: new_task_id.clone(),
                        owner: Some(owner.clone()),
                        project_id: None,
                    },
                )
                .await;
            tasklist_lifecycle::emit_wake(
                &self.event_bus,
                team_id,
                tasklist_id,
                tasklist_lifecycle::WakeReason::TaskAdded,
            )
            .await;
        }

        // For agent-owned project-stamped tasklists, mirror the task-added event
        // and any status-revival event onto the project SSE channel so the
        // frontend panel receives them.
        if let TasklistOwner::Agent { .. } = owner {
            if let Some(ref pid) = updated.project_id {
                let project_channel = format!("project:{}", pid);
                let synth_run_id = format!("tasklist:{}", tasklist_id);
                let event_agent_id = Self::event_agent_id(owner);

                if revived_to_paused || revived_to_active {
                    let status_str = if revived_to_active { "active" } else { "paused" };
                    self.event_bus
                        .emit(
                            &synth_run_id,
                            &project_channel,
                            None,
                            AgentEventPayload::TasklistStatusChanged {
                                team_id: String::new(),
                                tasklist_id: tasklist_id.to_string(),
                                status: status_str.to_string(),
                                owner: Some(owner.clone()),
                                project_id: Some(pid.clone()),
                            },
                        )
                        .await;
                }
                self.event_bus
                    .emit(
                        &synth_run_id,
                        &project_channel,
                        None,
                        AgentEventPayload::TasklistTaskAdded {
                            team_id: String::new(),
                            tasklist_id: tasklist_id.to_string(),
                            task_id: new_task_id.clone(),
                            owner: Some(owner.clone()),
                            project_id: Some(pid.clone()),
                        },
                    )
                    .await;
                // Also emit on the agent channel — project_id is set so the
                // per-agent chat SSE handler skips it (no leak into agent store).
                self.event_bus
                    .emit(
                        &synth_run_id,
                        &event_agent_id,
                        None,
                        AgentEventPayload::TasklistTaskAdded {
                            team_id: String::new(),
                            tasklist_id: tasklist_id.to_string(),
                            task_id: new_task_id.clone(),
                            owner: Some(owner.clone()),
                            project_id: Some(pid.clone()),
                        },
                    )
                    .await;
            }
        }

        self.feeder.advance(&updated).await?;

        if owner_was_unset {
            if let TasklistOwner::Team { team_id } = owner {
                self.feeder
                    .note_team_routing_unsupported(team_id, tasklist_id, &new_task_id)
                    .await;
            }
        }

        Ok(updated)
    }

    /// Update task fields (prompt, owner_agent_id, expected_outputs).
    pub async fn update_task(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        task_id: &str,
        prompt: Option<String>,
        owner_agent_id_update: Option<String>,
        expected_outputs: Option<Vec<String>>,
    ) -> Result<Tasklist, AoError> {
        let updated = match owner {
            TasklistOwner::Team { team_id } => {
                let task_id_s = task_id.to_string();
                let p = prompt.clone();
                let o = owner_agent_id_update.clone();
                let e = expected_outputs.clone();
                self.persistence
                    .tasklists
                    .mutate(team_id, tasklist_id, move |tl| {
                        let task = tl
                            .groups
                            .iter_mut()
                            .flat_map(|g| g.tasks.iter_mut())
                            .find(|t| t.id == task_id_s)
                            .ok_or_else(|| AoError::TaskNotFound(task_id_s.clone()))?;
                        if let Some(v) = p {
                            task.prompt = v;
                        }
                        if let Some(v) = o {
                            task.owner_agent_id = v;
                        }
                        if let Some(v) = e {
                            task.expected_outputs = v;
                        }
                        Ok(())
                    })
                    .await?
            }
            TasklistOwner::Agent { agent_id } => {
                let task_id_s = task_id.to_string();
                self.persistence
                    .tasklists
                    .mutate_for_agent(agent_id, tasklist_id, move |tl| {
                        let task = tl
                            .groups
                            .iter_mut()
                            .flat_map(|g| g.tasks.iter_mut())
                            .find(|t| t.id == task_id_s)
                            .ok_or_else(|| AoError::TaskNotFound(task_id_s.clone()))?;
                        if let Some(v) = prompt {
                            task.prompt = v;
                        }
                        if let Some(v) = owner_agent_id_update {
                            task.owner_agent_id = v;
                        }
                        if let Some(v) = expected_outputs {
                            task.expected_outputs = v;
                        }
                        Ok(())
                    })
                    .await?
            }
        };

        {
            let tl_id = tasklist_id.to_owned();
            let t_id = task_id.to_owned();
            self.feeder
                .emit_task_updated(owner, &tl_id, &t_id)
                .await;
        }

        Ok(updated)
    }

    /// Mark a task as completed and advance the list.
    ///
    /// Delegates to [`TaskFeeder::force_complete_and_advance`], which writes
    /// `Completed` to disk *before* running the terminal hook. The earlier
    /// implementation called `on_task_terminal` directly without that write, so
    /// for a SEQ list the terminal hook still saw the task as `InProgress`, the
    /// dispatch guard short-circuited, and TodoComplete returned success while
    /// the queue never advanced. The feeder method also propagates a dispatch
    /// failure as an error so the control tool reports honestly instead of
    /// claiming success against a queue that did not move.
    pub async fn complete_task(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        task_id: &str,
        _summary: Option<String>,
    ) -> Result<(), AoError> {
        let tl_id = tasklist_id.to_owned();
        let t_id = task_id.to_owned();
        self.feeder
            .force_complete_and_advance(owner, &tl_id, &t_id)
            .await
    }

    /// Skip a failed task.
    pub async fn skip_task(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        task_id: &str,
    ) -> Result<Tasklist, AoError> {
        let tl_id = tasklist_id.to_owned();
        let t_id = task_id.to_owned();
        match owner {
            TasklistOwner::Team { team_id } => {
                self.feeder.skip_task(team_id, &tl_id, &t_id).await
            }
            TasklistOwner::Agent { agent_id } => {
                let snapshot = self
                    .persistence
                    .tasklists
                    .get_for_agent(agent_id, &tl_id)
                    .await?
                    .ok_or_else(|| AoError::TasklistNotFound(tl_id.clone()))?;

                let task_status = snapshot
                    .groups
                    .iter()
                    .flat_map(|g| g.tasks.iter())
                    .find(|t| t.id == t_id)
                    .map(|t| t.status)
                    .ok_or_else(|| AoError::TaskNotFound(t_id.clone()))?;

                if task_status != TaskStatus::Pending {
                    return Err(AoError::InvalidTasklistTransition(format!(
                        "cannot skip task {} in status {:?}; only Pending tasks can be skipped on agent-owned tasklists",
                        t_id, task_status
                    )));
                }

                let t_id_inner = t_id.clone();
                let updated = self
                    .persistence
                    .tasklists
                    .mutate_by_owner(owner, &tl_id, move |tl| {
                        let task = tl
                            .groups
                            .iter_mut()
                            .flat_map(|g| g.tasks.iter_mut())
                            .find(|t| t.id == t_id_inner)
                            .ok_or_else(|| AoError::TaskNotFound(t_id_inner.clone()))?;
                        task.status = TaskStatus::Skipped;
                        Ok(())
                    })
                    .await?;

                if matches!(
                    updated.status,
                    TasklistStatus::Active | TasklistStatus::Paused
                ) {
                    if let Err(e) = self.feeder.advance(&updated).await {
                        tracing::warn!(
                            tasklist_id = %tl_id,
                            "agent-scope skip_task: advance failed: {}",
                            e
                        );
                    }
                }

                Ok(updated)
            }
        }
    }

    /// Stop a single in-flight task: flips it to `Stopped`, clears its
    /// assignment, and kills the in-flight CLI run so the executing agent
    /// halts immediately. `Stopped` is non-terminal — `resume_task` re-queues
    /// it as Pending for re-dispatch. Owner-neutral: works for any
    /// `TasklistOwner` since both the store mutation and the queue-manager
    /// kill are keyed by tasklist/task ids.
    pub async fn stop_task(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        task_id: &str,
    ) -> Result<Tasklist, AoError> {
        let tl_id = tasklist_id.to_owned();
        let t_id = task_id.to_owned();

        let snapshot = self
            .get(owner, &tl_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tl_id.clone()))?;

        let task_status = snapshot
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == t_id)
            .map(|t| t.status)
            .ok_or_else(|| AoError::TaskNotFound(t_id.clone()))?;

        if task_status != TaskStatus::InProgress {
            return Err(AoError::InvalidTasklistTransition(format!(
                "cannot stop task '{}' in status {:?}; only InProgress tasks can be stopped",
                t_id, task_status
            )));
        }

        let t_id_inner = t_id.clone();
        let updated = self
            .persistence
            .tasklists
            .mutate_by_owner(owner, &tl_id, move |tl| {
                let task = tl
                    .groups
                    .iter_mut()
                    .flat_map(|g| g.tasks.iter_mut())
                    .find(|t| t.id == t_id_inner)
                    .ok_or_else(|| AoError::TaskNotFound(t_id_inner.clone()))?;
                task.status = TaskStatus::Stopped;
                task.assignment = None;
                // Bump the token so any in-flight classifier CAS is rejected.
                task.classifier_token += 1;
                Ok(())
            })
            .await?;

        // Kill the in-flight CLI run so the executing agent stops processing.
        // The task is now Stopped (non-terminal) and can be resumed later.
        if let Some(qm) = &self.tasklist_queue_managers {
            qm.cancel_task_if_running(&tl_id, &t_id).await;
        }

        Ok(updated)
    }

    /// Resume a previously stopped task: flips it back to `Pending`, clears
    /// any stale assignment, and advances the feeder so it is re-dispatched.
    pub async fn resume_task(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        task_id: &str,
    ) -> Result<Tasklist, AoError> {
        let tl_id = tasklist_id.to_owned();
        let t_id = task_id.to_owned();

        let snapshot = self
            .get(owner, &tl_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tl_id.clone()))?;

        let task_status = snapshot
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == t_id)
            .map(|t| t.status)
            .ok_or_else(|| AoError::TaskNotFound(t_id.clone()))?;

        if task_status != TaskStatus::Stopped {
            return Err(AoError::InvalidTasklistTransition(format!(
                "cannot resume task '{}' in status {:?}; only Stopped tasks can be resumed",
                t_id, task_status
            )));
        }

        let t_id_inner = t_id.clone();
        let updated = self
            .persistence
            .tasklists
            .mutate_by_owner(owner, &tl_id, move |tl| {
                let task = tl
                    .groups
                    .iter_mut()
                    .flat_map(|g| g.tasks.iter_mut())
                    .find(|t| t.id == t_id_inner)
                    .ok_or_else(|| AoError::TaskNotFound(t_id_inner.clone()))?;
                task.status = TaskStatus::Pending;
                task.assignment = None;
                task.classifier_token += 1;
                Ok(())
            })
            .await?;

        if matches!(updated.status, TasklistStatus::Active | TasklistStatus::Paused) {
            if let Err(e) = self.feeder.advance(&updated).await {
                tracing::warn!(
                    tasklist_id = %tl_id,
                    task_id = %t_id,
                    "resume_task: advance failed: {}",
                    e
                );
            }
        }

        Ok(updated)
    }

    /// Discard/cancel a tasklist by user decision.
    pub async fn stop(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
    ) -> Result<Tasklist, AoError> {
        let tl_id = tasklist_id.to_owned();
        match owner {
            TasklistOwner::Team { team_id } => {
                // Snapshot InProgress task IDs before discarding so we can kill
                // their in-flight CLI runs and set them to Skipped afterward.
                let in_progress_ids: Vec<String> = self
                    .persistence
                    .tasklists
                    .get(team_id, &tl_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|tl| {
                        tl.groups
                            .iter()
                            .flat_map(|g| g.tasks.iter())
                            .filter(|t| t.status == TaskStatus::InProgress)
                            .map(|t| t.id.clone())
                            .collect()
                    })
                    .unwrap_or_default();

                let updated = self.feeder.discard_tasklist(team_id, &tl_id).await?;

                // Kill any in-flight CLI runs and mark those tasks Skipped so
                // they don't linger as zombies in the database.
                for task_id in &in_progress_ids {
                    let _ = self
                        .persistence
                        .tasklists
                        .set_task_status_by_owner(
                            &TasklistOwner::Team { team_id: team_id.clone() },
                            &tl_id,
                            task_id,
                            TaskStatus::Skipped,
                        )
                        .await;
                    if let Some(qm) = &self.tasklist_queue_managers {
                        qm.cancel_task_if_running(&tl_id, task_id).await;
                    }
                }

                Ok(updated)
            }
            TasklistOwner::Agent { .. } => {
                self.cancel_agent_tasklist(owner, &tl_id).await
            }
        }
    }

    /// Cancel an agent-owned tasklist: flip to Cancelled, mark all
    /// Pending/Blocked/InProgress tasks as Skipped, and kill any in-flight
    /// CLI runs so the subprocess stops rather than running to completion.
    async fn cancel_agent_tasklist(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
    ) -> Result<Tasklist, AoError> {
        let tl_id = tasklist_id.to_owned();
        let mut in_progress_ids: Vec<String> = Vec::new();
        let updated = self
            .persistence
            .tasklists
            .mutate_by_owner(owner, &tl_id, |tl| {
                if !matches!(
                    tl.status,
                    TasklistStatus::Active | TasklistStatus::Paused | TasklistStatus::Failed
                ) {
                    return Err(AoError::InvalidTasklistTransition(format!(
                        "cannot stop tasklist {} in status {:?}; only Active, Paused, or Failed tasklists can be stopped",
                        tl.id, tl.status
                    )));
                }
                tl.status = TasklistStatus::Cancelled;
                for group in &mut tl.groups {
                    for task in &mut group.tasks {
                        if matches!(task.status, TaskStatus::Pending | TaskStatus::Blocked) {
                            task.status = TaskStatus::Skipped;
                        } else if task.status == TaskStatus::InProgress {
                            // Collect before setting so we can send cancel signals below.
                            in_progress_ids.push(task.id.clone());
                            task.status = TaskStatus::Skipped;
                        }
                    }
                }
                Ok(())
            })
            .await?;

        // Send kill signals to each in-flight queue manager run. This causes
        // the CLI subprocess to be terminated instead of running to completion.
        if let Some(qm) = &self.tasklist_queue_managers {
            for task_id in &in_progress_ids {
                qm.cancel_task_if_running(&tl_id, task_id).await;
            }
        }

        if let (TasklistOwner::Agent { agent_id }, Some(reg)) =
            (owner, self.instance_registry.as_ref())
        {
            reg.clear_has_active_tasklist(agent_id).await;
        }
        self.feeder.fire_terminal_watcher(&updated).await;
        Ok(updated)
    }

    /// Pause or resume a tasklist.
    pub async fn set_status(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        status: &str,
    ) -> Result<Tasklist, AoError> {
        let tl_id = tasklist_id.to_owned();
        match owner {
            TasklistOwner::Team { team_id } => match status {
                "paused" => self.feeder.pause(team_id, &tl_id).await,
                "active" => self.feeder.resume(team_id, &tl_id).await,
                other => Err(AoError::ValidationError(format!(
                    "Unsupported status '{}': only 'active' or 'paused' are accepted",
                    other
                ))),
            },
            TasklistOwner::Agent { .. } => match status {
                "active" => self.resume_agent_tasklist(owner, &tl_id).await,
                "stopped" => self.cancel_agent_tasklist(owner, &tl_id).await,
                other => Err(AoError::ValidationError(format!(
                    "Unsupported status '{}': only 'active' or 'stopped' are accepted for agent-owned tasklists",
                    other
                ))),
            },
        }
    }

    /// Start (or resume) an agent-owned tasklist: flip Paused→Active, claim the
    /// agent's active-tasklist slot, announce the status change, then kick the
    /// feeder so newly-runnable tasks dispatch. This is the agent-scope analog
    /// of the team `feeder.resume()` path — it backs the user-facing "Start"
    /// action on a drafted (empty-then-populated) Todo list, where items are
    /// staged while Paused and only execute once the user commits.
    ///
    /// Idempotent on an already-Active list (re-kicks `advance()` without
    /// error); rejects terminal lists, which must be replayed rather than
    /// resumed.
    async fn resume_agent_tasklist(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
    ) -> Result<Tasklist, AoError> {
        let tl_id = tasklist_id.to_owned();
        let updated = self
            .persistence
            .tasklists
            .mutate_by_owner(owner, &tl_id, |tl| {
                if !matches!(
                    tl.status,
                    TasklistStatus::Paused | TasklistStatus::Active
                ) {
                    return Err(AoError::InvalidTasklistTransition(format!(
                        "cannot start tasklist {} in status {:?}; only Paused tasklists can be started",
                        tl.id, tl.status
                    )));
                }
                tl.status = TasklistStatus::Active;
                Ok(())
            })
            .await?;

        if let (TasklistOwner::Agent { agent_id }, Some(reg)) =
            (owner, self.instance_registry.as_ref())
        {
            reg.mark_has_active_tasklist(agent_id).await;
        }

        self.feeder
            .emit_tasklist_status_changed(owner, &tl_id, TasklistStatus::Active)
            .await;

        if let Err(e) = self.feeder.advance(&updated).await {
            tracing::warn!(
                tasklist_id = %tl_id,
                "agent-scope resume: advance failed: {}",
                e
            );
        }

        Ok(updated)
    }

    /// Determine what `start_for_agent` actually accomplished by diffing
    /// `<workspace>/tasks/{task_id}/` directory existence against the
    /// `existed_before` snapshot taken prior to kicking the feeder.
    ///
    /// This — not `attempt_count`, and not the in-memory `Tasklist` returned
    /// from the resume/re-kick call (which is captured before `advance()`
    /// runs and so never reflects a dispatch that just happened) — is the
    /// ground truth for "was a task genuinely handed to the feeder this
    /// call". See [`StartOutcomeKind`] docs for the full rationale.
    async fn classify_start_outcome(
        &self,
        agent_id: &str,
        tl_id: &str,
        candidate_ids: &[String],
        existed_before: &std::collections::HashSet<String>,
    ) -> Result<StartOutcome, AoError> {
        let data_root = self.persistence.tasklists.data_root();
        let mut dispatched_ids = Vec::new();
        for task_id in candidate_ids {
            if existed_before.contains(task_id) {
                continue;
            }
            if tokio::fs::try_exists(data_root.agent_tasklist_task_dir(agent_id, tl_id, task_id))
                .await
                .unwrap_or(false)
            {
                dispatched_ids.push(task_id.clone());
            }
        }

        if !dispatched_ids.is_empty() {
            return Ok(StartOutcome {
                tasklist_id: tl_id.to_string(),
                kind: StartOutcomeKind::Dispatched {
                    task_ids: dispatched_ids,
                },
            });
        }

        // Nothing newly dispatched. Re-fetch live state — rather than trust
        // the possibly-stale `tl`/`updated` snapshot from before the kick —
        // to tell "something is already in flight" from "nothing left to
        // run" from "a ready task sat there and the feeder never touched it".
        let fresh = self
            .persistence
            .tasklists
            .active_for_agent(agent_id)
            .await?
            .filter(|t| t.id == tl_id)
            .ok_or_else(|| AoError::TasklistNotFound(tl_id.to_string()))?;
        let all_tasks: Vec<&Task> = fresh.groups.iter().flat_map(|g| &g.tasks).collect();

        if all_tasks.iter().any(|t| t.status == TaskStatus::InProgress) {
            return Ok(StartOutcome {
                tasklist_id: tl_id.to_string(),
                kind: StartOutcomeKind::AlreadyRunning,
            });
        }

        let pending: Vec<&Task> = all_tasks
            .iter()
            .copied()
            .filter(|t| t.status == TaskStatus::Pending)
            .collect();
        if pending.is_empty() {
            return Ok(StartOutcome {
                tasklist_id: tl_id.to_string(),
                kind: StartOutcomeKind::NoPending,
            });
        }

        // At least one Pending task remains, nothing is in flight, and the
        // kick above created no new task workspace dir. If any of those
        // pending tasks already has a resolved assignment (i.e. it was ready
        // to run, not merely awaiting classifier routing), the feeder failed
        // to dispatch it — surface that as a real failure instead of the
        // fixed "active" success payload TodoStart used to return regardless
        // of outcome.
        let has_ready_pending = pending.iter().any(|t| t.assignment.is_some());
        if has_ready_pending {
            return Err(AoError::Internal(format!(
                "start_for_agent: tasklist '{}' has a ready pending task but the feeder \
                 dispatched nothing this call; the dispatcher may be unavailable",
                tl_id
            )));
        }

        // Every remaining pending task is awaiting classifier routing
        // (`assignment: None`) — that's routing in progress, not a stalled
        // dispatch, so it doesn't warrant a false failure.
        Ok(StartOutcome {
            tasklist_id: tl_id.to_string(),
            kind: StartOutcomeKind::NoPending,
        })
    }

    /// Continue a failed tasklist (resets Failed tasks to Pending and re-dispatches).
    pub async fn continue_failed(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
    ) -> Result<Tasklist, AoError> {
        let tl_id = tasklist_id.to_owned();
        match owner {
            TasklistOwner::Team { team_id } => {
                self.feeder.continue_tasklist(team_id, &tl_id).await
            }
            TasklistOwner::Agent { .. } => Err(AoError::Internal(
                "agent-scope continue not yet implemented".into(),
            )),
        }
    }

    /// Clone a terminal tasklist into a fresh one.
    pub async fn replay(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
    ) -> Result<Tasklist, AoError> {
        let tl_id = tasklist_id.to_owned();
        match owner {
            TasklistOwner::Team { team_id } => {
                self.feeder.replay_tasklist(team_id, &tl_id).await
            }
            TasklistOwner::Agent { .. } => Err(AoError::Internal(
                "agent-scope replay not yet implemented".into(),
            )),
        }
    }

    /// Attach a comment to a task. For user-authored comments on unowned tasks
    /// (Team path), re-submits the task to the routing classifier.
    pub async fn add_comment(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        task_id: &str,
        comment: TaskComment,
    ) -> Result<TaskComment, AoError> {
        let author_kind = comment.author_kind;
        let task_id_for_check = task_id.to_string();

        let updated = match owner {
            TasklistOwner::Team { team_id } => {
                let stored = comment.clone();
                let task_id_s = task_id.to_string();
                self.persistence
                    .tasklists
                    .mutate(team_id, tasklist_id, move |tl| {
                        let task = tl
                            .groups
                            .iter_mut()
                            .flat_map(|g| g.tasks.iter_mut())
                            .find(|t| t.id == task_id_s)
                            .ok_or_else(|| AoError::TaskNotFound(task_id_s.clone()))?;
                        task.comments.push(stored);
                        Ok(())
                    })
                    .await?
            }
            TasklistOwner::Agent { agent_id } => {
                let stored = comment.clone();
                let task_id_s = task_id.to_string();
                self.persistence
                    .tasklists
                    .mutate_for_agent(agent_id, tasklist_id, move |tl| {
                        let task = tl
                            .groups
                            .iter_mut()
                            .flat_map(|g| g.tasks.iter_mut())
                            .find(|t| t.id == task_id_s)
                            .ok_or_else(|| AoError::TaskNotFound(task_id_s.clone()))?;
                        task.comments.push(stored);
                        Ok(())
                    })
                    .await?
            }
        };

        {
            let tl_id = tasklist_id.to_owned();
            let t_id = task_id.to_owned();
            self.feeder
                .emit_task_updated(owner, &tl_id, &t_id)
                .await;
        }

        if matches!(author_kind, TaskCommentAuthorKind::User) {
            if let TasklistOwner::Team { team_id } = owner {
                let task_unowned = updated
                    .groups
                    .iter()
                    .flat_map(|g| g.tasks.iter())
                    .find(|t| t.id == task_id_for_check)
                    .map(|t| t.owner_agent_id.is_empty())
                    .unwrap_or(false);
                if task_unowned {
                    self.feeder
                        .note_team_routing_unsupported(team_id, tasklist_id, &task_id_for_check)
                        .await;
                }
            }
        }

        Ok(comment)
    }
}

#[async_trait]
impl TasklistServiceHandle for TasklistService {
    async fn agent_active(&self, agent_id: &str) -> Result<Option<Tasklist>, AoError> {
        self.persistence.tasklists.active_for_agent(agent_id).await
    }

    async fn create_for_agent(
        &self,
        agent_id: &str,
        name: String,
        groups: Vec<TaskGroup>,
    ) -> Result<Tasklist, AoError> {
        self.create(
            TasklistOwner::Agent {
                agent_id: agent_id.to_string(),
            },
            name,
            String::new(),
            groups,
            false,
        )
        .await
    }

    async fn create_for_agent_with_project(
        &self,
        agent_id: &str,
        name: String,
        groups: Vec<TaskGroup>,
        project_id: Option<String>,
        thread_id: Option<String>,
    ) -> Result<Tasklist, AoError> {
        if project_id.is_none() && thread_id.is_none() {
            return self.create_for_agent(agent_id, name, groups).await;
        }
        let mut tl = self.create_for_agent(agent_id, name, groups).await?;
        // Stamp both tags atomically (single write) before any feeder/watcher
        // can read the tasklist.
        let pid_clone = project_id.clone();
        let tid_clone = thread_id.clone();
        self.persistence
            .tasklists
            .mutate_for_agent(agent_id, &tl.id, move |t| {
                if let Some(pid) = &pid_clone {
                    t.project_id = Some(pid.clone());
                }
                if let Some(tid) = &tid_clone {
                    t.thread_id = Some(tid.clone());
                }
                Ok(())
            })
            .await?;
        if let Some(pid) = &project_id {
            tl.project_id = Some(pid.clone());
        }
        if let Some(tid) = &thread_id {
            tl.thread_id = Some(tid.clone());
        }
        // Emit TasklistCreated on the project channel now that the stamp is
        // set. Thread-only stamps (no project) don't change routing, so no
        // extra emit is needed in that case — the standard agent-scoped
        // TasklistCreated emit from `create_for_agent` above already covers it.
        if let Some(pid) = &project_id {
            let owner = TasklistOwner::Agent {
                agent_id: agent_id.to_string(),
            };
            self.event_bus
                .emit(
                    &format!("tasklist:{}", tl.id),
                    &format!("project:{}", pid),
                    None,
                    AgentEventPayload::TasklistCreated {
                        team_id: String::new(),
                        tasklist_id: tl.id.clone(),
                        owner: Some(owner),
                        project_id: Some(pid.clone()),
                    },
                )
                .await;
        }
        Ok(tl)
    }

    async fn get_agent_max_instances(&self, agent_id: &str) -> Result<u32, AoError> {
        let profile = self
            .persistence
            .agents
            .get(agent_id)
            .await?
            .ok_or_else(|| AoError::AgentNotFound(agent_id.to_string()))?;
        Ok(profile.max_instances)
    }

    async fn add_group_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        mut tasks: Vec<Task>,
        mode: TaskGroupMode,
    ) -> Result<Tasklist, AoError> {
        let group_id = Uuid::new_v4().to_string();
        for task in &mut tasks {
            task.group_id = group_id.clone();
        }
        // Collect task IDs before tasks is moved into the new group.
        let task_ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
        let new_group = TaskGroup { id: group_id, mode, tasks };
        let updated = self
            .persistence
            .tasklists
            .mutate_for_agent(agent_id, tasklist_id, move |tl| {
                tl.groups.push(new_group);
                Ok(())
            })
            .await?;

        // Fan out TasklistTaskAdded onto every channel that has a live consumer:
        //   1. tasklist:{id}   — open TodoPanel's useAgentTasklistRunSSE
        //   2. agent channel   — always-on useSSE.ts keeps agentTasklistStore current
        //   3. project:{pid}   — project chat panel (project-scoped tasklists only)
        // project_id is populated on every emit so the agent-channel handler
        // can skip project-scoped events (no leak into per-agent chat).
        let owner = TasklistOwner::Agent { agent_id: agent_id.to_string() };
        let synth_run_id = format!("tasklist:{}", tasklist_id);
        let tasklist_channel = format!("tasklist:{}", tasklist_id);
        let agent_channel = Self::event_agent_id(&owner);
        let project_channel = updated.project_id.as_ref().map(|pid| format!("project:{}", pid));
        for task_id in &task_ids {
            self.event_bus
                .emit(
                    &synth_run_id,
                    &tasklist_channel,
                    None,
                    AgentEventPayload::TasklistTaskAdded {
                        team_id: String::new(),
                        tasklist_id: tasklist_id.to_string(),
                        task_id: task_id.clone(),
                        owner: Some(owner.clone()),
                        project_id: updated.project_id.clone(),
                    },
                )
                .await;
            self.event_bus
                .emit(
                    &synth_run_id,
                    &agent_channel,
                    None,
                    AgentEventPayload::TasklistTaskAdded {
                        team_id: String::new(),
                        tasklist_id: tasklist_id.to_string(),
                        task_id: task_id.clone(),
                        owner: Some(owner.clone()),
                        project_id: updated.project_id.clone(),
                    },
                )
                .await;
            if let Some(ref project_ch) = project_channel {
                self.event_bus
                    .emit(
                        &synth_run_id,
                        project_ch,
                        None,
                        AgentEventPayload::TasklistTaskAdded {
                            team_id: String::new(),
                            tasklist_id: tasklist_id.to_string(),
                            task_id: task_id.clone(),
                            owner: Some(owner.clone()),
                            project_id: updated.project_id.clone(),
                        },
                    )
                    .await;
            }
        }

        self.feeder.advance(&updated).await?;
        Ok(updated)
    }

    async fn update_task_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
        prompt: Option<String>,
        owner_agent_id: Option<String>,
        expected_outputs: Option<Vec<String>>,
    ) -> Result<Tasklist, AoError> {
        let updated = self
            .update_task(
                &TasklistOwner::Agent { agent_id: agent_id.to_string() },
                tasklist_id,
                task_id,
                prompt,
                owner_agent_id,
                expected_outputs,
            )
            .await?;

        // Emit TasklistTaskUpdated on the tasklist-scoped SSE channel so the
        // frontend TodoPanel refreshes live. update_task() already emits on
        // the parent agent channel via feeder.emit_task_updated(); this
        // additional emit covers the dedicated tasklist stream that
        // useAgentTasklistRunSSE subscribes to.
        if let Some(task) = updated
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == task_id)
        {
            let owner = TasklistOwner::Agent { agent_id: agent_id.to_string() };
            let tasklist_channel = format!("tasklist:{}", tasklist_id);
            self.event_bus
                .emit(
                    &tasklist_channel,
                    &tasklist_channel,
                    None,
                    AgentEventPayload::TasklistTaskUpdated {
                        team_id: String::new(),
                        tasklist_id: tasklist_id.to_string(),
                        task: task.clone(),
                        owner: Some(owner),
                        project_id: None,
                    },
                )
                .await;
        }

        Ok(updated)
    }

    async fn complete_task_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
    ) -> Result<(), AoError> {
        self.complete_task(
            &TasklistOwner::Agent { agent_id: agent_id.to_string() },
            tasklist_id,
            task_id,
            None,
        )
        .await
    }

    async fn add_comment_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
        body: String,
    ) -> Result<TaskComment, AoError> {
        let comment = TaskComment {
            id: Uuid::new_v4().to_string(),
            author_id: agent_id.to_string(),
            author_kind: TaskCommentAuthorKind::Agent,
            body,
            created_at: Utc::now(),
        };
        self.add_comment(
            &TasklistOwner::Agent { agent_id: agent_id.to_string() },
            tasklist_id,
            task_id,
            comment,
        )
        .await
    }

    async fn terminal_watcher(
        &self,
        tasklist_id: &str,
    ) -> Result<TerminalWatcherGuard, AoError> {
        Ok(self.feeder.register_terminal_watcher(tasklist_id))
    }

    async fn cancel_for_agent(&self, agent_id: &str) -> Result<CancelOutcome, AoError> {
        let tl = match self.persistence.tasklists.active_for_agent(agent_id).await? {
            Some(tl) => tl,
            None => {
                return Err(AoError::ValidationError(format!(
                    "agent '{}' has no active tasklist to cancel",
                    agent_id
                )));
            }
        };
        let tasklist_id = tl.id.clone();
        let owner = TasklistOwner::Agent { agent_id: agent_id.to_string() };
        let updated = self.cancel_agent_tasklist(&owner, &tasklist_id).await?;

        let skipped_count = updated
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .filter(|t| t.status == TaskStatus::Skipped)
            .count();
        let in_flight_count = updated
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .filter(|t| t.status == TaskStatus::InProgress)
            .count();

        let progress_path = self
            .persistence
            .data_root
            .agent_tasklist_progress_log(agent_id, &tasklist_id);
        let block = ProgressBlock {
            task_id: None,
            title: None,
            status: "cancelled".to_string(),
            summary: None,
            started_at: None,
            ended_at: Some(Utc::now().to_rfc3339()),
            output_path: None,
            attempt_count: None,
        };
        if let Err(e) = append_progress_block(&progress_path, &block).await {
            tracing::warn!(
                agent_id = %agent_id,
                tasklist_id = %tasklist_id,
                "cancel_for_agent: failed to write cancelled block to progress.jsonl: {e}",
            );
        }

        Ok(CancelOutcome { tasklist_id, skipped_count, in_flight_count })
    }

    async fn set_assignment(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
        assignment: Option<TaskAssignment>,
        expected_token: u64,
    ) -> Result<bool, AoError> {
        let mut did_update = false;
        let updated = self
            .persistence
            .tasklists
            .mutate_for_agent(agent_id, tasklist_id, |tl| {
                for group in &mut tl.groups {
                    for task in &mut group.tasks {
                        if task.id == task_id {
                            if task.classifier_token != expected_token {
                                // Stale token: a newer classifier or edit already landed.
                                return Ok(());
                            }
                            task.assignment = assignment.clone();
                            task.classifier_token += 1;
                            did_update = true;
                            return Ok(());
                        }
                    }
                }
                // Task not found — already deleted; treat as stale.
                Ok(())
            })
            .await?;

        // Emit a TasklistTaskUpdated event when the CAS write actually
        // landed AND re-drive the feeder so the newly-classified task can
        // enter the dispatch slot. Without `advance()`, classification is
        // functionally inert in SEQ mode — `dispatch_group(SEQ)` only emits
        // `awaiting_classification` and returns when it encounters a
        // `Pending` task whose `assignment` is `None`, and nothing else
        // re-drives the feeder once the classifier writes back. The same
        // re-drive is required after a classifier-induced None→Some flip
        // and a manual re-assignment, so we always advance on `did_update`.
        if did_update {
            let owner = TasklistOwner::Agent {
                agent_id: agent_id.to_string(),
            };
            self.feeder
                .emit_task_updated(&owner, &tasklist_id.to_string(), &task_id.to_string())
                .await;
            if let Err(e) = self.feeder.advance(&updated).await {
                tracing::warn!(
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    "tasklist_service::set_assignment: post-classify advance failed: {}",
                    e
                );
            }
        }

        Ok(did_update)
    }

    async fn start_for_agent(&self, agent_id: &str) -> Result<StartOutcome, AoError> {
        let tl = match self.persistence.tasklists.active_for_agent(agent_id).await? {
            Some(t) => t,
            None => {
                return Err(AoError::InvalidTasklistTransition(format!(
                    "agent '{}' has no active or paused tasklist to start",
                    agent_id
                )));
            }
        };
        let tl_id = tl.id.clone();
        let owner = TasklistOwner::Agent { agent_id: agent_id.to_string() };

        // Ground-truth snapshot, taken *before* kicking the feeder: which
        // non-terminal tasks already have a `<workspace>/tasks/{task_id}/`
        // directory on disk. `attempt_count` cannot answer "did this call
        // dispatch anything" (it's a retry counter, 0 on a clean dispatch),
        // but that directory is created synchronously by
        // `TaskFeeder::dispatch_one`'s "started" hook, before the dispatcher
        // is ever invoked — so diffing its existence before/after is the
        // reliable signal `classify_start_outcome` below relies on.
        let data_root = self.persistence.tasklists.data_root();
        let candidate_ids: Vec<String> = tl
            .groups
            .iter()
            .flat_map(|g| &g.tasks)
            .filter(|t| !t.status.is_terminal())
            .map(|t| t.id.clone())
            .collect();
        let mut existed_before = std::collections::HashSet::with_capacity(candidate_ids.len());
        for task_id in &candidate_ids {
            if tokio::fs::try_exists(data_root.agent_tasklist_task_dir(agent_id, &tl_id, task_id))
                .await
                .unwrap_or(false)
            {
                existed_before.insert(task_id.clone());
            }
        }

        if tl.status == TasklistStatus::Active {
            // Already active — idempotent re-kick. Use `kick_and_reconcile`
            // rather than a bare `advance`: a plain advance against a list whose
            // head is a zombie `InProgress` task (runner died without reporting
            // terminal) is a guaranteed no-op, because the SEQ guard counts the
            // dead task as in-flight. `kick_and_reconcile` first verifies runner
            // liveness and recovers/redispatches any zombie before advancing, so
            // TodoStart can actually un-stick a stalled list instead of assuming
            // a live runner exists.
            match self.feeder.kick_and_reconcile(&owner, &tl_id).await {
                Ok(recovered) if recovered > 0 => tracing::info!(
                    tasklist_id = %tl_id,
                    recovered,
                    "start_for_agent: re-kick recovered zombie task(s) on already-active list",
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    tasklist_id = %tl_id,
                    "start_for_agent: re-kick on already-active list failed: {}",
                    e
                ),
            }
        } else {
            // Paused → Active
            self.resume_agent_tasklist(&owner, &tl_id).await?;
        }

        self.classify_start_outcome(agent_id, &tl_id, &candidate_ids, &existed_before)
            .await
    }

    async fn resume_for_agent(&self, agent_id: &str) -> Result<ResumeOutcome, AoError> {
        // Guard: reject if the agent already has an Active/Paused tasklist occupying
        // the single active slot. Resuming a Failed list on top of one would
        // violate the one-active-slot invariant.
        if let Some(active) = self.persistence.tasklists.active_for_agent(agent_id).await? {
            return Err(AoError::InvalidTasklistTransition(format!(
                "agent '{}' already has a {} tasklist '{}'; cancel or complete it before resuming a failed one",
                agent_id,
                format!("{:?}", active.status).to_lowercase(),
                active.id,
            )));
        }

        // Find the most recent Failed tasklist (list_for_agent returns newest-first).
        let tl = self
            .persistence
            .tasklists
            .list_for_agent(agent_id)
            .await?
            .into_iter()
            .find(|t| t.status == TasklistStatus::Failed)
            .ok_or_else(|| {
                AoError::InvalidTasklistTransition(format!(
                    "agent '{}' has no failed tasklist to resume",
                    agent_id
                ))
            })?;

        let tl_id = tl.id.clone();
        let owner = TasklistOwner::Agent { agent_id: agent_id.to_string() };

        let mut reset_count: usize = 0;
        let updated = self
            .persistence
            .tasklists
            .mutate_by_owner(&owner, &tl_id, |tl| {
                tl.status = TasklistStatus::Active;
                for group in &mut tl.groups {
                    for task in &mut group.tasks {
                        if task.status == TaskStatus::Failed {
                            task.status = TaskStatus::Pending;
                            task.attempt_count = 0;
                            task.error_log.clear();
                            reset_count += 1;
                        }
                    }
                }
                Ok(())
            })
            .await?;

        let agent_id_owned = agent_id.to_string();
        if let Some(reg) = self.instance_registry.as_ref() {
            reg.mark_has_active_tasklist(&agent_id_owned).await;
        }

        self.feeder
            .emit_tasklist_status_changed(&owner, &tl_id, TasklistStatus::Active)
            .await;

        if let Err(e) = self.feeder.advance(&updated).await {
            tracing::warn!(
                tasklist_id = %tl_id,
                "resume_for_agent: advance failed: {}",
                e
            );
        }

        Ok(ResumeOutcome { tasklist_id: tl_id, reset_count })
    }

    async fn delete_task_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
    ) -> Result<(), AoError> {
        let owner = TasklistOwner::Agent { agent_id: agent_id.to_string() };
        let updated = self.skip_task(&owner, tasklist_id, task_id).await?;

        // Emit TasklistTaskUpdated on the tasklist-scoped SSE channel so the
        // frontend TodoPanel reflects the deletion (Skipped status) in real time.
        if let Some(task) = updated
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == task_id)
        {
            let tasklist_channel = format!("tasklist:{}", tasklist_id);
            self.event_bus
                .emit(
                    &tasklist_channel,
                    &tasklist_channel,
                    None,
                    AgentEventPayload::TasklistTaskUpdated {
                        team_id: String::new(),
                        tasklist_id: tasklist_id.to_string(),
                        task: task.clone(),
                        owner: Some(owner),
                        project_id: None,
                    },
                )
                .await;
        }

        Ok(())
    }

    async fn requeue_task_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
    ) -> Result<(), AoError> {
        let owner = TasklistOwner::Agent { agent_id: agent_id.to_string() };
        let tl_id = tasklist_id.to_string();
        let t_id = task_id.to_string();

        let snapshot = self
            .persistence
            .tasklists
            .get_for_agent(agent_id, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tl_id.clone()))?;

        let task_status = snapshot
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == t_id)
            .map(|t| t.status)
            .ok_or_else(|| AoError::TaskNotFound(t_id.clone()))?;

        if task_status != TaskStatus::InProgress {
            return Err(AoError::InvalidTasklistTransition(format!(
                "cannot requeue task '{}' in status {:?}; only InProgress tasks can be requeued",
                t_id, task_status
            )));
        }

        let t_id_inner = t_id.clone();
        let updated = self
            .persistence
            .tasklists
            .mutate_by_owner(&owner, &tl_id, move |tl| {
                let task = tl
                    .groups
                    .iter_mut()
                    .flat_map(|g| g.tasks.iter_mut())
                    .find(|t| t.id == t_id_inner)
                    .ok_or_else(|| AoError::TaskNotFound(t_id_inner.clone()))?;
                task.status = TaskStatus::Pending;
                task.assignment = None;
                // Bump the token so any in-flight classifier CAS is rejected.
                task.classifier_token += 1;
                Ok(())
            })
            .await?;

        if matches!(updated.status, TasklistStatus::Active | TasklistStatus::Paused) {
            if let Err(e) = self.feeder.advance(&updated).await {
                tracing::warn!(
                    tasklist_id = %tl_id,
                    task_id = %t_id,
                    "requeue_task_for_agent: advance failed: {}",
                    e
                );
            }
        }

        Ok(())
    }

    async fn stop_task_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
    ) -> Result<(), AoError> {
        let owner = TasklistOwner::Agent { agent_id: agent_id.to_string() };
        self.stop_task(&owner, tasklist_id, task_id).await?;
        Ok(())
    }

    async fn resume_task_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
    ) -> Result<(), AoError> {
        let owner = TasklistOwner::Agent { agent_id: agent_id.to_string() };
        self.resume_task(&owner, tasklist_id, task_id).await?;
        Ok(())
    }

    async fn stamp_project_id_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        project_id: &str,
    ) -> Result<(), AoError> {
        let pid = project_id.to_string();
        self.persistence
            .tasklists
            .mutate_for_agent(agent_id, tasklist_id, move |tl| {
                tl.project_id = Some(pid.clone());
                Ok(())
            })
            .await?;
        Ok(())
    }

    async fn check_zombies_for_agent(
        &self,
        agent_id: &str,
        grace_secs: u64,
    ) -> Result<Vec<ZombieReport>, AoError> {
        let Some(registry) = self.instance_registry.as_ref() else {
            return Ok(vec![]);
        };

        let Some(tasklist) = self.persistence.tasklists.active_for_agent(agent_id).await? else {
            return Ok(vec![]);
        };

        let now = Instant::now();
        let grace = Duration::from_secs(grace_secs);
        let mut zombies = Vec::new();

        for group in &tasklist.groups {
            for task in &group.tasks {
                if task.status != TaskStatus::InProgress {
                    continue;
                }

                let dispatch_ts = self
                    .feeder
                    .dispatch_timestamp_for(&tasklist.id, &task.id)
                    .await;

                // Tasks dispatched recently enough are still starting; skip them.
                if let Some(at) = dispatch_ts {
                    if now.duration_since(at) < grace {
                        continue;
                    }
                }

                // Tasklist runs register under "tasklist:{id}:{agent_id}" — same
                // key format the watchdog uses.
                let registry_key =
                    format!("tasklist:{}:{}", tasklist.id, task.owner_agent_id);
                let running = registry.running_count(&registry_key).await;

                if running == 0 {
                    let secs = dispatch_ts.map(|at| now.duration_since(at).as_secs());
                    zombies.push(ZombieReport {
                        task_id: task.id.clone(),
                        task_title: task.prompt.chars().take(60).collect(),
                        secs_since_dispatch: secs,
                        agent_id: task.owner_agent_id.clone(),
                        tasklist_id: tasklist.id.clone(),
                    });
                }
            }
        }

        if !zombies.is_empty() {
            tracing::warn!(
                agent_id = %agent_id,
                zombie_count = zombies.len(),
                "check_zombies: detected InProgress tasks with no live runner",
            );
        }

        Ok(zombies)
    }
}

#[cfg(test)]
mod tests;
