use ao_protocol::error::AoError;
use ao_protocol::project::Project;

use crate::paths::DataRoot;

/// YAML-backed store for project records.
pub struct ProjectStore {
    data_root: DataRoot,
}

impl ProjectStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    fn project_path(&self, id: &str) -> std::path::PathBuf {
        self.data_root.project_path(id)
    }

    /// Create a new project. Fails if a project with the same ID already exists.
    pub async fn create(&self, project: &Project) -> Result<(), AoError> {
        let path = self.project_path(&project.id);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Err(AoError::ProjectAlreadyExists(project.id.clone()));
        }
        let yaml = serde_yaml::to_string(project).map_err(|e| AoError::Yaml(e.to_string()))?;
        tokio::fs::write(&path, yaml).await?;
        Ok(())
    }

    /// Get a project by ID. Returns None if not found.
    pub async fn get(&self, id: &str) -> Result<Option<Project>, AoError> {
        let path = self.project_path(id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(None);
        }
        let contents = tokio::fs::read_to_string(&path).await?;
        let project: Project =
            serde_yaml::from_str(&contents).map_err(|e| AoError::Yaml(e.to_string()))?;
        Ok(Some(project))
    }

    /// List all projects from the projects directory.
    pub async fn list(&self) -> Result<Vec<Project>, AoError> {
        let dir = self.data_root.projects_dir();
        if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let mut projects = Vec::new();
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
                let contents = tokio::fs::read_to_string(&path).await?;
                match serde_yaml::from_str::<Project>(&contents) {
                    Ok(p) => projects.push(p),
                    Err(e) => {
                        tracing::warn!("Failed to parse project file {:?}: {}", path, e);
                    }
                }
            }
        }
        Ok(projects)
    }

    /// Overwrite an existing project. Fails if the project does not exist.
    pub async fn save(&self, project: &Project) -> Result<(), AoError> {
        let path = self.project_path(&project.id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Err(AoError::ProjectNotFound(project.id.clone()));
        }
        let yaml = serde_yaml::to_string(project).map_err(|e| AoError::Yaml(e.to_string()))?;
        tokio::fs::write(&path, yaml).await?;
        Ok(())
    }

    /// Delete a project by ID. Returns false if not found.
    pub async fn delete(&self, id: &str) -> Result<bool, AoError> {
        let path = self.project_path(id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(false);
        }
        tokio::fs::remove_file(&path).await?;
        Ok(true)
    }
}
