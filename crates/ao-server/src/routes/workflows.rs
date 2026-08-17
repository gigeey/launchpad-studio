use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use chrono::Utc;
use uuid::Uuid;

use ao_engine::workflow_queue_manager::{build_phase_agent, phase_agent_id};
use ao_engine::AppState;
use ao_protocol::error::AoError;
use ao_protocol::message::QueuedMessage;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use ao_protocol::workflow::{
    PhaseStatus, TaskSnapshot, TaskStatus, WorkflowDefinition, WorkflowSummary,
};
use std::collections::HashMap;

use crate::error::AppError;

/// GET /workflows — list all workflow summaries.
pub async fn list_workflows(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<WorkflowSummary>>, AppError> {
    let registry = state.workflow_registry.read().await;
    let mut summaries: Vec<WorkflowSummary> =
        registry.list_summaries().into_iter().cloned().collect();
    drop(registry);

    // Single scan of tasks → map of workflow_id → latest created timestamp.
    // O(tasks), not O(workflows × tasks).
    let mut last_run_by_workflow: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
    if let Ok(task_ids) = state.workflow_runner.list_task_ids().await {
        for task_id in task_ids {
            let Ok(snapshot) = state.workflow_runner.get_task_state(&task_id).await else {
                continue;
            };
            let entry = last_run_by_workflow
                .entry(snapshot.workflow.clone())
                .or_insert(snapshot.created);
            if snapshot.created > *entry {
                *entry = snapshot.created;
            }
        }
    }

    for summary in &mut summaries {
        summary.last_run = last_run_by_workflow.get(&summary.id).copied();
    }

    Ok(Json(summaries))
}

/// GET /workflows/{id} — get full workflow definition.
pub async fn get_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkflowDefinition>, AppError> {
    let registry = state.workflow_registry.read().await;
    let definition = registry
        .get_definition(&id)
        .cloned()
        .ok_or_else(|| AoError::WorkflowNotFound(id))?;
    Ok(Json(definition))
}

#[derive(Serialize)]
pub struct RefreshResponse {
    pub count: usize,
}

#[derive(Deserialize)]
pub struct ImportWorkflowRequest {
    pub source_path: String,
}

#[derive(Serialize)]
pub struct ImportWorkflowResponse {
    pub workflow_id: String,
    pub status: String,
}

/// POST /workflows/import — copy a workflow folder into the workflows directory.
pub async fn import_workflow(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ImportWorkflowRequest>,
) -> Result<Json<ImportWorkflowResponse>, AppError> {
    let source = std::path::Path::new(&req.source_path);
    if !source.is_dir() {
        return Err(AppError(AoError::ValidationError(
            "Source path is not a directory".to_string(),
        )));
    }
    // Check workflow.yaml exists
    if !source.join("workflow.yaml").exists() {
        return Err(AppError(AoError::ValidationError(
            "No workflow.yaml found in the source directory".to_string(),
        )));
    }

    let folder_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| AoError::ValidationError("Invalid folder name".to_string()))?
        .to_string();

    let workflows_dir = state.persistence.data_root.root().join("workflows");
    let dest = workflows_dir.join(&folder_name);

    if dest.exists() {
        return Err(AppError(AoError::ValidationError(format!(
            "Workflow '{}' already exists",
            folder_name
        ))));
    }

    // Copy directory recursively
    fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            if src_path.is_dir() {
                copy_dir_recursive(&src_path, &dst_path)?;
            } else {
                std::fs::copy(&src_path, &dst_path)?;
            }
        }
        Ok(())
    }

    copy_dir_recursive(source, &dest)
        .map_err(|e| AoError::Internal(format!("Failed to copy workflow: {}", e)))?;

    // Refresh registry to pick up new workflow
    let mut registry = state.workflow_registry.write().await;
    registry.refresh().await?;

    Ok(Json(ImportWorkflowResponse {
        workflow_id: folder_name,
        status: "imported".to_string(),
    }))
}

#[derive(Deserialize)]
pub struct CloneExampleRequest {
    pub id: String,
    pub files: std::collections::HashMap<String, String>,
}

