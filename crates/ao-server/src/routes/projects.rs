use std::path::Component;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ao_engine::prompt_assembler::assemble_copilot_prompt;
use ao_engine::prompt_sections::COPILOT_PROFILE_ID;
use ao_engine::AppState;
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
};
use ao_protocol::error::AoError;
use ao_protocol::project::{Project, ProjectId, ProjectStatus};
use ao_protocol::tasklist::{
    TaskComment, TaskCommentAuthorKind, TaskGroupMode, Tasklist, TasklistOwner, TasklistStatus,
};

use crate::error::AppError;

#[derive(Debug, Deserialize)]
pub struct CreateProjectRequest {
    pub goal: String,
    pub agent_id: String,
    pub name: Option<String>,
    pub emoji: Option<String>,
    pub working_dir: Option<String>,
    #[serde(default)]
    pub attachments: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchProjectRequest {
    pub name: Option<String>,
    pub emoji: Option<Option<String>>,
    pub spec: Option<Option<String>>,
    pub status: Option<ProjectStatus>,
    pub working_dir: Option<Option<String>>,
}

#[derive(Debug, Serialize)]
pub struct ProjectSnapshot {
    pub id: ProjectId,
    pub name: String,
    pub emoji: Option<String>,
    pub status: ProjectStatus,
    pub agent_id: String,
    pub created_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
}

fn derive_name(goal: &str) -> String {
    let words: Vec<&str> = goal.split_whitespace().take(5).collect();
    if words.is_empty() {
        return "Untitled Project".to_string();
    }
    let name = words.join(" ");
    if name.chars().count() <= 40 {
        return name;
    }
    let truncated: String = name.chars().take(40).collect();
    format!("{}…", truncated)
}

/// POST /projects — create a new project.
pub async fn create_project(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateProjectRequest>,
) -> Result<(StatusCode, Json<Project>), AppError> {
    let agent = state.persistence.agents.get(&req.agent_id).await?;
    if agent.is_none() {
        return Err(AoError::ValidationError(format!(
            "Agent '{}' does not exist",
            req.agent_id
        ))
        .into());
    }

    let now = Utc::now();
    let name = req.name.unwrap_or_else(|| derive_name(&req.goal));
    let project = Project {
        id: Uuid::new_v4().to_string(),
        name,
        emoji: req.emoji,
        goal: req.goal,
        spec: None,
        agent_id: req.agent_id,
        working_dir: req.working_dir,
        attachments: req.attachments,
        status: ProjectStatus::Interviewing,
        summary: None,
        verifications: Vec::new(),
        created_at: now,
        updated_at: now,
    };

    state.persistence.projects.create(&project).await?;

    // Best-effort: record the creation event as a system transcript entry.
    // Failure must not fail project creation.
    if let Err(e) =
        super::project_messages::record_project_created(&state, &project.id, &project.name, &project.goal)
            .await
    {
        tracing::warn!(
            project_id = %project.id,
            "Failed to record project-created transcript entry: {}",
            e.0
        );
    }

    // Best-effort: kick off the interview agent with the project goal.
    // Failure must not fail project creation.
    if let Err(e) = super::project_messages::kickoff_project_message(
        &state,
        &project.id,
        &project.goal,
    )
    .await
    {
        tracing::warn!(
            project_id = %project.id,
            "Failed to kick off interview agent after project creation: {}",
            e.0
        );
    }

    Ok((StatusCode::CREATED, Json(project)))
}

/// GET /projects — list all projects as snapshots.
pub async fn list_projects(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<ProjectSnapshot>>, AppError> {
    let projects = state.persistence.projects.list().await?;
    let snapshots = projects
        .into_iter()
        .map(|p| ProjectSnapshot {
            id: p.id,
            name: p.name,
            emoji: p.emoji,
            status: p.status,
            agent_id: p.agent_id,
            created_at: p.created_at,
            updated_at: p.updated_at,
        })
        .collect();
    Ok(Json(snapshots))
}

/// GET /projects/{id} — get a full project.
pub async fn get_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Project>, AppError> {
    let project = state
        .persistence
        .projects
        .get(&id)
        .await?
        .ok_or_else(|| AoError::ProjectNotFound(id))?;
    Ok(Json(project))
}

/// PATCH /projects/{id} — partial update.
pub async fn patch_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<PatchProjectRequest>,
) -> Result<Json<Project>, AppError> {
    let mut project = state
        .persistence
        .projects
        .get(&id)
        .await?
        .ok_or_else(|| AoError::ProjectNotFound(id))?;

    if let Some(name) = req.name {
        project.name = name;
    }
    if let Some(emoji) = req.emoji {
        project.emoji = emoji;
    }
    if let Some(spec) = req.spec {
        project.spec = spec;
    }
    if let Some(status) = req.status {
        project.status = status;
    }
    if let Some(working_dir) = req.working_dir {
        project.working_dir = working_dir;
    }
    project.updated_at = Utc::now();

    state.persistence.projects.save(&project).await?;

    Ok(Json(project))
}

/// DELETE /projects/{id} — delete a project.
pub async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let found = state.persistence.projects.delete(&id).await?;
    if !found {
        return Err(AoError::ProjectNotFound(id).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct ListProjectTasklistsResponse {
    pub active: Option<Tasklist>,
    pub recent: Vec<Tasklist>,
}

/// GET /projects/{id}/tasklists — list active + recent tasklists tagged to this project.
pub async fn list_project_tasklists(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
) -> Result<Json<ListProjectTasklistsResponse>, AppError> {
    let project = state
        .persistence
        .projects
        .get(&project_id)
        .await?
        .ok_or_else(|| AoError::ProjectNotFound(project_id.clone()))?;

    let owner = TasklistOwner::Agent {
        agent_id: project.agent_id.clone(),
    };
    let all = state.tasklist_service.list(&owner).await?;

    let mut active = None;
    let mut recent = Vec::new();
    for tl in all {
        if tl.project_id.as_deref() != Some(&project_id) {
            continue;
        }
        match tl.status {
            TasklistStatus::Active | TasklistStatus::Paused if active.is_none() => {
                active = Some(tl)
            }
            _ => recent.push(tl),
        }
    }

    Ok(Json(ListProjectTasklistsResponse { active, recent }))
}

/// Resolve the project and build its TasklistOwner; return 404 if the project
/// does not exist or the tasklist is not stamped with this project_id.
async fn project_tasklist_owner_and_id(
    state: &Arc<AppState>,
    project_id: &str,
    tasklist_id: &str,
) -> Result<(TasklistOwner, String, Tasklist), AppError> {
    let project = state
        .persistence
        .projects
        .get(project_id)
        .await?
        .ok_or_else(|| AoError::ProjectNotFound(project_id.to_string()))?;

    let owner = TasklistOwner::Agent {
        agent_id: project.agent_id.clone(),
    };
    let tasklist = state
        .tasklist_service
        .get(&owner, tasklist_id)
        .await?
        .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.to_string()))?;

    if tasklist.project_id.as_deref() != Some(project_id) {
        return Err(AoError::TasklistNotFound(tasklist_id.to_string()).into());
    }

    Ok((owner, project.agent_id, tasklist))
}

/// GET /projects/{id}/tasklists/{tasklist_id}
pub async fn get_project_tasklist(
    State(state): State<Arc<AppState>>,
    Path((project_id, tasklist_id)): Path<(String, String)>,
) -> Result<Json<Tasklist>, AppError> {
    let (_, _, tasklist) =
        project_tasklist_owner_and_id(&state, &project_id, &tasklist_id).await?;
    Ok(Json(tasklist))
}

/// Request body for POST /projects/{id}/tasklists/{tasklist_id}/tasks.
#[derive(Debug, Deserialize)]
pub struct ProjectAppendTaskRequest {
    pub prompt: String,
    #[serde(default)]
    pub owner_agent_id: Option<String>,
    #[serde(default)]
    pub expected_outputs: Vec<String>,
    pub mode: TaskGroupMode,
    #[serde(default)]
    pub attachment_ids: Vec<String>,
}

/// POST /projects/{id}/tasklists/{tasklist_id}/tasks
pub async fn append_project_task(
    State(state): State<Arc<AppState>>,
    Path((project_id, tasklist_id)): Path<(String, String)>,
    Json(req): Json<ProjectAppendTaskRequest>,
) -> Result<Json<Tasklist>, AppError> {
    let (owner, _, _) =
        project_tasklist_owner_and_id(&state, &project_id, &tasklist_id).await?;
    let updated = state
        .tasklist_service
        .add_tasks(
            &owner,
            &tasklist_id,
            req.prompt,
            req.owner_agent_id,
            req.expected_outputs,
            req.mode,
            req.attachment_ids,
        )
        .await?;
    Ok(Json(updated))
}

/// Request body for POST /projects/{id}/tasklists/{tasklist_id}/status.
#[derive(Debug, Deserialize)]
pub struct ProjectSetStatusRequest {
    pub status: String,
}

/// POST /projects/{id}/tasklists/{tasklist_id}/status
pub async fn set_project_tasklist_status(
    State(state): State<Arc<AppState>>,
    Path((project_id, tasklist_id)): Path<(String, String)>,
    Json(req): Json<ProjectSetStatusRequest>,
) -> Result<Json<Tasklist>, AppError> {
    tracing::info!(project_id = %project_id, tasklist_id = %tasklist_id, "POST /projects/:id/tasklists/:id/status");
    let (owner, _, _) =
        project_tasklist_owner_and_id(&state, &project_id, &tasklist_id).await?;
    let updated = state
        .tasklist_service
        .set_status(&owner, &tasklist_id, &req.status)
        .await?;
    Ok(Json(updated))
}

/// POST /projects/{id}/tasklists/{tasklist_id}/continue
pub async fn continue_project_tasklist(
    State(state): State<Arc<AppState>>,
    Path((project_id, tasklist_id)): Path<(String, String)>,
) -> Result<Json<Tasklist>, AppError> {
    tracing::info!(project_id = %project_id, tasklist_id = %tasklist_id, "POST /projects/:id/tasklists/:id/continue");
    let (owner, _, _) =
        project_tasklist_owner_and_id(&state, &project_id, &tasklist_id).await?;
    let updated = state
        .tasklist_service
        .continue_failed(&owner, &tasklist_id)
        .await?;
    Ok(Json(updated))
}

/// POST /projects/{id}/tasklists/{tasklist_id}/discard
pub async fn discard_project_tasklist(
    State(state): State<Arc<AppState>>,
    Path((project_id, tasklist_id)): Path<(String, String)>,
) -> Result<Json<Tasklist>, AppError> {
    tracing::info!(project_id = %project_id, tasklist_id = %tasklist_id, "POST /projects/:id/tasklists/:id/discard");
    let (owner, _, _) =
        project_tasklist_owner_and_id(&state, &project_id, &tasklist_id).await?;
    let updated = state
        .tasklist_service
        .stop(&owner, &tasklist_id)
        .await?;
    Ok(Json(updated))
}

/// POST /projects/{id}/tasklists/{tasklist_id}/replay
pub async fn replay_project_tasklist(
    State(state): State<Arc<AppState>>,
    Path((project_id, tasklist_id)): Path<(String, String)>,
) -> Result<Json<Tasklist>, AppError> {
    tracing::info!(project_id = %project_id, tasklist_id = %tasklist_id, "POST /projects/:id/tasklists/:id/replay");
    let (owner, _, _) =
        project_tasklist_owner_and_id(&state, &project_id, &tasklist_id).await?;
    let new_tasklist = state
        .tasklist_service
        .replay(&owner, &tasklist_id)
        .await?;
    Ok(Json(new_tasklist))
}

/// POST /projects/{id}/tasklists/{tasklist_id}/tasks/{task_id}/skip
pub async fn skip_project_task(
    State(state): State<Arc<AppState>>,
    Path((project_id, tasklist_id, task_id)): Path<(String, String, String)>,
) -> Result<Json<Tasklist>, AppError> {
    tracing::info!(project_id = %project_id, tasklist_id = %tasklist_id, task_id = %task_id, "POST /projects/:id/tasklists/:id/tasks/:task_id/skip");
    let (owner, _, _) =
        project_tasklist_owner_and_id(&state, &project_id, &tasklist_id).await?;
    let updated = state
        .tasklist_service
        .skip_task(&owner, &tasklist_id, &task_id)
        .await?;
    Ok(Json(updated))
}

/// POST /projects/{id}/tasklists/{tasklist_id}/tasks/{task_id}/stop
///
/// Cancel a single in-flight task: flips it to Stopped and kills its
/// in-flight run. The task is non-terminal and can be resumed later.
pub async fn stop_project_task(
    State(state): State<Arc<AppState>>,
    Path((project_id, tasklist_id, task_id)): Path<(String, String, String)>,
) -> Result<Json<Tasklist>, AppError> {
    tracing::info!(project_id = %project_id, tasklist_id = %tasklist_id, task_id = %task_id, "POST /projects/:id/tasklists/:id/tasks/:task_id/stop");
    let (owner, _, _) =
        project_tasklist_owner_and_id(&state, &project_id, &tasklist_id).await?;
    let updated = state
        .tasklist_service
        .stop_task(&owner, &tasklist_id, &task_id)
        .await?;
    Ok(Json(updated))
}

/// POST /projects/{id}/tasklists/{tasklist_id}/tasks/{task_id}/resume
///
/// Re-queue a previously stopped task: flips it back to Pending and lets the
/// feeder re-dispatch it.
pub async fn resume_project_task(
    State(state): State<Arc<AppState>>,
    Path((project_id, tasklist_id, task_id)): Path<(String, String, String)>,
) -> Result<Json<Tasklist>, AppError> {
    tracing::info!(project_id = %project_id, tasklist_id = %tasklist_id, task_id = %task_id, "POST /projects/:id/tasklists/:id/tasks/:task_id/resume");
    let (owner, _, _) =
        project_tasklist_owner_and_id(&state, &project_id, &tasklist_id).await?;
    let updated = state
        .tasklist_service
        .resume_task(&owner, &tasklist_id, &task_id)
        .await?;
    Ok(Json(updated))
}

/// Request body for POST /projects/{id}/tasklists/{tasklist_id}/tasks/{task_id}/comments.
#[derive(Debug, Deserialize)]
pub struct ProjectAddCommentRequest {
    pub body: String,
    #[serde(default)]
    pub author_kind: Option<TaskCommentAuthorKind>,
    #[serde(default)]
    pub author_id: Option<String>,
}

/// POST /projects/{id}/tasklists/{tasklist_id}/tasks/{task_id}/comments
pub async fn add_project_task_comment(
    State(state): State<Arc<AppState>>,
    Path((project_id, tasklist_id, task_id)): Path<(String, String, String)>,
    Json(req): Json<ProjectAddCommentRequest>,
) -> Result<Json<TaskComment>, AppError> {
    if req.body.trim().is_empty() {
        return Err(AoError::ValidationError("Comment body is required".into()).into());
    }

    let author_kind = req.author_kind.unwrap_or(TaskCommentAuthorKind::User);
    let author_id = req.author_id.unwrap_or_else(|| match author_kind {
        TaskCommentAuthorKind::User => "user".to_string(),
        TaskCommentAuthorKind::Agent => String::new(),
    });

    let comment = TaskComment {
        id: Uuid::new_v4().to_string(),
        author_id,
        author_kind,
        body: req.body,
        created_at: Utc::now(),
    };

    let (owner, _, _) =
        project_tasklist_owner_and_id(&state, &project_id, &tasklist_id).await?;
    let returned = state
        .tasklist_service
        .add_comment(&owner, &tasklist_id, &task_id, comment)
        .await?;
    Ok(Json(returned))
}

/// GET /projects/{id}/tasklists/{tasklist_id}/outputs/{*filename}
pub async fn get_project_tasklist_output(
    State(state): State<Arc<AppState>>,
    Path((project_id, tasklist_id, filename)): Path<(String, String, String)>,
) -> Result<String, AppError> {
    let (_, agent_id, _) =
        project_tasklist_owner_and_id(&state, &project_id, &tasklist_id).await?;

    let rel = std::path::Path::new(&filename);
    for component in rel.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(AoError::ValidationError(format!(
                    "Invalid output path: {}",
                    filename
                ))
                .into());
            }
        }
    }

    let workspace = state
        .persistence
        .data_root
        .agent_tasklist_workspace_dir(&agent_id, &tasklist_id);
    let target = workspace.join(rel);

    let contents = tokio::fs::read_to_string(&target).await.map_err(|e| {
        AoError::Internal(format!(
            "Failed to read tasklist output '{}': {}",
            filename, e
        ))
    })?;
    Ok(contents)
}

