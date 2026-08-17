//! Liveness tracking and recovery of tasks whose runs stalled.
//!
//! A dispatched task can stop making progress without ever reporting terminal —
//! the runner died, the model hung, the process was killed. The watchdog tick
//! classifies each in-flight task’s liveness and re-dispatches or fails the ones
//! past their grace window.
//!
//! This is a continuation of the `impl TaskFeeder` block in the parent module,
//! split out for navigability rather than encapsulation: it shares the parent’s
//! imports and helpers via `use super::*`, exactly as `tests.rs` does.

use super::*;

impl TaskFeeder {
    /// Record that a run for `task_id` has actually started — i.e. its agent
    /// run has registered in the [`InstanceRegistry`]. Called by the agent
    /// runner at run start (see the CLI runner's run-start path). The watchdog
    /// reads this flag to tell a run that registered then vanished (recover
    /// immediately) from one that has merely not started yet (honour the
    /// dispatch grace window). Cleared by `on_task_terminal` and reset on
    /// re-dispatch in `recover_stuck_task` so a recovering task gets a fresh
    /// cold-start grace rather than being reaped on sight.
    pub async fn mark_run_observed(&self, tasklist_id: &TasklistId, task_id: &TaskId) {
        self.run_observed
            .write()
            .await
            .insert((tasklist_id.clone(), task_id.clone()));
    }

    /// Classify an `InProgress` task's liveness for a watchdog/reconcile sweep.
    ///
    /// The registry key is the coarse `tasklist:{id}:{agent}` form the runner
    /// registers under, so a positive `running_count` means *some* run for that
    /// (tasklist, agent) is live. The per-task `run_observed` flag is what makes
    /// the zero-run verdict precise: only when this exact task's run was once
    /// observed do we treat its disappearance as a genuine drop and bypass the
    /// grace window. A task that has never been observed is assumed to be
    /// cold-starting and is protected until the grace elapses (a missing
    /// dispatch timestamp — e.g. after a restart — counts as past-grace).
    pub(super) async fn task_liveness(
        &self,
        instance_registry: &InstanceRegistry,
        tasklist_id: &TasklistId,
        agent_id: &AgentId,
        task_id: &TaskId,
        now: Instant,
    ) -> TaskLiveness {
        let registry_key = format!("tasklist:{}:{}", tasklist_id, agent_id);
        if instance_registry.running_count(&registry_key).await > 0 {
            return TaskLiveness::Live;
        }
        let observed = {
            let seen = self.run_observed.read().await;
            seen.contains(&(tasklist_id.clone(), task_id.clone()))
        };
        if observed {
            // Registered at least once, now gone → genuine drop. Reap now.
            return TaskLiveness::Stuck;
        }
        let dispatched = {
            let times = self.dispatched_at.read().await;
            times.get(&(tasklist_id.clone(), task_id.clone())).copied()
        };
        if let Some(at) = dispatched {
            if now.duration_since(at) < self.watchdog_grace {
                return TaskLiveness::Starting;
            }
        }
        TaskLiveness::Stuck
    }

