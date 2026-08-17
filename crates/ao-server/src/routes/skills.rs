use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;

use ao_engine::skill_usage;
use ao_engine::skills::{
    import_file_to_pool, import_folder_to_pool, promote_launchpad_skill_to_global,
    resolve_user_pool_dir, scan_launchpad_global_skills, scan_launchpad_project_skills,
    scan_plugin_pool_for_agent, scan_user_pool_for_agent, AddedBy, LaunchpadSkillEntry,
    PromoteLaunchpadSkillOutcome, SkillDto, SkillManifest,
};
use ao_engine::AppState;
use ao_engine_tools_core::skill_registry::SkillRegistry;
use ao_engine_tools_engine::skill::review;
use ao_protocol::agent::canonical_project_key;
use ao_protocol::error::AoError;
use ao_protocol::slug::slugify;

use crate::error::AppError;

#[derive(Debug, serde::Deserialize)]
pub struct WriteSkillRequest {
    pub title: String,
    pub description: String,
    pub content: String,
    /// Accepted for API backward-compat but ignored (pool path is always used).
    #[serde(default)]
    pub focus_path: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct WriteSkillResponse {
    pub path: String,
    pub message: String,
}

/// POST /agents/{agent_id}/skills
///
/// Writes `<data_root>/skills/<slug>/SKILL.md`, appends the slug to the
/// agent's `AgentProfile.skills`, and invalidates the context cache.
pub async fn write_skill(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<WriteSkillRequest>,
) -> Result<(StatusCode, Json<WriteSkillResponse>), AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let title = req.title.trim().to_string();
    if title.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "title must not be empty".to_string(),
        )));
    }
    let slug = slugify(&title);
    if slug.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "title must contain at least one alphanumeric character".to_string(),
        )));
    }

    let pool_dir = resolve_user_pool_dir(&state.persistence.data_root);
    let skill_dir = pool_dir.join(&slug);

    tokio::fs::create_dir_all(&skill_dir).await.map_err(|e| {
        AppError(AoError::Internal(format!(
            "Failed to create skill directory: {}",
            e
        )))
    })?;

    let description = req.description.trim().to_string();
    let skill_content = format!(
        "---\nname: {slug}\ndescription: {desc}\n---\n\n{body}\n",
        desc = description,
        body = req.content,
    );
    let skill_md = skill_dir.join("SKILL.md");
    tokio::fs::write(&skill_md, skill_content).await.map_err(|e| {
        AppError(AoError::Internal(format!("Failed to write SKILL.md: {}", e)))
    })?;

    // Write sidecar manifest (sync, quick)
    let manifest_dir = skill_dir.clone();
    let manifest = SkillManifest {
        added_by: AddedBy::User,
        enabled: true,
        auto_sync: false,
        source_url: None,
        imported_at: Utc::now(),
    };
    tokio::task::spawn_blocking(move || ao_engine::skills::write_manifest(&manifest_dir, &manifest))
        .await
        .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?
        .map_err(|e| {
            AppError(AoError::Internal(format!("Failed to write manifest: {}", e)))
        })?;

    // Append to agent profile and persist
    let mut profile = agent;
    if !profile.skills.contains(&slug) {
        profile.skills.push(slug.clone());
    }
    state.persistence.agents.update(&profile).await?;

    state.context_cache.invalidate(&agent_id).await;

    let path_str = skill_md.to_string_lossy().to_string();
    tracing::info!(agent_id = %agent_id, path = %path_str, "Wrote skill to user pool");

    Ok((
        StatusCode::CREATED,
        Json(WriteSkillResponse {
            path: path_str,
            message: format!("Skill '{}' written to {}/SKILL.md", title, slug),
        }),
    ))
}

#[derive(Debug, serde::Deserialize)]
pub struct ImportPathRequest {
    pub src_path: String,
}

fn import_io_error_to_app_error(e: std::io::Error) -> AppError {
    match e.kind() {
        std::io::ErrorKind::NotFound
        | std::io::ErrorKind::InvalidInput
        | std::io::ErrorKind::PermissionDenied => {
            AppError(AoError::ValidationError(e.to_string()))
        }
        _ => AppError(AoError::Internal(format!("skill import failed: {e}"))),
    }
}

