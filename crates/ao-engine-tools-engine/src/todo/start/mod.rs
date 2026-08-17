mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, StartOutcomeKind, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

pub struct TodoStart;

#[async_trait]
impl EngineTool for TodoStart {
    fn name(&self) -> &str {
        "TodoStart"
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
                "TodoStart is not available inside a subagent context. \
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

        match svc.start_for_agent(&ctx.agent_id).await {
            Ok(outcome) => {
                // Derive `outcome`/`message` from what actually happened —
                // never a fixed optimistic string. See `StartOutcomeKind` for
                // why `dispatched_task_ids` is the only trustworthy signal.
                let (outcome_key, dispatched_task_ids, message) = match outcome.kind {
                    StartOutcomeKind::Dispatched { task_ids } => {
                        let message = format!(
                            "Dispatched {} task{}: {}",
                            task_ids.len(),
                            if task_ids.len() == 1 { "" } else { "s" },
                            task_ids.join(", "),
                        );
                        ("dispatched", task_ids, message)
                    }
                    StartOutcomeKind::AlreadyRunning => (
                        "already_running",
                        Vec::new(),
                        "Tasklist already has a task in flight; nothing new dispatched."
                            .to_string(),
                    ),
                    StartOutcomeKind::NoPending => (
                        "no_pending",
                        Vec::new(),
                        "Tasklist is active but has no pending task to dispatch.".to_string(),
                    ),
                };
                Ok(ToolOutput::structured(serde_json::json!({
                    "tasklist_id": outcome.tasklist_id,
                    "status": "active",
                    "outcome": outcome_key,
                    "dispatched_count": dispatched_task_ids.len(),
                    "dispatched_task_ids": dispatched_task_ids,
                    "message": message,
                })))
            }
            Err(AoError::InvalidTasklistTransition(msg)) => Ok(ToolOutput::error(&msg, true)),
            Err(AoError::Internal(msg)) if msg.contains("dispatcher may be unavailable") => {
                // The start path reached the tasklist but the feeder never
                // picked up a ready task — surface this as a real failure
                // (recoverable: a retry may succeed once the feeder is back)
                // rather than folding it into a fake "active" success.
                Ok(ToolOutput::error(&format!("dispatch_failed: {msg}"), true))
            }
            Err(e) => Ok(ToolOutput::error(
                &format!("failed to start tasklist: {e}"),
                false,
            )),
        }
    }
}
