mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use ao_protocol::workflow::TaskStatus;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct WorkflowActionSkipPhase;

#[async_trait]
impl IoTool for WorkflowActionSkipPhase {
    fn name(&self) -> &str {
        "WorkflowActionSkipPhase"
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
                    "description": "ID of the phase to skip."
                },
                "reason": {
                    "type": "string",
                    "description": "Explanation of why this phase is being skipped."
                }
            },
            "required": ["task_id", "phase_id", "reason"],
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
        let reason = match input.get("reason").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("reason is required", true)),
        };

        // Step 1: write skip state.
        if let Err(e) = runner.skip_phase(&task_id, &phase_id, &reason).await {
            return Ok(ToolOutput::Error { recoverable: true, message: e.to_string() });
        }

        // Step 2: notify queue. Queue ignores the message for Pending tasks;
        // Running tasks auto-advance. Best-effort.
        if let Err(e) = runner.notify_phase_completed(&task_id, &phase_id).await {
            tracing::warn!(
                task_id = %task_id,
                phase_id = %phase_id,
                "Failed to notify workflow queue of phase skip: {}",
                e
            );
        }

        // Step 3: result string + next-phase nudge for Pending tasks.
        let snapshot = runner.get_task_state(&task_id).await
            .map_err(|e| AoError::Internal(format!(
                "Read task state after skip_phase: {}", e
            )))?;

        let result_text = if snapshot.status == TaskStatus::Pending {
            match runner.get_next_phase(&task_id).await {
                Ok(Some(next_phase)) => format!(
                    "Phase '{}' skipped ({}).\n\n\
                     Next incomplete phase: **{}** (`{}`). If you have \
                     sufficient context, pre-fill it now. Otherwise, ask \
                     the user if they are ready to start the task.",
                    phase_id, reason, next_phase.name, next_phase.id
                ),
                Ok(None) => format!(
                    "Phase '{}' skipped ({}). No further phases to pre-fill \
                     — ask the user if they are ready to start the task.",
                    phase_id, reason
                ),
                Err(_) => format!("Phase '{}' skipped ({}).", phase_id, reason),
            }
        } else {
            format!(
                "Phase '{}' skipped ({}). The workflow runner will advance \
                 to the next phase.",
                phase_id, reason
            )
        };

        Ok(ToolOutput::text(result_text))
    }
}