/// POST /agents/{agent_id}/skills/import-folder
///
/// Copies `src_path` into the user pool as a folder-skill. Appends the
/// canonical name to `AgentProfile.skills` and persists the profile.
pub async fn import_folder(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<ImportPathRequest>,
) -> Result<Json<Vec<SkillDto>>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let src_raw = req.src_path.trim().to_string();
    if src_raw.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "src_path must not be empty".to_string(),
        )));
    }

    let pool_dir = resolve_user_pool_dir(&state.persistence.data_root);
    let src = std::path::PathBuf::from(src_raw);
    let agent_id_short: String = agent_id.chars().take(8).collect();

    let (canonical_name, skills) =
        tokio::task::spawn_blocking(move || import_folder_to_pool(&pool_dir, &src, &agent_id_short))
            .await
            .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?
            .map_err(import_io_error_to_app_error)?;

    let mut profile = agent;
    if !profile.skills.contains(&canonical_name) {
        profile.skills.push(canonical_name);
    }
    state.persistence.agents.update(&profile).await?;

    state.context_cache.invalidate(&agent_id).await;
    Ok(Json(skills))
}

/// POST /agents/{agent_id}/skills/import-file
///
/// Copies `src_path` (a `.md` file) into the user pool as a flat skill.
/// Appends the canonical name to `AgentProfile.skills` and persists.
pub async fn import_file(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<ImportPathRequest>,
) -> Result<Json<SkillDto>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let src_raw = req.src_path.trim().to_string();
    if src_raw.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "src_path must not be empty".to_string(),
        )));
    }

    let pool_dir = resolve_user_pool_dir(&state.persistence.data_root);
    let src = std::path::PathBuf::from(src_raw);
    let agent_id_short: String = agent_id.chars().take(8).collect();

    let (canonical_stem, dto) =
        tokio::task::spawn_blocking(move || import_file_to_pool(&pool_dir, &src, &agent_id_short))
            .await
            .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?
            .map_err(import_io_error_to_app_error)?;

    let mut profile = agent;
    if !profile.skills.contains(&canonical_stem) {
        profile.skills.push(canonical_stem);
    }
    state.persistence.agents.update(&profile).await?;

    state.context_cache.invalidate(&agent_id).await;
    Ok(Json(dto))
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct PatchSkillRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub auto_sync: Option<bool>,
}

