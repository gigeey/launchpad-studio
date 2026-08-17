use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ao_engine::AppState;
use ao_engine_tools_core::TasklistServiceHandle;
use ao_engine_tools_engine::classify_with_retry;
use ao_protocol::error::AoError;
use ao_protocol::tasklist::{
    Task, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistOwner, TasklistStatus,
};

use crate::error::AppError;

/// Splits a "Title: brief description" prompt back into its two parts.
/// Tools (`TodoCreate`, `TodoAdd`) format their prompts this way so the
/// classifier sees a structured title/description pair. The chat-input route
/// receives a free-form prompt — we feed it as the description with an empty
/// title; the classifier tolerates that and uses the description alone.
fn split_classifier_prompt(prompt: &str) -> (String, String) {
    let mut parts = prompt.splitn(2, ": ");
    let head = parts.next().unwrap_or("").to_string();
    match parts.next() {
        Some(rest) => (head, rest.to_string()),
        None => (String::new(), head),
    }
}

/// Spawn `classify_with_retry` for every task in the given groups that has no
/// pinned assignment. Mirrors the engine-side `TodoCreate`/`TodoAdd` spawn
/// blocks so frontend-created tasks don't have to wait for the periodic
/// reconciler tick to be classified. Each spawn participates in the shared
/// `classifier_in_flight` dedup so a concurrent reconciler tick can't fire a
/// duplicate against the same task.
fn spawn_classifiers_for_unowned(
    state: &AppState,
    agent_id: &str,
    tasklist_id: &str,
    groups: &[TaskGroup],
) {
    let classifier = Arc::clone(&state.task_classifier_handle);
    let svc: Arc<dyn TasklistServiceHandle + Send + Sync> =
        Arc::clone(&state.tasklist_service)
            as Arc<dyn TasklistServiceHandle + Send + Sync>;
    let in_flight = Arc::clone(&state.classifier_in_flight);
    for group in groups {
        for task in &group.tasks {
            if task.assignment.is_some() {
                continue;
            }
            let (title, desc) = split_classifier_prompt(&task.prompt);
            tokio::spawn(classify_with_retry(
                Arc::clone(&classifier),
                Arc::clone(&svc),
                Some(Arc::clone(&in_flight)),
                agent_id.to_string(),
                tasklist_id.to_string(),
                task.id.clone(),
                agent_id.to_string(),
                title,
                desc,
                task.classifier_token,
            ));
        }
    }
}