    /// Sweep every Active tasklist for `InProgress` tasks whose owning agent
    /// has zero active runs in the [`InstanceRegistry`]. Such tasks are stuck —
    /// either the run finished without firing `on_run_ended` (silent event
    /// loss / pause-then-resume), or the agent process died, or we recovered
    /// from a server restart. For each stuck task, applies the same recovery
    /// path as `on_run_ended`: bump `attempt_count`, reprompt or transition to
    /// `Failed` once the cap is reached. Tasks freshly dispatched within
    /// `watchdog_grace` are skipped to avoid racing with run startup.
    ///
    /// Returns the number of stuck tasks recovered (reprompted or failed).
    /// Requires `with_instance_registry` to have been wired; otherwise no-op.
    pub async fn watchdog_tick(&self) -> Result<usize, AoError> {
        let Some(instance_registry) = self.instance_registry.as_ref() else {
            return Ok(0);
        };
        let mut active = self.tasklist_store.list_active_across_teams().await?;
        let agent_active = self.tasklist_store.list_active_across_agents().await?;
        active.extend(agent_active);
        if active.is_empty() {
            return Ok(0);
        }

        let now = Instant::now();
        let mut recovered = 0usize;
        for tasklist in &active {
            // Snapshot the tasks that *might* be stuck. We re-check status
            // inside `recover_stuck_task` to avoid acting on stale state from
            // events that fired between the list call and the recovery call.
            let candidates: Vec<(AgentId, TaskId)> = tasklist
                .groups
                .iter()
                .flat_map(|g| g.tasks.iter())
                .filter(|t| t.status == TaskStatus::InProgress)
                .map(|t| (resolve_executor_agent_id(&tasklist.owner, t), t.id.clone()))
                .collect();
            for (agent_id, task_id) in candidates {
                match self
                    .task_liveness(instance_registry, &tasklist.id, &agent_id, &task_id, now)
                    .await
                {
                    // Live run, or still inside the cold-start grace window.
                    TaskLiveness::Live | TaskLiveness::Starting => continue,
                    TaskLiveness::Stuck => {}
                }
                tracing::warn!(
                    tasklist_id = %tasklist.id,
                    task_id = %task_id,
                    agent_id = %agent_id,
                    "watchdog: detected stuck InProgress task (agent idle); recovering",
                );
                if self
                    .recover_stuck_task(&tasklist.owner, &tasklist.id, &agent_id, &task_id)
                    .await?
                {
                    recovered += 1;
                }
            }
        }
        if recovered > 0 {
            tracing::info!(recovered, "watchdog tick recovered stuck tasks");
        }
        Ok(recovered)
    }