/// DELETE /agents/{agent_id}/skills/{skill_id}
///
/// Soft-deletes by removing `skill_id` from `AgentProfile.skills` (idempotent;
/// returns 204 even when absent). The file remains in the pool for other agents.
/// Plugin skills cannot be deleted via this route (returns 400).
pub async fn delete_skill(
    State(state): State<Arc<AppState>>,
    Path((agent_id, skill_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    // Reject plugin skills
    let data_root_path = state.persistence.data_root.root().clone();
    let agent_clone = agent.clone();
    let skill_id_clone = skill_id.clone();
    let is_plugin = tokio::task::spawn_blocking(move || {
        scan_plugin_pool_for_agent(&data_root_path, &agent_clone)
            .into_iter()
            .any(|s| s.id == skill_id_clone)
    })
    .await
    .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?;

    if is_plugin {
        return Err(AppError(AoError::ValidationError(
            "plugin skills are managed via plugin enablement".to_string(),
        )));
    }

    let mut profile = agent;
    profile.skills.retain(|s| s != &skill_id);
    state.persistence.agents.update(&profile).await?;

    state.context_cache.invalidate(&agent_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// PATCH /agents/{agent_id}/skills/{skill_id}
///
/// - `enabled: false` → removes `skill_id` from `AgentProfile.skills` (idempotent).
/// - `enabled: true`  → requires skill to exist in user pool; appends to `AgentProfile.skills`.
/// - `auto_sync: <any>` → 400 (auto_sync is retired).
pub async fn patch_skill(
    State(state): State<Arc<AppState>>,
    Path((agent_id, skill_id)): Path<(String, String)>,
    Json(req): Json<PatchSkillRequest>,
) -> Result<Json<SkillDto>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    if req.auto_sync.is_some() {
        return Err(AppError(AoError::ValidationError(
            "auto_sync is no longer supported; re-import the skill to refresh from source"
                .to_string(),
        )));
    }

    let pool_dir = resolve_user_pool_dir(&state.persistence.data_root);

    let mut profile = agent;
    let new_enabled = match req.enabled {
        Some(true) => {
            // Skill must exist in user pool
            let pool_dir_clone = pool_dir.clone();
            let skill_id_clone = skill_id.clone();
            let found = tokio::task::spawn_blocking(move || {
                let results = scan_user_pool_for_agent(&pool_dir_clone, &[skill_id_clone]);
                !results.is_empty()
            })
            .await
            .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?;

            if !found {
                return Err(AppError(AoError::SkillNotFound(format!(
                    "skill '{}' not found in user pool",
                    skill_id
                ))));
            }
            if !profile.skills.contains(&skill_id) {
                profile.skills.push(skill_id.clone());
            }
            true
        }
        Some(false) => {
            profile.skills.retain(|s| s != &skill_id);
            false
        }
        None => profile.skills.contains(&skill_id),
    };

    state.persistence.agents.update(&profile).await?;
    state.context_cache.invalidate(&agent_id).await;

    // Build response DTO from pool
    let pool_dir_clone = pool_dir.clone();
    let skill_id_clone = skill_id.clone();
    let mut dtos =
        tokio::task::spawn_blocking(move || scan_user_pool_for_agent(&pool_dir_clone, &[skill_id_clone]))
            .await
            .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?;

    let dto = match dtos.first_mut() {
        Some(d) => {
            d.enabled = new_enabled;
            d.clone()
        }
        None => {
            // Skill not in pool — return a minimal stub (already removed from profile)
            return Err(AppError(AoError::SkillNotFound(format!(
                "skill '{}' not found in user pool",
                skill_id
            ))));
        }
    };

    Ok(Json(dto))
}

/// POST /agents/{agent_id}/skills/refresh
///
/// Re-scans the user pool and returns the agent-scoped subset.
/// Does NOT run git pull (auto_sync retired).
pub async fn refresh_skills(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<SkillDto>>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let pool_dir = resolve_user_pool_dir(&state.persistence.data_root);
    let agent_skills = agent.skills.clone();

    let skills =
        tokio::task::spawn_blocking(move || scan_user_pool_for_agent(&pool_dir, &agent_skills))
            .await
            .map_err(|e| AppError(AoError::Internal(format!("Failed to rescan pool: {}", e))))?;

    state.context_cache.invalidate(&agent_id).await;
    Ok(Json(skills))
}

/// GET /agents/{agent_id}/skills
///
/// Lists skills from the user pool filtered by the agent's `AgentProfile.skills`,
/// plus plugin-pool skills the agent has enabled (with `source: "plugin"`).
/// Merges per-agent usage counts from `<agent_home>/skills/.usage.json`.
pub async fn list_skills(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<SkillDto>>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let pool_dir = resolve_user_pool_dir(&state.persistence.data_root);
    let data_root_path = state.persistence.data_root.root().clone();
    let agent_skills = agent.skills.clone();
    let agent_clone = agent.clone();

    let (user_skills, plugin_skills) = tokio::task::spawn_blocking(move || {
        let user = scan_user_pool_for_agent(&pool_dir, &agent_skills);
        let plugin = scan_plugin_pool_for_agent(&data_root_path, &agent_clone);
        (user, plugin)
    })
    .await
    .map_err(|e| AppError(AoError::Internal(format!("Failed to scan skills: {}", e))))?;

    // Per-agent usage lives in <agent_home>/skills/.usage.json (legacy location)
    let agent_home_skills = state
        .persistence
        .data_root
        .agent_home_dir(&agent_id)
        .join("skills");
    let usage = skill_usage::load(&agent_home_skills).await;

    let mut all_skills: Vec<SkillDto> = user_skills
        .into_iter()
        .chain(plugin_skills)
        .collect();

    for skill in &mut all_skills {
        if let Some(entry) = usage.get(&skill.id) {
            skill.usage_count = entry.count;
            skill.last_used = Some(entry.last_used);
        }
    }

    Ok(Json(all_skills))
}

// --- Skill review surface (mirrors the memory review queue in
// `routes::memories`) ---
//
// A parked skill (`disable-model-invocation: true`) already carries its own
// "not live yet" state in its frontmatter — see
// `ao_engine_tools_engine::skill::review`'s module doc for why that queue
// never overlaps with the memory review queue above, and for the two writers
// that park skills: `ao_engine::skill_distillation::SkillDistiller` (which
// also stamps `origin: distilled`) and the `SkillRegister` tool (which does
// not). Both land here; this queue is the only surface that can un-park
// either. These routes are the read/act surface over it: list what's parked
// plus what raw `Skill` observations are still eligible for manual promotion,
// act on one parked skill (`accept`/`edit`/`reject`), and promote one
// observation straight to a parked skill ahead of the automatic repetition
// threshold.

/// GET /agents/{agent_id}/skills/review
pub async fn list_skill_review_queue(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<review::SkillReviewQueue>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let data_dir = state.persistence.data_root.root();
    let registry = SkillRegistry::load(data_dir, &agent);
    let queue =
        review::list_queue(data_dir, &registry, &state.persistence.reflection_staging, &agent_id).await?;
    Ok(Json(queue))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillReviewAction {
    Accept,
    Edit,
    Reject,
}

#[derive(Debug, serde::Deserialize)]
pub struct SkillReviewActionRequest {
    pub action: SkillReviewAction,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub keep_parked: Option<bool>,
}

/// POST /agents/{agent_id}/skills/review/{skill_name} — act on one parked
/// skill: `accept`, `edit` (body must include non-empty `body`, optionally
/// `description`), or `reject`.
pub async fn act_on_skill_review_candidate(
    State(state): State<Arc<AppState>>,
    Path((agent_id, skill_name)): Path<(String, String)>,
    Json(req): Json<SkillReviewActionRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let data_dir = state.persistence.data_root.root();
    let registry = SkillRegistry::load(data_dir, &agent);

    let result = match req.action {
        SkillReviewAction::Accept => {
            review::accept(data_dir, &registry, &skill_name).await?;
            state.context_cache.invalidate(&agent_id).await;
            serde_json::json!({ "skill_name": skill_name, "action": "accept", "live": true })
        }
        SkillReviewAction::Edit => {
            let body = req
                .body
                .filter(|b| !b.trim().is_empty())
                .ok_or_else(|| AoError::ValidationError("edit requires non-empty body".to_string()))?;
            let keep_parked = req.keep_parked.unwrap_or(false);
            review::edit(data_dir, &registry, &skill_name, &body, req.description.as_deref(), keep_parked)
                .await?;
            state.context_cache.invalidate(&agent_id).await;
            serde_json::json!({
                "skill_name": skill_name,
                "action": "edit",
                "live": !keep_parked,
            })
        }
        SkillReviewAction::Reject => {
            review::reject(data_dir, &registry, &skill_name).await?;

            let mut profile = agent;
            profile.skills.retain(|s| s != &skill_name);
            state.persistence.agents.update(&profile).await?;

            state.context_cache.invalidate(&agent_id).await;
            serde_json::json!({ "skill_name": skill_name, "action": "reject", "rejected": true })
        }
    };
    Ok(Json(result))
}

#[derive(Debug, serde::Deserialize)]
pub struct PromoteSkillObservationRequest {
    pub candidate_id: String,
}

/// POST /agents/{agent_id}/skills/review/promote — manually promote one raw
/// `Skill`-kind reflection candidate to a parked distilled skill, ahead of
/// the automatic repetition threshold. Delegates the actual generalization
/// call to `SkillDistiller::generalize_single`, which also marks the source
/// candidate `Distilled` in the staging store as part of writing the parked
/// skill — nothing further to do here on success beyond shaping the
/// response.
pub async fn promote_skill_observation(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<PromoteSkillObservationRequest>,
) -> Result<Json<review::ParkedSkillCandidate>, AppError> {
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let observation = review::find_pending_skill_observation(
        &state.persistence.reflection_staging,
        &agent_id,
        &req.candidate_id,
    )
    .await?;

    let template = state
        .skill_distiller
        .generalize_single(&observation)
        .await
        .map_err(AoError::ValidationError)?;

    let data_dir = state.persistence.data_root.root();
    let candidate = review::ParkedSkillCandidate {
        name: template.written_as.clone(),
        description: template.description,
        body: template.body,
        origin: "distilled".to_string(),
        distilled_from: vec![observation.id],
        created_at: review::parked_skill_created_at(data_dir, &template.written_as),
    };

    state.context_cache.invalidate(&agent_id).await;
    Ok(Json(candidate))
}

// --- Convention-folder ("launchpad-skills") routes ---
//
// Human-dropped skill folders under `<data_root>/.launchpad/skills` (global)
// and `<focus_path>/.launchpad/skills` (project) — a separate pool source
// from the user pool / plugin pool above, gated by explicit per-agent
// enablement instead of `AddedBy`/trust_gate. Deliberately
// does not route through `import_folder_to_pool`/`POST .../skills/import-folder`
// — those copy into the trusted user pool and auto-enable; convention skills
// stay available-inert until explicitly enabled here.

#[derive(Debug, serde::Serialize)]
pub struct ListLaunchpadSkillsResponse {
    pub skills: Vec<LaunchpadSkillEntry>,
}

/// GET /skills/launchpad/global
///
/// Scans `<data_root>/.launchpad/skills`. Not agent-scoped — the folder is
/// one shared global root; per-agent gating happens at enable time.
pub async fn list_launchpad_global_skills(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ListLaunchpadSkillsResponse>, AppError> {
    let data_root_path = state.persistence.data_root.root().clone();
    let skills = tokio::task::spawn_blocking(move || scan_launchpad_global_skills(&data_root_path))
        .await
        .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?;
    Ok(Json(ListLaunchpadSkillsResponse { skills }))
}

#[derive(Debug, serde::Deserialize)]
pub struct ListLaunchpadProjectSkillsQuery {
    #[serde(default)]
    pub focus_path: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct ListLaunchpadProjectSkillsResponse {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_key: String,
    pub skills: Vec<LaunchpadSkillEntry>,
}

/// GET /skills/launchpad/project?focus_path=<abs>
///
/// Scans `<focus_path>/.launchpad/skills`. `project_key` is the canonicalized
/// form of `focus_path` (see `canonical_project_key`), returned so the caller
/// can echo it straight back in the enable/promote request bodies below.
/// Empty `skills` (dir missing) still resolves and returns `project_key`;
/// `project_key` is empty/omitted when `focus_path` is unset.
pub async fn list_launchpad_project_skills(
    Query(query): Query<ListLaunchpadProjectSkillsQuery>,
) -> Result<Json<ListLaunchpadProjectSkillsResponse>, AppError> {
    let focus_path = query.focus_path.filter(|p| !p.trim().is_empty());
    let project_key = focus_path
        .as_deref()
        .map(canonical_project_key)
        .unwrap_or_default();

    let focus_path_for_scan = focus_path.clone();
    let skills = tokio::task::spawn_blocking(move || {
        scan_launchpad_project_skills(focus_path_for_scan.as_deref())
    })
    .await
    .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?;

    Ok(Json(ListLaunchpadProjectSkillsResponse {
        project_key,
        skills,
    }))
}

#[derive(Debug, serde::Deserialize)]
pub struct SetLaunchpadGlobalSkillEnabledRequest {
    pub skill_name: String,
    pub enabled: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct LaunchpadGlobalSkillEnablementResponse {
    pub skill_name: String,
    pub enabled: bool,
}

/// POST /agents/{agent_id}/launchpad-skills/global
///
/// Adds/removes `skill_name` in `AgentProfile.enabled_launchpad_global_skills`
/// (explicit opt-in — absent/empty means none enabled) and persists via
/// `AgentProfileStore`, mirroring the fetch → mutate → `agents.update` →
/// `context_cache.invalidate` shape used by `patch_skill`/`delete_skill` above.
pub async fn set_launchpad_global_skill_enabled(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<SetLaunchpadGlobalSkillEnabledRequest>,
) -> Result<Json<LaunchpadGlobalSkillEnablementResponse>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let skill_name = req.skill_name.trim().to_string();
    if skill_name.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "skill_name must not be empty".to_string(),
        )));
    }

    let mut profile = agent;
    let mut enabled = profile.enabled_launchpad_global_skills.take().unwrap_or_default();
    if req.enabled {
        if !enabled.contains(&skill_name) {
            enabled.push(skill_name.clone());
        }
    } else {
        enabled.retain(|s| s != &skill_name);
    }
    profile.enabled_launchpad_global_skills = if enabled.is_empty() { None } else { Some(enabled) };

    state.persistence.agents.update(&profile).await?;
    state.context_cache.invalidate(&agent_id).await;

    Ok(Json(LaunchpadGlobalSkillEnablementResponse {
        skill_name,
        enabled: req.enabled,
    }))
}

#[derive(Debug, serde::Deserialize)]
pub struct SetLaunchpadProjectSkillEnabledRequest {
    pub project_key: String,
    pub skill_name: String,
    pub enabled: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct LaunchpadProjectSkillEnablementResponse {
    pub project_key: String,
    pub skill_name: String,
    pub enabled: bool,
}

/// POST /agents/{agent_id}/launchpad-skills/project
///
/// Adds/removes `skill_name` in
/// `AgentProfile.enabled_launchpad_project_skills[project_key]`. Disabling
/// the last enabled skill for a project drops the empty `Vec`, and disabling
/// a skill in a project with no entry at all is a no-op — neither leaves a
/// stray empty entry in the map.
pub async fn set_launchpad_project_skill_enabled(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<SetLaunchpadProjectSkillEnabledRequest>,
) -> Result<Json<LaunchpadProjectSkillEnablementResponse>, AppError> {
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let project_key = req.project_key.trim().to_string();
    if project_key.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "project_key must not be empty".to_string(),
        )));
    }
    let skill_name = req.skill_name.trim().to_string();
    if skill_name.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "skill_name must not be empty".to_string(),
        )));
    }

    let mut profile = agent;
    let mut enabled = profile
        .enabled_launchpad_project_skills
        .remove(&project_key)
        .unwrap_or_default();
    if req.enabled {
        if !enabled.contains(&skill_name) {
            enabled.push(skill_name.clone());
        }
    } else {
        enabled.retain(|s| s != &skill_name);
    }
    if !enabled.is_empty() {
        profile
            .enabled_launchpad_project_skills
            .insert(project_key.clone(), enabled);
    }

    state.persistence.agents.update(&profile).await?;
    state.context_cache.invalidate(&agent_id).await;

    Ok(Json(LaunchpadProjectSkillEnablementResponse {
        project_key,
        skill_name,
        enabled: req.enabled,
    }))
}

