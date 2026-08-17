use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;

use ao_engine::context_cache::{CachedContext, ContextCacheKey};
use ao_engine::system_prompt_composer::loader::{load_agent_home_context, load_workspace_context};
use ao_engine::AppState;
use ao_protocol::error::AoError;

use crate::error::AppError;

#[derive(Debug, serde::Deserialize)]
pub struct PrecomputeContextRequest {
    #[serde(default)]
    pub focus_path: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct PrecomputeContextResponse {
    pub status: String,
}

/// POST /agents/{agent_id}/precompute-context
///
/// Fire-and-forget endpoint that eagerly precomputes and caches agent context
/// (skills, rules, instruction files) so the first message has zero
/// context-assembly latency.
pub async fn precompute_context(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<PrecomputeContextRequest>,
) -> Result<Json<PrecomputeContextResponse>, AppError> {
    // Validate agent exists
    let agent = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    // Resolve effective_cwd: focus_path > agent.working_dir > home dir
    let effective_cwd = req
        .focus_path
        .or_else(|| agent.working_dir.clone())
        .unwrap_or_else(|| {
            dirs::home_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".".to_string())
        });

    let agent_home = agent.home_dir.as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| state.persistence.data_root.agent_home_dir(&agent_id));
    let context_cache = state.context_cache.clone();
    let persistence = state.persistence.clone();

    // Spawn async task for precomputation — fire-and-forget
    tokio::spawn(async move {
        let cache_key = ContextCacheKey {
            agent_id: agent_id.clone(),
            effective_cwd: std::path::PathBuf::from(&effective_cwd),
            agent_home: agent_home.clone(),
        };

        // Stat the agent profile file so cache lookups can detect whether the
        // profile was modified (e.g. a skill toggle) since the entry was stored.
        let profile_path = persistence.data_root.agents_dir()
            .join(format!("{}.yaml", agent_id));
        let profile_mtime = tokio::fs::metadata(&profile_path)
            .await
            .and_then(|m| m.modified())
            .ok();

        // Skip if already cached, not expired, and profile unchanged.
        if let Some(existing) = context_cache.get(&cache_key, profile_mtime).await {
            tracing::info!(
                agent_id = %agent_id,
                effective_cwd = %effective_cwd,
                agent_home = %agent_home.display(),
                cached_skill_count = existing.agent_home_context.skills.len(),
                cached_rule_count = existing.agent_home_context.rules.len(),
                "Precompute skipped: context already cached"
            );
            return;
        }

        // Ensure agent home exists
        if let Err(e) = ao_protocol::agent_home::ensure_agent_home(&agent_home).await {
            tracing::warn!(
                "Failed to scaffold agent home for {} during precompute: {}",
                agent_id,
                e
            );
        }

        // Compute context from disk using the canonical loader helpers.
        let agent_home_context = load_agent_home_context(&agent_home).await;
        let workspace_context =
            load_workspace_context(std::path::Path::new(&effective_cwd)).await;

        let skill_count = agent_home_context.skills.len();
        let rule_count = agent_home_context.rules.len();

        // Store in cache, recording the profile mtime so future lookups can
        // detect when the profile changes without a full TTL cycle.
        context_cache
            .set(
                cache_key,
                CachedContext {
                    agent_home_context,
                    workspace_context,
                },
                profile_mtime,
            )
            .await;

        tracing::info!(
            agent_id = %agent_id,
            effective_cwd = %effective_cwd,
            agent_home = %agent_home.display(),
            skill_count,
            rule_count,
            "Precomputed and cached context"
        );
    });

    Ok(Json(PrecomputeContextResponse {
        status: "precomputing".to_string(),
    }))
}
