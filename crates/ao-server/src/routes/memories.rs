use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use ao_engine::AppState;
use ao_engine_tools_engine::memory::promotion_budget::{self, ReviewDecision};
use ao_engine_tools_engine::memory::review;
use ao_persistence::{hash_project_key, resolve_project_key};
use ao_protocol::error::AoError;
use ao_protocol::memory::{MemoryEntry, MemoryScope, MemorySource};
use ao_protocol::outcome::ArtifactKind;
use ao_protocol::reflection_candidate::ReflectionCandidate;

use crate::error::AppError;

#[derive(serde::Deserialize)]
pub struct AddMemoryRequest {
    pub content: String,
}

/// GET /agents/{agent_id}/memories — list all memories for an agent.
pub async fn list_agent_memories(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<MemoryEntry>>, AppError> {
    // Validate agent exists
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let entries = state.persistence.memory.list(&agent_id).await?;
    Ok(Json(entries))
}

/// POST /agents/{agent_id}/memories — add a new memory for an agent.
pub async fn add_agent_memory(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<AddMemoryRequest>,
) -> Result<Json<MemoryEntry>, AppError> {
    // Validate agent exists
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let entry = state
        .persistence
        .memory
        .add(&agent_id, &req.content, MemorySource::Manual)
        .await?;
    Ok(Json(entry))
}

/// DELETE /agents/{agent_id}/memories/{memory_id} — delete a specific agent memory.
pub async fn delete_agent_memory(
    State(state): State<Arc<AppState>>,
    Path((agent_id, memory_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    // Validate agent exists
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let deleted = state
        .persistence
        .memory
        .delete(&agent_id, &memory_id)
        .await?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError(AoError::ThreadNotFound(format!(
            "Memory {} not found",
            memory_id
        ))))
    }
}

/// Per-agent rollup for the Settings → Memories agent list column: how many
/// candidates are held for review, and when this agent last made a
/// self-authored memory live. Mirrors exactly what the frontend previously
/// computed client-side from `list_review_queue` + `list_agent_memories`.
#[derive(serde::Serialize)]
pub struct AgentMemorySummary {
    pub pending: usize,
    /// `created_at` of the agent's most recent live entry with
    /// `source == Agent` (self-authored, not a user's own Manual note) --
    /// `None` if it has none.
    pub last_memory_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// GET /agents/memories/summary — every agent's review-queue count + most
/// recent self-authored memory timestamp, in one round trip.
///
/// Exists because the naive approach (one `list_review_queue` + one
/// `list_agent_memories` request per agent, fired from the browser) hits
/// browsers' ~6-connections-per-origin cap on HTTP/1.1 once the agent count
/// reaches dozens — requests queue client-side and the list's badges/sort
/// trickle in over several rounds instead of loading at once. This
/// recombines the same two per-agent reads server-side, fanned out
/// concurrently with `join_all` (both stores are `Arc`-wrapped, so this is
/// cheap), so the browser only pays for one request no matter how many
/// agents exist.
pub async fn get_agent_memory_summaries(
    State(state): State<Arc<AppState>>,
) -> Result<Json<std::collections::HashMap<String, AgentMemorySummary>>, AppError> {
    let agents = state.persistence.agents.list().await.unwrap_or_default();

    let summaries = futures_util::future::join_all(agents.into_iter().map(|agent| {
        let state = Arc::clone(&state);
        async move {
            let pending = state
                .persistence
                .reflection_staging
                .list_pending(&agent.id)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|c| c.kind == ArtifactKind::Memory)
                .count();
            let last_memory_at = state
                .persistence
                .memory
                .list(&agent.id)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| matches!(entry.source, Some(MemorySource::Agent)))
                .map(|entry| entry.created_at)
                .max();
            (
                agent.id,
                AgentMemorySummary {
                    pending,
                    last_memory_at,
                },
            )
        }
    }))
    .await
    .into_iter()
    .collect();

    Ok(Json(summaries))
}

