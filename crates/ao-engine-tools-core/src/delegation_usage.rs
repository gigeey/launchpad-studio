use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;
use std::collections::HashMap;

const USAGE_FILE_NAME: &str = "delegation_usage.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DelegationUsageEntry {
    pub delegate_count: u64,
    pub agent_count: u64,
    pub last_used: Option<DateTime<Utc>>,
}

fn lock_for_path(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = locks.lock().expect("delegation_usage lock map poisoned");
    guard
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

pub fn usage_file(agent_data_dir: &Path) -> PathBuf {
    agent_data_dir.join(USAGE_FILE_NAME)
}

/// Load the entry from `agent_data_dir/delegation_usage.json`. Returns a
/// zero-counter default if the file is missing or contains invalid JSON.
pub async fn load(agent_data_dir: &Path) -> DelegationUsageEntry {
    let path = usage_file(agent_data_dir);
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => DelegationUsageEntry::default(),
    }
}

/// Atomically increment the `delegate_count` and stamp `last_used = now`.
pub async fn increment_delegate(
    agent_data_dir: &Path,
) -> Result<DelegationUsageEntry, std::io::Error> {
    increment_inner(agent_data_dir, true).await
}

/// Atomically increment the `agent_count` and stamp `last_used = now`.
pub async fn increment_agent(
    agent_data_dir: &Path,
) -> Result<DelegationUsageEntry, std::io::Error> {
    increment_inner(agent_data_dir, false).await
}

async fn increment_inner(
    agent_data_dir: &Path,
    is_delegate: bool,
) -> Result<DelegationUsageEntry, std::io::Error> {
    let path = usage_file(agent_data_dir);
    let lock = lock_for_path(&path);
    let _guard = lock.lock().await;

    tokio::fs::create_dir_all(agent_data_dir).await?;

    let mut entry: DelegationUsageEntry = match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DelegationUsageEntry::default(),
        Err(e) => return Err(e),
    };

    let now = Utc::now();
    if is_delegate {
        entry.delegate_count += 1;
    } else {
        entry.agent_count += 1;
    }
    entry.last_used = Some(now);

    let json = serde_json::to_vec_pretty(&entry).map_err(std::io::Error::other)?;

    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    tokio::fs::write(&tmp, &json).await?;
    tokio::fs::rename(&tmp, &path).await?;

    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_load_returns_zero_default_on_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = load(tmp.path()).await;
        assert_eq!(entry.delegate_count, 0);
        assert_eq!(entry.agent_count, 0);
        assert!(entry.last_used.is_none());
    }

    #[tokio::test]
    async fn test_increment_delegate_persists_and_reads_back() {
        let tmp = tempfile::tempdir().unwrap();

        let e1 = increment_delegate(tmp.path()).await.unwrap();
        assert_eq!(e1.delegate_count, 1);
        assert_eq!(e1.agent_count, 0);

        let e2 = increment_delegate(tmp.path()).await.unwrap();
        assert_eq!(e2.delegate_count, 2);
        assert_eq!(e2.agent_count, 0);

        let loaded = load(tmp.path()).await;
        assert_eq!(loaded.delegate_count, 2);
        assert_eq!(loaded.agent_count, 0);
    }

    #[tokio::test]
    async fn test_increment_agent_persists_and_reads_back() {
        let tmp = tempfile::tempdir().unwrap();

        let e1 = increment_agent(tmp.path()).await.unwrap();
        assert_eq!(e1.agent_count, 1);
        assert_eq!(e1.delegate_count, 0);

        let e2 = increment_agent(tmp.path()).await.unwrap();
        assert_eq!(e2.agent_count, 2);
        assert_eq!(e2.delegate_count, 0);

        let loaded = load(tmp.path()).await;
        assert_eq!(loaded.agent_count, 2);
        assert_eq!(loaded.delegate_count, 0);
    }

    #[tokio::test]
    async fn test_concurrent_increments_delegate_and_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir: Arc<PathBuf> = Arc::new(tmp.path().to_path_buf());

        let n = 10usize;
        let mut handles = Vec::new();
        for _ in 0..n {
            let d = Arc::clone(&dir);
            handles.push(tokio::spawn(async move {
                increment_delegate(&d).await.unwrap();
            }));
        }
        for _ in 0..n {
            let d = Arc::clone(&dir);
            handles.push(tokio::spawn(async move {
                increment_agent(&d).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let loaded = load(&dir).await;
        assert_eq!(
            loaded.delegate_count, n as u64,
            "delegate_count should equal n concurrent delegate increments"
        );
        assert_eq!(
            loaded.agent_count, n as u64,
            "agent_count should equal n concurrent agent increments"
        );
    }
}
