use std::sync::Arc;

use ao_protocol::artifact::ArtifactGroup;
use ao_protocol::error::AoError;
use chrono::Utc;
use tokio::sync::Mutex;

use crate::paths::DataRoot;

/// Registry-JSON persistence for user-defined artifact groups — a single
/// flat file rather than the per-agent layout `ArtifactStore` uses, since
/// groups exist purely to organize the cross-agent pinned view (they're
/// meaningless scoped to one agent). Read-modify-write sequences are
/// serialized via an in-memory mutex, same shape as `ArtifactStore`'s
/// per-agent locks minus the need for more than one.
pub struct ArtifactGroupStore {
    data_root: DataRoot,
    lock: Arc<Mutex<()>>,
}

impl ArtifactGroupStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self {
            data_root,
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Create a new group.
    pub async fn create(&self, name: String) -> Result<ArtifactGroup, AoError> {
        let _guard = self.lock.lock().await;

        let group = ArtifactGroup {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            created_at: Utc::now(),
        };

        let mut groups = self.read().await?;
        groups.push(group.clone());
        self.write(&groups).await?;

        Ok(group)
    }

    /// List every group, oldest first (creation order).
    pub async fn list(&self) -> Result<Vec<ArtifactGroup>, AoError> {
        self.read().await
    }

    /// Delete a group. Callers are responsible for clearing the group off
    /// any artifacts that referenced it — see
    /// `ArtifactStore::clear_group_across_agents`.
    pub async fn delete(&self, group_id: &str) -> Result<(), AoError> {
        let _guard = self.lock.lock().await;

        let mut groups = self.read().await?;
        let idx = groups
            .iter()
            .position(|g| g.id == group_id)
            .ok_or_else(|| AoError::ArtifactGroupNotFound(group_id.to_string()))?;
        groups.remove(idx);
        self.write(&groups).await?;

        Ok(())
    }

    async fn read(&self) -> Result<Vec<ArtifactGroup>, AoError> {
        let path = self.data_root.artifact_groups_path();
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let contents = tokio::fs::read_to_string(&path).await?;
        serde_json::from_str(&contents).map_err(|e| AoError::Json(e.to_string()))
    }

    async fn write(&self, groups: &[ArtifactGroup]) -> Result<(), AoError> {
        let path = self.data_root.artifact_groups_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(groups).map_err(|e| AoError::Json(e.to_string()))?;
        let tmp_path = path.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, json).await?;
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, DataRoot) {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        (tmp, data_root)
    }

    #[tokio::test]
    async fn create_then_list() {
        let (_tmp, data_root) = setup();
        let store = ArtifactGroupStore::new(data_root);

        let group = store.create("Launch assets".to_string()).await.unwrap();
        assert_eq!(group.name, "Launch assets");

        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, group.id);
    }

    #[tokio::test]
    async fn list_empty_when_no_file() {
        let (_tmp, data_root) = setup();
        let store = ArtifactGroupStore::new(data_root);

        let listed = store.list().await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn create_preserves_insertion_order() {
        let (_tmp, data_root) = setup();
        let store = ArtifactGroupStore::new(data_root);

        let a = store.create("A".to_string()).await.unwrap();
        let b = store.create("B".to_string()).await.unwrap();

        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, a.id);
        assert_eq!(listed[1].id, b.id);
    }

    #[tokio::test]
    async fn delete_removes_group() {
        let (_tmp, data_root) = setup();
        let store = ArtifactGroupStore::new(data_root);

        let group = store.create("Temp".to_string()).await.unwrap();
        store.delete(&group.id).await.unwrap();

        let listed = store.list().await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn delete_missing_group_returns_not_found() {
        let (_tmp, data_root) = setup();
        let store = ArtifactGroupStore::new(data_root);

        let err = store.delete("ghost").await.unwrap_err();
        assert!(matches!(err, AoError::ArtifactGroupNotFound(_)));
    }
}