/// GET /memories/global — list all global memories.
pub async fn list_global_memories(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<MemoryEntry>>, AppError> {
    let entries = state.persistence.memory.list_global().await?;
    Ok(Json(entries))
}

/// POST /memories/global — add a new global memory.
pub async fn add_global_memory(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddMemoryRequest>,
) -> Result<Json<MemoryEntry>, AppError> {
    let entry = state
        .persistence
        .memory
        .add_global(&req.content, MemorySource::Manual)
        .await?;
    Ok(Json(entry))
}

/// DELETE /memories/global/{memory_id} — delete a specific global memory.
pub async fn delete_global_memory(
    State(state): State<Arc<AppState>>,
    Path(memory_id): Path<String>,
) -> Result<StatusCode, AppError> {
    let deleted = state
        .persistence
        .memory
        .delete_global(&memory_id)
        .await?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError(AoError::ThreadNotFound(format!(
            "Memory {} not found",
            memory_id
        ))))
    }
}

/// Resolve the project hash for an agent by inspecting its `working_dir`,
/// falling back to the server's current directory when unset.
///
/// This mirrors how the agent's own memory tools resolve project scope
/// (`resolve_project_key` + `hash_project_key`), so the panel surfaces exactly
/// the entries the agent reads and writes for the repo it operates in.
async fn resolve_agent_project_hash(
    state: &Arc<AppState>,
    agent_id: &str,
) -> Result<String, AppError> {
    let profile = state
        .persistence
        .agents
        .get(agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.to_string()))?;

    let cwd: PathBuf = profile
        .working_dir
        .filter(|dir| !dir.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let canonical_key = resolve_project_key(&cwd).await?;
    Ok(hash_project_key(&canonical_key))
}

