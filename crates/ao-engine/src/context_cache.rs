/// In-memory cache for computed agent context with TTL expiration.
///
/// Keyed by (agent_id, effective_cwd, agent_home) so that context is reused
/// across message turns without re-reading files from disk.
///
/// Cache entries also record the agent profile file's mtime at insertion time.
/// On every lookup, the caller supplies the current mtime; if it differs from
/// the stored mtime the entry is treated as stale and a miss is returned.  This
/// ensures that competency changes (skill toggles, etc.) are visible on the very
/// next turn without requiring callers to explicitly invalidate the cache.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use tokio::sync::RwLock;

use ao_engine_tools_core::AgentProfileCacheInvalidator;
use ao_protocol::system_prompt_context::{AgentHomeContext, WorkspaceContext};

/// Default TTL for cache entries: 5 minutes.
const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);

/// Cache key combining agent identity and directory paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextCacheKey {
    pub agent_id: String,
    pub effective_cwd: PathBuf,
    pub agent_home: PathBuf,
}

/// Cached context payload containing all computed context for an agent.
#[derive(Debug, Clone)]
pub struct CachedContext {
    pub agent_home_context: AgentHomeContext,
    pub workspace_context: WorkspaceContext,
}

/// A single cache entry with its creation timestamp and the agent profile mtime
/// captured at insertion time.
#[derive(Debug, Clone)]
struct CacheEntry {
    context: CachedContext,
    created_at: Instant,
    ttl: Duration,
    /// Mtime of `agents/{agent_id}.yaml` when this entry was stored.
    /// `None` when the stat was unavailable (e.g. profile not yet on disk).
    profile_mtime: Option<SystemTime>,
}

impl CacheEntry {
    fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.ttl
    }

    /// Returns `true` when the current profile mtime differs from the one
    /// recorded at insertion, indicating that the agent profile was modified
    /// (e.g. a skill was toggled) after this entry was built.
    ///
    /// If either mtime is unavailable the comparison is skipped and the entry
    /// is not considered stale — TTL expiration remains the fallback.
    fn is_profile_changed(&self, current_mtime: Option<SystemTime>) -> bool {
        match (self.profile_mtime, current_mtime) {
            (Some(stored), Some(current)) => stored != current,
            _ => false,
        }
    }
}

/// Thread-safe in-memory context cache with TTL-based expiration.
#[derive(Debug, Clone)]
pub struct ContextCache {
    entries: Arc<RwLock<HashMap<ContextCacheKey, CacheEntry>>>,
    ttl: Duration,
}

impl ContextCache {
    /// Create a new cache with the default 5-minute TTL.
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            ttl: DEFAULT_TTL,
        }
    }

    /// Create a new cache with a custom TTL (useful for testing).
    #[cfg(test)]
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            ttl,
        }
    }

    /// Get cached context for the given key, returning `None` if:
    /// - the key is absent,
    /// - the entry has exceeded its TTL, or
    /// - `profile_mtime` differs from the mtime recorded when the entry was stored
    ///   (meaning the agent profile was modified after this entry was built).
    pub async fn get(
        &self,
        key: &ContextCacheKey,
        profile_mtime: Option<SystemTime>,
    ) -> Option<CachedContext> {
        let entries = self.entries.read().await;
        match entries.get(key) {
            Some(entry)
                if !entry.is_expired() && !entry.is_profile_changed(profile_mtime) =>
            {
                Some(entry.context.clone())
            }
            _ => None,
        }
    }

    /// Store computed context in the cache, recording the current agent profile
    /// mtime so future lookups can detect stale entries without a full reload.
    pub async fn set(
        &self,
        key: ContextCacheKey,
        context: CachedContext,
        profile_mtime: Option<SystemTime>,
    ) {
        let mut entries = self.entries.write().await;
        entries.insert(
            key,
            CacheEntry {
                context,
                created_at: Instant::now(),
                ttl: self.ttl,
                profile_mtime,
            },
        );
    }

    /// Invalidate all cache entries for a given agent_id.
    pub async fn invalidate(&self, agent_id: &str) {
        let mut entries = self.entries.write().await;
        entries.retain(|key, _| key.agent_id != agent_id);
    }
}

