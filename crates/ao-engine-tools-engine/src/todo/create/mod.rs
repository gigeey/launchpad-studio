mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput, UserEvent};
use ao_protocol::{
    error::AoError,
    event::TodoListCreatedItem,
    tasklist::{AssignmentMode, Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus},
};
use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::todo::{TodoDispatchMode, TodoItem, TodoMode};

/// Heartbeat cadence while a sync TodoCreate is in-flight.
const HEARTBEAT_CADENCE_SECS: u64 = 10;

pub struct TodoCreate;

#[async_trait]
impl EngineTool for TodoCreate {
    fn name(&self) -> &str {
        "TodoCreate"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::input_schema()
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        // UnsupportedScope: only top-level agent contexts have a persistent queue.
        if ctx.depth > 0 {
            return Ok(ToolOutput::error(
                "TodoCreate is not available inside a subagent context. \
                 Only top-level agents with a persistent message queue can own a tasklist.",
                true,
            ));
        }

        let svc = match &ctx.tasklist_service {
            Some(s) => Arc::clone(s),
            None => {
                return Ok(ToolOutput::error(
                    "Tasklist service not available in this context.",
                    false,
                ));
            }
        };

        let name = match input.get("name").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => {
                return Ok(ToolOutput::error("missing or empty required field: name", true));
            }
        };

        let items_val = match input.get("items").and_then(|v| v.as_array()) {
            Some(a) if !a.is_empty() => a,
            Some(_) => {
                return Ok(ToolOutput::error("items must contain at least one entry", true));
            }
            None => {
                return Ok(ToolOutput::error("missing required field: items", true));
            }
        };

        let mode = match input.get("mode").and_then(|v| v.as_str()) {
            Some("par") => TodoMode::Par,
            Some("seq") | None => TodoMode::Seq,
            Some(other) => {
                return Ok(ToolOutput::error(
                    &format!("unknown mode '{}'; must be 'seq' or 'par'", other),
                    true,
                ));
            }
        };

        let dispatch_mode = match input.get("dispatch_mode").and_then(|v| v.as_str()) {
            Some("sync") | None => TodoDispatchMode::Sync,
            Some("async") => TodoDispatchMode::Async,
            Some(other) => {
                return Ok(ToolOutput::error(
                    &format!(
                        "unknown dispatch_mode '{}'; must be 'sync' or 'async'",
                        other
                    ),
                    true,
                ));
            }
        };

        let mut items: Vec<TodoItem> = Vec::with_capacity(items_val.len());
        for (i, entry) in items_val.iter().enumerate() {
            let title = match entry.get("title").and_then(|v| v.as_str()) {
                Some(s) if !s.trim().is_empty() => s.to_string(),
                _ => {
                    return Ok(ToolOutput::error(
                        &format!("items[{i}]: missing or empty 'title'"),
                        true,
                    ));
                }
            };
            let brief = match entry.get("brief").and_then(|v| v.as_str()) {
                Some(s) if !s.trim().is_empty() => s.to_string(),
                _ => {
                    return Ok(ToolOutput::error(
                        &format!("items[{i}]: missing or empty 'brief'"),
                        true,
                    ));
                }
            };
            let raw_owner = entry
                .get("owner")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let owner = match raw_owner {
                Some(raw) => {
                    match super::owner_resolution::resolve_owner(
                        ctx.agent_profile_store.as_ref(),
                        &ctx.agent_id,
                        raw,
                    )
                    .await
                    {
                        Ok(resolved) => Some(resolved),
                        Err(out) => return Ok(out),
                    }
                }
                None => None,
            };
            items.push(TodoItem { title, brief, owner });
        }