/// GET /agents/{agent_id}/memories/project — list project-scoped memories
/// for the repo the agent operates in.
pub async fn list_project_memories(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<MemoryEntry>>, AppError> {
    let hash = resolve_agent_project_hash(&state, &agent_id).await?;
    let entries = state.persistence.memory.list_project(&hash).await?;
    Ok(Json(entries))
}

/// POST /agents/{agent_id}/memories/project — add a project-scoped memory.
pub async fn add_project_memory(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<AddMemoryRequest>,
) -> Result<Json<MemoryEntry>, AppError> {
    let hash = resolve_agent_project_hash(&state, &agent_id).await?;
    let op = state
        .persistence
        .memory
        .add_project(&hash, &req.content, MemorySource::Manual)
        .await?;

    // `add_project` returns an op-result (id + dedup flag); re-read the scope so
    // we can hand back the full entry, matching the agent/global add shape.
    let entry = state
        .persistence
        .memory
        .list_project(&hash)
        .await?
        .into_iter()
        .find(|entry| entry.id == op.id)
        .ok_or_else(|| AoError::Internal("added project memory not found".to_string()))?;
    Ok(Json(entry))
}

/// DELETE /agents/{agent_id}/memories/project/{memory_id} — delete a project memory.
pub async fn delete_project_memory(
    State(state): State<Arc<AppState>>,
    Path((agent_id, memory_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let hash = resolve_agent_project_hash(&state, &agent_id).await?;
    state
        .persistence
        .memory
        .delete_project(&hash, &memory_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /memories/thread/{thread_id} — list all live memories for a thread.
pub async fn list_thread_memories(
    State(state): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
) -> Result<Json<Vec<MemoryEntry>>, AppError> {
    // Validate thread exists
    state
        .persistence
        .threads
        .get(&thread_id)
        .await?
        .ok_or_else(|| AoError::ThreadNotFound(thread_id.clone()))?;

    let entries = state.persistence.memory.list_thread(&thread_id).await?;
    Ok(Json(entries))
}

/// POST /memories/thread/{thread_id} — add a user-authored memory for a
/// thread. Always stamped `MemorySource::Manual`, same as the agent/global/
/// project add routes above, since this is a human typing into the UI —
/// never the agent's own reflection-derived writes.
pub async fn add_thread_memory(
    State(state): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
    Json(req): Json<AddMemoryRequest>,
) -> Result<Json<MemoryEntry>, AppError> {
    // Validate thread exists
    state
        .persistence
        .threads
        .get(&thread_id)
        .await?
        .ok_or_else(|| AoError::ThreadNotFound(thread_id.clone()))?;

    let entry = state
        .persistence
        .memory
        .add_thread(&thread_id, &req.content, MemorySource::Manual)
        .await?;
    Ok(Json(entry))
}

/// DELETE /memories/thread/{thread_id}/{memory_id} — delete a specific
/// thread memory.
pub async fn delete_thread_memory(
    State(state): State<Arc<AppState>>,
    Path((thread_id, memory_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    // Validate thread exists
    state
        .persistence
        .threads
        .get(&thread_id)
        .await?
        .ok_or_else(|| AoError::ThreadNotFound(thread_id.clone()))?;

    let deleted = state
        .persistence
        .memory
        .delete_thread(&thread_id, &memory_id)
        .await?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError(AoError::ThreadNotFound(format!(
            "Memory {} not found",
            memory_id
        ))))
    }
}

// --- Review/undo staging queue ---
//
// Headless-first: the queue's contents are exactly the set the trust
// gate marks `StageForReview` (never `NeverAuto` — see `memory::write`'s
// staging call site). These routes are the "read/act" surface; a Settings → Memories panel renders + drives them as a fast-
// follow, not part of this cut.

async fn require_agent_exists(state: &Arc<AppState>, agent_id: &str) -> Result<(), AppError> {
    state
        .persistence
        .agents
        .get(agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.to_string()))?;
    Ok(())
}

/// GET /agents/{agent_id}/memories/review — list this agent's pending
/// staged candidates (the review queue's read path).
///
/// Filtered to `ArtifactKind::Memory` only: a staged `Skill` candidate has
/// its own review surface (`crate::routes::skills::list_skill_review_queue`,
/// under "Promote an observation") and `memory::review`'s `keep`/`edit`/
/// `forget`/`pin` actions already reject a non-`Memory` candidate outright
/// (see that module's doc) — so returning one here would show it in this
/// queue with buttons that error the moment they're clicked.
pub async fn list_review_queue(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<ReflectionCandidate>>, AppError> {
    require_agent_exists(&state, &agent_id).await?;
    let pending: Vec<ReflectionCandidate> = state
        .persistence
        .reflection_staging
        .list_pending(&agent_id)
        .await?
        .into_iter()
        .filter(|c| c.kind == ArtifactKind::Memory)
        .collect();
    Ok(Json(pending))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAction {
    Keep,
    Edit,
    Forget,
    Pin,
}

#[derive(serde::Deserialize)]
pub struct ReviewActionRequest {
    pub action: ReviewAction,
    /// Required (and must be non-empty) when `action == "edit"`; ignored
    /// otherwise.
    #[serde(default)]
    pub content: Option<String>,
}

/// POST /agents/{agent_id}/memories/review/{candidate_id} — act on one
/// staged candidate: `keep`, `edit` (body must include `content`), `forget`,
/// or `pin`.
pub async fn act_on_review_candidate(
    State(state): State<Arc<AppState>>,
    Path((agent_id, candidate_id)): Path<(String, String)>,
    Json(req): Json<ReviewActionRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_agent_exists(&state, &agent_id).await?;

    let memory = &state.persistence.memory;
    let staging = &state.persistence.reflection_staging;
    let outcome_store = &state.persistence.outcome;

    // The human staging-gate decision made right here is the
    // ONLY ground truth that feeds the promotion judge's acceptance-rate
    // budget — never the judge's own confidence. `keep`/`edit`/`pin` all
    // count as `Accepted`; `forget` as `Rejected`. See
    // `ao_engine_tools_engine::memory::promotion_budget` for the controller
    // that consumes these.
    let result = match req.action {
        ReviewAction::Keep => {
            let outcome = review::keep(memory, staging, &agent_id, &candidate_id).await?;
            promotion_budget::record_review_decision(
                outcome_store,
                &agent_id,
                &candidate_id,
                ReviewDecision::Accepted,
            )
            .await?;
            serde_json::to_value(outcome).map_err(|e| AoError::Json(e.to_string()))?
        }
        ReviewAction::Edit => {
            let content = req
                .content
                .filter(|c| !c.trim().is_empty())
                .ok_or_else(|| AoError::ValidationError("edit requires non-empty content".to_string()))?;
            let outcome = review::edit(memory, staging, &agent_id, &candidate_id, &content).await?;
            promotion_budget::record_review_decision(
                outcome_store,
                &agent_id,
                &candidate_id,
                ReviewDecision::Accepted,
            )
            .await?;
            serde_json::to_value(outcome).map_err(|e| AoError::Json(e.to_string()))?
        }
        ReviewAction::Forget => {
            review::forget(staging, &agent_id, &candidate_id).await?;
            promotion_budget::record_review_decision(
                outcome_store,
                &agent_id,
                &candidate_id,
                ReviewDecision::Rejected,
            )
            .await?;
            serde_json::json!({ "candidate_id": candidate_id, "rejected": true })
        }
        ReviewAction::Pin => {
            let outcome = review::pin(memory, staging, &agent_id, &candidate_id).await?;
            promotion_budget::record_review_decision(
                outcome_store,
                &agent_id,
                &candidate_id,
                ReviewDecision::Accepted,
            )
            .await?;
            serde_json::to_value(outcome).map_err(|e| AoError::Json(e.to_string()))?
        }
    };
    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct UndoRequest {
    pub scope: MemoryScope,
    /// Storage key for `scope`: the project hash for `Project`, unused for
    /// `Global`. Defaults to the path's `agent_id` for `Agent` scope when
    /// omitted, since that is the only valid key for that scope anyway.
    #[serde(default)]
    pub scope_key: Option<String>,
    pub memory_id: String,
}

/// POST /agents/{agent_id}/memories/undo — reverse a live agent-authored
/// write by the id of the entry it produced. Works whether that entry went
/// live via the trust gate's `AutoConfirm` tier or via a `keep`/`edit`/
/// `pin` review action (see `review::undo`'s doc for why both reduce to the
/// same reversal).
pub async fn undo_memory_write(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<UndoRequest>,
) -> Result<Json<review::UndoOutcome>, AppError> {
    require_agent_exists(&state, &agent_id).await?;

    let scope_key = req.scope_key.or_else(|| {
        matches!(req.scope, MemoryScope::Agent).then(|| agent_id.clone())
    });
    let outcome = review::undo(
        &state.persistence.memory,
        &req.scope,
        scope_key.as_deref(),
        &req.memory_id,
    )
    .await?;
    Ok(Json(outcome))
}

#[cfg(test)]
mod review_route_tests {
    use super::*;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
    use ao_protocol::memory::MemorySource;
    use ao_protocol::reflection_candidate::ReflectionCandidateStatus;
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

    fn unwrap_ok<T>(r: Result<T, AppError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {:?}", e.0),
        }
    }

    /// GET /agents/{agent_id}/memories/review must return exactly the
    /// `StageForReview` set.
    #[tokio::test]
    async fn list_review_queue_returns_the_staged_set() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();

        let empty = unwrap_ok(
            list_review_queue(State(Arc::clone(&state)), Path("agent-1".to_string())).await,
        );
        assert!(empty.0.is_empty());

        let candidate = ReflectionCandidate {
            id: "cand-1".to_string(),
            kind: ao_protocol::outcome::ArtifactKind::Memory,
            agent_id: "agent-1".to_string(),
            source_thread_id: "session-1".to_string(),
            content: "prefer async/await".to_string(),
            status: ReflectionCandidateStatus::Pending,
            target_scope: MemoryScope::Agent,
            target_scope_key: Some("agent-1".to_string()),
            contradicts: None,
            reason: "test".to_string(),
            created_at: chrono::Utc::now(),
        };
        state.persistence.reflection_staging.stage("agent-1", &candidate).await.unwrap();

        let Json(pending) =
            unwrap_ok(list_review_queue(State(Arc::clone(&state)), Path("agent-1".to_string())).await);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "cand-1");
    }

    /// A pending `Skill`-kind candidate belongs to the skill-side review
    /// surface (`routes::skills::list_skill_review_queue`, under "Promote an
    /// observation") — it must never show up here too. Guards against the
    /// bug where this queue rendered every pending candidate regardless of
    /// kind, surfacing skill observations in "Held for review" with keep/
    /// edit/pin/forget buttons that `memory::review` rejects outright since
    /// they aren't `ArtifactKind::Memory`.
    #[tokio::test]
    async fn list_review_queue_excludes_skill_kind_candidates() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();

        let memory_candidate = ReflectionCandidate {
            id: "cand-mem".to_string(),
            kind: ao_protocol::outcome::ArtifactKind::Memory,
            agent_id: "agent-1".to_string(),
            source_thread_id: "session-1".to_string(),
            content: "prefer async/await".to_string(),
            status: ReflectionCandidateStatus::Pending,
            target_scope: MemoryScope::Agent,
            target_scope_key: Some("agent-1".to_string()),
            contradicts: None,
            reason: "test".to_string(),
            created_at: chrono::Utc::now(),
        };
        let skill_candidate = ReflectionCandidate {
            id: "cand-skill".to_string(),
            kind: ao_protocol::outcome::ArtifactKind::Skill,
            agent_id: "agent-1".to_string(),
            source_thread_id: "session-1".to_string(),
            content: "observed repeated grep-then-edit pattern".to_string(),
            status: ReflectionCandidateStatus::Pending,
            target_scope: MemoryScope::Agent,
            target_scope_key: Some("agent-1".to_string()),
            contradicts: None,
            reason: "test".to_string(),
            created_at: chrono::Utc::now(),
        };
        state.persistence.reflection_staging.stage("agent-1", &memory_candidate).await.unwrap();
        state.persistence.reflection_staging.stage("agent-1", &skill_candidate).await.unwrap();

        let Json(pending) =
            unwrap_ok(list_review_queue(State(Arc::clone(&state)), Path("agent-1".to_string())).await);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "cand-mem");
    }

    #[tokio::test]
    async fn list_review_queue_unknown_agent_returns_404() {
        let (state, _tmp) = setup_state().await;
        let err = list_review_queue(State(Arc::clone(&state)), Path("ghost".to_string()))
            .await
            .expect_err("unknown agent should fail");
        assert!(matches!(err.0, AoError::AgentNotFound(_)));
    }

    async fn stage_one(state: &Arc<AppState>, agent_id: &str, content: &str) -> String {
        let candidate = ReflectionCandidate {
            id: uuid::Uuid::new_v4().to_string(),
            kind: ao_protocol::outcome::ArtifactKind::Memory,
            agent_id: agent_id.to_string(),
            source_thread_id: "session-1".to_string(),
            content: content.to_string(),
            status: ReflectionCandidateStatus::Pending,
            target_scope: MemoryScope::Agent,
            target_scope_key: Some(agent_id.to_string()),
            contradicts: None,
            reason: "test".to_string(),
            created_at: chrono::Utc::now(),
        };
        state.persistence.reflection_staging.stage(agent_id, &candidate).await.unwrap();
        candidate.id
    }

    #[tokio::test]
    async fn act_keep_writes_the_entry_live_and_clears_the_queue() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();
        let candidate_id = stage_one(&state, "agent-1", "prefer async/await").await;

        let Json(result) = unwrap_ok(
            act_on_review_candidate(
                State(Arc::clone(&state)),
                Path(("agent-1".to_string(), candidate_id)),
                Json(ReviewActionRequest { action: ReviewAction::Keep, content: None }),
            )
            .await,
        );
        assert!(result.get("memory_id").is_some());

        let entries = state.persistence.memory.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "prefer async/await");

        let pending = state.persistence.reflection_staging.list_pending("agent-1").await.unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn act_edit_requires_content() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();
        let candidate_id = stage_one(&state, "agent-1", "original").await;

        let err = act_on_review_candidate(
            State(Arc::clone(&state)),
            Path(("agent-1".to_string(), candidate_id)),
            Json(ReviewActionRequest { action: ReviewAction::Edit, content: None }),
        )
        .await
        .expect_err("edit with no content must fail");
        assert!(matches!(err.0, AoError::ValidationError(_)));
    }

    #[tokio::test]
    async fn act_forget_writes_nothing() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();
        let candidate_id = stage_one(&state, "agent-1", "content").await;

        let _ = unwrap_ok(
            act_on_review_candidate(
                State(Arc::clone(&state)),
                Path(("agent-1".to_string(), candidate_id)),
                Json(ReviewActionRequest { action: ReviewAction::Forget, content: None }),
            )
            .await,
        );

        assert!(state.persistence.memory.list("agent-1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn act_pin_sets_the_pinned_flag() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();
        let candidate_id = stage_one(&state, "agent-1", "important fact").await;

        let _ = unwrap_ok(
            act_on_review_candidate(
                State(Arc::clone(&state)),
                Path(("agent-1".to_string(), candidate_id)),
                Json(ReviewActionRequest { action: ReviewAction::Pin, content: None }),
            )
            .await,
        );

        let entries = state.persistence.memory.list("agent-1").await.unwrap();
        assert!(entries[0].pinned);
    }

    /// GET /agents/memories/summary must roll up every agent's pending count
    /// and most recent self-authored memory in one call, matching exactly
    /// what per-agent `list_review_queue` + `list_agent_memories` would show.
    #[tokio::test]
    async fn get_agent_memory_summaries_rolls_up_every_agent() {
        let (state, _tmp) = setup_state().await;
        state.persistence.agents.create(&make_agent("agent-1")).await.unwrap();
        state.persistence.agents.create(&make_agent("agent-2")).await.unwrap();

        // agent-1: one pending memory candidate, one live self-authored
        // memory, plus a pending Skill-kind candidate that must NOT count
        // toward `pending` — that count feeds the sidebar's "N awaiting
        // review" badge for the memory-side queue, and a Skill candidate is
        // reviewed on its own surface (see `list_review_queue`'s doc).
        let _ = stage_one(&state, "agent-1", "pending thing").await;
        let skill_candidate = ReflectionCandidate {
            id: "cand-skill".to_string(),
            kind: ao_protocol::outcome::ArtifactKind::Skill,
            agent_id: "agent-1".to_string(),
            source_thread_id: "session-1".to_string(),
            content: "observed repeated pattern".to_string(),
            status: ReflectionCandidateStatus::Pending,
            target_scope: MemoryScope::Agent,
            target_scope_key: Some("agent-1".to_string()),
            contradicts: None,
            reason: "test".to_string(),
            created_at: chrono::Utc::now(),
        };
        state.persistence.reflection_staging.stage("agent-1", &skill_candidate).await.unwrap();
        let learned = state
            .persistence
            .memory
            .add("agent-1", "self-authored fact", MemorySource::Agent)
            .await
            .unwrap();
        // A user's own Manual note must NOT count toward `last_memory_at`.
        state
            .persistence
            .memory
            .add("agent-1", "user's own note", MemorySource::Manual)
            .await
            .unwrap();

        // agent-2: nothing at all.

        let Json(summaries) =
            unwrap_ok(get_agent_memory_summaries(State(Arc::clone(&state))).await);

        assert_eq!(summaries.len(), 2);
        let a1 = &summaries["agent-1"];
        assert_eq!(a1.pending, 1);
        assert_eq!(a1.last_memory_at, Some(learned.created_at));

        let a2 = &summaries["agent-2"];
        assert_eq!(a2.pending, 0);
        assert_eq!(a2.last_memory_at, None);
    }

    /// The route-level proof of the acceptance bar: `undo` reverses a write
    /// that went live via the trust gate's `AutoConfirm` tier (never staged
    /// at all), not just one applied through this review queue.
    #[tokio::test]
    async fn undo_route_reverses_an_autoconfirmed_write() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();
        let entry = state
            .persistence
            .memory
            .add("agent-1", "auto-confirmed fact", MemorySource::Agent)
            .await
            .unwrap();

        let Json(outcome) = unwrap_ok(
            undo_memory_write(
                State(Arc::clone(&state)),
                Path("agent-1".to_string()),
                Json(UndoRequest { scope: MemoryScope::Agent, scope_key: None, memory_id: entry.id.clone() }),
            )
            .await,
        );
        assert_eq!(outcome.memory_id, entry.id);

        assert!(state.persistence.memory.list("agent-1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn undo_route_unknown_entry_returns_404() {
        let (state, _tmp) = setup_state().await;
        let agent = make_agent("agent-1");
        state.persistence.agents.create(&agent).await.unwrap();

        let err = undo_memory_write(
            State(Arc::clone(&state)),
            Path("agent-1".to_string()),
            Json(UndoRequest {
                scope: MemoryScope::Agent,
                scope_key: None,
                memory_id: "nonexistent".to_string(),
            }),
        )
        .await
        .expect_err("undoing an unknown entry must fail");
        assert!(matches!(err.0, AoError::MemoryNotFound(_)));
    }
}

#[cfg(test)]
mod thread_memory_route_tests {
    use super::*;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::thread::Thread;

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

    fn unwrap_ok<T>(r: Result<T, AppError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {:?}", e.0),
        }
    }

    async fn make_thread(state: &Arc<AppState>) -> Thread {
        let row = state.persistence.threads.build_fresh_thread("agent-1", None);
        state.persistence.threads.create(row).await.unwrap()
    }

    /// POST then GET must round-trip: the added entry shows up in the list
    /// response, stamped `Manual` (user-authored, not reflection-derived).
    #[tokio::test]
    async fn add_then_list_roundtrips() {
        let (state, _tmp) = setup_state().await;
        let thread = make_thread(&state).await;

        let Json(added) = unwrap_ok(
            add_thread_memory(
                State(Arc::clone(&state)),
                Path(thread.id.clone()),
                Json(AddMemoryRequest { content: "keep an eye on the flaky test".to_string() }),
            )
            .await,
        );
        assert_eq!(added.content, "keep an eye on the flaky test");
        assert_eq!(added.scope, MemoryScope::Thread);
        assert_eq!(added.scope_key, Some(thread.id.clone()));
        assert_eq!(added.source, Some(MemorySource::Manual));

        let Json(listed) =
            unwrap_ok(list_thread_memories(State(Arc::clone(&state)), Path(thread.id.clone())).await);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, added.id);
    }

    /// Deleting one entry removes only that entry; a second delete of the
    /// same id reports the same not-found shape the agent-scope route uses.
    #[tokio::test]
    async fn delete_one_removes_only_that_entry() {
        let (state, _tmp) = setup_state().await;
        let thread = make_thread(&state).await;

        let Json(first) = unwrap_ok(
            add_thread_memory(
                State(Arc::clone(&state)),
                Path(thread.id.clone()),
                Json(AddMemoryRequest { content: "first note".to_string() }),
            )
            .await,
        );
        let Json(_second) = unwrap_ok(
            add_thread_memory(
                State(Arc::clone(&state)),
                Path(thread.id.clone()),
                Json(AddMemoryRequest { content: "second note".to_string() }),
            )
            .await,
        );

        let status = unwrap_ok(
            delete_thread_memory(
                State(Arc::clone(&state)),
                Path((thread.id.clone(), first.id.clone())),
            )
            .await,
        );
        assert_eq!(status, StatusCode::NO_CONTENT);

        let Json(listed) =
            unwrap_ok(list_thread_memories(State(Arc::clone(&state)), Path(thread.id.clone())).await);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].content, "second note");

        let err = delete_thread_memory(
            State(Arc::clone(&state)),
            Path((thread.id.clone(), first.id)),
        )
        .await
        .expect_err("deleting an already-deleted entry must fail");
        assert!(matches!(err.0, AoError::ThreadNotFound(_)));
    }

    #[tokio::test]
    async fn list_unknown_thread_returns_404() {
        let (state, _tmp) = setup_state().await;
        let err = list_thread_memories(State(Arc::clone(&state)), Path("ghost-thread".to_string()))
            .await
            .expect_err("unknown thread should fail");
        assert!(matches!(err.0, AoError::ThreadNotFound(_)));
    }
}
