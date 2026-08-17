//! Per-entry usage tracking for persisted memory, kept as a sidecar file next
//! to each memory scope's JSONL rather than as fields on `MemoryEntry` itself.
//!
//! Memory entries are append-only and soft-tombstoned (see `ao-persistence`'s
//! `MemoryStore`); rewriting an entry on every read to bump a counter would
//! fight that model and inflate the JSONL on every surface. Instead, each
//! scope file (an agent's JSONL, the global JSONL, a project's JSONL) gets its
//! own `.usage.json` sidecar mapping entry id to `{ use_count, last_used }`.
//! Reads never touch the JSONL; only `increment` writes, and only to the
//! sidecar.
//!
//! The write path mirrors the skill registry's usage counter
//! (`crate::skill_registry::usage`): a per-path async mutex serializes
//! concurrent read-modify-write cycles within this process, and the on-disk
//! update is a temp-file write followed by a rename so a crash mid-write
//! never leaves a torn sidecar.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

/// Extension swapped onto a scope's JSONL path to derive its usage sidecar
/// path, e.g. `agents/foo.jsonl` -> `agents/foo.usage.json`.
const USAGE_SIDECAR_EXTENSION: &str = "usage.json";

/// Usage counters recorded for a single memory entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryUsageEntry {
    pub use_count: u64,
    pub last_used: DateTime<Utc>,
}

/// Entry id -> usage counters, as persisted in a scope's `.usage.json` sidecar.
pub type MemoryUsageMap = HashMap<String, MemoryUsageEntry>;

/// Per-sidecar-path lock so concurrent read-modify-write cycles within this
/// process serialize on the same `.usage.json` file rather than racing.
fn lock_for_path(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = locks.lock().expect("memory_usage lock map poisoned");
    guard
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Derive a memory scope's usage sidecar path from its JSONL path.
///
/// The sidecar lives next to the JSONL file and shares its stem rather than
/// using one shared name per directory: several scopes (every agent's JSONL,
/// every project's JSONL) live side by side in the same directory, so a
/// single flat `.usage.json` per directory would mix unrelated scopes'
/// counters into one map. Deriving the name from the JSONL's own stem keeps
/// each scope's usage counters isolated.
pub fn usage_sidecar_path(scope_jsonl_path: &Path) -> PathBuf {
    scope_jsonl_path.with_extension(USAGE_SIDECAR_EXTENSION)
}

/// Load the usage map for the scope whose JSONL file lives at `scope_path`.
///
/// Returns an empty map if the sidecar is missing, unreadable, or holds
/// invalid JSON — a cold start or corrupt sidecar is never fatal to callers.
/// Never reads or modifies the scope's JSONL entries.
pub async fn load(scope_path: &Path) -> MemoryUsageMap {
    let path = usage_sidecar_path(scope_path);
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => MemoryUsageMap::new(),
    }
}

/// Atomically bump `entry_id`'s `use_count` and stamp `last_used = now` in
/// the sidecar belonging to the scope at `scope_path`.
///
/// `scope_path` is the scope's own JSONL path (e.g. what
/// `DataRoot::memory_agent_path` / `memory_global_path` / `memory_project_path`
/// return) — the sidecar location is derived from it via
/// [`usage_sidecar_path`]. Concurrent callers targeting the same scope
/// serialize on a per-path mutex; the on-disk write is temp-file + rename so
/// a crash mid-write can never corrupt the sidecar.
pub async fn increment(
    scope_path: &Path,
    entry_id: &str,
) -> Result<MemoryUsageEntry, std::io::Error> {
    let path = usage_sidecar_path(scope_path);
    let lock = lock_for_path(&path);
    let _guard = lock.lock().await;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut map: MemoryUsageMap = match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => MemoryUsageMap::new(),
        Err(e) => return Err(e),
    };

    let now = Utc::now();
    let entry = map.entry(entry_id.to_string()).or_insert(MemoryUsageEntry {
        use_count: 0,
        last_used: now,
    });
    entry.use_count += 1;
    entry.last_used = now;
    let snapshot = entry.clone();

    let json = serde_json::to_vec_pretty(&map).map_err(std::io::Error::other)?;

    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    tokio::fs::write(&tmp, &json).await?;
    tokio::fs::rename(&tmp, &path).await?;

    Ok(snapshot)
}

