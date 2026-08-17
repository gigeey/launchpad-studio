mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput};
use ao_protocol::{
    error::AoError,
    tasklist::{AssignmentMode, TaskGroupMode},
};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct TodoList;

#[async_trait]
impl EngineTool for TodoList {
    fn name(&self) -> &str {
        "TodoList"
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

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn invoke(&self, _input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
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
                return Ok(ToolOutput::structured(json!({
                    "active": false,
                    "message": "No active tasklist. Use TodoCreate to create one."
                })));
            }
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to fetch active tasklist: {e}"),
                    false,
                ));
            }
        };

        let groups: Vec<Value> = active
            .groups
            .iter()
            .map(|g| {
                let mode_str = match g.mode {
                    TaskGroupMode::Par => "par",
                    TaskGroupMode::Seq => "seq",
                };
                let tasks: Vec<Value> = g
                    .tasks
                    .iter()
                    .map(|t| {
                        // The feeder's `resolve_executor_agent_id` (task_feeder.rs)
                        // treats `assignment.owner_agent_id` as authoritative for
                        // Agent-owned tasklists, falling back to the base
                        // `owner_agent_id` only when `assignment` is absent/empty.
                        // Mirror that precedence here so the MCP list surface never
                        // disagrees with the feeder/UI (e.g. right after an owner
                        // pin via TodoUpdate, where only `assignment` is updated).
                        // TodoList doesn't currently know the tasklist's owner kind,
                        // but that's fine: Team-owned tasklists normally carry no
                        // `assignment`, so this falls straight back to the base
                        // field for them, unchanged from before.
                        let assignment_mode = t.assignment.as_ref().map(|a| match a.mode {
                            AssignmentMode::Pinned => "pinned",
                            AssignmentMode::Classified => "classified",
                        });
                        let assignee = t
                            .assignment
                            .as_ref()
                            .map(|a| a.owner_agent_id.clone())
                            .filter(|id| !id.is_empty())
                            .or_else(|| Some(t.owner_agent_id.clone()).filter(|id| !id.is_empty()));
                        json!({
                            "id": t.id,
                            "prompt": t.prompt,
                            "status": format!("{:?}", t.status).to_lowercase(),
                            "assignee": assignee,
                            "assignment_mode": assignment_mode
                        })
                    })
                    .collect();
                json!({
                    "group_id": g.id,
                    "mode": mode_str,
                    "tasks": tasks
                })
            })
            .collect();

        Ok(ToolOutput::structured(json!({
            "active": true,
            "tasklist_id": active.id,
            "name": active.title,
            "status": format!("{:?}", active.status).to_lowercase(),
            "groups": groups
        })))
    }
}