/// POST /workflows/clone-example — create a workflow from inline file contents.
pub async fn clone_example(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CloneExampleRequest>,
) -> Result<Json<ImportWorkflowResponse>, AppError> {
    let workflows_dir = state.persistence.data_root.root().join("workflows");
    let dest = workflows_dir.join(&req.id);

    if dest.exists() {
        return Err(AppError(AoError::ValidationError(format!(
            "Workflow '{}' already exists",
            req.id
        ))));
    }

    // Write each file
    for (filename, content) in &req.files {
        let file_path = dest.join(filename);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AoError::Internal(format!("Failed to create directory: {}", e)))?;
        }
        std::fs::write(&file_path, content)
            .map_err(|e| AoError::Internal(format!("Failed to write {}: {}", filename, e)))?;

        // Set execute permission on .sh files
        #[cfg(unix)]
        if filename.ends_with(".sh") {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            let _ = std::fs::set_permissions(&file_path, perms);
        }
    }

    // Refresh registry
    let mut registry = state.workflow_registry.write().await;
    registry.refresh().await?;

    Ok(Json(ImportWorkflowResponse {
        workflow_id: req.id,
        status: "imported".to_string(),
    }))
}

/// POST /workflows/refresh — re-scan workflows directory.
pub async fn refresh_workflows(
    State(state): State<Arc<AppState>>,
) -> Result<Json<RefreshResponse>, AppError> {
    let mut registry = state.workflow_registry.write().await;
    registry.refresh().await?;
    let count = registry.list_summaries().len();
    Ok(Json(RefreshResponse { count }))
}

#[derive(Deserialize)]
pub struct CreateTaskRequest {
    pub project_name: String,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
}

#[derive(Serialize)]
pub struct CreateTaskResponse {
    pub task_id: String,
}

/// POST /workflows/{id}/tasks — create a new workflow task.
pub async fn create_task(
    State(state): State<Arc<AppState>>,
    Path(workflow_id): Path<String>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<Json<CreateTaskResponse>, AppError> {
    let task_id = state
        .workflow_runner
        .create_task(
            &workflow_id,
            &req.project_name,
            req.working_directory,
            req.context,
        )
        .await?;
    Ok(Json(CreateTaskResponse { task_id }))
}

#[derive(Serialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub workflow: String,
    pub project_name: String,
    pub created: chrono::DateTime<chrono::Utc>,
    pub status: TaskStatus,
    pub completed_phases: usize,
    pub total_phases: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// True when the task is running but the current phase is paused (awaiting user action).
    #[serde(default)]
    pub is_paused: bool,
}

#[derive(Deserialize)]
pub struct ListTasksQuery {
    #[serde(default)]
    pub archived: Option<bool>,
}

