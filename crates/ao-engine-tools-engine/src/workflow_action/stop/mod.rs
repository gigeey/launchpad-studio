mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use ao_protocol::workflow::TaskStatus;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct WorkflowActionStop;

#[async_trait]
impl IoTool for WorkflowActionStop {
    fn name(&self) -> &str {
        "WorkflowActionStop"
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
                    "description": "ID of the workflow task to stop."
                }
            },
            "required": ["task_id"],
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

        // Idempotency: check current status before stopping
        let snapshot = match runner.get_task_state(&task_id).await {
            Ok(s) => s,
            Err(e) => return Ok(ToolOutput::Error { recoverable: true, message: e.to_string() }),
        };

        let status_label = match snapshot.status {
            TaskStatus::Stopped => "stopped",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Archived => "archived",
            _ => "",
        };

        if !status_label.is_empty() {
            return Ok(ToolOutput::text(format!(
                "Task '{}' is already in terminal state '{}'. No change.",
                task_id, status_label
            )));
        }

        match runner.stop_task(&task_id).await {
            Ok(output_dir) => Ok(ToolOutput::text(format!(
                "Stopped task '{}'. Outputs preserved at {}.",
                task_id,
                output_dir.display()
            ))),
            Err(e) => Ok(ToolOutput::Error { recoverable: true, message: e.to_string() }),
        }
    }
}
