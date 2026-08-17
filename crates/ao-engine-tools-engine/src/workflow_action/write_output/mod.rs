mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput, WorkflowRunnerHandle};
use ao_protocol::error::AoError;
use ao_protocol::workflow::PhaseStatus;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct WorkflowActionWriteOutput;

#[async_trait]
impl IoTool for WorkflowActionWriteOutput {
    fn name(&self) -> &str {
        "WorkflowActionWriteOutput"
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
                "filename": {
                    "type": "string",
                    "description": "Output filename (e.g. 'analysis.json')."
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file."
                }
            },
            "required": ["task_id", "filename", "content"],
            "additionalProperties": false
        })
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    fn mutates_filesystem(&self) -> bool {
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
        let filename = match input.get("filename").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("filename is required", true)),
        };
        let content = match input.get("content").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("content is required", true)),
        };

        if filename == "prd.json" {
            if let Some(err) = validate_prd_passes(&content, &task_id, runner.as_ref()).await {
                return Ok(err);
            }
        }

        match runner.write_phase_output(&task_id, &filename, &content).await {
            Ok(()) => {
                let base_msg = format!("Output written to '{}'.", filename);
                let progress = runner
                    .phase_write_progress_summary(&task_id, &filename)
                    .await;
                let message = match progress {
                    Some(summary) => format!("{} {}", base_msg, summary),
                    None => base_msg,
                };
                Ok(ToolOutput::text(message))
            }
            Err(e) => Ok(ToolOutput::Error { recoverable: true, message: e.to_string() }),
        }
    }
}

/// Validates that all userStories in a ralph-style prd.json have passes:false at PRD-creation time.
/// Returns Some(error) if validation fails, None if the write should proceed.
async fn validate_prd_passes(
    content: &str,
    task_id: &str,
    runner: &dyn WorkflowRunnerHandle,
) -> Option<ToolOutput> {
    let prd: Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return None,
    };

    let user_stories = match prd.get("userStories").and_then(Value::as_array) {
        Some(s) => s,
        None => return None,
    };

    // If the prd phase is already Completed, we're in implementation/review — allow passes:true.
    let prd_phase_completed = match runner.get_task_state(task_id).await {
        Ok(state) => state
            .phases
            .get("prd")
            .map(|p| p.status == PhaseStatus::Completed)
            .unwrap_or(false),
        Err(_) => false,
    };

    if prd_phase_completed {
        return None;
    }

    let violating: Vec<String> = user_stories
        .iter()
        .filter_map(|story| {
            let id = story.get("id").and_then(Value::as_str)?;
            if story.get("passes") != Some(&Value::Bool(false)) {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();

    if violating.is_empty() {
        return None;
    }

    let ids = violating.join(", ");
    Some(ToolOutput::Error {
        recoverable: true,
        message: format!(
            "PRD validation failed: stories [{}] have passes != false. \
All stories must be written with passes:false. \
The passes flag flips to true only after the implementation phase verifies acceptance criteria.",
            ids
        ),
    })
}