/// GET /tasks — list all task summaries.
/// Pass ?archived=true to return only archived tasks;
/// by default only non-archived tasks are returned.
pub async fn list_tasks(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<Vec<TaskSummary>>, AppError> {
    let want_archived = query.archived.unwrap_or(false);
    let task_ids = state.workflow_runner.list_task_ids().await?;
    let registry = state.workflow_registry.read().await;

    let mut summaries = Vec::new();
    for task_id in task_ids {
        let Ok(snapshot) = state.workflow_runner.get_task_state(&task_id).await else {
            continue;
        };
        let is_archived = snapshot.status == TaskStatus::Archived;
        if is_archived != want_archived {
            continue;
        }
        let total_phases = registry
            .get_definition(&snapshot.workflow)
            .map(|d| d.phases.len())
            .unwrap_or(0);
        let completed_at = if matches!(
            snapshot.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Stopped
        ) {
            snapshot
                .phases
                .values()
                .filter_map(|s| s.completed_at)
                .max()
        } else {
            None
        };
        let started_at = snapshot
            .phases
            .values()
            .filter_map(|s| s.started_at)
            .min();
        let is_paused = snapshot.status == TaskStatus::Running
            && snapshot.phases.values().any(|s| s.status == PhaseStatus::Paused);
        summaries.push(TaskSummary {
            task_id,
            workflow: snapshot.workflow,
            project_name: snapshot.project_name,
            created: snapshot.created,
            status: snapshot.status,
            completed_phases: snapshot
                .phases
                .values()
                .filter(|s| matches!(s.status, PhaseStatus::Completed | PhaseStatus::Skipped))
                .count(),
            total_phases,
            completed_at,
            started_at,
            is_paused,
        });
    }

    Ok(Json(summaries))
}

/// GET /tasks/{id} — get full task snapshot.
pub async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<TaskSnapshot>, AppError> {
    let snapshot = state
        .workflow_runner
        .get_task_state(&id)
        .await
        .map_err(|_| AoError::TaskNotFound(id))?;
    Ok(Json(snapshot))
}

/// GET /tasks/{id}/output/{filename} — get raw task output file content.
pub async fn get_task_output(
    State(state): State<Arc<AppState>>,
    Path((id, filename)): Path<(String, String)>,
) -> Result<String, AppError> {
    let content = state
        .workflow_runner
        .read_task_output(&id, &filename)
        .await?;
    Ok(content)
}

/// POST /tasks/{id}/phases/{phase}/complete — manually complete a phase.
pub async fn complete_phase(
    State(state): State<Arc<AppState>>,
    Path((task_id, phase_id)): Path<(String, String)>,
) -> Result<Json<TaskSnapshot>, AppError> {
    state
        .workflow_runner
        .complete_phase(&task_id, &phase_id)
        .await?;
    let snapshot = state.workflow_runner.get_task_state(&task_id).await?;
    Ok(Json(snapshot))
}

/// POST /tasks/{id}/start — start a pending task.
pub async fn start_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<TaskSnapshot>, AppError> {
    state.workflow_runner.start_task(&id).await?;

    // Tell the workflow queue manager to begin processing phases
    use ao_engine::workflow_queue_manager::WfQueueMsg;
    state
        .workflow_queue
        .send(WfQueueMsg::StartTask {
            task_id: id.clone(),
        })
        .await?;

    let snapshot = state.workflow_runner.get_task_state(&id).await?;
    Ok(Json(snapshot))
}

/// POST /tasks/{id}/resume — resume a paused or stopped task.
pub async fn resume_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<TaskSnapshot>, AppError> {
    // Verify task exists
    let snapshot = state
        .workflow_runner
        .get_task_state(&id)
        .await
        .map_err(|_| AoError::TaskNotFound(id.clone()))?;

    // Check if any phase is paused or stopped
    let has_paused = snapshot
        .phases
        .values()
        .any(|s| matches!(s.status, PhaseStatus::Paused));

    let has_stopped = matches!(snapshot.status, TaskStatus::Stopped);

    if !has_paused && !has_stopped {
        return Err(AppError(AoError::ValidationError(
            "No paused or stopped phase found on this task".to_string(),
        )));
    }

    // Send ResumeTask message to queue manager
    use ao_engine::workflow_queue_manager::WfQueueMsg;
    state
        .workflow_queue
        .send(WfQueueMsg::ResumeTask {
            task_id: id.clone(),
        })
        .await?;

    let snapshot = state.workflow_runner.get_task_state(&id).await?;
    Ok(Json(snapshot))
}

/// POST /tasks/{id}/cancel — cancel a running task.
pub async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<TaskSnapshot>, AppError> {
    state.workflow_runner.cancel_task(&id).await?;
    let snapshot = state.workflow_runner.get_task_state(&id).await?;
    Ok(Json(snapshot))
}

// ---------------------------------------------------------------------------
// Phase chat endpoints
// ---------------------------------------------------------------------------

// phase_agent_id and build_phase_agent are imported from ao_engine::workflow_queue_manager

/// GET /tasks/{id}/phases/{phase}/messages — read messages for a phase.
/// Reads from the main transcript store (where the agent runner writes
/// both user and agent messages) using the synthetic agent ID.
pub async fn get_phase_messages(
    State(state): State<Arc<AppState>>,
    Path((task_id, phase_id)): Path<(String, String)>,
) -> Result<Json<Vec<TranscriptEntry>>, AppError> {
    // Verify the task exists
    state
        .workflow_runner
        .get_task_state(&task_id)
        .await
        .map_err(|_| AoError::TaskNotFound(task_id.clone()))?;

    let agent_id = phase_agent_id(&task_id, &phase_id);
    let entries = state.persistence.transcripts.read_all(&agent_id).await?;
    Ok(Json(entries))
}

#[derive(Deserialize)]
pub struct SendPhaseMessageRequest {
    pub content: String,
    #[serde(default)]
    pub attachment_ids: Option<Vec<String>>,
}

/// POST /tasks/{id}/phases/{phase}/messages — send a user message to a phase agent.
pub async fn send_phase_message(
    State(state): State<Arc<AppState>>,
    Path((task_id, phase_id)): Path<(String, String)>,
    Json(req): Json<SendPhaseMessageRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let snapshot = state
        .workflow_runner
        .get_task_state(&task_id)
        .await
        .map_err(|_| AoError::TaskNotFound(task_id.clone()))?;

    // Build phase context for system prompt
    let registry = state.workflow_registry.read().await;
    let definition = registry
        .get_definition(&snapshot.workflow)
        .ok_or_else(|| AoError::WorkflowNotFound(snapshot.workflow.clone()))?;
    let phase = definition
        .phases
        .iter()
        .find(|p| p.id == phase_id)
        .ok_or_else(|| {
            AoError::ValidationError(format!("Phase '{}' not found in workflow", phase_id))
        })?
        .clone();
    let workflow_id = definition.id.clone();
    drop(registry);

    let context = state
        .workflow_runner
        .build_phase_context(&task_id, &phase)
        .await?;

    let agent = build_phase_agent(
        &task_id,
        &phase_id,
        &context,
        snapshot.working_directory.as_deref(),
        &workflow_id,
    );

    // Resolve attachment IDs to full Attachment objects
    let asset_key = format!("phase_{}_{}", task_id, phase_id);
    let attachment_ids = req.attachment_ids.unwrap_or_default();
    let mut attachments = Vec::with_capacity(attachment_ids.len());
    for aid in &attachment_ids {
        let attachment = state
            .persistence
            .assets
            .get_attachment(&asset_key, aid)
            .await
            .map_err(|_| AoError::ValidationError(format!("Attachment not found: {}", aid)))?;
        attachments.push(attachment);
    }

    // Mark each attachment as committed to this message
    let message_id = Uuid::new_v4().to_string();
    for aid in &attachment_ids {
        state
            .persistence
            .assets
            .mark_committed(&asset_key, aid, &message_id)
            .await?;
    }

    // Build metadata
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
        "message_id".to_string(),
        serde_json::Value::String(message_id.clone()),
    );

    if !attachments.is_empty() {
        let attachments_json: Vec<serde_json::Value> = attachments
            .iter()
            .map(|a| {
                serde_json::json!({
                    "id": a.id,
                    "file_path": a.file_path,
                    "mime_type": a.mime_type,
                    "original_filename": a.original_filename,
                    "attachment_type": a.attachment_type,
                })
            })
            .collect();
        metadata.insert(
            "attachments".to_string(),
            serde_json::Value::Array(attachments_json),
        );
    }

    // Persist user message
    let user_entry = TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("user".to_string()),
        content: req.content.clone(),
        event_type: "message".to_string(),
        metadata: Some(metadata),
        hidden_from_user: false,
    };
    state
        .workflow_runner
        .append_phase_message(&task_id, &phase_id, &user_entry)
        .await?;

    // Also persist to the regular transcript store so the agent runner can
    // find the conversation history under the synthetic agent ID.
    let agent_id = phase_agent_id(&task_id, &phase_id);
    state
        .persistence
        .transcripts
        .append(&agent_id, &user_entry)
        .await?;

    // Submit to queue manager
    let queued = QueuedMessage {
        message_id: message_id.clone(),
        content: req.content,
        queued_at: Utc::now(),
        attachments,
        source: None,
        focus_path: None,
        thread_id: None,
    };
    state.queue_managers.submit_message(&agent, queued).await?;

    Ok(Json(serde_json::json!({
        "message_id": message_id,
        "status": "queued"
    })))
}

