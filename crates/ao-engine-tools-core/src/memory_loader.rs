use std::cmp::Ordering;
use std::sync::Arc;

use ao_persistence::memory::MemoryStore;
use ao_protocol::memory::MemoryEntry;
use ao_search_index::{ArtifactKind, IndexScope, SearchFilter, SearchIndex};

/// Loads and formats the agent's persisted memory as a text blob for injection
/// into the system prompt.
///
/// Sharing the same `Arc<dyn MemoryLoader>` between parent and child ensures
/// the child resolves memory through the same loader instance — guaranteeing
/// memory parity across all five categories (user, feedback, project,
/// reference, global) at the spawn boundary without duplicating configuration.
///
/// # Object safety
///
/// The trait is object-safe; callers hold it as
/// `Arc<dyn MemoryLoader + Send + Sync>`.
pub trait MemoryLoader: Send + Sync {
    /// Assemble the full memory blob for injection into the system prompt.
    ///
    /// Returns an empty string when no memories are present. The format is
    /// the implementor's concern; the only contract is that the returned
    /// string is suitable for direct concatenation into the resolved system
    /// prompt.
    ///
    /// TODO(memory-usage): this trait returns an opaque pre-composed string with no
    /// per-entry visibility, so it cannot drive usage instrumentation itself.
    /// Today's real memory-surfacing path (fetch agent/project/global
    /// entries, then hand them to the system prompt composer) lives in
    /// `ao-engine`'s agent runners, not behind this trait — see the
    /// `crate::memory_usage::increment` calls next to the memory fetches in
    /// `ao-engine/src/agent_runner/native.rs` and
    /// `ao-engine/src/agent_runner/cli.rs`. When a real (non-Noop/Static)
    /// `MemoryLoader` implementation lands and starts backing this trait with
    /// actual entries (e.g. for subagent spawn parity), thread usage bumps
    /// through here too so every surface path stays covered.
    fn load_memory_blob(&self) -> String;
}

/// No-op [`MemoryLoader`] that returns an empty blob.
///
/// Used as the default in [`RunnerContext`](crate::context::RunnerContext) so
/// that contexts without a wired-up persistence layer still compile and
/// operate — they simply produce no memory injection.
pub struct NoopMemoryLoader;

impl MemoryLoader for NoopMemoryLoader {
    fn load_memory_blob(&self) -> String {
        String::new()
    }
}

/// A [`MemoryLoader`] backed by a pre-loaded string. Useful in tests that
/// need to assert specific memory content without a real persistence layer.
pub struct StaticMemoryLoader {
    blob: String,
}

impl StaticMemoryLoader {
    pub fn new(blob: impl Into<String>) -> Arc<Self> {
        Arc::new(Self { blob: blob.into() })
    }
}

impl MemoryLoader for StaticMemoryLoader {
    fn load_memory_blob(&self) -> String {
        self.blob.clone()
    }
}

/// Ranked hits considered from a single scope before the merged list is
/// formatted, when the shared FTS5 index has a `Memory` row for at least
/// one scope. Capping per scope (rather than taking a single global top-K
/// across the merged set) keeps a noisy agent scope from crowding out a
/// project or global entry that is the best match *within its own scope*.
pub const DEFAULT_TOP_K_PER_SCOPE: usize = 8;

/// A [`MemoryLoader`] that ranks agent/project/global memory entries against
/// a query via the shared FTS5 search index and formats the merged top hits
/// into a blob, instead of dumping every live entry unranked.
///
/// [`Self::build`] does all the async persistence/index work once, up front,
/// and caches the resulting blob — [`MemoryLoader::load_memory_blob`] itself
/// stays a synchronous clone of that cached string, so this type slots into
/// the existing trait (and every current call site, e.g. the subagent spawn
/// boundary in `background_agents::spawner`) without any signature changes.
///
/// Falls back to formatting every live entry across all three scopes,
/// unranked, when the index has no `Memory` rows yet at all (a cold-started
/// data root, or an index file that predates this feature) or when no index
/// was supplied — a subagent spawned before the index catches up still
/// receives every memory rather than none. A populated index that simply
/// finds no match for this query is a different case: that surfaces as an
/// empty blob, not a fallback, since the index has already spoken.
pub struct IndexedMemoryLoader {
    blob: String,
}

