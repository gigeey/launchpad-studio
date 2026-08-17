use std::path::PathBuf;

use ao_protocol::error::AoError;
use ao_protocol::transcript::TranscriptEntry;
use ao_protocol::workflow::{TaskSnapshot, WorkflowDefinition};

/// Reads workflow definitions from a workflows directory.
/// Expected layout: {base_path}/{workflow_id}/workflow.yaml
pub struct WorkflowStore {
    base_path: PathBuf,
}

impl WorkflowStore {
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
        }
    }

    pub fn base_path(&self) -> &PathBuf {
        &self.base_path
    }

    /// List all workflow directories that contain a workflow.yaml file.
    pub async fn list_workflow_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let Ok(mut entries) = tokio::fs::read_dir(&self.base_path).await else {
            return paths;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                let yaml_path = path.join("workflow.yaml");
                if tokio::fs::try_exists(&yaml_path).await.unwrap_or(false) {
                    paths.push(yaml_path);
                }
            }
        }
        paths.sort();
        paths
    }

    /// Read and parse a workflow definition by ID.
    pub async fn read_workflow(&self, id: &str) -> Result<WorkflowDefinition, AoError> {
        let path = self.base_path.join(id).join("workflow.yaml");
        let contents = tokio::fs::read_to_string(&path).await.map_err(|e| {
            AoError::Internal(format!("Failed to read workflow '{}': {}", id, e))
        })?;
        let definition: WorkflowDefinition =
            serde_yaml::from_str(&contents).map_err(|e| AoError::Yaml(e.to_string()))?;
        Ok(definition)
    }

    /// Read a phase prompt markdown file.
    pub async fn read_phase_prompt(
        &self,
        workflow_id: &str,
        phase_path: &str,
    ) -> Result<String, AoError> {
        let path = self.base_path.join(workflow_id).join(phase_path);
        let contents = tokio::fs::read_to_string(&path).await.map_err(|e| {
            AoError::Internal(format!(
                "Failed to read phase prompt '{}/{}': {}",
                workflow_id, phase_path, e
            ))
        })?;
        Ok(contents)
    }

    /// Read a phase schema JSON file.
    pub async fn read_phase_schema(
        &self,
        workflow_id: &str,
        schema_path: &str,
    ) -> Result<String, AoError> {
        let path = self.base_path.join(workflow_id).join(schema_path);
        let contents = tokio::fs::read_to_string(&path).await.map_err(|e| {
            AoError::Internal(format!(
                "Failed to read phase schema '{}/{}': {}",
                workflow_id, schema_path, e
            ))
        })?;
        Ok(contents)
    }
}

/// Reads and writes task snapshots and output files.
/// Layout: {base_path}/{task_id}/task.yaml and {base_path}/{task_id}/output/
pub struct TaskStore {
    base_path: PathBuf,
}

