use ao_protocol::assignment::AssignmentRun;
use ao_protocol::error::AoError;

use crate::paths::DataRoot;

/// Append-only JSONL persistence for assignment runs, one file per assignment
/// at [`DataRoot::assignment_runs_path`].
///
/// There is no in-memory cache: reads always go to disk. Appends never rewrite
/// existing bytes. An in-place [`Self::update`] (status transition) reads the
/// whole file, replaces the line whose `run.id` matches, and atomic-renames the
/// rewritten file back so a crash mid-write never corrupts the log.
pub struct AssignmentRunStore {
    data_root: DataRoot,
}

impl AssignmentRunStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Append a new run row as one JSON line to the assignment's JSONL file.
    /// Creates the file (and its parent directory) if absent.
    pub async fn append(
        &self,
        assignment_id: &str,
        run: &AssignmentRun,
    ) -> Result<(), AoError> {
        let path = self.data_root.assignment_runs_path(assignment_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let line = serde_json::to_string(run).map_err(|e| AoError::Json(e.to_string()))?;
        let line_with_newline = format!("{}\n", line);

        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(line_with_newline.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Read all runs for an assignment, oldest first. Returns an empty vec when
    /// the assignment has never produced a run.
    pub async fn list_for_assignment(
        &self,
        assignment_id: &str,
    ) -> Result<Vec<AssignmentRun>, AoError> {
        let path = self.data_root.assignment_runs_path(assignment_id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let contents = tokio::fs::read_to_string(&path).await?;
        let mut runs = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let run: AssignmentRun =
                serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?;
            runs.push(run);
        }
        Ok(runs)
    }

    /// Get a single run by `(assignment_id, run_id)`. Returns `None` if no run
    /// with that id exists in the assignment's log.
    pub async fn get(
        &self,
        assignment_id: &str,
        run_id: &str,
    ) -> Result<Option<AssignmentRun>, AoError> {
        let runs = self.list_for_assignment(assignment_id).await?;
        Ok(runs.into_iter().find(|r| r.id == run_id))
    }

    /// Update an existing run row in-place. Reads the file, replaces the line
    /// whose `run.id` matches, and atomic-renames the rewritten file back.
    /// Returns [`AoError::Internal`] if no run with that id is present.
    pub async fn update(
        &self,
        assignment_id: &str,
        run: &AssignmentRun,
    ) -> Result<(), AoError> {
        let path = self.data_root.assignment_runs_path(assignment_id);
        let existing = self.list_for_assignment(assignment_id).await?;
        if !existing.iter().any(|r| r.id == run.id) {
            return Err(AoError::Internal(format!(
                "Assignment run not found: {} (assignment {})",
                run.id, assignment_id
            )));
        }

        let mut buffer = String::new();
        for current in &existing {
            let chosen = if current.id == run.id { run } else { current };
            let line =
                serde_json::to_string(chosen).map_err(|e| AoError::Json(e.to_string()))?;
            buffer.push_str(&line);
            buffer.push('\n');
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp_path = path.with_extension("jsonl.tmp");
        tokio::fs::write(&tmp_path, buffer).await?;
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::assignment::{AssignmentRunStatus, AssignmentTriggerKind};
    use chrono::Utc;

    fn setup() -> (tempfile::TempDir, DataRoot) {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        (tmp, data_root)
    }

    fn run(id: &str, assignment_id: &str, status: AssignmentRunStatus) -> AssignmentRun {
        AssignmentRun {
            id: id.to_string(),
            assignment_id: assignment_id.to_string(),
            agent_id: "agent-1".to_string(),
            trigger_kind: AssignmentTriggerKind::Cron,
            trigger_payload: Some("0 9 * * *".to_string()),
            status,
            output_summary: None,
            thread_id: Some(format!("thread-{id}")),
            queued_at: Utc::now(),
            started_ts: None,
            finished_ts: None,
            error: None,
        }
    }

    #[tokio::test]
    async fn list_for_assignment_empty_when_no_file() {
        let (_tmp, data_root) = setup();
        let store = AssignmentRunStore::new(data_root);
        let runs = store.list_for_assignment("a1").await.unwrap();
        assert!(runs.is_empty());
    }

    #[tokio::test]
    async fn append_creates_file_and_preserves_order() {
        let (_tmp, data_root) = setup();
        let store = AssignmentRunStore::new(data_root);

        for i in 0..3 {
            let r = run(&format!("run-{i}"), "a1", AssignmentRunStatus::Queued);
            store.append("a1", &r).await.unwrap();
        }

        let runs = store.list_for_assignment("a1").await.unwrap();
        assert_eq!(runs.len(), 3);
        // Oldest first: append order is preserved.
        assert_eq!(runs[0].id, "run-0");
        assert_eq!(runs[1].id, "run-1");
        assert_eq!(runs[2].id, "run-2");
    }

    #[tokio::test]
    async fn runs_are_isolated_per_assignment() {
        let (_tmp, data_root) = setup();
        let store = AssignmentRunStore::new(data_root);

        store.append("a1", &run("r1", "a1", AssignmentRunStatus::Queued)).await.unwrap();
        store.append("a2", &run("r2", "a2", AssignmentRunStatus::Queued)).await.unwrap();

        let a1 = store.list_for_assignment("a1").await.unwrap();
        let a2 = store.list_for_assignment("a2").await.unwrap();
        assert_eq!(a1.len(), 1);
        assert_eq!(a2.len(), 1);
        assert_eq!(a1[0].id, "r1");
        assert_eq!(a2[0].id, "r2");
    }

    #[tokio::test]
    async fn get_returns_matching_run_or_none() {
        let (_tmp, data_root) = setup();
        let store = AssignmentRunStore::new(data_root);
        store.append("a1", &run("r1", "a1", AssignmentRunStatus::Queued)).await.unwrap();

        let found = store.get("a1", "r1").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "r1");

        let missing = store.get("a1", "nope").await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn update_replaces_matching_line_only() {
        let (_tmp, data_root) = setup();
        let store = AssignmentRunStore::new(data_root);

        store.append("a1", &run("r1", "a1", AssignmentRunStatus::Queued)).await.unwrap();
        store.append("a1", &run("r2", "a1", AssignmentRunStatus::Queued)).await.unwrap();

        // Transition r1 to Succeeded with an output summary.
        let mut updated = store.get("a1", "r1").await.unwrap().unwrap();
        updated.status = AssignmentRunStatus::Succeeded;
        updated.output_summary = Some("done".to_string());
        updated.finished_ts = Some(Utc::now());
        store.update("a1", &updated).await.unwrap();

        let runs = store.list_for_assignment("a1").await.unwrap();
        assert_eq!(runs.len(), 2, "update must not add or drop rows");
        // Order preserved.
        assert_eq!(runs[0].id, "r1");
        assert_eq!(runs[1].id, "r2");

        let r1 = runs.iter().find(|r| r.id == "r1").unwrap();
        assert_eq!(r1.status, AssignmentRunStatus::Succeeded);
        assert_eq!(r1.output_summary.as_deref(), Some("done"));

        // The sibling row is untouched.
        let r2 = runs.iter().find(|r| r.id == "r2").unwrap();
        assert_eq!(r2.status, AssignmentRunStatus::Queued);
        assert!(r2.output_summary.is_none());
    }

    #[tokio::test]
    async fn update_missing_run_errors() {
        let (_tmp, data_root) = setup();
        let store = AssignmentRunStore::new(data_root);
        store.append("a1", &run("r1", "a1", AssignmentRunStatus::Queued)).await.unwrap();

        let ghost = run("ghost", "a1", AssignmentRunStatus::Running);
        let err = store.update("a1", &ghost).await.unwrap_err();
        assert!(matches!(err, AoError::Internal(_)));
    }

    #[tokio::test]
    async fn update_round_trips_through_disk() {
        let (_tmp, data_root) = setup();
        let store = AssignmentRunStore::new(data_root.clone());
        store.append("a1", &run("r1", "a1", AssignmentRunStatus::Queued)).await.unwrap();

        let mut updated = store.get("a1", "r1").await.unwrap().unwrap();
        updated.status = AssignmentRunStatus::Failed;
        updated.error = Some("boom".to_string());
        store.update("a1", &updated).await.unwrap();

        // A fresh store reading the same path sees the persisted transition.
        let reload = AssignmentRunStore::new(data_root);
        let r1 = reload.get("a1", "r1").await.unwrap().unwrap();
        assert_eq!(r1.status, AssignmentRunStatus::Failed);
        assert_eq!(r1.error.as_deref(), Some("boom"));
    }
}
