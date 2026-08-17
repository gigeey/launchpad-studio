use std::collections::HashMap;

use ao_persistence::workflow_store::WorkflowStore;
use ao_protocol::error::AoError;
use ao_protocol::workflow::{
    PhaseType, WorkflowDefinition, WorkflowSource, WorkflowSummary,
};
use chrono::{DateTime, Utc};

/// In-memory registry of parsed workflow definitions.
/// Parses all workflows on construction and provides fast lookup.
pub struct WorkflowRegistry {
    summaries: HashMap<String, WorkflowSummary>,
    definitions: HashMap<String, WorkflowDefinition>,
    workflow_store: WorkflowStore,
}

impl WorkflowRegistry {
    /// Create a new registry by scanning and parsing all workflows from the store.
    pub async fn new(workflow_store: WorkflowStore) -> Result<Self, AoError> {
        let mut registry = Self {
            summaries: HashMap::new(),
            definitions: HashMap::new(),
            workflow_store,
        };
        registry.load_all().await?;
        Ok(registry)
    }

    /// Return all workflow summaries.
    pub fn list_summaries(&self) -> Vec<&WorkflowSummary> {
        self.summaries.values().collect()
    }

    /// Return a single workflow summary by ID.
    pub fn get_summary(&self, id: &str) -> Option<&WorkflowSummary> {
        self.summaries.get(id)
    }

    /// Return the full workflow definition by ID.
    pub fn get_definition(&self, id: &str) -> Option<&WorkflowDefinition> {
        self.definitions.get(id)
    }

    /// Re-scan the workflow directory and rebuild the registry.
    pub async fn refresh(&mut self) -> Result<(), AoError> {
        self.summaries.clear();
        self.definitions.clear();
        self.load_all().await
    }

