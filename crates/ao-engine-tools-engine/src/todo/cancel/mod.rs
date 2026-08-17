mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

pub struct TodoCancel;

#[async_trait]
impl EngineTool for TodoCancel {
    fn name(&self) -> &str {
        "TodoCancel"
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

    async fn invoke(&self, _input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        // Only top-level agents own tasklists.
        if ctx.depth > 0 {
            return Ok(ToolOutput::error(
                "TodoCancel is not available inside a subagent context. \
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

        match svc.cancel_for_agent(&ctx.agent_id).await {
            Ok(outcome) => Ok(ToolOutput::structured(serde_json::json!({
                "tasklist_id": outcome.tasklist_id,
                "status": "cancelled",
                "skipped_count": outcome.skipped_count,
                "in_flight_count": outcome.in_flight_count,
            }))),
            Err(AoError::ValidationError(msg)) => {
                Ok(ToolOutput::error(&msg, true))
            }
            Err(e) => Ok(ToolOutput::error(&format!("failed to cancel tasklist: {e}"), false)),
        }
    }
}
