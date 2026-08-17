//! Forward progress: choosing what to run next and handing it to a dispatcher.
//!
//! [`TaskFeeder::advance`] walks the tasklist for the first non-terminal group
//! and dispatches it, honouring SEQ/PAR group mode and the in-flight guards
//! that keep a task from being dispatched twice.
//!
//! This is a continuation of the `impl TaskFeeder` block in the parent module,
//! split out for navigability rather than encapsulation: it shares the parent’s
//! imports and helpers via `use super::*`, exactly as `tests.rs` does.

use super::*;

impl TaskFeeder {
    /// Walk the tasklist and dispatch the first non-terminal group's pending
    /// tasks. No-op unless the tasklist is `Active`. Idempotent: tasks already
    /// `InProgress` are skipped (PAR via the in-flight registry, SEQ via the
    /// in_flight guard). Public so the HTTP append endpoint can kick the
    /// dispatcher after appending a task to a running tasklist.
    pub async fn advance(&self, tasklist: &Tasklist) -> Result<(), AoError> {
        if tasklist.status != TasklistStatus::Active {
            tracing::info!(
                tasklist_id = %tasklist.id,
                status = ?tasklist.status,
                "TaskFeeder::advance no-op (status != Active)",
            );
            return Ok(());
        }
        for (idx, group) in tasklist.groups.iter().enumerate() {
            if group_is_terminal(group) {
                tracing::debug!(
                    tasklist_id = %tasklist.id,
                    group_index = idx,
                    group_id = %group.id,
                    "advance: group terminal, skipping",
                );
                continue;
            }
            tracing::info!(
                tasklist_id = %tasklist.id,
                group_index = idx,
                group_id = %group.id,
                mode = ?group.mode,
                "advance: dispatching first non-terminal group",
            );
            self.dispatch_group(tasklist, group).await?;
            return Ok(());
        }
        // Every group is terminal AND the tasklist is still Active. If no task
        // ended in `Failed`, transition the tasklist to Completed and emit
        // `tasklist.completed`. (Failed tasklists are halted from
        // `on_task_terminal` and never reach this branch via the dispatch
        // path, but the guard above is the authoritative check.)
        let any_failed = tasklist
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .any(|t| t.status == TaskStatus::Failed);
        if !any_failed {
            tracing::info!(
                tasklist_id = %tasklist.id,
                "advance: all groups terminal, transitioning tasklist to Completed",
            );
            let owner = &tasklist.owner;
            let completed = self
                .tasklist_store
                .set_status_by_owner(owner, &tasklist.id, TasklistStatus::Completed)
                .await?;
            self.emit_tasklist_completed(owner, &tasklist.id).await;
            if let (TasklistOwner::Agent { agent_id }, Some(reg)) =
                (owner, self.instance_registry.as_ref())
            {
                reg.clear_has_active_tasklist(agent_id).await;
            }
            // Agent-owned terminal handling — see on_task_terminal for the
            // sync-vs-async rationale. Same gating applies here on the
            // all-groups-terminal auto-complete path.
            let sync_waiter_caught = self.fire_terminal_watcher(&completed).await;
            if let TasklistOwner::Agent { agent_id } = owner {
                if !sync_waiter_caught {
                    self.post_completion_summary(agent_id, &completed).await;
                }
                self.emit_todo_list_complete(agent_id, &completed).await;
            }
        } else {
            tracing::debug!(
                tasklist_id = %tasklist.id,
                "advance: all groups terminal but at least one task Failed; not auto-completing",
            );
        }
        Ok(())
    }