    /// Reprompt or fail a single task without consulting the in-memory
    /// registry. Mirrors the body of [`Self::on_run_ended`] but accepts the
    /// `(agent_id, task_id)` pair directly so the watchdog can recover tasks
    /// even after a server restart wiped the registry. Returns `Ok(true)` if
    /// recovery was performed (reprompt or failure transition), `Ok(false)` if
    /// the task was already terminal or the tasklist no longer Active.
    pub(super) async fn recover_stuck_task(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &TasklistId,
        agent_id: &AgentId,
        task_id: &TaskId,
    ) -> Result<bool, AoError> {
        let tasklist = self
            .tasklist_store
            .get_by_owner(owner, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;
        if tasklist.status != TasklistStatus::Active {
            return Ok(false);
        }
        let task = match tasklist
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == *task_id)
            .cloned()
        {
            Some(t) => t,
            None => return Ok(false),
        };
        if task.status != TaskStatus::InProgress {
            return Ok(false);
        }

        // Reclaim the task for a reprompt dispatch. This is the CAS guard
        // against double-dispatch: `task.dispatch_token` above was read
        // unlocked, so a concurrent recoverer (another watchdog tick,
        // `kick_and_reconcile`, or `on_run_ended`) may have already reclaimed
        // this exact recovery cycle by the time we reach the lock. The store
        // re-reads fresh state under the per-tasklist write lock and only
        // bumps `attempt_count`/`dispatch_token` if our `expected_token`
        // still matches; a stale match means we lost the race and must not
        // dispatch a second time. See `try_reclaim_dispatch_by_owner`.
        let agent_id_for_msg = agent_id.clone();
        let claim = self
            .tasklist_store
            .try_reclaim_dispatch_by_owner(
                owner,
                tasklist_id,
                task_id,
                task.dispatch_token,
                self.max_attempts,
                move |new_count| {
                    format!(
                        "Attempt {}: dispatch watchdog detected agent {} idle while task was in progress \
                         (run ended without reporting completion or failure, or never started). \
                         Possible causes: context-window overflow (CLI accumulated internal context — each \
                         retry starts a fresh process), a tool permission prompt with no live approver, a \
                         sandbox write block, or an unavailable MCP server. If retries keep failing, reduce \
                         the task scope or check the agent profile (e.g. `--dangerously-skip-permissions`).",
                        new_count, agent_id_for_msg,
                    )
                },
            )
            .await?;

        let task = match claim {
            ReclaimDispatchOutcome::NotInProgress { observed } => {
                tracing::info!(
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    observed_status = ?observed,
                    "recover_stuck_task: task was no longer InProgress under the lock; another actor already resolved it, so the stuck-task recovery was skipped",
                );
                return Ok(false);
            }
            ReclaimDispatchOutcome::Stale => {
                tracing::info!(
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    "recover_stuck_task: lost the reclaim race to a concurrent recovery attempt; skipping",
                );
                return Ok(false);
            }
            ReclaimDispatchOutcome::Exhausted { attempt_count } => {
                tracing::warn!(
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    attempt_count,
                    max_attempts = self.max_attempts,
                    "watchdog: task exceeded max attempts; transitioning to Failed",
                );
                self.emit_task_updated(owner, tasklist_id, task_id).await;
                self.on_task_terminal(owner, tasklist_id, task_id).await?;
                return Ok(true);
            }
            ReclaimDispatchOutcome::Claimed { task, .. } => task,
        };

        let expected_outputs_text = if task.expected_outputs.is_empty() {
            String::from("(none declared)")
        } else {
            format!("[{}]", task.expected_outputs.join(", "))
        };
        let reprompt_prompt = format!(
            "Stuck task detected by watchdog: your run for task '{tid}' is no longer active and \
             the task is still InProgress. Re-do the task from scratch and emit either \
             <task action=\"complete\" task_id=\"{tid}\" /> (after writing all expected_outputs) or \
             <task action=\"fail\" task_id=\"{tid}\" reason=\"...\" />.\n\
             If the previous run was stopped by a tool permission prompt, sandbox block, or unavailable \
             MCP server, emit <task action=\"fail\" task_id=\"{tid}\" reason=\"...\" /> rather than \
             retrying — name the specific cause in the reason so the coordinator can fix the agent \
             profile or workspace.\n\
             Expected outputs: {outputs}.\n\n\
             Original task prompt:\n{prompt}",
            tid = task_id,
            outputs = expected_outputs_text,
            prompt = build_dispatch_prompt(&task),
        );
        // Refresh registry + dispatch timestamp so the next watchdog tick
        // honours the grace period for this re-dispatch.
        {
            let mut reg = self.registry.write().await;
            reg.entry(tasklist_id.clone())
                .or_insert_with(HashMap::new)
                .insert(agent_id.clone(), task_id.clone());
        }
        {
            let mut times = self.dispatched_at.write().await;
            times.insert((tasklist_id.clone(), task_id.clone()), Instant::now());
        }
        // Clear the observed flag: this is a fresh dispatch, so the recovering
        // run must earn a new cold-start grace window. Without this reset the
        // stale "observed" bit from the previous (now-dead) run would make the
        // next tick reap the re-dispatched run on sight, before it can start.
        {
            let mut seen = self.run_observed.write().await;
            seen.remove(&(tasklist_id.clone(), task_id.clone()));
        }
        self.dispatcher
            .dispatch_task(agent_id, reprompt_prompt, owner, tasklist_id, task_id)
            .await?;
        Ok(true)
    }

    /// For agent-owned tasklists that carry a `project_id`, return
    /// `Some(project_id)` so callers can dual-emit on the project SSE channel.
    /// Returns `None` for team-owned tasklists or agent-owned ones without a
    /// project stamp.
    pub(super) async fn project_id_for(&self, owner: &TasklistOwner, tasklist_id: &str) -> Option<String> {
        let TasklistOwner::Agent { .. } = owner else {
            return None;
        };
        match self.tasklist_store.get_by_owner(owner, tasklist_id).await {
            Ok(Some(tl)) => tl.project_id,
            _ => None,
        }
    }
}