/// Remove `entry_id`'s row from the sidecar belonging to the scope at
/// `scope_path`, if present.
///
/// Delete/tombstone call sites must pair every `MemoryStore::delete*` call
/// with this one — the hard invariant is that a removed entry never leaves
/// an orphaned row in the `.usage.json` sidecar behind. A missing sidecar or
/// an id the sidecar never recorded (an entry that was tombstoned before it
/// was ever surfaced-and-used) are both treated as success, not error: the
/// end state — no row for `entry_id` — already holds.
pub async fn remove_entry(scope_path: &Path, entry_id: &str) -> Result<(), std::io::Error> {
    let path = usage_sidecar_path(scope_path);
    let lock = lock_for_path(&path);
    let _guard = lock.lock().await;

    let mut map: MemoryUsageMap = match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    if map.remove(entry_id).is_none() {
        return Ok(());
    }

    let json = serde_json::to_vec_pretty(&map).map_err(std::io::Error::other)?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    tokio::fs::write(&tmp, &json).await?;
    tokio::fs::rename(&tmp, &path).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A scope's JSONL path need not exist on disk for these functions —
    /// they only ever touch the derived sidecar path, never the JSONL.
    fn fake_scope_path(dir: &Path, name: &str) -> PathBuf {
        dir.join(name).with_extension("jsonl")
    }

    #[test]
    fn sidecar_path_is_derived_per_scope_file() {
        let dir = Path::new("/data/memory/agents");
        let scope = fake_scope_path(dir, "agent-a");
        assert_eq!(
            usage_sidecar_path(&scope),
            Path::new("/data/memory/agents/agent-a.usage.json")
        );

        // A sibling scope file in the same directory gets its own sidecar,
        // not a shared one.
        let sibling = fake_scope_path(dir, "agent-b");
        assert_ne!(usage_sidecar_path(&scope), usage_sidecar_path(&sibling));
    }

    #[tokio::test]
    async fn increment_creates_sidecar_on_first_call() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_path = fake_scope_path(tmp.path(), "agent-a");

        assert!(!usage_sidecar_path(&scope_path).exists());

        let entry = increment(&scope_path, "mem-1").await.unwrap();
        assert_eq!(entry.use_count, 1);

        assert!(usage_sidecar_path(&scope_path).exists());
        let map = load(&scope_path).await;
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("mem-1").unwrap().use_count, 1);
    }

    #[tokio::test]
    async fn increment_bumps_existing_count() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_path = fake_scope_path(tmp.path(), "agent-a");

        for expected in 1..=3 {
            let entry = increment(&scope_path, "mem-1").await.unwrap();
            assert_eq!(entry.use_count, expected);
        }

        let map = load(&scope_path).await;
        assert_eq!(map.get("mem-1").unwrap().use_count, 3);
    }

    #[tokio::test]
    async fn increment_is_per_entry_id() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_path = fake_scope_path(tmp.path(), "agent-a");

        increment(&scope_path, "mem-alpha").await.unwrap();
        increment(&scope_path, "mem-alpha").await.unwrap();
        increment(&scope_path, "mem-beta").await.unwrap();

        let map = load(&scope_path).await;
        assert_eq!(map.get("mem-alpha").unwrap().use_count, 2);
        assert_eq!(map.get("mem-beta").unwrap().use_count, 1);
    }

    #[tokio::test]
    async fn increment_never_touches_the_scope_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_path = fake_scope_path(tmp.path(), "agent-a");
        tokio::fs::create_dir_all(scope_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&scope_path, b"{\"id\":\"mem-1\"}\n")
            .await
            .unwrap();
        let before = tokio::fs::read(&scope_path).await.unwrap();

        increment(&scope_path, "mem-1").await.unwrap();

        let after = tokio::fs::read(&scope_path).await.unwrap();
        assert_eq!(
            before, after,
            "the scope's JSONL must be untouched by increment"
        );
    }

    #[tokio::test]
    async fn load_returns_empty_when_sidecar_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_path = fake_scope_path(tmp.path(), "agent-a");
        let map = load(&scope_path).await;
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn load_returns_empty_on_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_path = fake_scope_path(tmp.path(), "agent-a");
        tokio::fs::write(usage_sidecar_path(&scope_path), b"not valid json")
            .await
            .unwrap();
        let map = load(&scope_path).await;
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn last_used_advances_on_each_increment() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_path = fake_scope_path(tmp.path(), "agent-a");

        let first = increment(&scope_path, "mem-1").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let second = increment(&scope_path, "mem-1").await.unwrap();

        assert!(
            second.last_used >= first.last_used,
            "last_used should advance on each increment"
        );
    }

    #[tokio::test]
    async fn concurrent_increments_do_not_corrupt_the_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_path: Arc<PathBuf> = Arc::new(fake_scope_path(tmp.path(), "agent-a"));

        let n_tasks = 20u64;
        let mut handles = Vec::new();
        for _ in 0..n_tasks {
            let path = Arc::clone(&scope_path);
            handles.push(tokio::spawn(async move {
                increment(&path, "busy").await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let map = load(&scope_path).await;
        assert_eq!(
            map.get("busy").unwrap().use_count,
            n_tasks,
            "concurrent increments should sum to n_tasks without lost updates"
        );

        let bytes = tokio::fs::read(usage_sidecar_path(&scope_path))
            .await
            .unwrap();
        let parsed: MemoryUsageMap = serde_json::from_slice(&bytes)
            .expect("on-disk .usage.json sidecar must remain valid JSON after concurrent writes");
        assert_eq!(parsed.get("busy").unwrap().use_count, n_tasks);
    }

    #[tokio::test]
    async fn concurrent_increments_across_distinct_scopes_stay_isolated() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_a: Arc<PathBuf> = Arc::new(fake_scope_path(tmp.path(), "agent-a"));
        let scope_b: Arc<PathBuf> = Arc::new(fake_scope_path(tmp.path(), "agent-b"));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let path = Arc::clone(&scope_a);
            handles.push(tokio::spawn(async move {
                increment(&path, "mem-1").await.unwrap();
            }));
        }
        for _ in 0..5 {
            let path = Arc::clone(&scope_b);
            handles.push(tokio::spawn(async move {
                increment(&path, "mem-1").await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(load(&scope_a).await.get("mem-1").unwrap().use_count, 10);
        assert_eq!(load(&scope_b).await.get("mem-1").unwrap().use_count, 5);
    }

    #[tokio::test]
    async fn remove_entry_deletes_only_the_matching_row() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_path = fake_scope_path(tmp.path(), "agent-a");

        increment(&scope_path, "mem-keep").await.unwrap();
        increment(&scope_path, "mem-gone").await.unwrap();

        remove_entry(&scope_path, "mem-gone").await.unwrap();

        let map = load(&scope_path).await;
        assert!(!map.contains_key("mem-gone"), "removed id must no longer appear in the sidecar");
        assert!(map.contains_key("mem-keep"), "unrelated ids must be untouched");
    }

    #[tokio::test]
    async fn remove_entry_on_missing_sidecar_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_path = fake_scope_path(tmp.path(), "agent-a");

        assert!(!usage_sidecar_path(&scope_path).exists());
        remove_entry(&scope_path, "never-existed").await.unwrap();
        assert!(
            !usage_sidecar_path(&scope_path).exists(),
            "removing from a sidecar that was never created must not create one"
        );
    }

    #[tokio::test]
    async fn remove_entry_for_unknown_id_leaves_sidecar_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let scope_path = fake_scope_path(tmp.path(), "agent-a");

        increment(&scope_path, "mem-1").await.unwrap();
        remove_entry(&scope_path, "mem-does-not-exist").await.unwrap();

        let map = load(&scope_path).await;
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("mem-1"));
    }
}
