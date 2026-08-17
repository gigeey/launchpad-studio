//! Outbound event emission and completion summaries for the task feeder.
//!
//! Every SSE event the feeder publishes on the owner channel lives here, plus
//! the completion-summary messages posted back into an agent or project queue
//! when a tasklist finishes.
//!
//! This is a continuation of the `impl TaskFeeder` block in the parent module,
//! split out for navigability rather than encapsulation: it shares the parent’s
//! imports and helpers via `use super::*`, exactly as `tests.rs` does.

use super::*;

impl TaskFeeder {
    /// Emit a `tasklist.task_updated` SSE event on the owner channel. Caller
    /// passes the *new* status string (snake_case `TaskStatus`). Public so
    /// `agent_runner` can fire from the `<task action="fail">` path that does
    /// its own `set_task_status` outside the feeder.
    ///
    /// The current `Task` is read from the tasklist store and emitted whole,
    /// so any field the caller's mutation just touched (status, owner_agent_id,
    /// expected_outputs, comments, error_log, attempt_count) reaches the client
    /// in a single payload without per-field plumbing.
    pub async fn emit_task_updated(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &TasklistId,
        task_id: &TaskId,
    ) {
        let Some(bus) = self.event_bus.as_ref() else {
            return;
        };
        let (task, project_id) = match self.tasklist_store.get_by_owner(owner, tasklist_id).await {
            Ok(Some(tl)) => {
                let pid = tl.project_id.clone();
                let found = tl
                    .groups
                    .into_iter()
                    .flat_map(|g| g.tasks.into_iter())
                    .find(|t| t.id == *task_id);
                (found, pid)
            }
            Ok(None) => (None, None),
            Err(e) => {
                tracing::warn!(
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    "emit_task_updated: failed to load tasklist: {}",
                    e
                );
                return;
            }
        };
        let Some(task) = task else {
            tracing::warn!(
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                "emit_task_updated: task not found post-mutation; skipping emit",
            );
            return;
        };
        let synth_run_id = format!("tasklist:{}", tasklist_id);
        let event_channel = owner_event_channel(owner);
        let team_id_for_event = owner_team_id_str(owner);
        let payload = AgentEventPayload::TasklistTaskUpdated {
            team_id: team_id_for_event,
            tasklist_id: tasklist_id.clone(),
            task,
            owner: Some(owner.clone()),
            project_id: project_id.clone(),
        };
        bus.emit(&synth_run_id, &event_channel, None, payload.clone())
            .await;
        if let Some(pid) = project_id {
            bus.emit(
                &synth_run_id,
                &format!("project:{}", pid),
                None,
                payload,
            )
            .await;
        }
    }

    pub(super) async fn emit_tasklist_completed(&self, owner: &TasklistOwner, tasklist_id: &TasklistId) {
        let Some(bus) = self.event_bus.as_ref() else {
            return;
        };
        let project_id = self.project_id_for(owner, tasklist_id).await;
        let synth_run_id = format!("tasklist:{}", tasklist_id);
        let event_channel = owner_event_channel(owner);
        let team_id_for_event = owner_team_id_str(owner);
        let payload = AgentEventPayload::TasklistCompleted {
            team_id: team_id_for_event,
            tasklist_id: tasklist_id.clone(),
            owner: Some(owner.clone()),
            project_id: project_id.clone(),
        };
        bus.emit(&synth_run_id, &event_channel, None, payload.clone())
            .await;
        if let Some(pid) = project_id {
            bus.emit(&synth_run_id, &format!("project:{}", pid), None, payload)
                .await;
        }
    }

    pub(super) async fn emit_tasklist_failed(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &TasklistId,
        reason: Option<String>,
    ) {
        let Some(bus) = self.event_bus.as_ref() else {
            return;
        };
        let project_id = self.project_id_for(owner, tasklist_id).await;
        let synth_run_id = format!("tasklist:{}", tasklist_id);
        let event_channel = owner_event_channel(owner);
        let team_id_for_event = owner_team_id_str(owner);
        let payload = AgentEventPayload::TasklistFailed {
            team_id: team_id_for_event,
            tasklist_id: tasklist_id.clone(),
            reason,
            owner: Some(owner.clone()),
            project_id: project_id.clone(),
        };
        bus.emit(&synth_run_id, &event_channel, None, payload.clone())
            .await;
        if let Some(pid) = project_id {
            bus.emit(&synth_run_id, &format!("project:{}", pid), None, payload)
                .await;
        }
    }

