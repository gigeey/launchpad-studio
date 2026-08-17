//! Operator-initiated tasklist lifecycle transitions.
//!
//! These are the state changes a user (or an HTTP handler acting for one) asks
//! for directly — pause, continue, skip, discard, replay, resume, start — as
//! opposed to the transitions the feeder drives on its own when a task reports
//! terminal (see `terminal.rs`) or the watchdog reaps a stalled run.
//!
//! This is a continuation of the `impl TaskFeeder` block in the parent module,
//! split out for navigability rather than encapsulation: it shares the parent’s
//! imports and helpers via `use super::*`, exactly as `tests.rs` does.

use super::*;

impl TaskFeeder {
    /// Pause a running tasklist. In-flight tasks keep running; new dispatch is
    /// suppressed (advance() early-returns and the PAR dispatch loop bails on
    /// the next iteration). The caller must verify the tasklist is currently
    /// `Active` — the store rejects invalid transitions.
    pub async fn pause(
        &self,
        team_id: &TeamId,
        tasklist_id: &TasklistId,
    ) -> Result<Tasklist, AoError> {
        tracing::info!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            "TaskFeeder::pause requested",
        );
        let updated = self
            .tasklist_store
            .set_status(team_id, tasklist_id, TasklistStatus::Paused)
            .await?;
        let owner = TasklistOwner::Team {
            team_id: team_id.clone(),
        };
        self.emit_tasklist_status_changed(&owner, tasklist_id, TasklistStatus::Paused)
            .await;
        Ok(updated)
    }

    /// Continue a Failed tasklist after the user has fixed the underlying
    /// cause (permissions, missing skill, broken script, etc.). Resets every
    /// `Failed` task back to `Pending` with a cleared attempt count and error
    /// log, flips the tasklist itself back to `Active`, then kicks `advance()`
    /// so the reset tasks re-dispatch from the same group they failed in.
    /// Errors with `InvalidTasklistTransition` if the tasklist is not in
    /// `Failed` status, or if the team already has another Active/Paused
    /// tasklist occupying the single active slot.
    pub async fn continue_tasklist(
        &self,
        team_id: &TeamId,
        tasklist_id: &TasklistId,
    ) -> Result<Tasklist, AoError> {
        tracing::info!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            "TaskFeeder::continue_tasklist requested",
        );

        // Preserve the one-active-slot invariant: another Active/Paused
        // tasklist for the same team must be resolved (paused/cancelled) before
        // the user can revive this Failed one.
        if let Some(other) = self.tasklist_store.find_active(team_id).await? {
            if other.id != *tasklist_id {
                return Err(AoError::TasklistAlreadyActive {
                    team_id: team_id.clone(),
                    tasklist_id: other.id,
                });
            }
        }

        let mut reset_task_ids: Vec<TaskId> = Vec::new();
        let updated = self
            .tasklist_store
            .mutate(team_id, tasklist_id, |tl| {
                if tl.status != TasklistStatus::Failed {
                    return Err(AoError::InvalidTasklistTransition(format!(
                        "cannot continue tasklist {} in status {:?}; only Failed tasklists can be continued",
                        tl.id, tl.status
                    )));
                }
                tl.status = TasklistStatus::Active;
                for group in &mut tl.groups {
                    for task in &mut group.tasks {
                        if task.status == TaskStatus::Failed {
                            task.status = TaskStatus::Pending;
                            task.attempt_count = 0;
                            task.error_log.clear();
                            reset_task_ids.push(task.id.clone());
                        }
                    }
                }
                Ok(())
            })
            .await?;

        let owner = TasklistOwner::Team {
            team_id: team_id.clone(),
        };
        self.emit_tasklist_status_changed(&owner, tasklist_id, TasklistStatus::Active)
            .await;
        for task_id in &reset_task_ids {
            self.emit_task_updated(&owner, tasklist_id, task_id).await;
        }
        // Failed → Pending transitions are the canonical "task revived"
        // wake signal. Emit only when we actually reset something so a no-op
        // continue (no failed tasks) doesn't churn the lifecycle.
        if !reset_task_ids.is_empty() {
            self.emit_lifecycle_wake(
                team_id,
                tasklist_id,
                crate::tasklist_lifecycle::WakeReason::TaskRevived,
            )
            .await;
        }

        tracing::info!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            reset_count = reset_task_ids.len(),
            "TaskFeeder::continue_tasklist kicking advance()",
        );
        self.advance(&updated).await?;
        Ok(updated)
    }

    /// Skip a single Failed task: marks it `Skipped` (terminal-but-not-failure)
    /// so `advance()` walks past it and the tasklist-completion check ignores
    /// it. If skipping this task leaves no `Failed` tasks behind AND the
    /// tasklist itself is `Failed`, the tasklist is revived to `Active` and
    /// dispatch resumes from the same group cursor. If other Failed tasks
    /// remain, the tasklist stays Failed (the user can Skip them too or
    /// Continue to retry the lot).
    ///
    /// Errors with `InvalidTasklistTransition` if the task is not currently
    /// `Failed`, or with `TasklistAlreadyActive` if revival would collide with
    /// another Active/Paused tasklist for the same team.
    pub async fn skip_task(
        &self,
        team_id: &TeamId,
        tasklist_id: &TasklistId,
        task_id: &TaskId,
    ) -> Result<Tasklist, AoError> {
        tracing::info!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            task_id = %task_id,
            "TaskFeeder::skip_task requested",
        );

        // Snapshot-and-decide: figure out if this skip will revive the tasklist
        // so we can pre-check the one-active-slot invariant before mutating.
        // (The mutate closure can't reach back to the store for find_active.)
        let snapshot = self
            .tasklist_store
            .get(team_id, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;

        let current_status = snapshot
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == *task_id)
            .map(|t| t.status)
            .ok_or_else(|| AoError::TaskNotFound(task_id.clone()))?;
        if current_status != TaskStatus::Failed {
            return Err(AoError::InvalidTasklistTransition(format!(
                "cannot skip task {} in status {:?}; only Failed tasks can be skipped",
                task_id, current_status
            )));
        }

        let other_failed_remaining = snapshot
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .any(|t| t.id != *task_id && t.status == TaskStatus::Failed);
        let will_revive = !other_failed_remaining && snapshot.status == TasklistStatus::Failed;

        if will_revive {
            if let Some(other) = self.tasklist_store.find_active(team_id).await? {
                if other.id != *tasklist_id {
                    return Err(AoError::TasklistAlreadyActive {
                        team_id: team_id.clone(),
                        tasklist_id: other.id,
                    });
                }
            }
        }

        let updated = self
            .tasklist_store
            .mutate(team_id, tasklist_id, |tl| {
                let task = tl
                    .groups
                    .iter_mut()
                    .flat_map(|g| g.tasks.iter_mut())
                    .find(|t| t.id == *task_id)
                    .ok_or_else(|| AoError::TaskNotFound(task_id.clone()))?;
                if task.status != TaskStatus::Failed {
                    return Err(AoError::InvalidTasklistTransition(format!(
                        "cannot skip task {} in status {:?}; only Failed tasks can be skipped",
                        task_id, task.status
                    )));
                }
                task.status = TaskStatus::Skipped;

                let any_failed = tl
                    .groups
                    .iter()
                    .flat_map(|g| g.tasks.iter())
                    .any(|t| t.status == TaskStatus::Failed);
                if !any_failed && tl.status == TasklistStatus::Failed {
                    tl.status = TasklistStatus::Active;
                }
                Ok(())
            })
            .await?;

        let owner = TasklistOwner::Team {
            team_id: team_id.clone(),
        };
        self.emit_task_updated(&owner, tasklist_id, task_id).await;

        if updated.status == TasklistStatus::Active && snapshot.status != TasklistStatus::Active {
            self.emit_tasklist_status_changed(&owner, tasklist_id, TasklistStatus::Active)
                .await;
            // skip_task that revives a Failed tasklist back to Active
            // brings outstanding non-terminal tasks back into rotation. That's
            // a wake by definition (downstream `is_tasklist_active` flips from
            // false to true).
            self.emit_lifecycle_wake(
                team_id,
                tasklist_id,
                crate::tasklist_lifecycle::WakeReason::TaskRevived,
            )
            .await;
        }

        if updated.status == TasklistStatus::Active {
            tracing::info!(
                team_id = %team_id,
                tasklist_id = %tasklist_id,
                revived = will_revive,
                "TaskFeeder::skip_task kicking advance()",
            );
            self.advance(&updated).await?;
        }

        Ok(updated)
    }

    /// Terminate a tasklist by user decision. Flips the tasklist to
    /// `Cancelled` (terminal) and marks any not-yet-dispatched tasks
    /// (`Pending`/`Blocked`) as `Skipped` so the panel doesn't show stranded
    /// rows. In-flight `InProgress` tasks are intentionally left alone — the
    /// agent finishes its current turn naturally; once it reaches a terminal
    /// status, `on_task_terminal` and `advance()` are no-ops because the
    /// tasklist is no longer `Active`.
    ///
    /// Allowed from `Active`, `Paused`, or `Failed`. Errors with
    /// `InvalidTasklistTransition` if the tasklist is already in a terminal
    /// state (`Completed` or `Cancelled`).
    pub async fn discard_tasklist(
        &self,
        team_id: &TeamId,
        tasklist_id: &TasklistId,
    ) -> Result<Tasklist, AoError> {
        tracing::info!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            "TaskFeeder::discard_tasklist requested",
        );

        let mut skipped_task_ids: Vec<TaskId> = Vec::new();
        let updated = self
            .tasklist_store
            .mutate(team_id, tasklist_id, |tl| {
                if !matches!(
                    tl.status,
                    TasklistStatus::Active
                        | TasklistStatus::Paused
                        | TasklistStatus::Failed
                ) {
                    return Err(AoError::InvalidTasklistTransition(format!(
                        "cannot discard tasklist {} in status {:?}; only Active, Paused, or Failed tasklists can be discarded",
                        tl.id, tl.status
                    )));
                }
                tl.status = TasklistStatus::Cancelled;
                for group in &mut tl.groups {
                    for task in &mut group.tasks {
                        if matches!(task.status, TaskStatus::Pending | TaskStatus::Blocked) {
                            task.status = TaskStatus::Skipped;
                            skipped_task_ids.push(task.id.clone());
                        }
                    }
                }
                Ok(())
            })
            .await?;

        let owner = TasklistOwner::Team {
            team_id: team_id.clone(),
        };
        self.emit_tasklist_status_changed(&owner, tasklist_id, TasklistStatus::Cancelled)
            .await;
        for task_id in &skipped_task_ids {
            self.emit_task_updated(&owner, tasklist_id, task_id).await;
        }
        self.fire_terminal_watcher(&updated).await;

        tracing::info!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            skipped_count = skipped_task_ids.len(),
            "TaskFeeder::discard_tasklist done",
        );
        Ok(updated)
    }

    /// Replay a terminal tasklist by cloning its plan into a brand-new tasklist
    /// with fresh ids, fresh statuses, and fresh on-disk workspace + transcripts
    /// directories. The original tasklist is left untouched in its terminal
    /// state for history. The new tasklist starts in `Active` and is
    /// bootstrapped via `start()` so dispatch begins immediately.
    ///
    /// Allowed only from `Completed`, `Failed`, or `Cancelled`. Errors with
    /// `InvalidTasklistTransition` if the source tasklist is still
    /// `Active`/`Paused`, or with `TasklistAlreadyActive` if the team's active
    /// slot is occupied by another tasklist.
    pub async fn replay_tasklist(
        &self,
        team_id: &TeamId,
        tasklist_id: &TasklistId,
    ) -> Result<Tasklist, AoError> {
        tracing::info!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            "TaskFeeder::replay_tasklist requested",
        );

        let original = self
            .tasklist_store
            .get(team_id, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;

        if !matches!(
            original.status,
            TasklistStatus::Completed | TasklistStatus::Failed | TasklistStatus::Cancelled
        ) {
            return Err(AoError::InvalidTasklistTransition(format!(
                "cannot replay tasklist {} in status {:?}; only Completed, Failed, or Cancelled tasklists can be replayed",
                original.id, original.status
            )));
        }

        // Pre-check the one-active-slot invariant. `tasklist_store.create()`
        // also enforces this, but checking here gives a clean error before any
        // disk side-effects (the create() path already creates dirs before the
        // slot check would fire — actually it checks first, but pre-checking
        // keeps behavior symmetric with continue_tasklist/skip_task).
        if let Some(other) = self.tasklist_store.find_active(team_id).await? {
            return Err(AoError::TasklistAlreadyActive {
                team_id: team_id.clone(),
                tasklist_id: other.id,
            });
        }

        let new_tasklist_id = Uuid::new_v4().to_string();
        let data_root = self.tasklist_store.data_root();
        let workspace_dir = data_root
            .tasklist_workspace_dir(team_id, &new_tasklist_id)
            .to_string_lossy()
            .to_string();
        let transcripts_dir = data_root
            .tasklist_transcripts_dir(team_id, &new_tasklist_id)
            .to_string_lossy()
            .to_string();

        let groups: Vec<TaskGroup> = original
            .groups
            .iter()
            .map(|g| {
                let new_group_id = Uuid::new_v4().to_string();
                let tasks = g
                    .tasks
                    .iter()
                    .map(|t| Task {
                        id: Uuid::new_v4().to_string(),
                        owner_agent_id: t.owner_agent_id.clone(),
                        prompt: t.prompt.clone(),
                        expected_outputs: t.expected_outputs.clone(),
                        status: TaskStatus::Pending,
                        group_id: new_group_id.clone(),
                        attempt_count: 0,
                        error_log: Vec::new(),
                        comments: Vec::new(),
                        attachments: t.attachments.clone(),
                        remind_me: t.remind_me.clone(),
                        parse_failed: false,
                        notification_parse_retry_count: 0,
                        assignment: None,
                        classifier_token: 0,
                        dispatch_token: 0,
                    })
                    .collect();
                TaskGroup {
                    id: new_group_id,
                    mode: g.mode,
                    tasks,
                }
            })
            .collect();

        let new_tasklist = Tasklist {
            id: new_tasklist_id.clone(),
            owner: ao_protocol::tasklist::TasklistOwner::Team {
                team_id: team_id.clone(),
            },
            team_id: Some(team_id.clone()),
            title: original.title.clone(),
            description: original.description.clone(),
            status: TasklistStatus::Active,
            groups,
            workspace_dir,
            transcripts_dir,
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        self.tasklist_store.create(&new_tasklist).await?;

        tracing::info!(
            team_id = %team_id,
            source_tasklist_id = %tasklist_id,
            new_tasklist_id = %new_tasklist_id,
            groups = new_tasklist.groups.len(),
            "TaskFeeder::replay_tasklist created clone, bootstrapping",
        );

        self.emit_tasklist_created(team_id, &new_tasklist_id).await;
        self.start(team_id, &new_tasklist_id).await?;

        Ok(new_tasklist)
    }

    /// Resume a paused tasklist. Flips status back to Active, emits the
    /// status-changed event, then kicks `advance()` once so any pending tasks
    /// in the current group re-dispatch.
    pub async fn resume(
        &self,
        team_id: &TeamId,
        tasklist_id: &TasklistId,
    ) -> Result<Tasklist, AoError> {
        tracing::info!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            "TaskFeeder::resume requested",
        );
        let updated = self
            .tasklist_store
            .set_status(team_id, tasklist_id, TasklistStatus::Active)
            .await?;
        let owner = TasklistOwner::Team {
            team_id: team_id.clone(),
        };
        self.emit_tasklist_status_changed(&owner, tasklist_id, TasklistStatus::Active)
            .await;
        tracing::info!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            "TaskFeeder::resume kicking advance()",
        );
        self.advance(&updated).await?;
        Ok(updated)
    }

    /// Bootstrap a tasklist by dispatching the first not-yet-terminal group.
    /// Idempotent on a tasklist that is already mid-flight: tasks already in
    /// `InProgress` are not re-dispatched.
    pub async fn start(&self, team_id: &TeamId, tasklist_id: &TasklistId) -> Result<(), AoError> {
        let tasklist = self
            .tasklist_store
            .get(team_id, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;

        if tasklist.status != TasklistStatus::Active {
            return Err(AoError::InvalidTasklistTransition(format!(
                "cannot start tasklist {} in status {:?}",
                tasklist.id, tasklist.status
            )));
        }

        tracing::info!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            groups = tasklist.groups.len(),
            "TaskFeeder::start bootstrapping tasklist",
        );
        self.advance(&tasklist).await
    }

}