/// Response for GET /projects/{id}/tasklists/{tasklist_id}/copilot.
#[derive(Debug, Serialize)]
pub struct ProjectCopilotResponse {
    pub agent_id: String,
}

/// GET /projects/{id}/tasklists/{tasklist_id}/copilot
pub async fn get_project_copilot(
    State(state): State<Arc<AppState>>,
    Path((project_id, tasklist_id)): Path<(String, String)>,
) -> Result<Json<ProjectCopilotResponse>, AppError> {
    let (owner, agent_id_of_project, _) =
        project_tasklist_owner_and_id(&state, &project_id, &tasklist_id).await?;

    // Stamp last_opened_at on the tasklist.
    state
        .persistence
        .tasklists
        .mutate_by_owner(&owner, &tasklist_id, |tl| {
            tl.last_opened_at = Some(Utc::now());
            Ok(())
        })
        .await?;

    // Return existing copilot binding if present.
    if let Some(existing) = state
        .persistence
        .tasklists
        .get_by_owner(&owner, &tasklist_id)
        .await?
        .and_then(|tl| tl.copilot_agent_id)
    {
        return Ok(Json(ProjectCopilotResponse { agent_id: existing }));
    }

    let new_agent_id = Uuid::new_v4().to_string();
    let profile = build_project_copilot_profile(&new_agent_id, &agent_id_of_project)?;

    state.persistence.agents.create(&profile).await?;

    let agent_home = state
        .persistence
        .data_root
        .agent_home_dir(&new_agent_id);
    ao_protocol::agent_home::ensure_agent_home(&agent_home)
        .await
        .map_err(|e| AoError::Internal(format!("Failed to create agent home: {e}")))?;

    // Bind atomically — first writer wins.
    state
        .persistence
        .tasklists
        .mutate_by_owner(&owner, &tasklist_id, {
            let nid = new_agent_id.clone();
            move |tl| {
                if tl.copilot_agent_id.is_none() {
                    tl.copilot_agent_id = Some(nid);
                }
                Ok(())
            }
        })
        .await?;

    let canonical_id = state
        .persistence
        .tasklists
        .get_by_owner(&owner, &tasklist_id)
        .await?
        .and_then(|tl| tl.copilot_agent_id)
        .ok_or_else(|| {
            AoError::Internal("co-pilot binding vanished between write and re-read".to_string())
        })?;

    if canonical_id != new_agent_id {
        if let Err(err) = state.persistence.agents.delete(&new_agent_id).await {
            tracing::warn!(
                orphan_agent_id = %new_agent_id,
                "Failed to clean up orphan co-pilot agent profile after race loss: {}",
                err
            );
        }
        if let Err(err) = tokio::fs::remove_dir_all(&agent_home).await {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    orphan_agent_id = %new_agent_id,
                    "Failed to clean up orphan co-pilot home dir after race loss: {}",
                    err
                );
            }
        }
    }

    Ok(Json(ProjectCopilotResponse {
        agent_id: canonical_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_name_short_goal_unchanged() {
        assert_eq!(derive_name("Build a rocket ship"), "Build a rocket ship");
    }

    #[test]
    fn derive_name_empty_goal_returns_untitled() {
        assert_eq!(derive_name(""), "Untitled Project");
        assert_eq!(derive_name("   "), "Untitled Project");
    }

    #[test]
    fn derive_name_takes_first_five_words() {
        assert_eq!(
            derive_name("one two three four five six seven"),
            "one two three four five"
        );
    }

    #[test]
    fn derive_name_caps_long_word_at_40_chars_with_ellipsis() {
        let long_path = "/Users/alice/projects/very-long-repo-name/src/components/SomeDeepFilename.tsx";
        let goal = format!("Refactor {}", long_path);
        let name = derive_name(&goal);
        let char_count = name.chars().count();
        assert!(
            char_count <= 41,
            "name should be ≤41 chars (incl. ellipsis), got {char_count}: {name}"
        );
        assert!(name.ends_with('…'), "truncated name must end with …, got: {name}");
    }

    #[test]
    fn derive_name_exactly_40_chars_no_truncation() {
        // A name that is exactly 40 chars should not be truncated.
        let goal = "a".repeat(40);
        let name = derive_name(&goal);
        assert_eq!(name.chars().count(), 40);
        assert!(!name.ends_with('…'));
    }

    #[test]
    fn derive_name_41_chars_gets_truncated() {
        let goal = "a".repeat(41);
        let name = derive_name(&goal);
        // 40 chars of 'a' + '…' = 41 char count
        assert_eq!(name.chars().count(), 41);
        assert!(name.ends_with('…'));
    }
}

fn build_project_copilot_profile(
    agent_id: &str,
    _project_agent_id: &str,
) -> Result<AgentProfile, AppError> {
    let system_prompt = assemble_copilot_prompt()
        .map_err(|e| AoError::Internal(format!("Failed to assemble copilot prompt: {e}")))?;

    Ok(AgentProfile {
        id: agent_id.to_string(),
        name: "Tasklist Co-pilot".to_string(),
        description: "Per-tasklist co-pilot agent.".to_string(),
        emoji: None,
        provider: ProviderConfig::Cli(CliProviderConfig {
            command: "claude".to_string(),
            args: vec![
                "--print".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "--include-partial-messages".to_string(),
            ],
            normalizer: Some("Claude".to_string()),
            output_format: OutputFormat::StreamJson,
            input_mode: InputMode::Arg,
            model_arg: Some("--model".to_string()),
            model_aliases: std::collections::HashMap::new(),
            system_prompt_arg: Some("--append-system-prompt".to_string()),
            session_arg: None,
            resume_args: vec![],
            session_id_fields: vec![],
            clear_env: false,
            no_output_timeout_ms: 30000,
            file_capabilities: None,
        }),
        model: None,
        skills: vec![],
        system_prompt: Some(system_prompt),
        tools: None,
        env: std::collections::HashMap::new(),
        max_instances: 1,
        timeout_seconds: 30000,
        working_dir: None,
        home_dir: None,
        serialize: true,
        workflows: None,
        template: Some(COPILOT_PROFILE_ID.to_string()),
        runner_mode: Default::default(),
        enabled_plugins: std::collections::HashMap::new(),
        owning_team_id: None,
        native_provider: None,
        thinking: None,
        max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
        delegates_to: vec![],
        persona: None,
        special_instructions: None,
        legacy_system_prompt: None,
        max_delegation_depth: None,
        channels: vec![],
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
        max_turns: None,
    })
}
