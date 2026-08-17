mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput};
use ao_protocol::{
    error::AoError,
    tasklist::{AssignmentMode, Task, TaskAssignment, TaskGroupMode, TaskStatus},
};
use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::todo::{TodoItem, TodoMode};

pub struct TodoAdd;

#[async_trait]
impl EngineTool for TodoAdd {
    fn name(&self) -> &str {
        "TodoAdd"
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
        let svc = match &ctx.tasklist_service {
            Some(s) => Arc::clone(s),
            None => {
                return Ok(ToolOutput::error(
                    "Tasklist service not available in this context.",
                    false,
                ));
            }
        };

        let active = match svc.agent_active(&ctx.agent_id).await {
            Ok(Some(tl)) => tl,
            Ok(None) => {
                return Ok(ToolOutput::error(
                    "No active tasklist found. Use TodoCreate to create one first.",
                    true,
                ));
            }
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to check for active tasklist: {e}"),
                    false,
                ));
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

        let tg_mode = match mode {
            TodoMode::Par => TaskGroupMode::Par,
            TodoMode::Seq => TaskGroupMode::Seq,
        };

        // Items with an explicit owner get a `Pinned` assignment that
        // bypasses classification; items without an owner start as `None`
        // and are resolved by the background classifier spawned below
        // (mirrors the TodoCreate flow so newly-appended tasks dispatch
        // without waiting for the 6h boot sweep).
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
                    group_id: String::new(), // filled in by service
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

        let added_count = tasks.len();

        let tl = match svc
            .add_group_for_agent(&ctx.agent_id, &active.id, tasks, tg_mode)
            .await
        {
            Ok(tl) => tl,
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to add tasks: {e}"),
                    false,
                ));
            }
        };

        // Spawn background classifiers for tasks in the just-appended group
        // that have no pinned owner. We identify the appended group as the
        // last group on the tasklist — `add_group_for_agent` always appends.
        // Each spawn carries the task's `classifier_token` so a concurrent
        // edit invalidates the write-back via CAS.
        if let Some(classifier) = ctx.classifier.as_ref() {
            if let Some(last_group) = tl.groups.last() {
                for task in last_group.tasks.iter().filter(|t| t.assignment.is_none()) {
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
        }

        let total = tl.groups.iter().map(|g| g.tasks.len()).sum::<usize>();
        Ok(ToolOutput::structured(serde_json::json!({
            "tasklist_id": tl.id,
            "added_count": added_count,
            "total_items": total,
            "mode": if tg_mode == TaskGroupMode::Par { "par" } else { "seq" }
        })))
    }
}
