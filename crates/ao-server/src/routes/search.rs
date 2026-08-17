use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;

use ao_engine::AppState;
use ao_protocol::error::AoError;
use ao_protocol::transcript::TranscriptEntry;

use crate::error::AppError;

#[derive(serde::Deserialize)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub limit: Option<usize>,
    pub agent_id: Option<String>,
    /// When `true`, include transcript hits from inline team coordinators
    /// (agents with `owning_team_id` set). Defaults to `false` so chat search
    /// matches the chat agent list.
    #[serde(default)]
    pub include_team_coordinators: bool,
}

#[derive(serde::Serialize)]
pub struct SearchResultItem {
    pub agent_id: String,
    pub agent_name: String,
    pub entry: TranscriptEntry,
}

#[derive(serde::Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResultItem>,
}

/// GET /search?q=<query>&limit=<N>&agent_id=<optional>
pub async fn search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, AppError> {
    let query = params
        .q
        .filter(|q| !q.is_empty())
        .ok_or_else(|| AoError::ValidationError("q parameter is required".to_string()))?;

    let limit = params.limit.unwrap_or(50);

    // Snapshot of inline-coordinator agent ids to filter out (unless caller opts in).
    // Read once up front so we don't re-lock the snapshot per result.
    let team_coordinator_ids: std::collections::HashSet<String> = if params.include_team_coordinators
    {
        std::collections::HashSet::new()
    } else {
        let snap = state.persistence.snapshots.get().await;
        snap.agents
            .values()
            .filter(|a| a.owning_team_id.is_some())
            .map(|a| a.agent_id.clone())
            .collect()
    };

    let mut results: Vec<SearchResultItem> = if let Some(agent_id) = &params.agent_id {
        // Scoped search: hide hits when the requested agent is itself an inline coordinator
        // and the caller didn't opt in (mirrors list-agent behavior).
        if team_coordinator_ids.contains(agent_id) {
            Vec::new()
        } else {
            let entries = state
                .persistence
                .transcripts
                .ripgrep_search(agent_id, &query, limit)
                .await?;

            let agent_name = state
                .persistence
                .agents
                .get(agent_id)
                .await?
                .map(|a| a.name.clone())
                .unwrap_or_else(|| agent_id.clone());

            entries
                .into_iter()
                .map(|entry| SearchResultItem {
                    agent_id: agent_id.clone(),
                    agent_name: agent_name.clone(),
                    entry,
                })
                .collect()
        }
    } else {
        let entries = state
            .persistence
            .transcripts
            .ripgrep_search_all(&query, limit)
            .await?;

        let mut items = Vec::with_capacity(entries.len());
        for (agent_id, entry) in entries {
            if team_coordinator_ids.contains(&agent_id) {
                continue;
            }
            let agent_name = state
                .persistence
                .agents
                .get(&agent_id)
                .await?
                .map(|a| a.name.clone())
                .unwrap_or_else(|| agent_id.clone());

            items.push(SearchResultItem {
                agent_id,
                agent_name,
                entry,
            });
        }
        items
    };

    // Sort by timestamp descending (most recent first)
    results.sort_by(|a, b| b.entry.ts.cmp(&a.entry.ts));

    Ok(Json(SearchResponse { results }))
}
