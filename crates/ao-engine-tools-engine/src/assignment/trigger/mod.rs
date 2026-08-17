mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::assignment::AssignmentRunStatus;
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct AssignmentTrigger;

#[async_trait]
impl IoTool for AssignmentTrigger {
    fn name(&self) -> &str {
        "AssignmentTrigger"
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
                    "description": "ID of the assignment to fire immediately."
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

        let fire = match &ctx.assignment_fire {
            Some(f) => f.clone(),
            None => {
                return Ok(ToolOutput::Error {
                    recoverable: false,
                    message: "Assignment firing is not available in this context.".into(),
                });
            }
        };

        let assignment_id = match input.get("assignment_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Ok(ToolOutput::error("assignment_id is required", true)),
        };

        let assignment = match store.get(&assignment_id).await {
            Some(a) => a,
            None => {
                return Ok(ToolOutput::Error {
                    recoverable: true,
                    message: format!("[Assignment error: \"{}\" not found]", assignment_id),
                });
            }
        };

        if !assignment.enabled {
            return Ok(ToolOutput::error(
                format!("[Assignment error: \"{}\" is disabled]", assignment_id),
                true,
            ));
        }

        let timezone = super::resolve_timezone(ctx).await;
        let run = fire.fire_now(&assignment, timezone.as_deref()).await?;

        Ok(ToolOutput::text(format!(
            "[Assignment \"{}\" fired: run_id=\"{}\" status={}]",
            assignment_id,
            run.id,
            run_status_str(run.status)
        )))
    }
}

fn run_status_str(status: AssignmentRunStatus) -> &'static str {
    match status {
        AssignmentRunStatus::Queued => "queued",
        AssignmentRunStatus::Running => "running",
        AssignmentRunStatus::Succeeded => "succeeded",
        AssignmentRunStatus::Failed => "failed",
    }
}
