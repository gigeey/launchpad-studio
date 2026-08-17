mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

/// Read-only snapshot of the project the running agent is managing.
///
/// Returns all fields from the persisted `Project` record so the agent can
/// orient itself at session start or check current state before deciding on
/// next actions. No fields are mutated; this tool is always safe to call.
pub struct ProjectGet;

#[async_trait]
impl EngineTool for ProjectGet {
    fn name(&self) -> &str {
        "ProjectGet"
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
        let project_id = match &ctx.project_id {
            Some(id) => id.clone(),
            None => {
                return Ok(ToolOutput::error(
                    "ProjectGet is only available inside a project-scoped session. \
                     This run has no project scope.",
                    false,
                ));
            }
        };

        let store = match &ctx.project_store {
            Some(s) => s,
            None => {
                return Ok(ToolOutput::error(
                    "Project store not available in this context.",
                    false,
                ));
            }
        };

        let project = match store.get(&project_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Ok(ToolOutput::error(
                    &format!("project '{}' not found", project_id),
                    false,
                ));
            }
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to load project: {e}"),
                    false,
                ));
            }
        };

        let status_str = serde_json::to_value(&project.status)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown".to_string());

        Ok(ToolOutput::structured(serde_json::json!({
            "id": project.id,
            "name": project.name,
            "emoji": project.emoji,
            "goal": project.goal,
            "spec": project.spec,
            "status": status_str,
            "working_dir": project.working_dir,
            "attachments": project.attachments,
            "summary": project.summary,
            "created_at": project.created_at,
            "updated_at": project.updated_at,
        })))
    }
}