impl IndexedMemoryLoader {
    /// Build a loader ranking against `query`, using [`DEFAULT_TOP_K_PER_SCOPE`]
    /// as the per-scope retrieval budget.
    pub async fn build(
        memory_store: &MemoryStore,
        index: Option<&SearchIndex>,
        agent_id: &str,
        project_key: Option<&str>,
        query: &str,
    ) -> Arc<Self> {
        Self::build_with_limit(
            memory_store,
            index,
            agent_id,
            project_key,
            query,
            DEFAULT_TOP_K_PER_SCOPE,
        )
        .await
    }

    /// Same as [`Self::build`] with an explicit per-scope retrieval budget —
    /// mainly useful for tests that want a small corpus to exceed the cap
    /// without constructing dozens of entries.
    pub async fn build_with_limit(
        memory_store: &MemoryStore,
        index: Option<&SearchIndex>,
        agent_id: &str,
        project_key: Option<&str>,
        query: &str,
        top_k_per_scope: usize,
    ) -> Arc<Self> {
        // Failures reading a scope are treated the same as "nothing there
        // yet" — mirrors the resilience the real system-prompt call sites
        // already apply to these same three fetches (see the native/CLI
        // agent runners).
        let agent_entries = memory_store.list(agent_id).await.unwrap_or_default();
        let global_entries = memory_store.list_global().await.unwrap_or_default();
        let project_entries = match project_key {
            Some(key) => memory_store.list_project(key).await.unwrap_or_default(),
            None => Vec::new(),
        };

        let index_is_usable = match index {
            None => false,
            Some(idx) => !idx.is_artifact_empty(ArtifactKind::Memory).await.unwrap_or(true),
        };

        let blob = if !index_is_usable {
            format_full_blob(&agent_entries, &project_entries, &global_entries)
        } else {
            let idx = index.expect("index_is_usable implies index is Some");
            let ranked = rank_across_scopes(
                idx,
                query,
                top_k_per_scope,
                agent_id,
                project_key,
                &agent_entries,
                &project_entries,
                &global_entries,
            )
            .await;
            format_ranked_blob(&ranked)
        };

        Arc::new(Self { blob })
    }
}

impl MemoryLoader for IndexedMemoryLoader {
    fn load_memory_blob(&self) -> String {
        self.blob.clone()
    }
}

/// Run one ranked query per populated scope (capped at `top_k_per_scope`
/// hits each), map each hit's id back to its full entry, and return the
/// merged set — unsorted; callers order by score.
#[allow(clippy::too_many_arguments)]
async fn rank_across_scopes<'a>(
    index: &SearchIndex,
    query: &str,
    top_k_per_scope: usize,
    agent_id: &str,
    project_key: Option<&str>,
    agent_entries: &'a [MemoryEntry],
    project_entries: &'a [MemoryEntry],
    global_entries: &'a [MemoryEntry],
) -> Vec<(f64, &'static str, &'a MemoryEntry)> {
    let mut ranked = Vec::new();

    let agent_filter = SearchFilter::new()
        .with_scope(IndexScope::Agent(agent_id.to_string()))
        .with_artifact(ArtifactKind::Memory);
    let agent_hits = index
        .query(query.to_string(), agent_filter, top_k_per_scope)
        .await
        .unwrap_or_default();
    for hit in agent_hits {
        if let Some(entry) = find_entry(agent_entries, &hit.id) {
            ranked.push((hit.score, "agent", entry));
        }
    }

    if let Some(key) = project_key {
        let project_filter = SearchFilter::new()
            .with_scope(IndexScope::Project(key.to_string()))
            .with_artifact(ArtifactKind::Memory);
        let project_hits = index
            .query(query.to_string(), project_filter, top_k_per_scope)
            .await
            .unwrap_or_default();
        for hit in project_hits {
            if let Some(entry) = find_entry(project_entries, &hit.id) {
                ranked.push((hit.score, "project", entry));
            }
        }
    }

    let global_filter = SearchFilter::new()
        .with_scope(IndexScope::Global)
        .with_artifact(ArtifactKind::Memory);
    let global_hits = index
        .query(query.to_string(), global_filter, top_k_per_scope)
        .await
        .unwrap_or_default();
    for hit in global_hits {
        if let Some(entry) = find_entry(global_entries, &hit.id) {
            ranked.push((hit.score, "global", entry));
        }
    }

    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
    ranked
}

