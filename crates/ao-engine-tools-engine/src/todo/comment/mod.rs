mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

pub struct TodoComment;

#[async_trait]
impl EngineTool for TodoComment {
    fn name(&self) -> &str {
        "TodoComment"
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

        let task_id = match input.get("task_id").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => {
                return Ok(ToolOutput::error(
                    "missing or empty required field: task_id",
                    true,
                ));
            }
        };

        let comment_body = match input.get("comment").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => {
                return Ok(ToolOutput::error(
                    "missing or empty required field: comment",
                    true,
                ));
            }
        };

        match svc
            .add_comment_for_agent(&ctx.agent_id, &active.id, &task_id, comment_body)
            .await
        {
            Ok(_) => Ok(ToolOutput::text(format!("comment added to task '{task_id}'"))),
            Err(AoError::TaskNotFound(_)) => Ok(ToolOutput::error(
                &format!("task '{}' not found in the active tasklist", task_id),
                true,
            )),
            Err(e) => Ok(ToolOutput::error(
                &format!("failed to add comment: {e}"),
                false,
            )),
        }
    }
}