        // MaxInstancesTooLow guard.
        match svc.get_agent_max_instances(&ctx.agent_id).await {
            Ok(max) if max < 2 => {
                return Ok(ToolOutput::error(
                    &format!(
                        "TodoCreate requires max_instances >= 2 (current: {}). \
                         Increase max_instances in the agent profile to at least 2 \
                         so the tasklist dispatcher and the user chat thread can run concurrently.",
                        max
                    ),
                    true,
                ));
            }
            Ok(_) => {}
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to read agent profile: {e}"),
                    false,
                ));
            }
        }

        // AlreadyExists guard.
        match svc.agent_active(&ctx.agent_id).await {
            Ok(Some(existing)) => {
                return Ok(ToolOutput::error(
                    &format!(
                        "agent already has an active tasklist '{}' (id: {}). \
                         Complete or stop the existing list before creating a new one.",
                        existing.title, existing.id
                    ),
                    true,
                ));
            }
            Ok(None) => {}
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to check for existing tasklist: {e}"),
                    false,
                ));
            }
        }

        // Build the task group from input items.
        // Items with an owner get Pinned assignment; others start with None and
        // are resolved by the background classifier.
        let group_id = Uuid::new_v4().to_string();
        let tg_mode = match mode {
            TodoMode::Par => TaskGroupMode::Par,
            TodoMode::Seq => TaskGroupMode::Seq,
        };
        let tasks: Vec<Task> = items
            .iter()
            .map(|item| {
                let assignment = item.owner.as_ref().map(|owner_id| TaskAssignment {
                    owner_agent_id: owner_id.clone(),
                    mode: AssignmentMode::Pinned,
                });
                Task {
                    id: Uuid::new_v4().to_string(),
                    owner_agent_id: item.owner.clone().unwrap_or_default(),
                    prompt: format!("{}: {}", item.title, item.brief),
                    expected_outputs: Vec::new(),
                    status: TaskStatus::Pending,
                    group_id: group_id.clone(),
                    attempt_count: 0,
                    error_log: Vec::new(),
                    comments: Vec::new(),
                    attachments: Vec::new(),
                    remind_me: None,
                    parse_failed: false,
                    notification_parse_retry_count: 0,
                    assignment,
                    classifier_token: 0,
                    dispatch_token: 0,
                }
            })
            .collect();

        let groups = vec![TaskGroup {
            id: group_id,
            mode: tg_mode,
            tasks,
        }];

        let tl = match svc
            .create_for_agent_with_project(
                &ctx.agent_id,
                name.clone(),
                groups,
                ctx.project_id.clone(),
                ctx.thread_id.clone(),
            )
            .await
        {
            Ok(tl) => tl,
            Err(e) => return Ok(ToolOutput::error(&format!("failed to create tasklist: {e}"), false)),
        };

        let item_count = tl.groups.iter().map(|g| g.tasks.len()).sum::<usize>();
        let all_tasks: Vec<_> = tl.groups.iter().flat_map(|g| g.tasks.iter()).collect();

        // Spawn background classifiers for items that have no pinned owner.
        // Each spawn carries the task's current classifier_token for CAS write-back.
        if let Some(classifier) = ctx.classifier.as_ref() {
            for task in all_tasks.iter().filter(|t| t.assignment.is_none()) {
                let (task_title, task_desc) = {
                    let mut parts = task.prompt.splitn(2, ": ");
                    let title = parts.next().unwrap_or("").to_string();
                    let desc = parts.next().unwrap_or("").to_string();
                    (title, desc)
                };
                tokio::spawn(super::classify_with_retry(
                    Arc::clone(classifier),
                    Arc::clone(&svc),
                    ctx.classifier_in_flight.clone(),
                    ctx.agent_id.clone(),
                    tl.id.clone(),
                    task.id.clone(),
                    ctx.agent_id.clone(),
                    task_title,
                    task_desc,
                    task.classifier_token,
                ));
            }
        }

        // Emit TodoListCreated on the parent's chat channel. Snapshot includes
        // assignment state at emit time; classified items may show None while
        // the background spawns above are still in-flight.
        let created_items: Vec<TodoListCreatedItem> = all_tasks
            .iter()
            .zip(items.iter())
            .map(|(task, item)| TodoListCreatedItem {
                task_id: task.id.clone(),
                title: item.title.clone(),
                assignment: task.assignment.clone(),
            })
            .collect();
        let _ = ctx
            .event_sink
            .emit(UserEvent::TodoListCreated {
                tasklist_id: tl.id.clone(),
                item_count,
                items: created_items,
            })
            .await;

        let exec_mode_str = if tl
            .groups
            .first()
            .map(|g| g.mode == TaskGroupMode::Par)
            .unwrap_or(false)
        {
            "par"
        } else {
            "seq"
        };

        match dispatch_mode {
            TodoDispatchMode::Async => Ok(ToolOutput::structured(serde_json::json!({
                "tasklist_id": tl.id,
                "name": tl.title,
                "item_count": item_count,
                "mode": exec_mode_str,
                "dispatch_mode": "async",
                "status": "active"
            }))),
            TodoDispatchMode::Sync => {
                // Register the watcher BEFORE yielding so a fast tasklist cannot
                // reach terminal state between create and the await below.
                let guard = match svc.terminal_watcher(&tl.id).await {
                    Ok(g) => g,
                    Err(e) => {
                        return Ok(ToolOutput::error(
                            &format!("failed to register sync watcher: {e}"),
                            false,
                        ));
                    }
                };

                // Pin the watcher future so it can be polled repeatedly inside
                // the select! loop without being consumed on each iteration.
                let wait_fut = guard.wait();
                tokio::pin!(wait_fut);

                // Skip the immediate first tick; subsequent ticks fire every
                // HEARTBEAT_CADENCE_SECS so the frontend pill stays live.
                let mut heartbeat =
                    tokio::time::interval(Duration::from_secs(HEARTBEAT_CADENCE_SECS));
                heartbeat.tick().await;

                let report = loop {
                    tokio::select! {
                        biased;

                        // Watcher fired — tasklist reached a terminal state.
                        result = &mut wait_fut => {
                            break match result {
                                Ok(r) => r,
                                Err(e) => {
                                    return Ok(ToolOutput::error(
                                        &format!(
                                            "tasklist closed before reaching terminal state: {e}"
                                        ),
                                        false,
                                    ))
                                }
                            };
                        }

                        // Parent run cancelled — surface as a recoverable error.
                        _ = ctx.cancel.cancelled() => {
                            return Ok(ToolOutput::error(
                                "sync tasklist wait cancelled by parent run",
                                true,
                            ));
                        }

                        // Heartbeat tick — emit a tool_progress event so the
                        // frontend pill updates and the CLI idle-timeout stays
                        // paused (tools_in_flight counter already covers this,
                        // but the event gives the UI live progress data).
                        _ = heartbeat.tick() => {
                            if let Ok(Some(current_tl)) =
                                svc.agent_active(&ctx.agent_id).await
                            {
                                let all_tasks: Vec<_> = current_tl
                                    .groups
                                    .iter()
                                    .flat_map(|g| g.tasks.iter())
                                    .collect();
                                let items_done = all_tasks
                                    .iter()
                                    .filter(|t| t.status.is_terminal())
                                    .count();
                                let last_terminal_task_title = all_tasks
                                    .iter()
                                    .filter(|t| t.status.is_terminal())
                                    .last()
                                    .map(|t| {
                                        t.prompt.lines().next().unwrap_or("").to_string()
                                    });
                                let _ = ctx
                                    .event_sink
                                    .emit(UserEvent::ToolProgress {
                                        tasklist_id: tl.id.clone(),
                                        items_done,
                                        items_total: item_count,
                                        last_terminal_task_title,
                                    })
                                    .await;
                            }
                        }
                    }
                };

                let progress_log =
                    format!("{}/progress.jsonl", tl.workspace_dir.trim_end_matches('/'));

                let tasks_json: Vec<serde_json::Value> = report
                    .tasks
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "id": t.id,
                            "title": t.title,
                            "status": t.status,
                            // `summary`/`details` come from each subagent's
                            // <task-item-notification> via the changelog —
                            // the concluded result of the item, not just its title.
                            "summary": t.summary,
                            "details": t.details,
                            "output_path": t.output_path.to_string_lossy(),
                            "attempt_count": t.attempt_count,
                        })
                    })
                    .collect();

                Ok(ToolOutput::structured(serde_json::json!({
                    "tasklist_id": tl.id,
                    "status": report.status,
                    "counts": {
                        "succeeded": report.counts.succeeded,
                        "failed": report.counts.failed,
                        "skipped": report.counts.skipped,
                    },
                    "tasks": tasks_json,
                    "progress_log": progress_log,
                    "guidance": prompt::SYNC_COMPLETION_GUIDANCE,
                })))
            }
        }
    }
}
