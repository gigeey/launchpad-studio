//! Terminal-state handling: what happens when a dispatched task finishes.
//!
//! The entry point is [`TaskFeeder::on_task_terminal`], which records the
//! outcome, applies halt-on-failure, and advances the tasklist. The remaining
//! methods are the surrounding reconciliation paths — validating a claimed
//! completion, forcing one through, and detecting that a run ended without ever
//! reporting a terminal state.
//!
//! This is a continuation of the `impl TaskFeeder` block in the parent module,
//! split out for navigability rather than encapsulation: it shares the parent’s
//! imports and helpers via `use super::*`, exactly as `tests.rs` does.

use super::*;

impl TaskFeeder {
    /// Notify the feeder that a task reached a terminal state. Clears the
    /// agent's registry entry, halts the tasklist if the task ended in
    /// `Failed`, then advances. Already-running PAR tasks in the same group
    /// are left alone; halt-on-failure only prevents NEW dispatch (the next
    /// SEQ task or the next group).
    pub async fn on_task_terminal(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &TasklistId,
        task_id: &TaskId,
    ) -> Result<(), AoError> {
        let mut tasklist = self
            .tasklist_store
            .get_by_owner(owner, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;

        let task_meta = tasklist
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == *task_id)
            .map(|t| {
                // For agent-owned tasklists the registry key is the executor agent
                // (assignment.owner_agent_id), not necessarily owner_agent_id.
                let registry_key = match owner {
                    TasklistOwner::Agent {
                        agent_id: parent_id,
                    } => t
                        .assignment
                        .as_ref()
                        .map(|a| a.owner_agent_id.clone())
                        .unwrap_or_else(|| parent_id.clone()),
                    TasklistOwner::Team { .. } => t.owner_agent_id.clone(),
                };
                (registry_key, t.status)
            });

        tracing::info!(
            tasklist_id = %tasklist_id,
            task_id = %task_id,
            tasklist_status = ?tasklist.status,
            task_status = ?task_meta.as_ref().map(|(_, s)| *s),
            "on_task_terminal",
        );

        if let Some((task_owner_agent_id, _)) = &task_meta {
            let mut reg = self.registry.write().await;
            if let Some(per_tl) = reg.get_mut(tasklist_id) {
                if per_tl
                    .get(task_owner_agent_id)
                    .map(|tid| tid == task_id)
                    .unwrap_or(false)
                {
                    per_tl.remove(task_owner_agent_id);
                }
            }
        }
        {
            let mut times = self.dispatched_at.write().await;
            times.remove(&(tasklist_id.clone(), task_id.clone()));
        }
        {
            let mut seen = self.run_observed.write().await;
            seen.remove(&(tasklist_id.clone(), task_id.clone()));
        }

        if matches!(
            task_meta.as_ref().map(|(_, s)| *s),
            Some(TaskStatus::Failed)
        ) && tasklist.status == TasklistStatus::Active
        {
            tasklist = self
                .tasklist_store
                .set_status_by_owner(owner, tasklist_id, TasklistStatus::Failed)
                .await?;
            let reason = tasklist
                .groups
                .iter()
                .flat_map(|g| g.tasks.iter())
                .find(|t| t.id == *task_id)
                .and_then(|t| t.error_log.last().cloned())
                .or_else(|| Some(format!("task {} failed", task_id)));
            self.emit_tasklist_failed(owner, tasklist_id, reason).await;
            if let (TasklistOwner::Agent { agent_id }, Some(reg)) =
                (owner, self.instance_registry.as_ref())
            {
                reg.clear_has_active_tasklist(agent_id).await;
            }
            // Agent-owned terminal handling:
            //   1. fire_terminal_watcher() — returns true iff a sync TodoCreate
            //      caller is awaiting the TerminalReport inline. In that case
            //      the agent already gets the result from its own tool call;
            //      a queued summary message would be redundant.
            //   2. If no sync waiter caught it (async TodoCreate, or no waiter
            //      at all), post_completion_summary queues a message into the
            //      agent's own mailbox so it wakes up on the next turn and can
            //      react to the terminal state instead of going silent.
            //   3. TodoListComplete fires either way so the UI gets a single
            //      authoritative terminal event regardless of sync/async mode.
            let sync_waiter_caught = self.fire_terminal_watcher(&tasklist).await;
            if let TasklistOwner::Agent { agent_id } = owner {
                if !sync_waiter_caught {
                    self.post_completion_summary(agent_id, &tasklist).await;
                }
                self.emit_todo_list_complete(agent_id, &tasklist).await;
            }
        }

        // Append a ProgressBlock to progress.jsonl for agent-owned tasklists.
        // Errors are logged at warn level and swallowed — a write failure must not abort
        // the tasklist run.
        if let TasklistOwner::Agent { agent_id } = owner {
            let task_snap = tasklist
                .groups
                .iter()
                .flat_map(|g| g.tasks.iter())
                .find(|t| t.id == *task_id)
                .cloned();
            if let Some(task) = task_snap {
                let data_root = self.tasklist_store.data_root();
                let progress_path = data_root.agent_tasklist_progress_log(agent_id, tasklist_id);
                let output_path =
                    data_root.agent_tasklist_task_output_path(agent_id, tasklist_id, task_id);
                let status_str = match task.status {
                    TaskStatus::Completed => "completed",
                    TaskStatus::Failed => "failed",
                    TaskStatus::Skipped => "skipped",
                    other => {
                        tracing::warn!(
                            agent_id = %agent_id,
                            tasklist_id = %tasklist_id,
                            task_id = %task_id,
                            task_status = ?other,
                            "on_task_terminal: unexpected task status for progress block",
                        );
                        "unknown"
                    }
                };
                let title = task.prompt.lines().next().map(str::to_string);

                // Carry the producing agent's self-reported summary onto the
                // durable per-task records. It was persisted to the changelog
                // immediately before this terminal transition (see
                // `record_task_item_changelog` in agent_runner), so the entry
                // is already on disk by the time this hook reads it back.
                // Best-effort / absent when no `<task-item-notification>` was
                // emitted (cancel, watchdog, skip, retry-exhaustion paths) or
                // the changelog read failed.
                let task_summary = self
                    .load_task_summaries(&tasklist)
                    .await
                    .get(task_id)
                    .map(|e| e.summary.clone());

                // Load the dispatch-time meta (written by `dispatch_one`) so we
                // carry forward the real `created_at`/`started_at` and the
                // resolved executor. Rewriting them from scratch here stamps
                // `now()` for both timestamps — collapsing a task's whole
                // lifetime to a single instant — and, for classifier-assigned
                // tasks (where `task.owner_agent_id` is empty and the executor
                // lives in `task.assignment`), drops the executor id entirely.
                let meta_path = data_root.task_meta_path(agent_id, tasklist_id, task_id);
                let prior_meta = match read_task_meta(&meta_path).await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(
                            agent_id = %agent_id,
                            tasklist_id = %tasklist_id,
                            task_id = %task_id,
                            "on_task_terminal: failed to read dispatch-time task meta.json: {}",
                            e
                        );
                        None
                    }
                };
                let ended = Utc::now();
                let created_at = prior_meta.as_ref().map(|m| m.created_at).unwrap_or(ended);
                let started_at = prior_meta.as_ref().and_then(|m| m.started_at);
                // Prefer the executor recorded at dispatch; fall back to the
                // assignment, then to the (possibly empty) legacy field.
                let owner_agent_id = prior_meta
                    .as_ref()
                    .and_then(|m| m.owner_agent_id.clone())
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        task.assignment
                            .as_ref()
                            .map(|a| a.owner_agent_id.clone())
                            .filter(|s| !s.is_empty())
                    })
                    .or_else(|| Some(task.owner_agent_id.clone()).filter(|s| !s.is_empty()));

                let block = ProgressBlock {
                    task_id: Some(task_id.clone()),
                    title,
                    status: status_str.to_string(),
                    summary: task_summary.clone(),
                    started_at: started_at.map(|t| t.to_rfc3339()),
                    ended_at: Some(ended.to_rfc3339()),
                    output_path: Some(output_path),
                    attempt_count: Some(task.attempt_count),
                };
                if let Err(e) = append_progress_block(&progress_path, &block).await {
                    tracing::warn!(
                        agent_id = %agent_id,
                        tasklist_id = %tasklist_id,
                        task_id = %task_id,
                        "on_task_terminal: failed to write progress block: {}",
                        e
                    );
                }

                // Rewrite meta.json at the "terminal" hook — preserve the
                // dispatch-time timestamps and executor, stamping only the
                // final status and `ended_at`.
                let terminal_meta = TaskMeta {
                    task_id: task_id.clone(),
                    tasklist_id: tasklist_id.clone(),
                    parent_agent_id: agent_id.clone(),
                    owner_agent_id,
                    assignment_mode: task.assignment.as_ref().map(|a| a.mode),
                    title: task
                        .prompt
                        .lines()
                        .next()
                        .unwrap_or(&task.prompt)
                        .to_string(),
                    status: task.status,
                    created_at,
                    started_at,
                    ended_at: Some(ended),
                    summary: task_summary,
                    model_used: None,
                };
                if let Err(e) = write_task_meta(&meta_path, &terminal_meta).await {
                    tracing::warn!(
                        agent_id = %agent_id,
                        tasklist_id = %tasklist_id,
                        task_id = %task_id,
                        "on_task_terminal: failed to write terminal task meta.json: {}",
                        e
                    );
                }

                // Append the executor's summary as a TaskComment so it shows up
                // in the Task Detail modal Comments section. Best-effort: a write
                // failure logs a warning but does not abort the terminal transition.
                if let Some(body) = terminal_meta
                    .summary
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                {
                    let author_id = terminal_meta
                        .owner_agent_id
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(agent_id)
                        .to_string();
                    let comment = TaskComment {
                        id: Uuid::new_v4().to_string(),
                        author_id,
                        author_kind: TaskCommentAuthorKind::Agent,
                        body: body.to_string(),
                        created_at: Utc::now(),
                    };
                    let task_id_s = task_id.to_string();
                    let result = self
                        .tasklist_store
                        .mutate_for_agent(agent_id, tasklist_id, move |tl| {
                            let task = tl
                                .groups
                                .iter_mut()
                                .flat_map(|g| g.tasks.iter_mut())
                                .find(|t| t.id == task_id_s)
                                .ok_or_else(|| {
                                    AoError::TaskNotFound(task_id_s.clone())
                                })?;
                            task.comments.push(comment);
                            Ok(())
                        })
                        .await;
                    match result {
                        Ok(_) => {
                            self.emit_task_updated(owner, tasklist_id, task_id).await;
                        }
                        Err(e) => {
                            tracing::warn!(
                                agent_id = %agent_id,
                                tasklist_id = %tasklist_id,
                                task_id = %task_id,
                                "on_task_terminal: failed to append completion comment: {}",
                                e
                            );
                        }
                    }
                }
            }
        }

        // No synchronous sleep emission here. `advance()` calls
        // `set_status(Completed)` on the auto-complete path which stamps
        // `last_active_at = now()`, so the grace window can never have elapsed
        // immediately after a task transition. The lifecycle module exposes
        // `maybe_emit_sleep` for the deferred check; the mailbox poller
        // will tick it on its enrolled-set walk.
        self.advance(&tasklist).await
    }

    /// Validate that every `expected_outputs` filename for `task_id` exists in
    /// the tasklist's workspace directory, then either complete the task,
    /// reprompt the same agent, or fail (after `max_attempts` validation
    /// failures). Called from `process_task_tag_action` when an agent emits
    /// `<task action="complete">` so we never trust the agent's self-report.
    pub async fn validate_and_complete(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &TasklistId,
        task_id: &TaskId,
    ) -> Result<(), AoError> {
        let tasklist = self
            .tasklist_store
            .get_by_owner(owner, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;

        let task = tasklist
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == *task_id)
            .cloned()
            .ok_or_else(|| AoError::TaskNotFound(task_id.clone()))?;

        let workspace_dir = std::path::PathBuf::from(&tasklist.workspace_dir);
        let mut missing: Vec<String> = Vec::new();
        for filename in &task.expected_outputs {
            let candidate = workspace_dir.join(filename);
            if !tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
                missing.push(filename.clone());
            }
        }

        if missing.is_empty() {
            tracing::info!(
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                expected_outputs = task.expected_outputs.len(),
                "validate_and_complete: outputs verified, marking Completed",
            );
            self.tasklist_store
                .set_task_status_by_owner(owner, tasklist_id, task_id, TaskStatus::Completed)
                .await?;
            self.emit_task_updated(owner, tasklist_id, task_id).await;
            return self.on_task_terminal(owner, tasklist_id, task_id).await;
        }

        tracing::warn!(
            tasklist_id = %tasklist_id,
            task_id = %task_id,
            missing_count = missing.len(),
            "validate_and_complete: missing expected_outputs, will reprompt",
        );

        // Reclaim the task for a reprompt dispatch. `task.dispatch_token`
        // above was read unlocked, so a concurrent recoverer (a watchdog
        // tick, `kick_and_reconcile`, or `on_run_ended` racing this same
        // completion) may have already reclaimed this exact recovery cycle
        // by the time we reach the lock. `try_reclaim_dispatch_by_owner`
        // re-reads fresh state under the per-tasklist write lock and only
        // bumps `attempt_count`/`dispatch_token` if `expected_token` still
        // matches; a stale match means we lost the race and must not
        // dispatch a second time.
        let missing_for_error = missing.clone();
        let claim = self
            .tasklist_store
            .try_reclaim_dispatch_by_owner(
                owner,
                tasklist_id,
                task_id,
                task.dispatch_token,
                self.max_attempts,
                |new_count| {
                    format!(
                        "Attempt {}: missing expected outputs: [{}]",
                        new_count,
                        missing_for_error.join(", "),
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
                    "validate_and_complete: agent reported completion but the task was no longer InProgress under the lock; another actor already resolved it, so the missing-outputs reprompt was skipped",
                );
                return Ok(());
            }
            ReclaimDispatchOutcome::Stale => {
                tracing::info!(
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    "validate_and_complete: lost the reclaim race to a concurrent recovery attempt; skipping",
                );
                return Ok(());
            }
            ReclaimDispatchOutcome::Exhausted { attempt_count } => {
                tracing::warn!(
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    attempt_count,
                    max_attempts = self.max_attempts,
                    "Task exceeded max attempts; transitioning to Failed",
                );
                self.emit_task_updated(owner, tasklist_id, task_id).await;
                return self.on_task_terminal(owner, tasklist_id, task_id).await;
            }
            ReclaimDispatchOutcome::Claimed { task, .. } => task,
        };

        let reprompt_prompt = format!(
            "Output validation failed: the following expected_outputs files are missing from the tasklist workspace ({}): [{}].\n\
             Re-do the task and ensure every declared expected_output exists in the workspace before emitting <task action=\"complete\" task_id=\"{}\" />.\n\n\
             Original task prompt:\n{}",
            workspace_dir.display(),
            missing.join(", "),
            task_id,
            build_dispatch_prompt(&task),
        );
        self.dispatcher
            .dispatch_task(
                &task.owner_agent_id,
                reprompt_prompt,
                owner,
                tasklist_id,
                task_id,
            )
            .await
    }

    /// Control-tool completion path (TodoComplete). Unlike the agent-runner
    /// path ([`Self::validate_and_complete`]) this does NOT re-validate the
    /// task's `expected_outputs` — the caller (a coordinator or operator) is
    /// explicitly forcing the task done, e.g. to recover a stalled SEQ list.
    ///
    /// The critical invariant it enforces is that the queue *actually advances*
    /// rather than silently no-opping: it writes `Completed` to disk BEFORE
    /// invoking [`Self::on_task_terminal`]. Without the up-front write the
    /// terminal hook reads the task as still `InProgress`, so the SEQ dispatch
    /// guard (`in_progress > 0`) returns early and the next pending task is
    /// never dispatched — the exact failure that made TodoComplete return a
    /// bare success while the queue stayed frozen.
    ///
    /// If dispatching the next task fails (e.g. the downstream dispatch actor
    /// is gone), the error propagates so the caller can report an honest
    /// failure instead of a misleading success. As a post-condition guard it
    /// re-reads the list and rejects with an `Internal` error if the target
    /// task somehow did not reach a terminal state.
    pub async fn force_complete_and_advance(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &TasklistId,
        task_id: &TaskId,
    ) -> Result<(), AoError> {
        let tasklist = self
            .tasklist_store
            .get_by_owner(owner, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;

        let current_status = tasklist
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == *task_id)
            .map(|t| t.status)
            .ok_or_else(|| AoError::TaskNotFound(task_id.clone()))?;

        // Write Completed up-front for any non-terminal status so the SEQ guard
        // in `dispatch_group` no longer counts this task as in-flight. Already
        // terminal tasks are left as-is (idempotent) — we still fall through to
        // the terminal hook so the list re-advances if it had stalled.
        if !current_status.is_terminal() {
            tracing::info!(
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                prior_status = ?current_status,
                "force_complete_and_advance: writing Completed before advancing",
            );
            self.tasklist_store
                .set_task_status_by_owner(owner, tasklist_id, task_id, TaskStatus::Completed)
                .await?;
            self.emit_task_updated(owner, tasklist_id, task_id).await;
        }

        // Drive the terminal hook, which clears the in-memory dispatch slot for
        // this task and advances the list. A dispatch failure here surfaces as
        // an Err to the caller rather than being swallowed.
        self.on_task_terminal(owner, tasklist_id, task_id).await?;

        // Honest-result post-condition: the target task MUST be terminal now.
        // If it isn't, something prevented the completion from landing and we
        // must not let the caller believe the queue advanced.
        let after = self
            .tasklist_store
            .get_by_owner(owner, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;
        let landed_terminal = after
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == *task_id)
            .map(|t| t.status.is_terminal())
            // Task disappeared (deleted concurrently) — treat as resolved.
            .unwrap_or(true);
        if !landed_terminal {
            return Err(AoError::Internal(format!(
                "TodoComplete could not advance the queue: task '{}' did not reach a terminal state",
                task_id
            )));
        }

        Ok(())
    }

    /// Idempotent re-kick used by the TodoStart control tool on an already
    /// Active list. Rather than assuming a live runner exists behind every
    /// `InProgress` task, it verifies liveness against the [`InstanceRegistry`]:
    /// any `InProgress` task whose owning agent has zero live runs (and which is
    /// past the dispatch grace window) is a zombie left over from a runner that
    /// died without reporting terminal — it is recovered (reprompted/redispatched
    /// or failed) via the same path the watchdog uses. After reconciling, the
    /// list is advanced from fresh on-disk state so a freed SEQ slot dispatches
    /// the next pending task.
    ///
    /// Returns the number of zombie tasks recovered. A return of `Ok(0)` simply
    /// means nothing needed recovery (healthy list); the advance still ran.
    /// Requires `with_instance_registry`; without it the liveness check is
    /// skipped and this degrades to a plain advance.
    pub async fn kick_and_reconcile(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &TasklistId,
    ) -> Result<usize, AoError> {
        let tasklist = self
            .tasklist_store
            .get_by_owner(owner, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;
        if tasklist.status != TasklistStatus::Active {
            // Nothing to re-kick on a non-Active list.
            return Ok(0);
        }

        let mut recovered = 0usize;
        if let Some(instance_registry) = self.instance_registry.as_ref() {
            let now = Instant::now();
            let candidates: Vec<(AgentId, TaskId)> = tasklist
                .groups
                .iter()
                .flat_map(|g| g.tasks.iter())
                .filter(|t| t.status == TaskStatus::InProgress)
                .map(|t| (resolve_executor_agent_id(&tasklist.owner, t), t.id.clone()))
                .collect();
            for (agent_id, task_id) in candidates {
                // Respect the cold-start grace for a freshly dispatched task
                // whose run has not been observed yet, but reap immediately a
                // task whose run was once observed and has since vanished. A
                // task with no dispatch timestamp (registry wiped by a restart)
                // is treated as past-grace and eligible immediately.
                match self
                    .task_liveness(instance_registry, tasklist_id, &agent_id, &task_id, now)
                    .await
                {
                    TaskLiveness::Live | TaskLiveness::Starting => continue,
                    TaskLiveness::Stuck => {}
                }
                tracing::warn!(
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    agent_id = %agent_id,
                    "kick_and_reconcile: InProgress task has no live runner; recovering",
                );
                if self
                    .recover_stuck_task(owner, tasklist_id, &agent_id, &task_id)
                    .await?
                {
                    recovered += 1;
                }
            }
        }

        // Advance from fresh state — recovery above may have freed a SEQ slot
        // (a zombie failed out) or re-dispatched the head.
        let fresh = self
            .tasklist_store
            .get_by_owner(owner, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;
        self.advance(&fresh).await?;
        Ok(recovered)
    }

    /// Look up the task currently assigned to `agent_id` in `tasklist_id`, if
    /// any. Used by `on_run_ended` to detect stale runs.
    pub async fn current_task_for_agent(
        &self,
        tasklist_id: &TasklistId,
        agent_id: &AgentId,
    ) -> Option<TaskId> {
        let reg = self.registry.read().await;
        reg.get(tasklist_id).and_then(|m| m.get(agent_id)).cloned()
    }

    /// Notify the feeder that an agent's run ended without it emitting
    /// `<task action="complete">` or `<task action="fail">` for its assigned
    /// task. If the agent has no assigned task or the task is no longer
    /// `InProgress`, this is a no-op (clean completion or already-handled).
    /// Otherwise, the same attempt cap as output validation applies: bump
    /// `attempt_count`, append a stale-run error, then either reprompt the
    /// agent (via the same dispatcher path) or transition the task to
    /// `Failed` once the cap is reached.
    pub async fn on_run_ended(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &TasklistId,
        agent_id: &AgentId,
    ) -> Result<(), AoError> {
        let task_id = match self.current_task_for_agent(tasklist_id, agent_id).await {
            Some(id) => id,
            None => {
                tracing::debug!(
                    tasklist_id = %tasklist_id,
                    agent_id = %agent_id,
                    "on_run_ended: no task assigned to agent (clean completion)",
                );
                return Ok(());
            }
        };

        let tasklist = self
            .tasklist_store
            .get_by_owner(owner, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.clone()))?;

        if tasklist.status != TasklistStatus::Active {
            tracing::info!(
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                agent_id = %agent_id,
                status = ?tasklist.status,
                "on_run_ended: tasklist not Active, skipping stale-run reprompt",
            );
            return Ok(());
        }

        let task = tasklist
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == task_id)
            .cloned()
            .ok_or_else(|| AoError::TaskNotFound(task_id.clone()))?;

        if task.status != TaskStatus::InProgress {
            tracing::debug!(
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                task_status = ?task.status,
                "on_run_ended: task no longer in_progress, no reprompt needed",
            );
            return Ok(());
        }

        tracing::warn!(
            tasklist_id = %tasklist_id,
            task_id = %task_id,
            agent_id = %agent_id,
            attempt_count = task.attempt_count,
            "on_run_ended: stale run detected (no <task action=complete|fail>); will reprompt",
        );

        // Reclaim the task for a reprompt dispatch. `task.dispatch_token`
        // above was read unlocked, so a concurrent recoverer (a watchdog
        // tick or `kick_and_reconcile` racing this same run-ended event) may
        // have already reclaimed this exact recovery cycle by the time we
        // reach the lock. `try_reclaim_dispatch_by_owner` re-reads fresh
        // state under the per-tasklist write lock and only bumps
        // `attempt_count`/`dispatch_token` if `expected_token` still
        // matches; a stale match means we lost the race and must not
        // dispatch a second time.
        let claim = self
            .tasklist_store
            .try_reclaim_dispatch_by_owner(
                owner,
                tasklist_id,
                &task_id,
                task.dispatch_token,
                self.max_attempts,
                |new_count| {
                    format!(
                        "Attempt {}: agent run ended without reporting task completion or failure. \
                         Possible causes include: the CLI accumulated too much internal context (context-window \
                         overflow — each retry starts a fresh process), a tool prompted for permission with no \
                         live approver, a sandbox blocked a write, or an MCP server was unavailable. \
                         If retries keep failing, reduce the task scope or check the agent profile \
                         (e.g. `--dangerously-skip-permissions` for tasklist runs).",
                        new_count,
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
                    "on_run_ended: task was no longer InProgress under the lock; another actor already resolved it, so the stale-run reprompt was skipped",
                );
                return Ok(());
            }
            ReclaimDispatchOutcome::Stale => {
                tracing::info!(
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    "on_run_ended: lost the reclaim race to a concurrent recovery attempt; skipping",
                );
                return Ok(());
            }
            ReclaimDispatchOutcome::Exhausted { attempt_count } => {
                tracing::warn!(
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    attempt_count,
                    max_attempts = self.max_attempts,
                    "Task exceeded max attempts due to stale runs; transitioning to Failed",
                );
                self.emit_task_updated(owner, tasklist_id, &task_id).await;
                return self.on_task_terminal(owner, tasklist_id, &task_id).await;
            }
            ReclaimDispatchOutcome::Claimed { task, .. } => task,
        };

        let expected_outputs_text = if task.expected_outputs.is_empty() {
            String::from("(none declared)")
        } else {
            format!("[{}]", task.expected_outputs.join(", "))
        };
        let reprompt_prompt = format!(
            "Stale run detected: your previous run for task '{tid}' ended without emitting \
             <task action=\"complete\" task_id=\"{tid}\" /> or <task action=\"fail\" task_id=\"{tid}\" reason=\"...\" />.\n\
             Either complete the task by ensuring all expected_outputs exist in the workspace and emitting \
             <task action=\"complete\" task_id=\"{tid}\" />, or fail it via \
             <task action=\"fail\" task_id=\"{tid}\" reason=\"...\" />.\n\
             If the previous run stopped on a tool permission prompt, sandbox block, or unavailable MCP server, \
             emit <task action=\"fail\" task_id=\"{tid}\" reason=\"...\" /> rather than retrying — name the \
             specific cause in the reason so the coordinator can fix the agent profile or workspace.\n\
             Expected outputs: {outputs}.\n\n\
             Original task prompt:\n{prompt}",
            tid = task_id,
            outputs = expected_outputs_text,
            prompt = build_dispatch_prompt(&task),
        );
        self.dispatcher
            .dispatch_task(
                &task.owner_agent_id,
                reprompt_prompt,
                owner,
                tasklist_id,
                &task_id,
            )
            .await
    }

}
