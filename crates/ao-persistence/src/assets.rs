use std::path::Path;
use std::time::Duration;

use ao_protocol::attachment::{Attachment, AttachmentType};
use ao_protocol::error::AoError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths::DataRoot;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetRegistryEntry {
    pub id: String,
    pub original_filename: String,
    pub stored_filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub attachment_type: AttachmentType,
    pub checksum_sha256: String,
    pub created_at: DateTime<Utc>,
    pub message_id: Option<String>,
    /// Original folder path for Folder attachments (not used for files).
    #[serde(default)]
    pub folder_path: Option<String>,
}

pub struct AssetStore {
    data_root: DataRoot,
}

impl AssetStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Store a file on disk with UUID-based filename, preserving extension.
    /// Returns existing Attachment if SHA-256 checksum matches a previous upload.
    pub async fn store_file(
        &self,
        agent_id: &str,
        filename: &str,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<Attachment, AoError> {
        let checksum = hex_sha256(bytes);

        // Check for deduplication
        let registry = self.read_registry(agent_id).await?;
        if let Some(existing) = registry.iter().find(|e| e.checksum_sha256 == checksum) {
            return Ok(entry_to_attachment(existing, &self.data_root, agent_id));
        }

        let id = uuid::Uuid::new_v4().to_string();
        let extension = Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let stored_filename = if extension.is_empty() {
            id.clone()
        } else {
            format!("{}.{}", id, extension)
        };

        let files_dir = self.data_root.assets_dir(agent_id);
        tokio::fs::create_dir_all(&files_dir).await?;

        let file_path = files_dir.join(&stored_filename);
        tokio::fs::write(&file_path, bytes).await?;

        let attachment_type = mime_to_attachment_type(mime_type);
        let entry = AssetRegistryEntry {
            id: id.clone(),
            original_filename: filename.to_string(),
            stored_filename,
            mime_type: mime_type.to_string(),
            size_bytes: bytes.len() as u64,
            attachment_type: attachment_type.clone(),
            checksum_sha256: checksum,
            created_at: Utc::now(),
            message_id: None,
            folder_path: None,
        };

        let mut registry = registry;
        registry.push(entry);
        self.write_registry(agent_id, &registry).await?;

        Ok(Attachment {
            id,
            file_path: file_path.to_string_lossy().to_string(),
            mime_type: mime_type.to_string(),
            original_filename: filename.to_string(),
            size_bytes: bytes.len() as u64,
            attachment_type,
        })
    }

    /// Store a folder reference without copying. Validates folder exists.
    pub async fn store_folder_reference(
        &self,
        agent_id: &str,
        folder_path: &str,
    ) -> Result<Attachment, AoError> {
        let path = Path::new(folder_path);
        if !path.is_dir() {
            return Err(AoError::Internal(format!(
                "Folder does not exist: {}",
                folder_path
            )));
        }

        let id = uuid::Uuid::new_v4().to_string();
        let folder_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("folder");

        let entry = AssetRegistryEntry {
            id: id.clone(),
            original_filename: folder_name.to_string(),
            stored_filename: String::new(),
            mime_type: "inode/directory".to_string(),
            size_bytes: 0,
            attachment_type: AttachmentType::Folder,
            checksum_sha256: String::new(),
            created_at: Utc::now(),
            message_id: None,
            folder_path: Some(folder_path.to_string()),
        };

        // Ensure registry parent dir exists
        let registry_path = self.data_root.asset_registry_path(agent_id);
        if let Some(parent) = registry_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut registry = self.read_registry(agent_id).await?;
        registry.push(entry);
        self.write_registry(agent_id, &registry).await?;

        Ok(Attachment {
            id,
            file_path: folder_path.to_string(),
            mime_type: "inode/directory".to_string(),
            original_filename: folder_name.to_string(),
            size_bytes: 0,
            attachment_type: AttachmentType::Folder,
        })
    }

    /// Get file bytes and mime_type for a given attachment.
    pub async fn get_file(
        &self,
        agent_id: &str,
        attachment_id: &str,
    ) -> Result<(Vec<u8>, String), AoError> {
        let registry = self.read_registry(agent_id).await?;
        let entry = registry
            .iter()
            .find(|e| e.id == attachment_id)
            .ok_or_else(|| AoError::AttachmentNotFound(attachment_id.to_string()))?;

        let file_path = self
            .data_root
            .assets_dir(agent_id)
            .join(&entry.stored_filename);
        let bytes = tokio::fs::read(&file_path).await?;
        Ok((bytes, entry.mime_type.clone()))
    }

    /// Delete a file from disk and remove from registry.
    pub async fn delete_file(
        &self,
        agent_id: &str,
        attachment_id: &str,
    ) -> Result<(), AoError> {
        let mut registry = self.read_registry(agent_id).await?;
        let idx = registry
            .iter()
            .position(|e| e.id == attachment_id)
            .ok_or_else(|| AoError::AttachmentNotFound(attachment_id.to_string()))?;

        let entry = registry.remove(idx);

        // Only delete file from disk for non-folder entries
        if entry.attachment_type != AttachmentType::Folder && !entry.stored_filename.is_empty() {
            let file_path = self
                .data_root
                .assets_dir(agent_id)
                .join(&entry.stored_filename);
            if tokio::fs::try_exists(&file_path).await.unwrap_or(false) {
                tokio::fs::remove_file(&file_path).await?;
            }
        }

        self.write_registry(agent_id, &registry).await?;
        Ok(())
    }

    /// List all assets for an agent.
    pub async fn list_files(&self, agent_id: &str) -> Result<Vec<Attachment>, AoError> {
        let registry = self.read_registry(agent_id).await?;
        Ok(registry
            .iter()
            .map(|e| entry_to_attachment(e, &self.data_root, agent_id))
            .collect())
    }

    /// Get a single attachment by ID without fetching file bytes.
    pub async fn get_attachment(
        &self,
        agent_id: &str,
        attachment_id: &str,
    ) -> Result<Attachment, AoError> {
        let registry = self.read_registry(agent_id).await?;
        let entry = registry
            .iter()
            .find(|e| e.id == attachment_id)
            .ok_or_else(|| AoError::AttachmentNotFound(attachment_id.to_string()))?;
        Ok(entry_to_attachment(entry, &self.data_root, agent_id))
    }

    /// Link an asset to a sent message.
    pub async fn mark_committed(
        &self,
        agent_id: &str,
        attachment_id: &str,
        message_id: &str,
    ) -> Result<(), AoError> {
        let mut registry = self.read_registry(agent_id).await?;
        let entry = registry
            .iter_mut()
            .find(|e| e.id == attachment_id)
            .ok_or_else(|| AoError::AttachmentNotFound(attachment_id.to_string()))?;
        entry.message_id = Some(message_id.to_string());
        self.write_registry(agent_id, &registry).await?;
        Ok(())
    }

    /// Remove assets not linked to any message older than the given duration.
    /// Returns the number of assets cleaned up.
    pub async fn cleanup_uncommitted(
        &self,
        agent_id: &str,
        older_than: Duration,
    ) -> Result<u32, AoError> {
        let mut registry = self.read_registry(agent_id).await?;
        let cutoff = Utc::now() - chrono::Duration::from_std(older_than).unwrap_or_default();

        let mut to_remove = Vec::new();
        for (i, entry) in registry.iter().enumerate() {
            if entry.message_id.is_none() && entry.created_at < cutoff {
                to_remove.push(i);
            }
        }

        let count = to_remove.len() as u32;

        // Remove in reverse order to preserve indices
        for &i in to_remove.iter().rev() {
            let entry = registry.remove(i);
            if entry.attachment_type != AttachmentType::Folder && !entry.stored_filename.is_empty() {
                let file_path = self
                    .data_root
                    .assets_dir(agent_id)
                    .join(&entry.stored_filename);
                if tokio::fs::try_exists(&file_path).await.unwrap_or(false) {
                    let _ = tokio::fs::remove_file(&file_path).await;
                }
            }
        }

        self.write_registry(agent_id, &registry).await?;
        Ok(count)
    }

    /// Run cleanup_uncommitted across all agents that have asset directories.
    /// Returns a list of (agent_id, count_cleaned, bytes_freed).
    pub async fn cleanup_all_uncommitted(
        &self,
        older_than: Duration,
    ) -> Result<Vec<(String, u32, u64)>, AoError> {
        let base = self.data_root.assets_base_dir();
        if !tokio::fs::try_exists(&base).await.unwrap_or(false) {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();
        let mut dir = tokio::fs::read_dir(&base).await?;
        while let Some(entry) = dir.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let agent_id = entry.file_name().to_string_lossy().to_string();
            // Calculate size before cleanup
            let registry_before = self.read_registry(&agent_id).await?;
            let size_before: u64 = registry_before
                .iter()
                .filter(|e| e.message_id.is_none())
                .map(|e| e.size_bytes)
                .sum();

            let count = self.cleanup_uncommitted(&agent_id, older_than).await?;
            if count > 0 {
                // Calculate freed bytes
                let registry_after = self.read_registry(&agent_id).await?;
                let size_after: u64 = registry_after
                    .iter()
                    .filter(|e| e.message_id.is_none())
                    .map(|e| e.size_bytes)
                    .sum();
                let freed = size_before.saturating_sub(size_after);
                results.push((agent_id, count, freed));
            }
        }
        Ok(results)
    }

    /// Get storage summary across all agents.
    /// Returns (total_assets, total_size_bytes, per_agent: Vec<(agent_id, count, size)>).
    pub async fn storage_summary(
        &self,
    ) -> Result<(u64, u64, Vec<(String, u64, u64)>), AoError> {
        let base = self.data_root.assets_base_dir();
        if !tokio::fs::try_exists(&base).await.unwrap_or(false) {
            return Ok((0, 0, Vec::new()));
        }

        let mut total_assets: u64 = 0;
        let mut total_size: u64 = 0;
        let mut per_agent = Vec::new();

        let mut dir = tokio::fs::read_dir(&base).await?;
        while let Some(entry) = dir.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let agent_id = entry.file_name().to_string_lossy().to_string();
            let registry = self.read_registry(&agent_id).await?;
            let count = registry.len() as u64;
            let size: u64 = registry.iter().map(|e| e.size_bytes).sum();
            if count > 0 {
                per_agent.push((agent_id, count, size));
            }
            total_assets += count;
            total_size += size;
        }

        Ok((total_assets, total_size, per_agent))
    }

    /// Delete an entire asset key directory (registry + all files).
    /// Used when a task is deleted to clean up associated phase attachments.
    pub async fn delete_asset_key(&self, asset_key: &str) -> Result<(), AoError> {
        let asset_dir = self.data_root.assets_base_dir().join(asset_key);
        if tokio::fs::try_exists(&asset_dir).await.unwrap_or(false) {
            tokio::fs::remove_dir_all(&asset_dir).await?;
        }
        Ok(())
    }

    async fn read_registry(&self, agent_id: &str) -> Result<Vec<AssetRegistryEntry>, AoError> {
        let path = self.data_root.asset_registry_path(agent_id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let contents = tokio::fs::read_to_string(&path).await?;
        let registry: Vec<AssetRegistryEntry> =
            serde_json::from_str(&contents).map_err(|e| AoError::Json(e.to_string()))?;
        Ok(registry)
    }

    async fn write_registry(
        &self,
        agent_id: &str,
        registry: &[AssetRegistryEntry],
    ) -> Result<(), AoError> {
        let path = self.data_root.asset_registry_path(agent_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(registry)
            .map_err(|e| AoError::Json(e.to_string()))?;
        tokio::fs::write(&path, json).await?;
        Ok(())
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn mime_to_attachment_type(mime: &str) -> AttachmentType {
    if mime.starts_with("image/") {
        AttachmentType::Image
    } else if mime.starts_with("text/") || mime == "application/json" || mime == "application/xml" {
        AttachmentType::Code
    } else if mime == "application/pdf"
        || mime == "application/msword"
        || mime.starts_with("application/vnd.openxmlformats-officedocument.wordprocessing")
    {
        AttachmentType::Document
    } else if mime == "text/csv"
        || mime.starts_with("application/vnd.openxmlformats-officedocument.spreadsheet")
        || mime.starts_with("application/vnd.ms-excel")
    {
        AttachmentType::Spreadsheet
    } else {
        AttachmentType::Other
    }
}

fn entry_to_attachment(entry: &AssetRegistryEntry, data_root: &DataRoot, agent_id: &str) -> Attachment {
    let file_path = if entry.attachment_type == AttachmentType::Folder {
        entry.folder_path.clone().unwrap_or_default()
    } else {
        data_root
            .assets_dir(agent_id)
            .join(&entry.stored_filename)
            .to_string_lossy()
            .to_string()
    };
    Attachment {
        id: entry.id.clone(),
        file_path,
        mime_type: entry.mime_type.clone(),
        original_filename: entry.original_filename.clone(),
        size_bytes: entry.size_bytes,
        attachment_type: entry.attachment_type.clone(),
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
    async fn test_store_and_get_file() {
        let (_tmp, data_root) = setup();
        let store = AssetStore::new(data_root);

        let attachment = store
            .store_file("agent-1", "test.png", "image/png", b"fake png data")
            .await
            .unwrap();

        assert_eq!(attachment.original_filename, "test.png");
        assert_eq!(attachment.mime_type, "image/png");
        assert_eq!(attachment.size_bytes, 13);
        assert_eq!(attachment.attachment_type, AttachmentType::Image);

        let (bytes, mime) = store.get_file("agent-1", &attachment.id).await.unwrap();
        assert_eq!(bytes, b"fake png data");
        assert_eq!(mime, "image/png");
    }

    #[tokio::test]
    async fn test_deduplication() {
        let (_tmp, data_root) = setup();
        let store = AssetStore::new(data_root);

        let a1 = store
            .store_file("agent-1", "file.txt", "text/plain", b"same content")
            .await
            .unwrap();
        let a2 = store
            .store_file("agent-1", "file2.txt", "text/plain", b"same content")
            .await
            .unwrap();

        // Should return the same attachment (dedup by checksum)
        assert_eq!(a1.id, a2.id);

        // Registry should have only one entry
        let list = store.list_files("agent-1").await.unwrap();
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn test_delete_file() {
        let (_tmp, data_root) = setup();
        let store = AssetStore::new(data_root);

        let attachment = store
            .store_file("agent-1", "delete-me.txt", "text/plain", b"data")
            .await
            .unwrap();

        store.delete_file("agent-1", &attachment.id).await.unwrap();

        let list = store.list_files("agent-1").await.unwrap();
        assert!(list.is_empty());

        // File on disk should be gone
        assert!(store.get_file("agent-1", &attachment.id).await.is_err());
    }

    #[tokio::test]
    async fn test_list_files() {
        let (_tmp, data_root) = setup();
        let store = AssetStore::new(data_root);

        store
            .store_file("agent-1", "a.txt", "text/plain", b"aaa")
            .await
            .unwrap();
        store
            .store_file("agent-1", "b.png", "image/png", b"bbb")
            .await
            .unwrap();

        let list = store.list_files("agent-1").await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn test_list_files_empty() {
        let (_tmp, data_root) = setup();
        let store = AssetStore::new(data_root);

        let list = store.list_files("no-agent").await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_store_folder_reference() {
        let (tmp, data_root) = setup();
        let store = AssetStore::new(data_root);

        // Use the temp dir itself as a valid folder
        let folder_path = tmp.path().to_string_lossy().to_string();
        let attachment = store
            .store_folder_reference("agent-1", &folder_path)
            .await
            .unwrap();

        assert_eq!(attachment.attachment_type, AttachmentType::Folder);
        assert_eq!(attachment.mime_type, "inode/directory");
        assert_eq!(attachment.size_bytes, 0);
    }

    #[tokio::test]
    async fn test_store_folder_reference_nonexistent() {
        let (_tmp, data_root) = setup();
        let store = AssetStore::new(data_root);

        let result = store
            .store_folder_reference("agent-1", "/nonexistent/path/12345")
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mark_committed() {
        let (_tmp, data_root) = setup();
        let store = AssetStore::new(data_root.clone());

        let attachment = store
            .store_file("agent-1", "file.txt", "text/plain", b"data")
            .await
            .unwrap();

        store
            .mark_committed("agent-1", &attachment.id, "msg-123")
            .await
            .unwrap();

        let registry = store.read_registry("agent-1").await.unwrap();
        assert_eq!(registry[0].message_id, Some("msg-123".to_string()));
    }

    #[tokio::test]
    async fn test_cleanup_uncommitted() {
        let (_tmp, data_root) = setup();
        let store = AssetStore::new(data_root.clone());

        // Store two files
        let a1 = store
            .store_file("agent-1", "committed.txt", "text/plain", b"committed")
            .await
            .unwrap();
        let _a2 = store
            .store_file("agent-1", "uncommitted.txt", "text/plain", b"uncommitted")
            .await
            .unwrap();

        // Mark one as committed
        store
            .mark_committed("agent-1", &a1.id, "msg-1")
            .await
            .unwrap();

        // Backdate the uncommitted entry's created_at
        let mut registry = store.read_registry("agent-1").await.unwrap();
        for entry in &mut registry {
            if entry.message_id.is_none() {
                entry.created_at = Utc::now() - chrono::Duration::hours(2);
            }
        }
        store.write_registry("agent-1", &registry).await.unwrap();

        // Cleanup uncommitted older than 1 hour
        let cleaned = store
            .cleanup_uncommitted("agent-1", Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(cleaned, 1);

        let remaining = store.list_files("agent-1").await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, a1.id);
    }

    #[tokio::test]
    async fn test_lazy_directory_creation() {
        let (_tmp, data_root) = setup();
        let store = AssetStore::new(data_root.clone());

        // Directories don't exist yet
        assert!(!data_root.assets_dir("agent-1").exists());

        // Storing a file should create them lazily
        store
            .store_file("agent-1", "test.txt", "text/plain", b"data")
            .await
            .unwrap();

        assert!(data_root.assets_dir("agent-1").exists());
    }

    #[tokio::test]
    async fn test_uuid_filename_preserves_extension() {
        let (_tmp, data_root) = setup();
        let store = AssetStore::new(data_root.clone());

        let attachment = store
            .store_file("agent-1", "photo.jpg", "image/jpeg", b"jpeg data")
            .await
            .unwrap();

        // The stored file should have .jpg extension
        assert!(attachment.file_path.ends_with(".jpg"));
    }

    #[tokio::test]
    async fn test_agents_are_isolated() {
        let (_tmp, data_root) = setup();
        let store = AssetStore::new(data_root);

        store
            .store_file("agent-a", "file.txt", "text/plain", b"a")
            .await
            .unwrap();
        store
            .store_file("agent-b", "file.txt", "text/plain", b"b")
            .await
            .unwrap();

        let a_files = store.list_files("agent-a").await.unwrap();
        let b_files = store.list_files("agent-b").await.unwrap();
        assert_eq!(a_files.len(), 1);
        assert_eq!(b_files.len(), 1);
        assert_ne!(a_files[0].id, b_files[0].id);
    }
}
