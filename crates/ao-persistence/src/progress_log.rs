use std::path::{Path, PathBuf};

use ao_protocol::error::AoError;
use serde::{Deserialize, Serialize};

/// One entry in the per-tasklist `progress.jsonl` audit trail.
///
/// Each terminal task appends one block; the optional `task_id: None` / `status:
/// "cancelled"` shape is used by the cancel tool to record tasklist-level
/// cancellation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressBlock {
    pub task_id: Option<String>,
    pub title: Option<String>,
    /// One of: completed | failed | skipped | cancelled
    pub status: String,
    pub summary: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub output_path: Option<PathBuf>,
    pub attempt_count: Option<u32>,
}

/// Append one `ProgressBlock` as a single JSON line to `path`.
///
/// Creates parent directories on demand. Uses `O_APPEND` for atomic
/// single-write semantics on POSIX (same as the `_changelog.jsonl` writer in
/// `changelog.rs`) — no separate file lock is needed for payloads well under
/// `PIPE_BUF`.
pub async fn append_progress_block(path: &Path, block: &ProgressBlock) -> Result<(), AoError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let line = serde_json::to_string(block).map_err(|e| AoError::Json(e.to_string()))?;
    let line_with_newline = format!("{}\n", line);

    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(line_with_newline.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

#[cfg(test)]
mod progress_log_tests {
    use super::*;

    fn make_block(task_id: &str, status: &str) -> ProgressBlock {
        ProgressBlock {
            task_id: Some(task_id.to_string()),
            title: Some(format!("Task {task_id}")),
            status: status.to_string(),
            summary: Some(format!("summary for {task_id}")),
            started_at: Some("2026-05-25T00:00:00Z".to_string()),
            ended_at: Some("2026-05-25T00:01:00Z".to_string()),
            output_path: Some(PathBuf::from(format!("/tmp/tasks/{task_id}/output.txt"))),
            attempt_count: Some(1),
        }
    }

    #[tokio::test]
    async fn append_creates_parent_dir_and_writes_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workspace").join("progress.jsonl");

        assert!(!path.exists());

        let block = make_block("task-1", "completed");
        append_progress_block(&path, &block).await.unwrap();

        assert!(path.exists());
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: ProgressBlock = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed, block);
        assert!(contents.ends_with('\n'));
    }

    #[tokio::test]
    async fn concurrent_appends_produce_no_torn_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("progress.jsonl");
        let path = std::sync::Arc::new(path);

        let handles: Vec<_> = (0..16u32)
            .map(|i| {
                let p = path.clone();
                tokio::spawn(async move {
                    let block = ProgressBlock {
                        task_id: Some(format!("task-{i}")),
                        title: Some(format!("Task {i}")),
                        status: "completed".to_string(),
                        summary: Some(format!("done {i}")),
                        started_at: None,
                        ended_at: None,
                        output_path: None,
                        attempt_count: Some(i),
                    };
                    append_progress_block(&p, &block).await.unwrap();
                })
            })
            .collect();

        for h in handles {
            h.await.unwrap();
        }

        let contents = tokio::fs::read_to_string(path.as_ref()).await.unwrap();
        let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 16, "expected exactly 16 lines, got {}", lines.len());
        for line in &lines {
            let parsed: ProgressBlock =
                serde_json::from_str(line).expect("each line must be valid JSON");
            assert!(
                parsed.task_id.is_some(),
                "each block must have a task_id, got: {parsed:?}"
            );
        }
    }

    #[tokio::test]
    async fn garbage_tail_does_not_corrupt_new_append() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("progress.jsonl");

        // Pre-populate with a half-written (garbage) line then a complete line
        tokio::fs::write(&path, b"{\"status\":\"completed\",\"broken_no_closing\n{\"status\":\"completed\",\"task_id\":\"pre-existing\"}\n").await.unwrap();

        let block = make_block("task-new", "completed");
        append_progress_block(&path, &block).await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        // The last complete line is the newly appended block
        let last_line = contents.lines().last().unwrap();
        let parsed: ProgressBlock = serde_json::from_str(last_line).unwrap();
        assert_eq!(parsed.task_id.as_deref(), Some("task-new"));
        // The pre-existing content is still there
        assert!(contents.contains("pre-existing"));
    }
}