/// POST /tasks/{id}/phases/{phase}/start — cold-start the agent for a phase.
/// For phases that need to initiate (e.g., interview), this triggers the
/// agent to begin with an initial prompt.
/// Skips pause-type, input-type, and currently-paused phases.
pub async fn start_phase_agent(
    State(state): State<Arc<AppState>>,
    Path((task_id, phase_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let snapshot = state
        .workflow_runner
        .get_task_state(&task_id)
        .await
        .map_err(|_| AoError::TaskNotFound(task_id.clone()))?;

    // Reject if phase is currently paused
    if let Some(phase_state) = snapshot.phases.get(&phase_id) {
        if matches!(
            phase_state.status,
            ao_protocol::workflow::PhaseStatus::Paused
        ) {
            return Ok(Json(serde_json::json!({
                "message_id": "",
                "status": "skipped"
            })));
        }
    }

    // Idempotency guard: if the backend already cold-started this phase,
    // skip to avoid spawning a duplicate agent session.
    let agent_id = phase_agent_id(&task_id, &phase_id);
    let existing = state.persistence.transcripts.read_recent(&agent_id, 1).await.unwrap_or_default();
    if existing.iter().any(|e| e.event_type == "cold_start") {
        return Ok(Json(serde_json::json!({
            "message_id": "",
            "status": "already_started"
        })));
    }

    // Build phase context
    let registry = state.workflow_registry.read().await;
    let definition = registry
        .get_definition(&snapshot.workflow)
        .ok_or_else(|| AoError::WorkflowNotFound(snapshot.workflow.clone()))?;
    let phase = definition
        .phases
        .iter()
        .find(|p| p.id == phase_id)
        .ok_or_else(|| {
            AoError::ValidationError(format!("Phase '{}' not found in workflow", phase_id))
        })?
        .clone();

    // Reject pause-type, input-type, and folder-type phases.
    // Folder phases are executed via run.sh by the queue manager — cold-starting
    // an agent would conflict with the already-running script.
    if matches!(
        phase.phase_type,
        Some(ao_protocol::workflow::PhaseType::Pause)
            | Some(ao_protocol::workflow::PhaseType::Input)
            | Some(ao_protocol::workflow::PhaseType::Folder)
    ) {
        drop(registry);
        return Ok(Json(serde_json::json!({
            "message_id": "",
            "status": "skipped"
        })));
    }

    let workflow_id = definition.id.clone();
    drop(registry);

    let context = state
        .workflow_runner
        .build_phase_context(&task_id, &phase)
        .await?;

    let agent = build_phase_agent(
        &task_id,
        &phase_id,
        &context,
        snapshot.working_directory.as_deref(),
        &workflow_id,
    );

    // Cold-start message: triggers the agent to begin working on the phase
    let message_id = Uuid::new_v4().to_string();
    let cold_start_content = format!(
        "Begin working on phase '{}'. Follow the system prompt instructions. \
         If this phase requires user interaction (like an interview), start by \
         greeting the user and asking your first question.",
        phase.name
    );

    let system_entry = TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("system".to_string()),
        content: cold_start_content.clone(),
        event_type: "cold_start".to_string(),
        metadata: Some({
            let mut m = std::collections::HashMap::new();
            m.insert(
                "message_id".to_string(),
                serde_json::Value::String(message_id.clone()),
            );
            m
        }),
        hidden_from_user: false,
    };
    state
        .workflow_runner
        .append_phase_message(&task_id, &phase_id, &system_entry)
        .await?;

    let agent_id = phase_agent_id(&task_id, &phase_id);
    state
        .persistence
        .transcripts
        .append(&agent_id, &system_entry)
        .await?;

    // Submit to queue manager to trigger agent run
    let queued = QueuedMessage {
        message_id: message_id.clone(),
        content: cold_start_content,
        queued_at: Utc::now(),
        attachments: vec![],
        source: None,
        focus_path: None,
        thread_id: None,
    };
    state.queue_managers.submit_message(&agent, queued).await?;

    Ok(Json(serde_json::json!({
        "message_id": message_id,
        "status": "started"
    })))
}

/// POST /tasks/{id}/phases/{phase}/submit-input — submit form values for an input phase.
/// Writes values as YAML to the task output directory and completes the phase.
pub async fn submit_input(
    State(state): State<Arc<AppState>>,
    Path((task_id, phase_id)): Path<(String, String)>,
    Json(values): Json<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Verify task exists
    let snapshot = state
        .workflow_runner
        .get_task_state(&task_id)
        .await
        .map_err(|_| AoError::TaskNotFound(task_id.clone()))?;

    // Validate the phase exists and is an input type
    let registry = state.workflow_registry.read().await;
    let definition = registry
        .get_definition(&snapshot.workflow)
        .ok_or_else(|| AoError::WorkflowNotFound(snapshot.workflow.clone()))?;
    let phase = definition
        .phases
        .iter()
        .find(|p| p.id == phase_id)
        .ok_or_else(|| {
            AoError::ValidationError(format!("Phase '{}' not found in workflow", phase_id))
        })?;

    // Validate required fields
    for field in &phase.fields {
        if field.required && !values.contains_key(&field.name) {
            return Err(AppError(AoError::ValidationError(format!(
                "Required field '{}' is missing",
                field.name
            ))));
        }
    }
    drop(registry);

    // Read existing inputs.yaml and merge new values into it.
    // All input phases write to a single shared file.
    let mut existing: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Ok(content) = state
        .workflow_runner
        .read_task_output(&task_id, "inputs.yaml")
        .await
    {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                existing.insert(
                    k.trim().to_string(),
                    v.trim().trim_matches('"').trim_matches('\'').to_string(),
                );
            }
        }
    }
    existing.extend(values.clone());

    let mut yaml_content = String::from("# Workflow input values (auto-generated)\n");
    let mut keys: Vec<&String> = existing.keys().collect();
    keys.sort();
    for k in keys {
        let v = &existing[k];
        if v.contains(':') || v.contains('#') || v.contains('\n') || v.starts_with(' ') {
            yaml_content.push_str(&format!("{}: \"{}\"\n", k, v.replace('"', "\\\"")));
        } else {
            yaml_content.push_str(&format!("{}: {}\n", k, v));
        }
    }
    state
        .workflow_runner
        .write_task_output(&task_id, "inputs.yaml", &yaml_content)
        .await?;

    // Complete the phase
    state
        .workflow_runner
        .complete_phase(&task_id, &phase_id)
        .await?;

    // Notify the workflow queue manager to advance
    use ao_engine::workflow_queue_manager::WfQueueMsg;
    state
        .workflow_queue
        .send(WfQueueMsg::PhaseCompleted {
            task_id: task_id.clone(),
            phase_id: phase_id.clone(),
        })
        .await?;

    Ok(Json(serde_json::json!({
        "status": "completed",
        "output": format!("{}.yaml", phase_id)
    })))
}

/// DELETE /tasks/{id} — delete a task and its associated attachments from disk.
pub async fn delete_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Read the task snapshot to get phase IDs before deletion
    if let Ok(snapshot) = state.workflow_runner.get_task_state(&id).await {
        for phase_id in snapshot.phases.keys() {
            let asset_key = format!("phase_{}_{}", id, phase_id);
            let _ = state.persistence.assets.delete_asset_key(&asset_key).await;
        }
    }

    state
        .workflow_runner
        .delete_task(&id)
        .await
        .map_err(|_| AoError::TaskNotFound(id.clone()))?;
    Ok(Json(serde_json::json!({ "status": "deleted" })))
}

/// POST /tasks/{id}/archive — archive a task.
pub async fn archive_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    state
        .workflow_runner
        .archive_task(&id)
        .await
        .map_err(|_| AoError::TaskNotFound(id.clone()))?;
    Ok(Json(serde_json::json!({ "status": "archived" })))
}