    /// Emit a `TasklistStatusChanged` event on the owner's channel (agent or
    /// team). Public so `TasklistService` can announce an agent-scope
    /// Paused→Active "start" transition — the agent path has no feeder-level
    /// resume of its own, so the service drives the status flip + advance and
    /// reuses this emitter to keep the Todo panel's status pill live.
    pub async fn emit_tasklist_status_changed(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &TasklistId,
        status: TasklistStatus,
    ) {
        let Some(bus) = self.event_bus.as_ref() else {
            return;
        };
        let project_id = self.project_id_for(owner, tasklist_id).await;
        let synth_run_id = format!("tasklist:{}", tasklist_id);
        let event_channel = owner_event_channel(owner);
        let team_id_for_event = owner_team_id_str(owner);
        let payload = AgentEventPayload::TasklistStatusChanged {
            team_id: team_id_for_event,
            tasklist_id: tasklist_id.clone(),
            status: tasklist_status_to_str(status).to_string(),
            owner: Some(owner.clone()),
            project_id: project_id.clone(),
        };
        bus.emit(&synth_run_id, &event_channel, None, payload.clone())
            .await;
        if let Some(pid) = project_id {
            bus.emit(&synth_run_id, &format!("project:{}", pid), None, payload)
                .await;
        }
    }

    /// Emit a `TaskDeferred` event on the `tasklist:{tasklist_id}` channel so
    /// the Tasks panel can render the 'Classifying…' badge. No-op when the event
    /// bus is not wired.
    pub(super) async fn emit_task_deferred(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &TasklistId,
        task_id: &TaskId,
        reason: &str,
    ) {
        let Some(bus) = self.event_bus.as_ref() else {
            return;
        };
        let synth_run_id = format!("tasklist:{}", tasklist_id);
        let event_channel = format!("tasklist:{}", tasklist_id);
        let team_id_for_event = owner_team_id_str(owner);
        bus.emit(
            &synth_run_id,
            &event_channel,
            None,
            AgentEventPayload::TaskDeferred {
                team_id: team_id_for_event,
                tasklist_id: tasklist_id.clone(),
                task_id: task_id.clone(),
                reason: reason.to_string(),
                owner: Some(owner.clone()),
                project_id: None,
            },
        )
        .await;
    }