    async fn dispatch_group(&self, tasklist: &Tasklist, group: &TaskGroup) -> Result<(), AoError> {
        let pending = group
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Pending)
            .count();
        let in_progress = group
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::InProgress)
            .count();
        tracing::debug!(
            tasklist_id = %tasklist.id,
            group_id = %group.id,
            mode = ?group.mode,
            total = group.tasks.len(),
            pending,
            in_progress,
            "dispatch_group: entering",
        );
        match group.mode {
            TaskGroupMode::Par => {
                // PAR semantics: parallel ACROSS distinct agents, serial WITHIN
                // an agent. The registry holds at most one (tasklist, agent) →
                // task entry, so dispatching multiple PAR tasks owned by the
                // same agent in one shot would overwrite earlier entries and
                // leave those tasks unrecoverable when their runs end. Bucket
                // pending tasks by owner_agent_id and dispatch only one per
                // agent that doesn't already have an in-flight task; the
                // remaining tasks for that agent stay Pending and re-dispatch
                // from `on_task_terminal` once the agent's current task
                // terminates.
                let claimed_agents: HashSet<AgentId> = {
                    let reg = self.registry.read().await;
                    reg.get(&tasklist.id)
                        .map(|per_tl| per_tl.keys().cloned().collect())
                        .unwrap_or_default()
                };
                let mut to_dispatch: Vec<&Task> = Vec::new();
                let mut claimed_this_pass: HashSet<AgentId> = HashSet::new();
                for task in &group.tasks {
                    if task.status != TaskStatus::Pending {
                        continue;
                    }
                    // For agent-owned tasklists, route based on task.assignment; for team-owned,
                    // use the legacy owner_agent_id path (unchanged).
                    let executor_id: String = match &tasklist.owner {
                        TasklistOwner::Agent { .. } => match &task.assignment {
                            None => {
                                // Awaiting classification — emit deferred event and submit to
                                // routing. The routing callback sets task.assignment and
                                // re-drives advance(), at which point assignment is Some and the
                                // task enters the dispatch slot.
                                self.emit_task_deferred(
                                    &tasklist.owner,
                                    &tasklist.id,
                                    &task.id,
                                    "awaiting_classification",
                                )
                                .await;
                                self.submit_routing_for(tasklist, task).await;
                                continue;
                            }
                            Some(a) => a.owner_agent_id.clone(),
                        },
                        TasklistOwner::Team { .. } => {
                            if task.owner_agent_id.is_empty() {
                                self.submit_routing_for(tasklist, task).await;
                                continue;
                            }
                            task.owner_agent_id.clone()
                        }
                    };
                    if claimed_agents.contains(&executor_id)
                        || claimed_this_pass.contains(&executor_id)
                    {
                        continue;
                    }
                    claimed_this_pass.insert(executor_id);
                    to_dispatch.push(task);
                }

                let mut dispatched = 0usize;
                for task in to_dispatch {
                    // Re-check live tasklist status before each dispatch:
                    // a pause that lands mid-batch must interrupt the
                    // remainder of the loop (without this, a 20-task PAR
                    // group fires all pendings synchronously and pause
                    // would only catch the next group).
                    if let Ok(Some(live)) = self
                        .tasklist_store
                        .get_by_owner(&tasklist.owner, &tasklist.id)
                        .await
                    {
                        if live.status != TasklistStatus::Active {
                            tracing::info!(
                                tasklist_id = %tasklist.id,
                                group_id = %group.id,
                                status = ?live.status,
                                dispatched_so_far = dispatched,
                                "dispatch_group(PAR): live status changed, bailing mid-batch",
                            );
                            return Ok(());
                        }
                    }
                    self.dispatch_one(tasklist, task).await?;
                    dispatched += 1;
                }
                tracing::info!(
                    tasklist_id = %tasklist.id,
                    group_id = %group.id,
                    dispatched,
                    in_progress,
                    pending,
                    "dispatch_group(PAR): done (one task per distinct agent per pass)",
                );
            }
            TaskGroupMode::Seq => {
                let stopped = group
                    .tasks
                    .iter()
                    .filter(|t| t.status == TaskStatus::Stopped)
                    .count();
                // SEQ ordering: block on both in-progress AND stopped tasks.
                // A stopped task holds its position; tasks behind it must wait
                // until it is resumed (→ Pending) and completes normally.
                let in_flight = in_progress > 0 || stopped > 0;
                if in_flight {
                    tracing::info!(
                        tasklist_id = %tasklist.id,
                        group_id = %group.id,
                        in_progress,
                        stopped,
                        "dispatch_group(SEQ): waiting on in-progress/stopped task, no dispatch",
                    );
                    return Ok(());
                }
                if let Some(next) = group.tasks.iter().find(|t| t.status == TaskStatus::Pending) {
                    // For agent-owned tasklists check assignment; for team-owned, use legacy path.
                    match &tasklist.owner {
                        TasklistOwner::Agent { .. } => match &next.assignment {
                            None => {
                                self.emit_task_deferred(
                                    &tasklist.owner,
                                    &tasklist.id,
                                    &next.id,
                                    "awaiting_classification",
                                )
                                .await;
                                self.submit_routing_for(tasklist, next).await;
                                return Ok(());
                            }
                            Some(_) => {
                                tracing::info!(
                                    tasklist_id = %tasklist.id,
                                    group_id = %group.id,
                                    next_task = %next.id,
                                    "dispatch_group(SEQ): dispatching next pending task (assignment-routed)",
                                );
                                self.dispatch_one(tasklist, next).await?;
                            }
                        },
                        TasklistOwner::Team { .. } => {
                            if next.owner_agent_id.is_empty() {
                                self.submit_routing_for(tasklist, next).await;
                                return Ok(());
                            }
                            tracing::info!(
                                tasklist_id = %tasklist.id,
                                group_id = %group.id,
                                next_task = %next.id,
                                "dispatch_group(SEQ): dispatching next pending task",
                            );
                            self.dispatch_one(tasklist, next).await?;
                        }
                    }
                } else {
                    tracing::debug!(
                        tasklist_id = %tasklist.id,
                        group_id = %group.id,
                        "dispatch_group(SEQ): no pending tasks left",
                    );
                }
            }
        }
        Ok(())
    }

    /// Submit `task` to the appropriate routing channel based on the
    /// tasklist's owner. Agent-owned → per-agent delegate classifier,
    /// no-op with a debug log when that channel isn't wired.
    /// Team-owned → no channel exists in this build; see
    /// `note_team_routing_unsupported`.
    async fn submit_routing_for(&self, tasklist: &Tasklist, task: &Task) {
        match &tasklist.owner {
            TasklistOwner::Team { team_id } => {
                self.note_team_routing_unsupported(team_id, &tasklist.id, &task.id)
                    .await;
            }
            TasklistOwner::Agent { agent_id } => {
                let Some(agent_channel) = self.agent_routing_queue.get() else {
                    tracing::debug!(
                        agent_id = %agent_id,
                        tasklist_id = %tasklist.id,
                        task_id = %task.id,
                        "dispatch_group: agent-owned unowned task but agent routing channel not wired; leaving Pending",
                    );
                    return;
                };
                tracing::info!(
                    agent_id = %agent_id,
                    tasklist_id = %tasklist.id,
                    task_id = %task.id,
                    "dispatch_group: submitting agent-owned unowned task for agent routing",
                );
                let request = AgentRoutingRequest {
                    agent_id: agent_id.clone(),
                    tasklist_id: tasklist.id.clone(),
                    task_id: task.id.clone(),
                };
                if let Err(e) = agent_channel.submit_agent_routing(agent_id, request).await {
                    tracing::warn!(
                        agent_id = %agent_id,
                        tasklist_id = %tasklist.id,
                        task_id = %task.id,
                        "dispatch_group: failed to submit agent routing request: {}",
                        e
                    );
                }
            }
        }
    }

    /// Record that a team-owned tasklist has an unowned task with nowhere to
    /// route it. Team-owned tasklists had a per-team coordinator classifier
    /// in an earlier build; that channel was retired and only agent-owned
    /// tasklists are auto-routed now. This is a deliberate, named terminal
    /// state — distinct from "routed" and from "genuinely has no owner
    /// concept" — not a silent fallthrough, so `dispatch_group` never
    /// mistakes an unsupported team-owned task for one that's merely waiting
    /// its turn.
    ///
    /// Logs `warn` once per `tasklist_id` (subsequent calls for the same
    /// tasklist log at `debug`) so a task left Pending indefinitely doesn't
    /// spam the log on every `dispatch_group`/watchdog tick. Called both from
    /// `submit_routing_for` (the per-tick dispatch path) and from
    /// `TasklistService`'s owner-unset / user-comment routing hooks, so every
    /// path that used to submit a `RoutingRequest` now reports through here.
    pub async fn note_team_routing_unsupported(
        &self,
        team_id: &TeamId,
        tasklist_id: &str,
        task_id: &str,
    ) {
        let already_warned = self
            .team_routing_unsupported_warned
            .read()
            .await
            .contains(tasklist_id);
        if already_warned {
            tracing::debug!(
                team_id = %team_id,
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                "team_routing_unsupported: unowned task in team-owned tasklist (already warned for this tasklist)",
            );
            return;
        }
        self.team_routing_unsupported_warned
            .write()
            .await
            .insert(tasklist_id.to_string());
        tracing::warn!(
            team_id = %team_id,
            tasklist_id = %tasklist_id,
            task_id = %task_id,
            "team_routing_unsupported: team-owned tasklists have no auto-routing in this build; task will remain Pending until manually assigned",
        );
    }

    async fn dispatch_one(&self, tasklist: &Tasklist, task: &Task) -> Result<(), AoError> {
        // Derive the executor agent ID. For agent-owned tasklists, read from
        // task.assignment (set by the classifier or Pinned by TodoCreate).
        // For team-owned tasklists, use the legacy owner_agent_id field.
        let executor_agent_id: String = match &tasklist.owner {
            TasklistOwner::Agent {
                agent_id: parent_id,
            } => task
                .assignment
                .as_ref()
                .map(|a| a.owner_agent_id.clone())
                .unwrap_or_else(|| parent_id.clone()),
            TasklistOwner::Team { .. } => task.owner_agent_id.clone(),
        };

        // Safety net: if the assigned executor no longer exists, fail the task
        // rather than dispatching to a ghost agent. The cascade-delete path
        // re-classifies orphaned NotStarted tasks before the dispatcher sees
        // them; this check is the fallback for any that slip through.
        if let TasklistOwner::Agent {
            agent_id: parent_id,
        } = &tasklist.owner
        {
            if executor_agent_id != *parent_id {
                let data_root = self.tasklist_store.data_root();
                let agent_profile = data_root
                    .agents_dir()
                    .join(format!("{}.yaml", executor_agent_id));
                if !tokio::fs::try_exists(&agent_profile).await.unwrap_or(false) {
                    let owner = &tasklist.owner;
                    let reason = format!(
                        "owner_agent_missing: agent '{}' no longer exists",
                        executor_agent_id
                    );
                    tracing::warn!(
                        tasklist_id = %tasklist.id,
                        task_id = %task.id,
                        executor_agent_id = %executor_agent_id,
                        "dispatch_one: executor agent missing, failing task",
                    );
                    let task_id_owned = task.id.clone();
                    let reason_owned = reason.clone();
                    self.tasklist_store
                        .mutate_by_owner(owner, &tasklist.id, move |tl| {
                            for group in &mut tl.groups {
                                for t in &mut group.tasks {
                                    if t.id == task_id_owned {
                                        t.status = TaskStatus::Failed;
                                        t.error_log.push(reason_owned.clone());
                                    }
                                }
                            }
                            Ok(())
                        })
                        .await?;
                    self.emit_task_updated(owner, &tasklist.id, &task.id).await;
                    // Return Ok — the task is now Failed; the next advance() or
                    // watchdog_tick() will pick it up via on_task_terminal and
                    // drive the tasklist to its terminal state.
                    return Ok(());
                }
            }
        }

        tracing::info!(
            tasklist_id = %tasklist.id,
            task_id = %task.id,
            executor_agent_id = %executor_agent_id,
            "dispatch_one: dispatching task",
        );
        let owner = &tasklist.owner;
        // Atomically claim the task before doing any dispatch work. This is the
        // guard against double-dispatch: `dispatch_group` decides what to send
        // from an in-memory snapshot that may already be stale by the time we
        // get here (a concurrent `advance()` — from the classifier write-back,
        // a sibling task completing, or the reconciler — can have moved this
        // task to InProgress or a terminal state in the meantime). The claim
        // re-reads the live status under the store's per-tasklist lock and only
        // flips `Pending|Blocked -> InProgress`; if it returns false the task is
        // already in flight or finished, so we skip silently rather than
        // running it a second time.
        let claimed = self
            .tasklist_store
            .try_begin_task_by_owner(owner, &tasklist.id, &task.id)
            .await?;
        if !claimed {
            tracing::info!(
                tasklist_id = %tasklist.id,
                task_id = %task.id,
                "dispatch_one: task no longer dispatchable (already claimed or terminal), skipping",
            );
            return Ok(());
        }
        self.emit_task_updated(owner, &tasklist.id, &task.id).await;

        // Write meta.json at the "started" hook for agent-owned tasklists.
        if let TasklistOwner::Agent {
            agent_id: parent_agent_id,
        } = owner
        {
            let data_root = self.tasklist_store.data_root();
            let meta_path = data_root.task_meta_path(parent_agent_id, &tasklist.id, &task.id);
            let meta = TaskMeta {
                task_id: task.id.clone(),
                tasklist_id: tasklist.id.clone(),
                parent_agent_id: parent_agent_id.clone(),
                owner_agent_id: Some(executor_agent_id.clone()),
                assignment_mode: task.assignment.as_ref().map(|a| a.mode),
                title: task
                    .prompt
                    .lines()
                    .next()
                    .unwrap_or(&task.prompt)
                    .to_string(),
                status: TaskStatus::InProgress,
                created_at: Utc::now(),
                started_at: Some(Utc::now()),
                ended_at: None,
                summary: None,
                model_used: None,
            };
            if let Err(e) = write_task_meta(&meta_path, &meta).await {
                tracing::warn!(
                    parent_agent_id = %parent_agent_id,
                    tasklist_id = %tasklist.id,
                    task_id = %task.id,
                    "dispatch_one: failed to write task meta.json: {}",
                    e
                );
            }
        }

        {
            let mut reg = self.registry.write().await;
            reg.entry(tasklist.id.clone())
                .or_insert_with(HashMap::new)
                .insert(executor_agent_id.clone(), task.id.clone());
        }
        {
            let mut times = self.dispatched_at.write().await;
            times.insert((tasklist.id.clone(), task.id.clone()), Instant::now());
        }

        if let Err(e) = self
            .dispatcher
            .dispatch_task(
                &executor_agent_id,
                build_dispatch_prompt(task),
                owner,
                &tasklist.id,
                &task.id,
            )
            .await
        {
            let mut reg = self.registry.write().await;
            if let Some(per_tl) = reg.get_mut(&tasklist.id) {
                if per_tl
                    .get(&executor_agent_id)
                    .map(|tid| tid == &task.id)
                    .unwrap_or(false)
                {
                    per_tl.remove(&executor_agent_id);
                }
            }
            drop(reg);
            {
                let mut times = self.dispatched_at.write().await;
                times.remove(&(tasklist.id.clone(), task.id.clone()));
            }
            {
                let mut seen = self.run_observed.write().await;
                seen.remove(&(tasklist.id.clone(), task.id.clone()));
            }
            tracing::error!(
                tasklist_id = %tasklist.id,
                task_id = %task.id,
                "Failed to dispatch task: {}", e
            );
            return Err(e);
        }
        Ok(())
    }

}
