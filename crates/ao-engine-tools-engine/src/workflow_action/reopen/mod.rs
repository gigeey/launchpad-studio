mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct WorkflowActionReopen;

#[async_trait]
impl IoTool for WorkflowActionReopen {
    fn name(&self) -> &str {
        "WorkflowActionReopen"
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
                    "description": "ID of the workflow task to reopen."
                },
                "phase_id": {
                    "type": "string",
                    "description": "ID of the phase to rewind to for re-execution."
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

        match runner.reopen_task(&task_id, &phase_id).await {
            Ok(file_count) => Ok(ToolOutput::text(format!(
                "Reopened task '{}' to phase '{}'. {} existing output file{} preserved.",
                task_id,
                phase_id,
                file_count,
                if file_count == 1 { "" } else { "s" }
            ))),
            Err(e) => Ok(ToolOutput::Error {
                recoverable: true,
                message: e.to_string(),
            }),
        }
    }
}