fn find_entry<'a>(entries: &'a [MemoryEntry], id: &str) -> Option<&'a MemoryEntry> {
    entries.iter().find(|e| e.id == id)
}

/// Format every live entry across all three scopes, unranked — the
/// full-blob fallback used when no usable index is available.
fn format_full_blob(
    agent_entries: &[MemoryEntry],
    project_entries: &[MemoryEntry],
    global_entries: &[MemoryEntry],
) -> String {
    let lines: Vec<String> = agent_entries
        .iter()
        .map(|e| ("agent", e))
        .chain(project_entries.iter().map(|e| ("project", e)))
        .chain(global_entries.iter().map(|e| ("global", e)))
        .map(|(scope, entry)| format!("- [{scope}] {}", entry.content))
        .collect();
    wrap_lines(lines)
}

/// Format a score-ordered ranked list into the same line shape
/// [`format_full_blob`] produces, so the injected format is identical
/// whichever path produced it.
fn format_ranked_blob(ranked: &[(f64, &'static str, &MemoryEntry)]) -> String {
    let lines: Vec<String> = ranked
        .iter()
        .map(|(_, scope, entry)| format!("- [{scope}] {}", entry.content))
        .collect();
    wrap_lines(lines)
}

fn wrap_lines(lines: Vec<String>) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("# Memory\n{}", lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_loader_returns_empty_string() {
        let loader = NoopMemoryLoader;
        assert_eq!(loader.load_memory_blob(), "");
    }

    #[test]
    fn static_loader_returns_preset_blob() {
        let loader = StaticMemoryLoader::new("user: expert\nproject: phase4");
        assert_eq!(loader.load_memory_blob(), "user: expert\nproject: phase4");
    }

    #[test]
    fn static_loader_is_object_safe() {
        let loader: Arc<dyn MemoryLoader> = StaticMemoryLoader::new("sentinel");
        assert_eq!(loader.load_memory_blob(), "sentinel");
    }

    // --- IndexedMemoryLoader ---

    use ao_persistence::paths::DataRoot;
    use ao_protocol::memory::MemorySource;

    fn make_store(tmp: &tempfile::TempDir) -> MemoryStore {
        MemoryStore::new(DataRoot::new(tmp.path()))
    }

    #[tokio::test]
    async fn falls_back_to_full_blob_when_no_index_is_supplied() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        store.add("agent-1", "agent fact one", MemorySource::Agent).await.unwrap();
        store.add_global("global fact one", MemorySource::Manual).await.unwrap();
        store
            .add_project("proj-hash", "project fact one", MemorySource::Manual)
            .await
            .unwrap();

        let loader =
            IndexedMemoryLoader::build(&store, None, "agent-1", Some("proj-hash"), "irrelevant query")
                .await;
        let blob = loader.load_memory_blob();

        assert!(blob.contains("agent fact one"));
        assert!(blob.contains("project fact one"));
        assert!(blob.contains("global fact one"));
    }

    #[tokio::test]
    async fn falls_back_to_full_blob_when_index_has_no_memory_rows_yet() {
        let tmp = tempfile::tempdir().unwrap();
        // Deliberately not `.with_index(..)` — entries land only in the
        // JSONL log, mirroring a data root whose index hasn't been
        // backfilled (`MemoryStore::reindex_all`) yet.
        let store = make_store(&tmp);
        store.add("agent-1", "unindexed agent fact", MemorySource::Agent).await.unwrap();
        store.add_global("unindexed global fact", MemorySource::Manual).await.unwrap();

        let index = SearchIndex::open_in_memory().unwrap();
        let loader =
            IndexedMemoryLoader::build(&store, Some(&index), "agent-1", None, "some query").await;
        let blob = loader.load_memory_blob();

        assert!(blob.contains("unindexed agent fact"));
        assert!(blob.contains("unindexed global fact"));
    }

    #[tokio::test]
    async fn empty_everything_yields_empty_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let loader = IndexedMemoryLoader::build(&store, None, "agent-1", None, "query").await;
        assert_eq!(loader.load_memory_blob(), "");
    }

    #[tokio::test]
    async fn populated_index_with_no_match_yields_empty_blob_not_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let index = SearchIndex::open_in_memory().unwrap();
        let store = make_store(&tmp).with_index(index.clone());
        store.add("agent-1", "completely unrelated content", MemorySource::Agent).await.unwrap();

        let loader = IndexedMemoryLoader::build(
            &store,
            Some(&index),
            "agent-1",
            None,
            "zzz_no_such_term_zzz",
        )
        .await;

        assert_eq!(
            loader.load_memory_blob(),
            "",
            "a populated index that matches nothing must not fall back to the full blob"
        );
    }

    #[tokio::test]
    async fn ranked_retrieval_keeps_only_top_k_within_a_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let index = SearchIndex::open_in_memory().unwrap();
        let store = make_store(&tmp).with_index(index.clone());

        store.add("agent-1", "alpha beta gamma", MemorySource::Agent).await.unwrap();
        store.add("agent-1", "alpha beta", MemorySource::Agent).await.unwrap();
        store.add("agent-1", "alpha", MemorySource::Agent).await.unwrap();

        let loader =
            IndexedMemoryLoader::build_with_limit(&store, Some(&index), "agent-1", None, "alpha beta gamma", 1)
                .await;
        let blob = loader.load_memory_blob();
        let lines: Vec<&str> = blob.lines().collect();

        assert_eq!(
            lines,
            vec!["# Memory", "- [agent] alpha beta gamma"],
            "only the best-matching entry survives a per-scope cap of 1; got:\n{blob}"
        );
    }

    #[tokio::test]
    async fn scope_merge_gives_every_scope_its_own_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let index = SearchIndex::open_in_memory().unwrap();
        let store = make_store(&tmp).with_index(index.clone());

        // Three agent entries all outrank the lone project/global entries on
        // term-match count — a single global top-K by score would starve
        // project/global out entirely. Scope-aware merging must not let that
        // happen: each scope gets its own top-1 budget.
        store.add("agent-1", "zeta eta theta", MemorySource::Agent).await.unwrap();
        store.add("agent-1", "zeta eta", MemorySource::Agent).await.unwrap();
        store.add("agent-1", "zeta", MemorySource::Agent).await.unwrap();
        store.add_project("proj-hash", "eta", MemorySource::Manual).await.unwrap();
        store.add_global("theta", MemorySource::Manual).await.unwrap();

        let loader = IndexedMemoryLoader::build_with_limit(
            &store,
            Some(&index),
            "agent-1",
            Some("proj-hash"),
            "zeta eta theta",
            1,
        )
        .await;
        let blob = loader.load_memory_blob();
        let lines: std::collections::HashSet<&str> = blob.lines().collect();

        assert!(lines.contains("- [agent] zeta eta theta"), "the top agent hit must be present; got:\n{blob}");
        assert!(
            lines.contains("- [project] eta"),
            "project's own top hit must survive despite scoring below excluded agent entries; got:\n{blob}"
        );
        assert!(
            lines.contains("- [global] theta"),
            "global's own top hit must survive despite scoring below excluded agent entries; got:\n{blob}"
        );
        assert_eq!(
            blob.lines().count(),
            4,
            "expected exactly the header plus one line per scope; got:\n{blob}"
        );
    }
}
