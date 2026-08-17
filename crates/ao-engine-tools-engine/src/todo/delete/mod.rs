mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

pub struct TodoDelete;

#[async_trait]
impl EngineTool for TodoDelete {
    fn name(&self) -> &str {
        "TodoDelete"
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
        // Only top-level agents own tasklists.
        if ctx.depth > 0 {
            return Ok(ToolOutput::error(
                "TodoDelete is not available inside a subagent context. \
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

        let task_id = match input.get("task_id").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => {
                return Ok(ToolOutput::error(
                    "missing or empty required field: task_id",
                    true,
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

        match svc
            .delete_task_for_agent(&ctx.agent_id, &active.id, &task_id)
            .await
        {
            Ok(()) => Ok(ToolOutput::text(format!(
                "task '{}' removed from tasklist",
                task_id
            ))),
            Err(AoError::TaskNotFound(_)) => Ok(ToolOutput::error(
                &format!("task '{}' not found in the active tasklist", task_id),
                true,
            )),
            Err(AoError::InvalidTasklistTransition(msg)) => {
                Ok(ToolOutput::error(&msg, true))
            }
            Err(e) => Ok(ToolOutput::error(
                &format!("failed to remove task: {e}"),
                false,
            )),
        }
    }
}