impl TaskStore {
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
        }
    }

    pub fn base_path(&self) -> &PathBuf {
        &self.base_path
    }

    /// Create the task directory and output subdirectory.
    pub async fn create_task_dir(&self, task_id: &str) -> Result<PathBuf, AoError> {
        let task_dir = self.base_path.join(task_id);
        let output_dir = task_dir.join("output");
        tokio::fs::create_dir_all(&output_dir).await?;
        Ok(task_dir)
    }

    /// Write a task snapshot to {task_id}/task.yaml.
    pub async fn write_task_snapshot(
        &self,
        task_id: &str,
        snapshot: &TaskSnapshot,
    ) -> Result<(), AoError> {
        let path = self.base_path.join(task_id).join("task.yaml");
        let yaml =
            serde_yaml::to_string(snapshot).map_err(|e| AoError::Yaml(e.to_string()))?;
        tokio::fs::write(&path, yaml).await?;
        Ok(())
    }

    /// Read a task snapshot from {task_id}/task.yaml.
    pub async fn read_task_snapshot(&self, task_id: &str) -> Result<TaskSnapshot, AoError> {
        let path = self.base_path.join(task_id).join("task.yaml");
        let contents = tokio::fs::read_to_string(&path).await.map_err(|e| {
            AoError::Internal(format!("Failed to read task snapshot '{}': {}", task_id, e))
        })?;
        let snapshot: TaskSnapshot =
            serde_yaml::from_str(&contents).map_err(|e| AoError::Yaml(e.to_string()))?;
        Ok(snapshot)
    }

    /// List all task directories (by directory name).
    pub async fn list_tasks(&self) -> Result<Vec<String>, AoError> {
        let mut tasks = Vec::new();
        let Ok(mut entries) = tokio::fs::read_dir(&self.base_path).await else {
            return Ok(tasks);
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    tasks.push(name.to_string());
                }
            }
        }
        tasks.sort();
        Ok(tasks)
    }

    /// Write content to {task_id}/output/{filename}.
    pub async fn write_output(
        &self,
        task_id: &str,
        filename: &str,
        content: &str,
    ) -> Result<(), AoError> {
        let path = self.base_path.join(task_id).join("output").join(filename);
        tokio::fs::write(&path, content).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Phase message storage: {task_id}/messages/{phase_id}.jsonl
    // -----------------------------------------------------------------------

    /// Path to the JSONL message file for a given task phase.
    fn phase_messages_path(&self, task_id: &str, phase_id: &str) -> PathBuf {
        self.base_path
            .join(task_id)
            .join("messages")
            .join(format!("{}.jsonl", phase_id))
    }

    /// Append a transcript entry to a phase's message log.
    pub async fn append_phase_message(
        &self,
        task_id: &str,
        phase_id: &str,
        entry: &TranscriptEntry,
    ) -> Result<(), AoError> {
        let path = self.phase_messages_path(task_id, phase_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let line = serde_json::to_string(entry).map_err(|e| AoError::Json(e.to_string()))?;
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(format!("{}\n", line).as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Read all transcript entries for a task phase. Returns empty vec if not found.
    pub async fn read_phase_messages(
        &self,
        task_id: &str,
        phase_id: &str,
    ) -> Result<Vec<TranscriptEntry>, AoError> {
        let path = self.phase_messages_path(task_id, phase_id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let contents = tokio::fs::read_to_string(&path).await?;
        let mut entries = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: TranscriptEntry =
                serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Delete a task directory entirely.
    pub async fn delete_task(&self, task_id: &str) -> Result<(), AoError> {
        let task_dir = self.base_path.join(task_id);
        if !task_dir.exists() {
            return Err(AoError::TaskNotFound(task_id.to_string()));
        }
        tokio::fs::remove_dir_all(&task_dir).await?;
        Ok(())
    }

    /// Read content from {task_id}/output/{filename}.
    pub async fn read_output(
        &self,
        task_id: &str,
        filename: &str,
    ) -> Result<String, AoError> {
        let path = self.base_path.join(task_id).join("output").join(filename);
        let contents = tokio::fs::read_to_string(&path).await.map_err(|e| {
            AoError::Internal(format!(
                "Failed to read output '{}/output/{}': {}",
                task_id, filename, e
            ))
        })?;
        Ok(contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::workflow::{PhaseDefinition, PhaseState, PhaseStatus, TaskStatus, WorkflowDefinition};
    use chrono::Utc;
    use std::collections::HashMap;

    fn make_test_workflow() -> WorkflowDefinition {
        WorkflowDefinition {
            id: "test-workflow".to_string(),
            name: "Test Workflow".to_string(),
            version: Some("1.0".to_string()),
            description: Some("A test workflow".to_string()),
            phases: vec![PhaseDefinition {
                id: "phase-1".to_string(),
                name: "Phase One".to_string(),
                intent: Some("Do something".to_string()),
                path: "phase1/prompt.md".to_string(),
                phase_type: None,
                auto_advance: true,
                schema: None,
                inputs: vec![],
                outputs: vec![],
                fields: vec![],
            }],
        }
    }

    fn make_test_snapshot() -> TaskSnapshot {
        TaskSnapshot {
            status: TaskStatus::default(),
            workflow: "test-workflow".to_string(),
            workflow_version: Some("1.0".to_string()),
            created: Utc::now(),
            project_name: "Test Project".to_string(),
            working_directory: Some("/tmp/test".to_string()),
            context: HashMap::new(),
            phases: HashMap::new(),
        }
    }

    // --- WorkflowStore tests ---

    #[tokio::test]
    async fn test_list_workflow_paths_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::new(tmp.path());
        let paths = store.list_workflow_paths().await;
        assert!(paths.is_empty());
    }

    #[tokio::test]
    async fn test_list_workflow_paths_finds_workflows() {
        let tmp = tempfile::tempdir().unwrap();
        // Create two workflow dirs
        let wf1_dir = tmp.path().join("workflow-a");
        let wf2_dir = tmp.path().join("workflow-b");
        tokio::fs::create_dir_all(&wf1_dir).await.unwrap();
        tokio::fs::create_dir_all(&wf2_dir).await.unwrap();
        tokio::fs::write(wf1_dir.join("workflow.yaml"), "id: workflow-a\nname: A\n")
            .await
            .unwrap();
        tokio::fs::write(wf2_dir.join("workflow.yaml"), "id: workflow-b\nname: B\n")
            .await
            .unwrap();
        // Create a non-workflow dir (no workflow.yaml)
        let other_dir = tmp.path().join("not-a-workflow");
        tokio::fs::create_dir_all(&other_dir).await.unwrap();

        let store = WorkflowStore::new(tmp.path());
        let paths = store.list_workflow_paths().await;
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("workflow-a/workflow.yaml"));
        assert!(paths[1].ends_with("workflow-b/workflow.yaml"));
    }

    #[tokio::test]
    async fn test_read_workflow_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let wf_dir = tmp.path().join("test-workflow");
        tokio::fs::create_dir_all(&wf_dir).await.unwrap();

        let workflow = make_test_workflow();
        let yaml = serde_yaml::to_string(&workflow).unwrap();
        tokio::fs::write(wf_dir.join("workflow.yaml"), &yaml)
            .await
            .unwrap();

        let store = WorkflowStore::new(tmp.path());
        let loaded = store.read_workflow("test-workflow").await.unwrap();
        assert_eq!(loaded.id, "test-workflow");
        assert_eq!(loaded.name, "Test Workflow");
        assert_eq!(loaded.phases.len(), 1);
        assert_eq!(loaded.phases[0].id, "phase-1");
    }

    #[tokio::test]
    async fn test_read_workflow_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::new(tmp.path());
        let result = store.read_workflow("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_phase_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let wf_dir = tmp.path().join("my-workflow");
        let phase_dir = wf_dir.join("phase1");
        tokio::fs::create_dir_all(&phase_dir).await.unwrap();
        tokio::fs::write(phase_dir.join("prompt.md"), "# Do the thing\nInstructions here.")
            .await
            .unwrap();

        let store = WorkflowStore::new(tmp.path());
        let content = store
            .read_phase_prompt("my-workflow", "phase1/prompt.md")
            .await
            .unwrap();
        assert_eq!(content, "# Do the thing\nInstructions here.");
    }

    #[tokio::test]
    async fn test_read_phase_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let wf_dir = tmp.path().join("my-workflow");
        let phase_dir = wf_dir.join("phase1");
        tokio::fs::create_dir_all(&phase_dir).await.unwrap();
        tokio::fs::write(phase_dir.join("schema.json"), r#"{"type":"object"}"#)
            .await
            .unwrap();

        let store = WorkflowStore::new(tmp.path());
        let content = store
            .read_phase_schema("my-workflow", "phase1/schema.json")
            .await
            .unwrap();
        assert_eq!(content, r#"{"type":"object"}"#);
    }

    // --- TaskStore tests ---

    #[tokio::test]
    async fn test_create_task_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TaskStore::new(tmp.path());
        let task_dir = store.create_task_dir("task-001").await.unwrap();

        assert!(tokio::fs::try_exists(&task_dir).await.unwrap());
        assert!(tokio::fs::try_exists(task_dir.join("output")).await.unwrap());
    }

    #[tokio::test]
    async fn test_task_snapshot_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TaskStore::new(tmp.path());
        store.create_task_dir("task-001").await.unwrap();

        let mut snapshot = make_test_snapshot();
        snapshot.phases.insert(
            "phase-1".to_string(),
            PhaseState {
                status: PhaseStatus::Completed,
                completed_at: Some(Utc::now()),
                skipped_at: None,
                started_at: Some(Utc::now()),
                reason: None,
                error: None,
                failed_at: None,
                paused_reason: None,
                input_tokens: None,
                output_tokens: None,
            },
        );

        store
            .write_task_snapshot("task-001", &snapshot)
            .await
            .unwrap();
        let loaded = store.read_task_snapshot("task-001").await.unwrap();

        assert_eq!(loaded.workflow, "test-workflow");
        assert_eq!(loaded.project_name, "Test Project");
        assert_eq!(loaded.phases.len(), 1);
        assert!(loaded.phases.contains_key("phase-1"));
    }

    #[tokio::test]
    async fn test_read_task_snapshot_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TaskStore::new(tmp.path());
        let result = store.read_task_snapshot("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_tasks_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TaskStore::new(tmp.path());
        let tasks = store.list_tasks().await.unwrap();
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn test_list_tasks_returns_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TaskStore::new(tmp.path());

        store.create_task_dir("task-a").await.unwrap();
        store.create_task_dir("task-b").await.unwrap();
        // Create a file (not a directory) — should be ignored
        tokio::fs::write(tmp.path().join("not-a-task.txt"), "hi")
            .await
            .unwrap();

        let tasks = store.list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0], "task-a");
        assert_eq!(tasks[1], "task-b");
    }

    #[tokio::test]
    async fn test_write_and_read_output() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TaskStore::new(tmp.path());
        store.create_task_dir("task-001").await.unwrap();

        store
            .write_output("task-001", "result.json", r#"{"score": 42}"#)
            .await
            .unwrap();

        let content = store.read_output("task-001", "result.json").await.unwrap();
        assert_eq!(content, r#"{"score": 42}"#);
    }

    #[tokio::test]
    async fn test_read_output_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TaskStore::new(tmp.path());
        store.create_task_dir("task-001").await.unwrap();

        let result = store.read_output("task-001", "nonexistent.txt").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_workflow_paths_nonexistent_dir() {
        let store = WorkflowStore::new("/nonexistent/path");
        let paths = store.list_workflow_paths().await;
        assert!(paths.is_empty());
    }

    #[tokio::test]
    async fn test_list_tasks_nonexistent_dir() {
        let store = TaskStore::new("/nonexistent/path");
        let tasks = store.list_tasks().await.unwrap();
        assert!(tasks.is_empty());
    }
}