/// Request body for `POST /agents/{agent_id}/tasklists`.
#[derive(Debug, Deserialize)]
pub struct CreateTasklistRequest {
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub groups: Vec<CreateTaskGroup>,
    #[serde(default)]
    pub allow_empty_groups: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTaskGroup {
    pub mode: TaskGroupMode,
    #[serde(default)]
    pub tasks: Vec<CreateTask>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTask {
    pub owner_agent_id: String,
    pub prompt: String,
    #[serde(default)]
    pub expected_outputs: Vec<String>,
}

/// Response shape for `GET /agents/{agent_id}/tasklists`.
#[derive(Debug, Serialize)]
pub struct ListTasklistsResponse {
    pub active: Option<Tasklist>,
    pub recent: Vec<Tasklist>,
}

/// Request body for `POST /agents/{agent_id}/tasklists/{tasklist_id}/tasks`.
#[derive(Debug, Deserialize)]
pub struct AppendTaskRequest {
    pub prompt: String,
    #[serde(default)]
    pub owner_agent_id: Option<String>,
    #[serde(default)]
    pub expected_outputs: Vec<String>,
    pub mode: TaskGroupMode,
    #[serde(default)]
    pub attachment_ids: Vec<String>,
}

/// Request body for `POST /agents/{agent_id}/tasklists/{tasklist_id}/status`.
#[derive(Debug, Deserialize)]
pub struct SetTasklistStatusRequest {
    pub status: String,
}

fn build_task_groups(raw: Vec<CreateTaskGroup>) -> Vec<TaskGroup> {
    raw.into_iter()
        .map(|g| {
            let group_id = Uuid::new_v4().to_string();
            let tasks = g
                .tasks
                .into_iter()
                .map(|t| Task {
                    id: Uuid::new_v4().to_string(),
                    owner_agent_id: t.owner_agent_id,
                    prompt: t.prompt,
                    expected_outputs: t.expected_outputs,
                    status: TaskStatus::Pending,
                    group_id: group_id.clone(),
                    attempt_count: 0,
                    error_log: Vec::new(),
                    comments: Vec::new(),
                    attachments: Vec::new(),
                    remind_me: None,
                    parse_failed: false,
                    notification_parse_retry_count: 0,
                    assignment: None,
                    classifier_token: 0,
                    dispatch_token: 0,
                })
                .collect();
            TaskGroup {
                id: group_id,
                mode: g.mode,
                tasks,
            }
        })
        .collect()
}

/// POST /agents/{agent_id}/tasklists — create a new tasklist for the agent.
pub async fn create_tasklist(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<CreateTasklistRequest>,
) -> Result<Json<Tasklist>, AppError> {
    let owner = TasklistOwner::Agent {
        agent_id: agent_id.clone(),
    };
    let groups = build_task_groups(req.groups);
    let tasklist = state
        .tasklist_service
        .create(
            owner,
            req.title,
            req.description,
            groups,
            req.allow_empty_groups.unwrap_or(false),
        )
        .await?;
    // Kick off background classification for any task that landed without a
    // pinned assignment. Without this, frontend-created tasks would only get
    // classified by the periodic boot sweep (6h cadence).
    spawn_classifiers_for_unowned(&state, &agent_id, &tasklist.id, &tasklist.groups);
    Ok(Json(tasklist))
}

/// GET /agents/{agent_id}/tasklists — list active + recently completed tasklists.
pub async fn list_tasklists(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<ListTasklistsResponse>, AppError> {
    let owner = TasklistOwner::Agent { agent_id };
    let all = state.tasklist_service.list(&owner).await?;
    let mut active = None;
    let mut recent = Vec::new();
    for tl in all {
        // Project-scoped tasklists are owned by the agent but belong to a
        // project surface; they're served by GET /projects/{id}/tasklists.
        // Excluding them here keeps them out of the agent's personal chat so
        // the same tasklist doesn't render in two places.
        if tl.project_id.is_some() {
            continue;
        }
        match tl.status {
            TasklistStatus::Active | TasklistStatus::Paused if active.is_none() => {
                active = Some(tl)
            }
            _ => recent.push(tl),
        }
    }
    Ok(Json(ListTasklistsResponse { active, recent }))
}

/// GET /agents/{agent_id}/tasklists/{tasklist_id} — return full tasklist state.
pub async fn get_tasklist(
    State(state): State<Arc<AppState>>,
    Path((agent_id, tasklist_id)): Path<(String, String)>,
) -> Result<Json<Tasklist>, AppError> {
    let owner = TasklistOwner::Agent { agent_id };
    let tasklist = state
        .tasklist_service
        .get(&owner, &tasklist_id)
        .await?
        .ok_or_else(|| AoError::TasklistNotFound(tasklist_id))?;
    Ok(Json(tasklist))
}

/// POST /agents/{agent_id}/tasklists/{tasklist_id}/tasks
pub async fn append_task(
    State(state): State<Arc<AppState>>,
    Path((agent_id, tasklist_id)): Path<(String, String)>,
    Json(req): Json<AppendTaskRequest>,
) -> Result<Json<Tasklist>, AppError> {
    let owner = TasklistOwner::Agent {
        agent_id: agent_id.clone(),
    };
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
    // `add_tasks` appends the new task to the last group (existing or fresh),
    // so the newly-added row is always `groups.last().tasks.last()`. Spawn a
    // classifier if it landed unowned — chat-UI input lands here, and without
    // this it would sit "Classifying" forever (until the 6h boot sweep).
    if let Some(last_group) = updated.groups.last() {
        if let Some(new_task) = last_group.tasks.last() {
            if new_task.assignment.is_none() {
                // Synthesize a single-task group so we can reuse the same
                // spawn helper used by `create_tasklist` — we don't want to
                // re-classify the older tasks already on this tasklist.
                let synthetic = vec![TaskGroup {
                    id: last_group.id.clone(),
                    mode: last_group.mode,
                    tasks: vec![new_task.clone()],
                }];
                spawn_classifiers_for_unowned(&state, &agent_id, &updated.id, &synthetic);
            }
        }
    }
    Ok(Json(updated))
}

/// POST /agents/{agent_id}/tasklists/{tasklist_id}/status
pub async fn set_tasklist_status(
    State(state): State<Arc<AppState>>,
    Path((agent_id, tasklist_id)): Path<(String, String)>,
    Json(req): Json<SetTasklistStatusRequest>,
) -> Result<Json<Tasklist>, AppError> {
    tracing::info!(
        agent_id = %agent_id,
        tasklist_id = %tasklist_id,
        requested_status = %req.status,
        "POST /agents/:id/tasklists/:id/status",
    );
    let owner = TasklistOwner::Agent { agent_id };
    let updated = state
        .tasklist_service
        .set_status(&owner, &tasklist_id, &req.status)
        .await?;
    Ok(Json(updated))
}

/// POST /agents/{agent_id}/tasklists/{tasklist_id}/tasks/{task_id}/skip
pub async fn skip_task(
    State(state): State<Arc<AppState>>,
    Path((agent_id, tasklist_id, task_id)): Path<(String, String, String)>,
) -> Result<Json<Tasklist>, AppError> {
    tracing::info!(
        agent_id = %agent_id,
        tasklist_id = %tasklist_id,
        task_id = %task_id,
        "POST /agents/:id/tasklists/:id/tasks/:task_id/skip",
    );
    let owner = TasklistOwner::Agent { agent_id };
    let updated = state
        .tasklist_service
        .skip_task(&owner, &tasklist_id, &task_id)
        .await?;
    Ok(Json(updated))
}