    /// Internal: scan all workflow paths and parse definitions + summaries.
    async fn load_all(&mut self) -> Result<(), AoError> {
        let paths = self.workflow_store.list_workflow_paths().await;
        for path in paths {
            // Extract workflow ID from path: {base_path}/{workflow_id}/workflow.yaml
            let workflow_id = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());

            let Some(id) = workflow_id else {
                tracing::warn!("Could not extract workflow ID from path: {:?}", path);
                continue;
            };

            match self.workflow_store.read_workflow(&id).await {
                Ok(mut definition) => {
                    // Resolve phase types from the filesystem when not
                    // explicitly set in the YAML.  The "input" type is
                    // never inferred — it must be declared in the YAML
                    // because it represents a user-facing form phase.
                    for phase in &mut definition.phases {
                        if phase.phase_type.is_none() {
                            let phase_path = self
                                .workflow_store
                                .base_path()
                                .join(&id)
                                .join(&phase.path);
                            let is_dir = tokio::fs::metadata(&phase_path)
                                .await
                                .map(|m| m.is_dir())
                                .unwrap_or(false);
                            phase.phase_type = Some(if is_dir {
                                PhaseType::Folder
                            } else {
                                PhaseType::Prompt
                            });
                        }
                    }
                    let updated_on = tokio::fs::metadata(&path)
                        .await
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map(DateTime::<Utc>::from);
                    let summary = WorkflowSummary {
                        id: definition.id.clone(),
                        name: definition.name.clone(),
                        version: definition.version.clone(),
                        description: definition.description.clone(),
                        phase_count: definition.phases.len(),
                        source: WorkflowSource::User,
                        updated_on,
                        last_run: None,
                    };
                    self.summaries.insert(definition.id.clone(), summary);
                    self.definitions.insert(definition.id.clone(), definition);
                }
                Err(e) => {
                    tracing::warn!("Failed to parse workflow '{}': {}", id, e);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::workflow::PhaseDefinition;

    fn make_test_workflow(id: &str, name: &str, phase_count: usize) -> WorkflowDefinition {
        let phases = (0..phase_count)
            .map(|i| PhaseDefinition {
                id: format!("phase-{}", i),
                name: format!("Phase {}", i),
                intent: Some(format!("Do phase {}", i)),
                path: format!("phase{}/prompt.md", i),
                phase_type: None,
                auto_advance: true,
                schema: None,
                inputs: vec![],
                outputs: vec![],
                fields: vec![],
            })
            .collect();
        WorkflowDefinition {
            id: id.to_string(),
            name: name.to_string(),
            version: Some("1.0".to_string()),
            description: Some(format!("Test workflow {}", id)),
            phases,
        }
    }

    async fn setup_workflow_dir(
        tmp: &tempfile::TempDir,
        workflows: &[WorkflowDefinition],
    ) {
        for wf in workflows {
            let wf_dir = tmp.path().join(&wf.id);
            tokio::fs::create_dir_all(&wf_dir).await.unwrap();
            let yaml = serde_yaml::to_string(wf).unwrap();
            tokio::fs::write(wf_dir.join("workflow.yaml"), yaml)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_registry_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkflowStore::new(tmp.path());
        let registry = WorkflowRegistry::new(store).await.unwrap();

        assert!(registry.list_summaries().is_empty());
        assert!(registry.get_summary("nonexistent").is_none());
        assert!(registry.get_definition("nonexistent").is_none());
    }

    #[tokio::test]
    async fn test_registry_loads_workflows() {
        let tmp = tempfile::tempdir().unwrap();
        let wf1 = make_test_workflow("workflow-a", "Workflow A", 3);
        let wf2 = make_test_workflow("workflow-b", "Workflow B", 2);
        setup_workflow_dir(&tmp, &[wf1, wf2]).await;

        let store = WorkflowStore::new(tmp.path());
        let registry = WorkflowRegistry::new(store).await.unwrap();

        assert_eq!(registry.list_summaries().len(), 2);

        let summary_a = registry.get_summary("workflow-a").unwrap();
        assert_eq!(summary_a.name, "Workflow A");
        assert_eq!(summary_a.phase_count, 3);

        let summary_b = registry.get_summary("workflow-b").unwrap();
        assert_eq!(summary_b.name, "Workflow B");
        assert_eq!(summary_b.phase_count, 2);
    }

    #[tokio::test]
    async fn test_registry_get_definition() {
        let tmp = tempfile::tempdir().unwrap();
        let wf = make_test_workflow("my-wf", "My Workflow", 2);
        setup_workflow_dir(&tmp, &[wf]).await;

        let store = WorkflowStore::new(tmp.path());
        let registry = WorkflowRegistry::new(store).await.unwrap();

        let def = registry.get_definition("my-wf").unwrap();
        assert_eq!(def.id, "my-wf");
        assert_eq!(def.phases.len(), 2);
        assert_eq!(def.phases[0].id, "phase-0");
        assert_eq!(def.phases[1].id, "phase-1");
    }

    #[tokio::test]
    async fn test_registry_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let wf1 = make_test_workflow("wf-1", "First", 1);
        setup_workflow_dir(&tmp, &[wf1]).await;

        let store = WorkflowStore::new(tmp.path());
        let mut registry = WorkflowRegistry::new(store).await.unwrap();
        assert_eq!(registry.list_summaries().len(), 1);

        // Add a new workflow to disk
        let wf2 = make_test_workflow("wf-2", "Second", 2);
        setup_workflow_dir(&tmp, &[wf2]).await;

        registry.refresh().await.unwrap();
        assert_eq!(registry.list_summaries().len(), 2);
        assert!(registry.get_summary("wf-2").is_some());
    }

    #[tokio::test]
    async fn test_registry_skips_invalid_yaml() {
        let tmp = tempfile::tempdir().unwrap();

        // Create a valid workflow
        let wf = make_test_workflow("valid-wf", "Valid", 1);
        setup_workflow_dir(&tmp, &[wf]).await;

        // Create an invalid workflow.yaml
        let bad_dir = tmp.path().join("bad-wf");
        tokio::fs::create_dir_all(&bad_dir).await.unwrap();
        tokio::fs::write(bad_dir.join("workflow.yaml"), "not: valid: yaml: [[[")
            .await
            .unwrap();

        let store = WorkflowStore::new(tmp.path());
        let registry = WorkflowRegistry::new(store).await.unwrap();

        // Should load the valid one and skip the bad one
        assert_eq!(registry.list_summaries().len(), 1);
        assert!(registry.get_summary("valid-wf").is_some());
        assert!(registry.get_summary("bad-wf").is_none());
    }

    #[tokio::test]
    async fn test_registry_nonexistent_directory() {
        let store = WorkflowStore::new("/nonexistent/path");
        let registry = WorkflowRegistry::new(store).await.unwrap();
        assert!(registry.list_summaries().is_empty());
    }
}