#[derive(Debug, serde::Deserialize)]
pub struct PromoteLaunchpadSkillRequest {
    pub focus_path: String,
    pub skill_name: String,
}

#[derive(Debug, serde::Serialize)]
pub struct PromoteLaunchpadSkillResponse {
    pub promoted: String,
}

/// POST /skills/launchpad/promote
///
/// Copies `<focus_path>/.launchpad/skills/<skill_name>/` into
/// `<data_root>/.launchpad/skills/<skill_name>/` ("Make available globally").
/// Refuse-and-report on name collision: if a
/// folder with that name already exists at the global root, returns 409
/// without overwriting it. Not agent-scoped — promotion affects the shared
/// global root, not any one agent's enablement.
pub async fn promote_launchpad_skill(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PromoteLaunchpadSkillRequest>,
) -> Result<Json<PromoteLaunchpadSkillResponse>, AppError> {
    let focus_path = req.focus_path.trim().to_string();
    if focus_path.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "focus_path must not be empty".to_string(),
        )));
    }
    let skill_name = req.skill_name.trim().to_string();
    if skill_name.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "skill_name must not be empty".to_string(),
        )));
    }

    let data_root_path = state.persistence.data_root.root().clone();
    let skill_name_for_copy = skill_name.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        promote_launchpad_skill_to_global(&data_root_path, &focus_path, &skill_name_for_copy)
    })
    .await
    .map_err(|e| AppError(AoError::Internal(format!("join error: {e}"))))?
    .map_err(import_io_error_to_app_error)?;

    match outcome {
        PromoteLaunchpadSkillOutcome::Promoted => {
            Ok(Json(PromoteLaunchpadSkillResponse { promoted: skill_name }))
        }
        PromoteLaunchpadSkillOutcome::AlreadyExistsGlobally => Err(AppError(AoError::Conflict(format!(
            "a global skill named '{skill_name}' already exists; refusing to overwrite"
        )))),
    }
}

