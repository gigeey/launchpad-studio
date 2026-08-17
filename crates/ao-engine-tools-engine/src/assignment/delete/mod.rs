mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct AssignmentDelete;

#[async_trait]
impl IoTool for AssignmentDelete {
    fn name(&self) -> &str {
        "AssignmentDelete"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "assignment_id": {
                    "type": "string",
                    "description": "ID of the assignment to delete."
                }
            },
            "required": ["assignment_id"],
            "additionalProperties": false
        })
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        if let Some(err) = super::reject_if_subagent(ctx) {
            return Ok(err);
        }

        let store = match &ctx.assignment_store {
            Some(s) => s.clone(),
            None => {
                return Ok(ToolOutput::Error {
                    recoverable: false,
                    message: "Assignment store not available in this context.".into(),
                });
            }
        };

        let assignment_id = match input.get("assignment_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Ok(ToolOutput::error("assignment_id is required", true)),
        };

        if store.get(&assignment_id).await.is_none() {
            return Ok(ToolOutput::Error {
                recoverable: true,
                message: format!("[Assignment error: \"{}\" not found]", assignment_id),
            });
        }

        store.remove(&assignment_id).await?;

        Ok(ToolOutput::text(format!(
            "[Assignment \"{}\" deleted]",
            assignment_id
        )))
    }
}
