mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct WorkflowActionReadState;

#[async_trait]
impl IoTool for WorkflowActionReadState {
    fn name(&self) -> &str {
        "WorkflowActionReadState"
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
                    "description": "ID of the workflow task to inspect."
                }
            },
            "required": ["task_id"],
            "additionalProperties": false
        })
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
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

        match runner.get_task_state(&task_id).await {
            Ok(snapshot) => {
                let json_str = serde_json::to_string_pretty(&snapshot)
                    .unwrap_or_else(|_| "{}".to_string());
                Ok(ToolOutput::text(format!(
                    "## Workflow Task State\n```json\n{}\n```",
                    json_str
                )))
            }
            Err(e) => Ok(ToolOutput::Error { recoverable: true, message: e.to_string() }),
        }
    }
}
