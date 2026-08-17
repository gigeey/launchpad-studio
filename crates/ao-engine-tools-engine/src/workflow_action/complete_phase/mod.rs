mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use ao_protocol::workflow::TaskStatus;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct WorkflowActionCompletePhase;

#[async_trait]
impl IoTool for WorkflowActionCompletePhase {
    fn name(&self) -> &str {
        "WorkflowActionCompletePhase"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "ID of the workflow task."
                },
                "phase_id": {
                    "type": "string",
                    "description": "ID of the phase to complete."
                }
            },
            "required": ["task_id", "phase_id"],
            "additionalProperties": false
        })
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let runner = match &ctx.workflow_runner {
            Some(r) => r.clone(),
            None => {
                return Ok(ToolOutput::Error {
                    recoverable: false,
                    message: "Workflow runner not available in this context.".into(),
                });
            }
        };

        let task_id = match input.get("task_id").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("task_id is required", true)),
        };
        let phase_id = match input.get("phase_id").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("phase_id is required", true)),
        };

        // Step 1: write the phase completion state.
        if let Err(e) = runner.complete_phase(&task_id, &phase_id).await {
            return Ok(ToolOutput::Error { recoverable: true, message: e.to_string() });
        }

        // Step 2: notify the workflow queue manager. For Running tasks this
        // triggers auto-advance to the next phase; for Pending tasks the
        // queue manager intentionally ignores the message (pre-fill mode
        // requires explicit WorkflowActionStart). Best-effort — a failure
        // here doesn't roll back the state write.
        if let Err(e) = runner.notify_phase_completed(&task_id, &phase_id).await {
            tracing::warn!(
                task_id = %task_id,
                phase_id = %phase_id,
                "Failed to notify workflow queue of phase completion: {}",
                e
            );
        }

        // Step 3: build a result string that nudges the agent forward.
        // For Pending tasks (pre-fill mode) point at the next phase so the
        // agent can pre-fill it on the following turn. For Running tasks
        // the queue manager handles dispatch — the result is just a
        // confirmation that the state changed.
        let snapshot = runner.get_task_state(&task_id).await
            .map_err(|e| AoError::Internal(format!(
                "Read task state after complete_phase: {}", e
            )))?;

        let result_text = if snapshot.status == TaskStatus::Pending {
            match runner.get_next_phase(&task_id).await {
                Ok(Some(next_phase)) => format!(
                    "Phase '{}' marked complete.\n\n\
                     Next incomplete phase: **{}** (`{}`). If you have \
                     sufficient context, pre-fill it now. Otherwise, ask \
                     the user if they are ready to start the task.",
                    phase_id, next_phase.name, next_phase.id
                ),
                Ok(None) => format!(
                    "Phase '{}' marked complete. All phases are now \
                     pre-filled — ask the user if they are ready to start \
                     the task.",
                    phase_id
                ),
                Err(_) => format!("Phase '{}' marked complete.", phase_id),
            }
        } else {
            format!(
                "Phase '{}' marked complete. The workflow runner will \
                 advance to the next phase.",
                phase_id
            )
        };

        Ok(ToolOutput::text(result_text))
    }
}