#[cfg(test)]
mod skill_review_route_tests {
    use super::*;
    use ao_engine::reflection_subscriber::ProviderResolver;
    use ao_engine::skill_distillation::SkillDistiller;
    use ao_engine_tools_core::skill_registry::parse_frontmatter;
    use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
    use ao_protocol::outcome::ArtifactKind;
    use ao_protocol::reflection_candidate::{ReflectionCandidate, ReflectionCandidateStatus};
    use std::collections::HashMap;

    fn make_agent(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: "Test".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            enabled_plugins: HashMap::new(),
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
        }
    }

    async fn setup_state() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("Failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = PersistenceLayer::init();
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock).await.expect("AppState init")
        };
        (Arc::new(state), tmp)
    }

    /// Same as [`setup_state`], but with `skill_distiller` replaced by one
    /// driven by `provider` instead of the real `build_reflection_provider`
    /// seam — needed for the `promote` route, which is the only handler here
    /// that actually invokes a model.
    async fn setup_state_with_provider(provider: Arc<MockProviderClient>) -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("Failed to create temp dir");
        let mut state = {
            let _guard = crate::routes::env_lock::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = PersistenceLayer::init();
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock).await.expect("AppState init")
        };
        let resolver: ProviderResolver = Arc::new(move |_profile: &AgentProfile| {
            Some(provider.clone() as Arc<dyn ao_engine_tools_runner::provider::ProviderClient>)
        });
        state.skill_distiller =
            Arc::new(SkillDistiller::new(Arc::clone(&state.persistence), resolver));
        (Arc::new(state), tmp)
    }

    fn unwrap_ok<T>(r: Result<T, AppError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {:?}", e.0),
        }
    }

    fn turn(text: &str) -> Vec<CompletionEvent> {
        vec![
            CompletionEvent::AssistantText(text.to_string()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ]
    }

    fn seed_parked_skill(data_dir: &std::path::Path, name: &str, description: &str, body: &str, distilled_from: &[&str]) {
        let sources = distilled_from
            .iter()
            .map(|id| format!("  - {id}\n"))
            .collect::<String>();
        let content = format!(
            "---\nname: {name}\ndescription: {description}\ndisable-model-invocation: true\norigin: distilled\ndistilled-from:\n{sources}---\n{body}\n"
        );
        let skill_dir = data_dir.join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    fn skill_observation(id: &str, agent_id: &str, content: &str) -> ReflectionCandidate {
        ReflectionCandidate {
            id: id.to_string(),
            kind: ArtifactKind::Skill,
            agent_id: agent_id.to_string(),
            source_thread_id: "thread-1".to_string(),
            content: content.to_string(),
            status: ReflectionCandidateStatus::Pending,
            target_scope: ao_protocol::memory::MemoryScope::Agent,
            target_scope_key: Some(agent_id.to_string()),
            contradicts: None,
            reason: "test".to_string(),
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn skill_review_full_round_trip() {
        let (state, tmp) = setup_state().await;

        let mut agent = make_agent("agent-1");
        seed_parked_skill(tmp.path(), "parked-accept-me", "desc one", "body one", &["cand-a"]);
        seed_parked_skill(tmp.path(), "parked-reject-me", "desc two", "body two", &["cand-b"]);
        agent.skills = vec!["parked-accept-me".to_string(), "parked-reject-me".to_string()];
        state.persistence.agents.create(&agent).await.unwrap();

        state
            .persistence
            .reflection_staging
            .stage("agent-1", &skill_observation("obs-1", "agent-1", "an observed procedure"))
            .await
            .unwrap();

        // GET review lists both parked skills plus the pending observation.
        let Json(queue) =
            unwrap_ok(list_skill_review_queue(State(Arc::clone(&state)), Path("agent-1".to_string())).await);
        assert_eq!(queue.candidates.len(), 2);
        assert!(queue.candidates.iter().any(|c| c.name == "parked-accept-me" && c.origin == "distilled"));
        assert!(queue.candidates.iter().any(|c| c.name == "parked-reject-me"));
        assert_eq!(queue.observations.len(), 1);
        assert_eq!(queue.observations[0].id, "obs-1");

        // POST accept flips disable-model-invocation to false.
        let Json(accept_result) = unwrap_ok(
            act_on_skill_review_candidate(
                State(Arc::clone(&state)),
                Path(("agent-1".to_string(), "parked-accept-me".to_string())),
                Json(SkillReviewActionRequest {
                    action: SkillReviewAction::Accept,
                    body: None,
                    description: None,
                    keep_parked: None,
                }),
            )
            .await,
        );
        assert_eq!(accept_result["live"], serde_json::json!(true));
        let accepted = std::fs::read_to_string(tmp.path().join("skills/parked-accept-me/SKILL.md")).unwrap();
        assert!(!parse_frontmatter(&accepted).unwrap().disable_model_invocation);

        // POST reject deletes the parked skill file and drops it from the
        // agent's profile.skills.
        let _ = unwrap_ok(
            act_on_skill_review_candidate(
                State(Arc::clone(&state)),
                Path(("agent-1".to_string(), "parked-reject-me".to_string())),
                Json(SkillReviewActionRequest {
                    action: SkillReviewAction::Reject,
                    body: None,
                    description: None,
                    keep_parked: None,
                }),
            )
            .await,
        );
        assert!(!tmp.path().join("skills/parked-reject-me").exists());
        let updated_agent = state.persistence.agents.get("agent-1").await.unwrap().unwrap();
        assert!(!updated_agent.skills.contains(&"parked-reject-me".to_string()));
        assert!(updated_agent.skills.contains(&"parked-accept-me".to_string()));
    }

    #[tokio::test]
    async fn list_skill_review_queue_unknown_agent_returns_404() {
        let (state, _tmp) = setup_state().await;
        let err = list_skill_review_queue(State(Arc::clone(&state)), Path("ghost".to_string()))
            .await
            .expect_err("unknown agent should fail");
        assert!(matches!(err.0, AoError::AgentNotFound(_)));
    }

    #[tokio::test]
    async fn act_edit_can_rewrite_body_and_stay_parked() {
        let (state, tmp) = setup_state().await;
        let mut agent = make_agent("agent-1");
        seed_parked_skill(tmp.path(), "parked-one", "desc", "old body", &["cand-a"]);
        agent.skills = vec!["parked-one".to_string()];
        state.persistence.agents.create(&agent).await.unwrap();

        let Json(result) = unwrap_ok(
            act_on_skill_review_candidate(
                State(Arc::clone(&state)),
                Path(("agent-1".to_string(), "parked-one".to_string())),
                Json(SkillReviewActionRequest {
                    action: SkillReviewAction::Edit,
                    body: Some("new body".to_string()),
                    description: Some("new desc".to_string()),
                    keep_parked: Some(true),
                }),
            )
            .await,
        );
        assert_eq!(result["live"], serde_json::json!(false));

        let after = std::fs::read_to_string(tmp.path().join("skills/parked-one/SKILL.md")).unwrap();
        let parsed = parse_frontmatter(&after).unwrap();
        assert_eq!(parsed.body, "new body");
        assert_eq!(parsed.description, "new desc");
        assert!(parsed.disable_model_invocation, "keep_parked:true must leave the skill parked");
    }

    #[tokio::test]
    async fn promote_skill_observation_produces_a_parked_skill_and_marks_observation_distilled() {
        let provider = Arc::new(MockProviderClient::new(vec![turn(
            r#"{"name":"promoted-skill","description":"A promoted skill.","body":"do the promoted thing"}"#,
        )]));
        let (state, _tmp) = setup_state_with_provider(provider).await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();

        state
            .persistence
            .reflection_staging
            .stage("agent-1", &skill_observation("obs-1", "agent-1", "an observed procedure"))
            .await
            .unwrap();

        let Json(candidate) = unwrap_ok(
            promote_skill_observation(
                State(Arc::clone(&state)),
                Path("agent-1".to_string()),
                Json(PromoteSkillObservationRequest { candidate_id: "obs-1".to_string() }),
            )
            .await,
        );

        assert_eq!(candidate.origin, "distilled");
        assert_eq!(candidate.distilled_from, vec!["obs-1".to_string()]);
        assert!(!candidate.name.is_empty());

        let data_dir = state.persistence.data_root.root();
        let content =
            std::fs::read_to_string(data_dir.join("skills").join(&candidate.name).join("SKILL.md")).unwrap();
        let parsed = parse_frontmatter(&content).unwrap();
        assert!(parsed.disable_model_invocation, "a promoted skill must still be parked pending review");

        let all = state.persistence.reflection_staging.read_all("agent-1").await.unwrap();
        let updated = all.iter().find(|c| c.id == "obs-1").unwrap();
        assert_eq!(updated.status, ReflectionCandidateStatus::Distilled);
    }

    #[tokio::test]
    async fn promote_skill_observation_errors_when_candidate_missing() {
        let provider = Arc::new(MockProviderClient::new(vec![]));
        let (state, _tmp) = setup_state_with_provider(provider).await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();

        let err = promote_skill_observation(
            State(Arc::clone(&state)),
            Path("agent-1".to_string()),
            Json(PromoteSkillObservationRequest { candidate_id: "ghost".to_string() }),
        )
        .await
        .expect_err("missing candidate should fail");
        assert!(matches!(err.0, AoError::MemoryNotFound(_)));
    }
}