    /// Post a natural-language completion-summary message to an agent's queue
    /// when a tasklist it owns reaches a terminal state. Originally team-only
    /// (coordinator escalation); now also fires for agent-owned
    /// tasklists in async mode so the agent receives a wake-up entry in its
    /// own mailbox and can decide whether to follow up, retry, or surface to
    /// the user. Sync TodoCreate callers are excluded at the call site —
    /// they already receive the TerminalReport inline from the tool call, so
    /// queuing a second message would just produce a duplicate turn.
    ///
    /// No-op when neither `notification_dispatcher` nor `project_dispatcher` is
    /// wired (test fixtures that don't care about this behaviour stay green).
    pub(super) async fn post_completion_summary(&self, agent_id: &str, tasklist: &Tasklist) {
        let summaries = self.load_task_summaries(tasklist).await;
        let data_root = self.tasklist_store.data_root();
        let all_tasks: Vec<&Task> = tasklist
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .collect();
        let succeeded = all_tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .count();
        let failed = all_tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Failed)
            .count();
        let skipped = all_tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Skipped)
            .count();
        // One bullet per item carrying the subagent's notification summary so
        // the agent reads concluded results inline rather than re-deriving them
        // from titles. Non-completed items also get a pointer to their full
        // output file so the agent can dig in when the one-liner isn't enough.
        let mut item_lines: Vec<String> = Vec::with_capacity(all_tasks.len());
        for t in &all_tasks {
            let title = t.prompt.lines().next().unwrap_or(t.prompt.as_str());
            let status = match t.status {
                TaskStatus::Completed => "completed",
                TaskStatus::Failed => "failed",
                TaskStatus::Skipped => "skipped",
                TaskStatus::Pending => "pending",
                TaskStatus::InProgress => "in_progress",
                TaskStatus::Blocked => "blocked",
                TaskStatus::Stopped => "stopped",
            };
            let summary = summaries
                .get(&t.id)
                .map(|e| e.summary.as_str())
                .unwrap_or("(no summary reported)");
            let mut line = format!("- {title} [{status}]: {summary}");
            if t.status != TaskStatus::Completed {
                let output_path =
                    data_root.agent_tasklist_task_output_path(agent_id, &tasklist.id, &t.id);
                line.push_str(&format!(" (full output: {})", output_path.display()));
            }
            item_lines.push(line);
        }
        let items_block = if item_lines.is_empty() {
            String::new()
        } else {
            format!("\n\nPer-item results:\n{}", item_lines.join("\n"))
        };

        // For project-tagged tasklists, route the summary back through the
        // project channel so the agent's response streams to the project page
        // and the project orchestration system prompt is injected. The guidance
        // appended here tells the agent to validate progress against the project
        // goal and create follow-up tasklists for any remaining gaps.
        if let Some(project_id) = &tasklist.project_id {
            let Some(proj_dispatcher) = self.project_dispatcher.get() else {
                // No project dispatcher wired; fall through to personal queue.
                tracing::debug!(
                    agent_id = %agent_id,
                    tasklist_id = %tasklist.id,
                    project_id = %project_id,
                    "post_completion_summary: project_dispatcher not wired, falling back to agent queue",
                );
                self.post_completion_summary_to_agent(
                    agent_id, tasklist, succeeded, failed, skipped, &items_block,
                ).await;
                return;
            };
            let content = format!(
                "Tasklist '{}' finished: {} succeeded, {} failed, {} skipped.{}\n\n{}\n\n{}",
                tasklist.title,
                succeeded,
                failed,
                skipped,
                items_block,
                TASKLIST_COMPLETION_GUIDANCE,
                PROJECT_COMPLETION_GUIDANCE,
            );
            let message = QueuedMessage {
                message_id: Uuid::new_v4().to_string(),
                content,
                queued_at: Utc::now(),
                attachments: vec![],
                source: None,
                focus_path: None,
                thread_id: tasklist.thread_id.clone(),
            };
            if let Err(e) = proj_dispatcher.submit_to_project(project_id, message).await {
                tracing::warn!(
                    agent_id = %agent_id,
                    tasklist_id = %tasklist.id,
                    project_id = %project_id,
                    error = %e,
                    "post_completion_summary: failed to queue message to project channel",
                );
            }
            return;
        }

        self.post_completion_summary_to_agent(
            agent_id, tasklist, succeeded, failed, skipped, &items_block,
        ).await;
    }

    /// Submit the completion summary to the owning agent's personal queue.
    async fn post_completion_summary_to_agent(
        &self,
        agent_id: &str,
        tasklist: &Tasklist,
        succeeded: usize,
        failed: usize,
        skipped: usize,
        items_block: &str,
    ) {
        let Some(dispatcher) = self.notification_dispatcher.get() else {
            return;
        };
        let content = format!(
            "Tasklist '{}' finished: {} succeeded, {} failed, {} skipped.{}\n\n{}",
            tasklist.title, succeeded, failed, skipped, items_block, TASKLIST_COMPLETION_GUIDANCE
        );
        let message = QueuedMessage {
            message_id: Uuid::new_v4().to_string(),
            content,
            queued_at: Utc::now(),
            attachments: vec![],
            source: None,
            focus_path: None,
            thread_id: tasklist.thread_id.clone(),
        };
        if let Err(e) = dispatcher.submit_to_agent(agent_id, message).await {
            tracing::warn!(
                agent_id = %agent_id,
                tasklist_id = %tasklist.id,
                error = %e,
                "post_completion_summary: failed to queue message",
            );
        }
    }

    /// Emit a single `TodoListComplete` event on the parent agent's chat
    /// channel when an agent-owned tasklist reaches terminal. No-op when the
    /// event bus is not wired (e.g. test fixtures that don't care about events).
    ///
    /// Suppression is keyed on scope (Agent), not on whether a sync watcher
    /// fired — both sync and async agent-owned runs produce this event, while
    /// `post_completion_summary` is never called for agent scope.
    pub(super) async fn emit_todo_list_complete(&self, agent_id: &str, tasklist: &Tasklist) {
        let Some(bus) = self.event_bus.as_ref() else {
            return;
        };
        let all_tasks: Vec<&Task> = tasklist
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .collect();
        let succeeded = all_tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .count();
        let failed = all_tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Failed)
            .count();
        let skipped = all_tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Skipped)
            .count();
        let status_str = match tasklist.status {
            TasklistStatus::Completed => "completed",
            TasklistStatus::Failed => "failed",
            TasklistStatus::Cancelled => "cancelled",
            _ => "unknown",
        };
        let tasks = all_tasks
            .iter()
            .map(|t| {
                let task_status = match t.status {
                    TaskStatus::Completed => "completed",
                    TaskStatus::Failed => "failed",
                    TaskStatus::Skipped => "skipped",
                    TaskStatus::Pending => "not_started",
                    TaskStatus::InProgress => "in_progress",
                    TaskStatus::Blocked => "blocked",
                    TaskStatus::Stopped => "stopped",
                };
                TodoListCompleteTask {
                    task_id: t.id.clone(),
                    title: t.prompt.lines().next().unwrap_or(&t.prompt).to_string(),
                    status: task_status.to_string(),
                    summary: None,
                    owner_agent_id: t.assignment.as_ref().map(|a| a.owner_agent_id.clone()),
                }
            })
            .collect();
        let agent_id_str = agent_id.to_string();
        let complete_payload = AgentEventPayload::TodoListComplete {
            tasklist_id: tasklist.id.clone(),
            status: status_str.to_string(),
            counts: TodoListTerminalCounts {
                succeeded,
                failed,
                skipped,
                cancelled: 0,
            },
            tasks,
        };
        // Route the completion pill to exactly one surface. A project-owned
        // tasklist belongs to the project chat: emit on `project:{pid}` and
        // persist to the project transcript only — never the coordinator
        // agent's own chat, where it would read as noise unrelated to the
        // user's direct conversation. A plain agent-owned tasklist routes to
        // the agent's own channel and transcript as before. The `source` arg
        // stays the originating agent in both cases; only the delivery channel
        // (and the matching persisted transcript below) differs.
        let (event_channel, transcript_key) = match &tasklist.project_id {
            Some(pid) => (format!("project:{pid}"), format!("project_{pid}")),
            None => (agent_id_str.clone(), agent_id_str.clone()),
        };
        bus.emit(
            &agent_id_str,
            &event_channel,
            tasklist.thread_id.clone(),
            complete_payload,
        )
        .await;

        // The bus event above only reaches clients connected at the instant of
        // completion. When the agent then wakes and replies, a user who later
        // navigates back to the thread sees that reply with no indication of
        // what triggered it — the live pill is gone because it was never on
        // disk. Persist a matching system entry so the marker survives reloads
        // and keeps sitting just before the follow-up reply. Wording mirrors
        // the live client pill so both render identically.
        let verb = match status_str {
            "failed" => "ended with failures",
            "cancelled" => "was cancelled",
            _ => "completed",
        };
        let mut detail = format!("{succeeded} done");
        if failed > 0 {
            detail.push_str(&format!(", {failed} failed"));
        }
        if skipped > 0 {
            detail.push_str(&format!(", {skipped} skipped"));
        }
        let entry = TranscriptEntry {
            ts: Utc::now(),
            role: TranscriptRole::System("system".to_string()),
            content: format!("Todo list {verb} · {detail}"),
            event_type: "todo_list_complete".to_string(),
            metadata: None,
            hidden_from_user: false,
        };
        let transcripts = TranscriptStore::new(self.tasklist_store.data_root().clone());
        // Route the on-disk marker to the thread's own transcript file when
        // `TodoCreate` happened on a non-default thread, so it lands next to
        // the conversation that started the tasklist instead of always
        // falling back to the agent's (or project's) legacy transcript.
        let thread = match self.threads.get() {
            Some(store) => store.resolve_non_default(tasklist.thread_id.as_deref()).await,
            None => None,
        };
        let write_result = match thread {
            Some(thread) => {
                transcripts
                    .append_at(&std::path::PathBuf::from(&thread.transcript_path), &entry)
                    .await
            }
            None => transcripts.append(&transcript_key, &entry).await,
        };
        if let Err(e) = write_result {
            tracing::warn!(
                error = %e,
                transcript_key = %transcript_key,
                "failed to persist todo_list_complete transcript marker",
            );
        }
    }

    /// Emit a `TasklistWoke` event with the supplied reason. No-op
    /// when the feeder was constructed without an event bus (e.g. some test
    /// fixtures), so call sites don't need to gate on bus availability.
    pub(super) async fn emit_lifecycle_wake(
        &self,
        team_id: &TeamId,
        tasklist_id: &TasklistId,
        reason: crate::tasklist_lifecycle::WakeReason,
    ) {
        let Some(bus) = self.event_bus.as_ref() else {
            return;
        };
        crate::tasklist_lifecycle::emit_wake(bus, team_id, tasklist_id, reason).await;
    }

    pub(super) async fn emit_tasklist_created(&self, team_id: &TeamId, tasklist_id: &TasklistId) {
        let Some(bus) = self.event_bus.as_ref() else {
            return;
        };
        let synth_run_id = format!("tasklist:{}", tasklist_id);
        let team_event_id = format!("team:{}", team_id);
        bus.emit(
            &synth_run_id,
            &team_event_id,
            None,
            AgentEventPayload::TasklistCreated {
                team_id: team_id.clone(),
                tasklist_id: tasklist_id.clone(),
                owner: Some(TasklistOwner::Team {
                    team_id: team_id.clone(),
                }),
                project_id: None,
            },
        )
        .await;
    }

}
