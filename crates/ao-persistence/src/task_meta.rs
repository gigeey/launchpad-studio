use std::path::Path;

use ao_protocol::error::AoError;
use ao_protocol::tasklist::{AssignmentMode, TaskStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Sidecar metadata written alongside each task's sub-transcript and output.
/// Optional fields are serialized as JSON null when absent so the schema is
/// stable — consumers can expect every key to be present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskMeta {
    pub task_id: String,
    pub tasklist_id: String,
    pub parent_agent_id: String,
    pub owner_agent_id: Option<String>,
    pub assignment_mode: Option<AssignmentMode>,
    pub title: String,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub summary: Option<String>,
    pub model_used: Option<String>,
}

/// Atomically rewrite `{path}` with the serialized `meta`.
///
/// Creates parent directories on demand. Uses the write-to-.tmp-then-rename
/// pattern so readers never observe a partial write.
pub async fn write_task_meta(path: &Path, meta: &TaskMeta) -> Result<(), AoError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string_pretty(meta).map_err(|e| AoError::Json(e.to_string()))?;
    let tmp = path.with_file_name(format!("meta.{}.tmp", uuid::Uuid::new_v4().simple()));
    tokio::fs::write(&tmp, &json).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Read and deserialize the task meta at `path`.
///
/// Returns `Ok(None)` when the file does not exist yet — e.g. the task reached
/// a terminal state without ever passing through the dispatch hook that seeds
/// the file. Callers use this to carry forward timestamps (`created_at`,
/// `started_at`) recorded at dispatch time instead of overwriting them.
pub async fn read_task_meta(path: &Path) -> Result<Option<TaskMeta>, AoError> {
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return Ok(None);
    }
    let bytes = tokio::fs::read(path).await?;
    let meta: TaskMeta =
        serde_json::from_slice(&bytes).map_err(|e| AoError::Json(e.to_string()))?;
    Ok(Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn make_meta(task_id: &str) -> TaskMeta {
        TaskMeta {
            task_id: task_id.to_string(),
            tasklist_id: "tl-1".to_string(),
            parent_agent_id: "parent".to_string(),
            owner_agent_id: Some("owner".to_string()),
            assignment_mode: Some(AssignmentMode::Pinned),
            title: "Test task".to_string(),
            status: TaskStatus::Pending,
            created_at: Utc::now(),
            started_at: None,
            ended_at: None,
            summary: None,
            model_used: None,
        }
    }

    #[tokio::test]
    async fn task_meta_roundtrip() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("meta.json");
        let meta = make_meta("task-1");
        write_task_meta(&path, &meta).await.unwrap();
        let bytes = tokio::fs::read(&path).await.unwrap();
        let loaded: TaskMeta = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded.task_id, meta.task_id);
        assert_eq!(loaded.owner_agent_id, meta.owner_agent_id);
        assert_eq!(loaded.assignment_mode, meta.assignment_mode);
        assert_eq!(loaded.status, meta.status);
        assert_eq!(loaded.tasklist_id, meta.tasklist_id);
        assert_eq!(loaded.parent_agent_id, meta.parent_agent_id);
    }

    #[tokio::test]
    async fn task_meta_optional_fields_serialize_as_null() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("meta.json");
        let meta = TaskMeta {
            task_id: "t-1".to_string(),
            tasklist_id: "tl-1".to_string(),
            parent_agent_id: "parent".to_string(),
            owner_agent_id: None,
            assignment_mode: None,
            title: "Task".to_string(),
            status: TaskStatus::Pending,
            created_at: Utc::now(),
            started_at: None,
            ended_at: None,
            summary: None,
            model_used: None,
        };
        write_task_meta(&path, &meta).await.unwrap();
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let val: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(val["owner_agent_id"].is_null(), "owner_agent_id must be null, not absent");
        assert!(val["assignment_mode"].is_null(), "assignment_mode must be null, not absent");
        assert!(val["started_at"].is_null(), "started_at must be null, not absent");
        assert!(val["ended_at"].is_null(), "ended_at must be null, not absent");
        assert!(val["summary"].is_null(), "summary must be null, not absent");
        assert!(val["model_used"].is_null(), "model_used must be null, not absent");
    }

    #[tokio::test]
    async fn task_meta_creates_parent_dirs() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("deep").join("nested").join("meta.json");
        let meta = make_meta("t-deep");
        write_task_meta(&path, &meta).await.unwrap();
        assert!(path.exists(), "meta.json must exist after write");
    }

    #[tokio::test]
    async fn task_meta_classified_roundtrip() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("meta.json");
        let meta = TaskMeta {
            assignment_mode: Some(AssignmentMode::Classified),
            ..make_meta("task-cls")
        };
        write_task_meta(&path, &meta).await.unwrap();
        let loaded: TaskMeta =
            serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
        assert_eq!(loaded.assignment_mode, Some(AssignmentMode::Classified));
    }

    #[tokio::test]
    async fn task_meta_concurrent_writes_produce_valid_final_file() {
        // 16 concurrent writers to the same path; the atomic rename ensures the
        // final file is always one complete, well-formed write.
        let tmp = tempdir().unwrap();
        let path = Arc::new(tmp.path().join("meta.json"));

        // Seed an initial file so the parent dir is created once.
        write_task_meta(&path, &make_meta("seed")).await.unwrap();

        let handles: Vec<_> = (0u32..16)
            .map(|i| {
                let path = path.clone();
                tokio::spawn(async move {
                    let meta = TaskMeta {
                        task_id: format!("task-{i}"),
                        tasklist_id: "tl-concurrent".to_string(),
                        parent_agent_id: "parent".to_string(),
                        owner_agent_id: Some(format!("owner-{i}")),
                        assignment_mode: Some(AssignmentMode::Classified),
                        title: format!("Concurrent task {i}"),
                        status: TaskStatus::Pending,
                        created_at: Utc::now(),
                        started_at: None,
                        ended_at: None,
                        summary: None,
                        model_used: None,
                    };
                    write_task_meta(&path, &meta).await.unwrap();
                })
            })
            .collect();

        for h in handles {
            h.await.unwrap();
        }

        let bytes = tokio::fs::read(&*path).await.unwrap();
        let loaded: TaskMeta =
            serde_json::from_slice(&bytes).expect("final file must be well-formed JSON");
        assert!(
            loaded.task_id.starts_with("task-") || loaded.task_id == "seed",
            "final task_id must be one of the written values: {}",
            loaded.task_id
        );
        assert_eq!(loaded.tasklist_id, "tl-concurrent");
    }
}
