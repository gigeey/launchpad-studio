use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use ao_engine::AppState;

use crate::error::AppError;
use crate::log_buffer;

#[derive(Serialize)]
pub struct AgentStorageInfo {
    pub agent_id: String,
    pub asset_count: u64,
    pub total_size_bytes: u64,
}

#[derive(Serialize)]
pub struct StorageResponse {
    pub total_assets: u64,
    pub total_size_bytes: u64,
    pub per_agent: Vec<AgentStorageInfo>,
}

#[derive(Serialize)]
pub struct CleanupAgentResult {
    pub agent_id: String,
    pub cleaned_count: u32,
    pub freed_bytes: u64,
}

#[derive(Serialize)]
pub struct CleanupResponse {
    pub total_cleaned: u32,
    pub total_freed_bytes: u64,
    pub per_agent: Vec<CleanupAgentResult>,
}

pub async fn get_storage(
    State(state): State<Arc<AppState>>,
) -> Result<Json<StorageResponse>, AppError> {
    let (total_assets, total_size_bytes, per_agent) =
        state.persistence.assets.storage_summary().await?;

    Ok(Json(StorageResponse {
        total_assets,
        total_size_bytes,
        per_agent: per_agent
            .into_iter()
            .map(|(agent_id, asset_count, total_size_bytes)| AgentStorageInfo {
                agent_id,
                asset_count,
                total_size_bytes,
            })
            .collect(),
    }))
}

pub async fn trigger_cleanup(
    State(state): State<Arc<AppState>>,
) -> Result<Json<CleanupResponse>, AppError> {
    let results = state
        .persistence
        .assets
        .cleanup_all_uncommitted(Duration::from_secs(3600))
        .await?;

    let total_cleaned: u32 = results.iter().map(|(_, c, _)| c).sum();
    let total_freed_bytes: u64 = results.iter().map(|(_, _, f)| f).sum();

    Ok(Json(CleanupResponse {
        total_cleaned,
        total_freed_bytes,
        per_agent: results
            .into_iter()
            .map(|(agent_id, cleaned_count, freed_bytes)| CleanupAgentResult {
                agent_id,
                cleaned_count,
                freed_bytes,
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
pub struct LogsQuery {
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct LogsResponse {
    pub entries: Vec<log_buffer::LogEntry>,
    pub total: usize,
}

pub async fn get_logs(Query(query): Query<LogsQuery>) -> Json<LogsResponse> {
    let limit = query.limit.unwrap_or(500);
    let entries = log_buffer::get_logs(limit);
    let total = entries.len();
    Json(LogsResponse { entries, total })
}

pub async fn clear_logs() -> Json<serde_json::Value> {
    log_buffer::clear_logs();
    Json(serde_json::json!({ "cleared": true }))
}

/// Runtime config values the frontend cannot safely hardcode because they
/// may be overridden per-environment (e.g. a demo lane lowering the
/// `AgentWatch` poll floor). Extend this struct rather than adding new
/// single-purpose config routes.
#[derive(Serialize)]
pub struct AppConfigResponse {
    pub min_agent_watch_poll_interval_secs: u64,
}

/// GET /system/config — server-computed runtime config. Every field is
/// resolved fresh on each request rather than cached, so an env override
/// flip is visible to clients without a server restart.
pub async fn get_config() -> Json<AppConfigResponse> {
    Json(AppConfigResponse {
        min_agent_watch_poll_interval_secs:
            ao_protocol::assignment::effective_min_agent_watch_poll_interval_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate the process-global poll-interval
    /// override env var — separate from `crate::routes::env_lock::ENV_LOCK`,
    /// which is reserved for `LAUNCHPAD_STUDIO_DATA_DIR`.
    static POLL_ENV_OVERRIDE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[tokio::test]
    async fn get_config_reports_default_floor_when_override_unset() {
        let _guard = POLL_ENV_OVERRIDE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let key = ao_protocol::assignment::MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR;
        let prev = std::env::var(key).ok();
        std::env::remove_var(key);

        let Json(config) = get_config().await;
        assert_eq!(
            config.min_agent_watch_poll_interval_secs,
            ao_protocol::assignment::MIN_AGENT_WATCH_POLL_INTERVAL_SECS
        );

        if let Some(v) = prev {
            std::env::set_var(key, v);
        }
    }

    #[tokio::test]
    async fn get_config_reflects_override_when_set() {
        let _guard = POLL_ENV_OVERRIDE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let key = ao_protocol::assignment::MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR;
        let prev = std::env::var(key).ok();
        std::env::set_var(key, "60");

        let Json(config) = get_config().await;
        assert_eq!(config.min_agent_watch_poll_interval_secs, 60);

        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}
