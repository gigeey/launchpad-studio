mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput, UserEvent};
use ao_protocol::{
    error::AoError,
    project::ProjectStatus,
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;

/// Partial update of the project the running agent is managing.
///
/// Accepts any subset of mutable fields. The `activate` flag is the dedicated
/// pathway for the `Interviewing → Active` transition: it lets the agent record
/// the gathered spec and flip the status in one atomic call at the close of the
/// interview. Illegal status transitions are rejected with a recoverable error.
pub struct ProjectUpdate;

#[async_trait]
impl EngineTool for ProjectUpdate {
    fn name(&self) -> &str {
        "ProjectUpdate"
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

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let project_id = match &ctx.project_id {
            Some(id) => id.clone(),
            None => {
                return Ok(ToolOutput::error(
                    "ProjectUpdate is only available inside a project-scoped session. \
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

        let mut project = match store.get(&project_id).await {
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

        // Apply optional field patches.
        if let Some(name) = input.get("name").and_then(|v| v.as_str()) {
            if !name.trim().is_empty() {
                project.name = name.to_string();
            }
        }

        // emoji: null clears, string sets
        if input.get("emoji").is_some() {
            let raw = &input["emoji"];
            if raw.is_null() {
                project.emoji = None;
            } else if let Some(s) = raw.as_str() {
                project.emoji = Some(s.to_string());
            }
        }

        // spec: null clears, string replaces
        if input.get("spec").is_some() {
            let raw = &input["spec"];
            if raw.is_null() {
                project.spec = None;
            } else if let Some(s) = raw.as_str() {
                project.spec = Some(s.to_string());
            }
        }

        // working_dir: null clears, string sets
        if input.get("working_dir").is_some() {
            let raw = &input["working_dir"];
            if raw.is_null() {
                project.working_dir = None;
            } else if let Some(s) = raw.as_str() {
                project.working_dir = Some(s.to_string());
            }
        }

        // activate: legal only from Interviewing
        let wants_activate = input.get("activate").and_then(|v| v.as_bool()).unwrap_or(false);
        if wants_activate {
            match &project.status {
                ProjectStatus::Interviewing => {
                    project.status = ProjectStatus::Active;
                }
                ProjectStatus::Active => {
                    return Ok(ToolOutput::error(
                        "The project is already Active — no transition needed.",
                        true,
                    ));
                }
                ProjectStatus::Completed => {
                    return Ok(ToolOutput::error(
                        "Cannot activate a Completed project. The goal has already been \
                         reached. Create a new project if you want to start fresh.",
                        true,
                    ));
                }
                ProjectStatus::Archived => {
                    return Ok(ToolOutput::error(
                        "Cannot activate an Archived project. Ask the user if they want to \
                         create a new project instead.",
                        true,
                    ));
                }
                ProjectStatus::NeedsReview => {
                    return Ok(ToolOutput::error(
                        "Cannot activate a project in NeedsReview status — human review is \
                         required first.",
                        true,
                    ));
                }
            }
        }

        project.updated_at = Utc::now();

        if let Err(e) = store.save(&project).await {
            return Ok(ToolOutput::error(
                &format!("failed to save project: {e}"),
                false,
            ));
        }

        let status_str = serde_json::to_value(&project.status)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown".to_string());

        // Notify the UI so the project panel reflects the change immediately.
        let _ = ctx
            .event_sink
            .emit(UserEvent::ProjectStateChanged {
                project_id: project.id.clone(),
                status: status_str.clone(),
                name: project.name.clone(),
            })
            .await;

        let activated = wants_activate && status_str == "active";
        Ok(ToolOutput::structured(serde_json::json!({
            "id": project.id,
            "name": project.name,
            "emoji": project.emoji,
            "status": status_str,
            "spec": project.spec,
            "working_dir": project.working_dir,
            "activated": activated,
        })))
    }
}
