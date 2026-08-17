mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::{agent::WorkflowBinding, error::AoError};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct WorkflowActionCreate;

#[async_trait]
impl IoTool for WorkflowActionCreate {
    fn name(&self) -> &str {
        "WorkflowActionCreate"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workflow_id": {
                    "type": "string",
                    "description": "ID of the workflow to create a task for."
                },
                "project_name": {
                    "type": "string",
                    "description": "Human-readable name for this task instance."
                },
                "working_dir": {
                    "type": "string",
                    "description": "Working directory for the task. Defaults to the agent's cwd."
                },
                "context": {
                    "type": "string",
                    "description": "Optional initial context to attach to the task."
                }
            },
            "required": ["workflow_id", "project_name"],
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

        let workflow_id = match input.get("workflow_id").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("workflow_id is required", true)),
        };
        let project_name = match input.get("project_name").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("project_name is required", true)),
        };

        // Binding gate: agent must be bound to this workflow.
        let bound = match &ctx.agent_workflows {
            None => false,
            Some(WorkflowBinding::All) => true,
            Some(WorkflowBinding::List(ids)) => ids.iter().any(|id| id == &workflow_id),
            Some(WorkflowBinding::None) => false,
        };
        if !bound {
            return Ok(ToolOutput::Error {
                recoverable: true,
                message: format!("Agent is not bound to workflow '{}'.", workflow_id),
            });
        }

        let working_dir = input
            .get("working_dir")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .or_else(|| {
                ctx.cwd
                    .read()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });
        let context = input
            .get("context")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        let task_id = match runner
            .create_task(&workflow_id, &project_name, working_dir, context)
            .await
        {
            Ok(id) => id,
            Err(e) => return Ok(ToolOutput::Error { recoverable: true, message: e.to_string() }),
        };

        let summary = runner
            .build_create_summary(&task_id, &workflow_id)
            .await
            .unwrap_or_else(|e| {
                format!(
                    "Task {} created (pending). Failed to load summary: {}",
                    task_id, e
                )
            });

        Ok(ToolOutput::text(summary))
    }
}
