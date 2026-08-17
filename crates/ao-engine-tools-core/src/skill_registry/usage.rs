use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

const USAGE_FILE_NAME: &str = ".usage.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillUsageEntry {
    pub count: u64,
    pub last_used: DateTime<Utc>,
}

pub type UsageMap = HashMap<String, SkillUsageEntry>;

/// Per-path lock so concurrent read-modify-write cycles within this process
/// serialize on the same `.usage.json` file.
fn lock_for_path(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = locks.lock().expect("skill_usage lock map poisoned");
    guard
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

pub fn usage_file(skills_dir: &Path) -> PathBuf {
    skills_dir.join(USAGE_FILE_NAME)
}

/// Load the usage map from `skills_dir/.usage.json`. Returns an empty map if
/// the file is missing, unreadable, or contains invalid JSON.
pub async fn load(skills_dir: &Path) -> UsageMap {
    let path = usage_file(skills_dir);
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => UsageMap::new(),
    }
}

/// Atomically increment `skill_id`'s counter and stamp `last_used = now`.
///
/// Uses a per-path mutex to serialize concurrent writes within this process,
/// and a temp-file + rename to avoid torn writes on the final path.
pub async fn increment(
    skills_dir: &Path,
    skill_id: &str,
) -> Result<SkillUsageEntry, std::io::Error> {
    let path = usage_file(skills_dir);
    let lock = lock_for_path(&path);
    let _guard = lock.lock().await;

    tokio::fs::create_dir_all(skills_dir).await?;

    let mut map: UsageMap = match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UsageMap::new(),
        Err(e) => return Err(e),
    };

    let now = Utc::now();
    let entry = map
        .entry(skill_id.to_string())
        .or_insert(SkillUsageEntry {
            count: 0,
            last_used: now,
        });
    entry.count += 1;
    entry.last_used = now;
    let snapshot = entry.clone();

    let json = serde_json::to_vec_pretty(&map)
        .map_err(std::io::Error::other)?;

    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    tokio::fs::write(&tmp, &json).await?;
    tokio::fs::rename(&tmp, &path).await?;

    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_increment_creates_file_on_first_call() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path();

        assert!(!usage_file(skills_dir).exists());

        let entry = increment(skills_dir, "my-skill").await.unwrap();
        assert_eq!(entry.count, 1);

        assert!(usage_file(skills_dir).exists());
        let map = load(skills_dir).await;
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("my-skill").unwrap().count, 1);
    }

    #[tokio::test]
    async fn test_increment_bumps_existing_count() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path();

        for expected in 1..=3 {
            let entry = increment(skills_dir, "repeat").await.unwrap();
            assert_eq!(entry.count, expected);
        }

        let map = load(skills_dir).await;
        assert_eq!(map.get("repeat").unwrap().count, 3);
    }

    #[tokio::test]
    async fn test_increment_is_per_skill_id() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path();

        increment(skills_dir, "alpha").await.unwrap();
        increment(skills_dir, "alpha").await.unwrap();
        increment(skills_dir, "beta").await.unwrap();

        let map = load(skills_dir).await;
        assert_eq!(map.get("alpha").unwrap().count, 2);
        assert_eq!(map.get("beta").unwrap().count, 1);
    }

    #[tokio::test]
    async fn test_load_returns_empty_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let map = load(tmp.path()).await;
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn test_load_returns_empty_on_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(usage_file(tmp.path()), b"not valid json")
            .await
            .unwrap();
        let map = load(tmp.path()).await;
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn test_concurrent_increments_do_not_corrupt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir: Arc<PathBuf> = Arc::new(tmp.path().to_path_buf());

        let n_tasks = 20u64;
        let mut handles = Vec::new();
        for _ in 0..n_tasks {
            let dir = Arc::clone(&skills_dir);
            handles.push(tokio::spawn(async move {
                increment(&dir, "busy").await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let map = load(&skills_dir).await;
        assert_eq!(
            map.get("busy").unwrap().count,
            n_tasks,
            "concurrent increments should sum to n_tasks without lost updates"
        );

        let bytes = tokio::fs::read(usage_file(&skills_dir)).await.unwrap();
        let parsed: UsageMap = serde_json::from_slice(&bytes)
            .expect("on-disk .usage.json must remain valid JSON after concurrent writes");
        assert_eq!(parsed.get("busy").unwrap().count, n_tasks);
    }

    #[tokio::test]
    async fn test_last_used_is_updated_on_increment() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path();

        let first = increment(skills_dir, "stamped").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let second = increment(skills_dir, "stamped").await.unwrap();

        assert!(
            second.last_used >= first.last_used,
            "last_used should advance on each increment"
        );
    }
}