impl Default for ContextCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Lets tools that only see the `ao-engine-tools-core` trait surface (e.g.
/// `AgentAuthor`, injected via `Arc<dyn AgentProfileCacheInvalidator>`)
/// invalidate this cache without depending on this crate directly.
#[async_trait]
impl AgentProfileCacheInvalidator for ContextCache {
    async fn invalidate(&self, agent_id: &str) {
        ContextCache::invalidate(self, agent_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::system_prompt_context::{AgentHomeContext, WorkspaceContext};

    fn make_key(agent_id: &str, cwd: &str, home: &str) -> ContextCacheKey {
        ContextCacheKey {
            agent_id: agent_id.to_string(),
            effective_cwd: PathBuf::from(cwd),
            agent_home: PathBuf::from(home),
        }
    }

    fn make_context() -> CachedContext {
        CachedContext {
            agent_home_context: AgentHomeContext {
                claude_md_content: Some("Be helpful.".to_string()),
                rules: vec![],
                skills: vec!["React Patterns — Use hooks.".to_string()],
                skills_block: None,
            },
            workspace_context: WorkspaceContext {
                root_path: "/project".to_string(),
                claude_md_content: Some("Project rules.".to_string()),
                rules: vec![],
            },
        }
    }

    #[tokio::test]
    async fn test_get_returns_none_on_empty_cache() {
        let cache = ContextCache::new();
        let key = make_key("agent-1", "/project", "/home/agent-1");
        assert!(cache.get(&key, None).await.is_none());
    }

    #[tokio::test]
    async fn test_set_and_get() {
        let cache = ContextCache::new();
        let key = make_key("agent-1", "/project", "/home/agent-1");
        let ctx = make_context();

        cache.set(key.clone(), ctx.clone(), None).await;
        let cached = cache.get(&key, None).await;

        assert!(cached.is_some());
        let cached = cached.unwrap();
        assert_eq!(cached.agent_home_context.skills.len(), 1);
        assert!(cached.workspace_context.claude_md_content.is_some());
    }

    #[tokio::test]
    async fn test_different_keys_are_independent() {
        let cache = ContextCache::new();
        let key1 = make_key("agent-1", "/project-a", "/home/agent-1");
        let key2 = make_key("agent-1", "/project-b", "/home/agent-1");
        let ctx = make_context();

        cache.set(key1.clone(), ctx, None).await;

        assert!(cache.get(&key1, None).await.is_some());
        assert!(cache.get(&key2, None).await.is_none());
    }

    #[tokio::test]
    async fn test_expired_entry_returns_none() {
        let cache = ContextCache::with_ttl(Duration::from_millis(1));
        let key = make_key("agent-1", "/project", "/home/agent-1");

        cache.set(key.clone(), make_context(), None).await;
        // Wait for expiration
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(cache.get(&key, None).await.is_none());
    }

    #[tokio::test]
    async fn test_invalidate_removes_all_entries_for_agent() {
        let cache = ContextCache::new();
        let key1 = make_key("agent-1", "/project-a", "/home/agent-1");
        let key2 = make_key("agent-1", "/project-b", "/home/agent-1");
        let key3 = make_key("agent-2", "/project-a", "/home/agent-2");
        let ctx = make_context();

        cache.set(key1.clone(), ctx.clone(), None).await;
        cache.set(key2.clone(), ctx.clone(), None).await;
        cache.set(key3.clone(), ctx, None).await;

        cache.invalidate("agent-1").await;

        assert!(cache.get(&key1, None).await.is_none());
        assert!(cache.get(&key2, None).await.is_none());
        assert!(cache.get(&key3, None).await.is_some());
    }

    #[tokio::test]
    async fn test_set_overwrites_existing_entry() {
        let cache = ContextCache::new();
        let key = make_key("agent-1", "/project", "/home/agent-1");

        let ctx1 = CachedContext {
            agent_home_context: AgentHomeContext {
                claude_md_content: None,
                rules: vec![],
                skills: vec!["old skill content".to_string()],
                skills_block: None,
            },
            workspace_context: WorkspaceContext {
                root_path: "/project".to_string(),
                claude_md_content: None,
                rules: vec![],
            },
        };
        cache.set(key.clone(), ctx1, None).await;

        let ctx2 = CachedContext {
            agent_home_context: AgentHomeContext {
                claude_md_content: None,
                rules: vec![],
                skills: vec![
                    "new-a skill content".to_string(),
                    "new-b skill content".to_string(),
                ],
                skills_block: None,
            },
            workspace_context: WorkspaceContext {
                root_path: "/project".to_string(),
                claude_md_content: None,
                rules: vec![],
            },
        };
        cache.set(key.clone(), ctx2, None).await;

        let cached = cache.get(&key, None).await.unwrap();
        assert_eq!(cached.agent_home_context.skills.len(), 2);
        assert_eq!(cached.agent_home_context.skills[0], "new-a skill content");
    }

    #[tokio::test]
    async fn test_default_creates_new_cache() {
        let cache = ContextCache::default();
        let key = make_key("agent-1", "/project", "/home/agent-1");
        assert!(cache.get(&key, None).await.is_none());
    }

    #[tokio::test]
    async fn test_profile_mtime_change_evicts_entry() {
        let cache = ContextCache::new();
        let key = make_key("agent-1", "/project", "/home/agent-1");

        let mtime_a = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mtime_b = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_001);

        cache.set(key.clone(), make_context(), Some(mtime_a)).await;

        // Same mtime — should hit
        assert!(cache.get(&key, Some(mtime_a)).await.is_some());

        // Different mtime — profile changed, should miss
        assert!(cache.get(&key, Some(mtime_b)).await.is_none());
    }

    #[tokio::test]
    async fn test_profile_mtime_none_falls_back_to_ttl() {
        let cache = ContextCache::new();
        let key = make_key("agent-1", "/project", "/home/agent-1");

        let mtime_a = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);

        // Stored with a known mtime, looked up with None — should still hit
        // (unavailable current mtime is not treated as a change).
        cache.set(key.clone(), make_context(), Some(mtime_a)).await;
        assert!(cache.get(&key, None).await.is_some());

        // Stored with None, looked up with a mtime — same, no false eviction.
        cache.set(key.clone(), make_context(), None).await;
        assert!(cache.get(&key, Some(mtime_a)).await.is_some());
    }
}
