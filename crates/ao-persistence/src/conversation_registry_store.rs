//! Durable, channel-agnostic conversation→thread registry.
//!
//! Storage layout: **one JSON file per `(agent_id, binding_id)` channel
//! binding**, holding a map from an opaque [`ConversationKey`] (composed by
//! the calling channel — Discord's `channel_id`, Telegram's `chat_id`,
//! Email's `sender::normalized_subject`) to a [`ConversationRow`].
//! `(agent_id, binding_id)` is the sharding key: every inbound message a
//! channel transport dispatches already carries its own agent and binding,
//! so reads and writes never need to touch any other binding's file, and one
//! busy binding's rewrite churn can't contend with a quiet one's.
//! `binding_id` alone is not sufficient — it is a fixed constant per channel
//! kind (every Telegram binding is `"telegram"`), so two different agents
//! would otherwise collide on the same file and steal each other's inbound
//! conversations.
//!
//! Modeled directly on
//! [`crate::slack_conversation_registry_store::SlackConversationRegistryStore`],
//! generalized to serve Discord/Telegram/Email behind one shared
//! implementation instead of cloning Slack's store per channel. Slack's own
//! registry is left untouched — sharded by `team_id` rather than
//! `binding_id`, for reasons specific to Slack (see that module's doc
//! comment) — migrating it onto this shape is a documented fast-follow, not
//! part of this phase.
//!
//! # Cap and GC policy
//!
//! See [`MAX_CONVERSATIONS_PER_BINDING`] and [`IDLE_EVICT_AFTER_DAYS`] for
//! the chosen values, copied verbatim in spirit from Slack's registry. Both
//! are enforced by [`apply_gc`], run opportunistically at the end of every
//! [`ConversationRegistryStore::upsert`] and
//! [`ConversationRegistryStore::get_or_create`] call — there is no separate
//! background sweep. Idle eviction runs first (drops anything stale
//! regardless of the cap), then cap eviction trims whatever remains down to
//! the cap, oldest `last_seen_at` first. [`ConversationRegistryStore::gc`]
//! exposes the same pass as a standalone, explicitly-callable operation that
//! returns the evicted rows — the hook a caller holding a `LeaseGate` uses to
//! release an idle-evicted conversation's in-memory lease state (not wired
//! here).

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use ao_protocol::conversation_registry::{ConversationKey, ConversationRow};
use ao_protocol::error::AoError;

use crate::paths::DataRoot;

/// Max conversation rows retained per channel binding before the
/// least-recently-active ones are evicted to make room for a new one.
/// Mirrors `SlackConversationRegistryStore::MAX_CONVERSATIONS_PER_TEAM`'s
/// reasoning: comfortably outlives realistic usage while bounding the
/// per-binding JSON file to a cheap read/GC/rewrite on every inbound
/// message.
pub const MAX_CONVERSATIONS_PER_BINDING: usize = 300;

/// A row untouched for this many days is evicted regardless of whether its
/// binding is anywhere near the cap. Mirrors
/// `SlackConversationRegistryStore::IDLE_EVICT_AFTER_DAYS`.
pub const IDLE_EVICT_AFTER_DAYS: i64 = 30;

type BindingMap = BTreeMap<ConversationKey, ConversationRow>;

/// On-disk store for the generic conversation→thread registry, one
/// [`BindingMap`] per channel binding `binding_id`.
pub struct ConversationRegistryStore {
    data_root: DataRoot,
}

impl ConversationRegistryStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Looks up the row for `(agent_id, binding_id, key)`, or `None` if this
    /// conversation has never been provisioned.
    pub async fn get(
        &self,
        agent_id: &str,
        binding_id: &str,
        key: &ConversationKey,
    ) -> Result<Option<ConversationRow>, AoError> {
        let map = Self::read_binding(&self.data_root, agent_id, binding_id).await?;
        Ok(map.get(key).cloned())
    }

    /// Persists `row` for `(agent_id, binding_id, key)`, overwriting any
    /// prior row, then runs GC (see the module doc) using `now` and returns
    /// whatever it evicted.
    pub async fn upsert(
        &self,
        agent_id: &str,
        binding_id: &str,
        key: ConversationKey,
        row: ConversationRow,
        now: DateTime<Utc>,
    ) -> Result<Vec<ConversationRow>, AoError> {
        let mut map = Self::read_binding(&self.data_root, agent_id, binding_id).await?;
        map.insert(key, row);
        let evicted = apply_gc(&mut map, now);
        Self::write_binding(&self.data_root, agent_id, binding_id, &map).await?;
        Ok(evicted)
    }

    /// Lazily provisions the row for `(agent_id, binding_id, key)`: if one is
    /// already registered, bumps its `last_seen_at` to `now` and returns it
    /// unchanged otherwise; if this is the first inbound message ever seen
    /// for this conversation, calls `mint` exactly once to mint a new bridge
    /// thread id, stores a fresh row, and returns that.
    ///
    /// `agent_id` both locates the on-disk shard (via
    /// [`DataRoot::conversation_registry_path`]) and, on a fresh row, seeds
    /// [`ConversationRow::agent_id`] — the two can never disagree, because
    /// every row in an agent's shard was necessarily created by a call that
    /// passed that same `agent_id`.
    ///
    /// This is the only place a row is created — there is no up-front
    /// provisioning step, matching Slack's "provisioned once, on first
    /// inbound" precedent.
    pub async fn get_or_create(
        &self,
        agent_id: &str,
        binding_id: &str,
        key: ConversationKey,
        now: DateTime<Utc>,
        mint: impl FnOnce() -> String,
    ) -> Result<ConversationRow, AoError> {
        let mut map = Self::read_binding(&self.data_root, agent_id, binding_id).await?;

        let row = match map.get(&key) {
            Some(existing) => {
                let mut row = existing.clone();
                row.last_seen_at = now;
                row
            }
            None => ConversationRow {
                agent_id: agent_id.to_string(),
                thread_id: mint(),
                created_at: now,
                last_seen_at: now,
            },
        };

        map.insert(key, row.clone());
        apply_gc(&mut map, now);
        Self::write_binding(&self.data_root, agent_id, binding_id, &map).await?;
        Ok(row)
    }

    /// Standalone GC pass for `(agent_id, binding_id)`: idle-evicts rows past
    /// [`IDLE_EVICT_AFTER_DAYS`], then enforces [`MAX_CONVERSATIONS_PER_BINDING`]
    /// (LRU by `last_seen_at`), persisting the result and returning every
    /// evicted row. Callable independently of [`Self::upsert`]/
    /// [`Self::get_or_create`] — e.g. by a caller that needs to react to
    /// eviction (release a `LeaseGate`'s in-memory state) rather than just
    /// have it happen silently as part of a write.
    pub async fn gc(&self, agent_id: &str, binding_id: &str, now: DateTime<Utc>) -> Result<Vec<ConversationRow>, AoError> {
        let mut map = Self::read_binding(&self.data_root, agent_id, binding_id).await?;
        let evicted = apply_gc(&mut map, now);
        if !evicted.is_empty() {
            Self::write_binding(&self.data_root, agent_id, binding_id, &map).await?;
        }
        Ok(evicted)
    }

    async fn read_binding(data_root: &DataRoot, agent_id: &str, binding_id: &str) -> Result<BindingMap, AoError> {
        let path = data_root.conversation_registry_path(agent_id, binding_id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(BindingMap::new());
        }
        let bytes = tokio::fs::read(&path).await?;
        let map: BindingMap = serde_json::from_slice(&bytes).map_err(|e| AoError::Json(e.to_string()))?;
        Ok(map)
    }

    async fn write_binding(
        data_root: &DataRoot,
        agent_id: &str,
        binding_id: &str,
        map: &BindingMap,
    ) -> Result<(), AoError> {
        let path = data_root.conversation_registry_path(agent_id, binding_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(map).map_err(|e| AoError::Json(e.to_string()))?;
        let tmp = path.with_file_name(format!(
            "{}.{}.tmp",
            path.file_name().and_then(|f| f.to_str()).unwrap_or("conversation_registry"),
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::write(&tmp, &json).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }
}

/// Applies the cap + GC policy to `map` in place, returning every row it
/// removed. Idle eviction runs first (unconditional on size), then cap
/// eviction trims whatever remains down to [`MAX_CONVERSATIONS_PER_BINDING`],
/// removing the lowest `last_seen_at` entries first. Both passes are cheap
/// at this scale (a few hundred rows at most).
fn apply_gc(map: &mut BindingMap, now: DateTime<Utc>) -> Vec<ConversationRow> {
    let idle_cutoff = now - Duration::days(IDLE_EVICT_AFTER_DAYS);
    let mut evicted = Vec::new();

    let stale_keys: Vec<ConversationKey> = map
        .iter()
        .filter(|(_, row)| row.last_seen_at <= idle_cutoff)
        .map(|(key, _)| key.clone())
        .collect();
    for key in stale_keys {
        if let Some(row) = map.remove(&key) {
            evicted.push(row);
        }
    }

    if map.len() > MAX_CONVERSATIONS_PER_BINDING {
        let mut by_age: Vec<(ConversationKey, DateTime<Utc>)> =
            map.iter().map(|(key, row)| (key.clone(), row.last_seen_at)).collect();
        by_age.sort_by_key(|(_, last_seen_at)| *last_seen_at);

        let excess = map.len() - MAX_CONVERSATIONS_PER_BINDING;
        for (key, _) in by_age.into_iter().take(excess) {
            if let Some(row) = map.remove(&key) {
                evicted.push(row);
            }
        }
    }

    evicted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn store_at(dir: &Path) -> ConversationRegistryStore {
        ConversationRegistryStore::new(DataRoot::new(dir))
    }

    fn row(agent_id: &str, thread_id: &str, at: DateTime<Utc>) -> ConversationRow {
        ConversationRow {
            agent_id: agent_id.to_string(),
            thread_id: thread_id.to_string(),
            created_at: at,
            last_seen_at: at,
        }
    }

    #[tokio::test]
    async fn get_returns_none_when_nothing_persisted_yet() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        assert_eq!(store.get("agent-a", "B1", &ConversationKey::new("C1")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn conversation_row_round_trips_through_persistence() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();
        let r = row("agent-a", "thread-1", now);

        store.upsert("agent-a", "B1", ConversationKey::new("C1"), r.clone(), now).await.unwrap();
        let loaded = store.get("agent-a", "B1", &ConversationKey::new("C1")).await.unwrap();

        assert_eq!(loaded, Some(r));
    }

    #[tokio::test]
    async fn different_bindings_are_isolated_in_separate_files() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        store.upsert("agent-a", "B1", ConversationKey::new("C1"), row("agent-a", "thread-1", now), now).await.unwrap();
        store.upsert("agent-b", "B2", ConversationKey::new("C1"), row("agent-b", "thread-2", now), now).await.unwrap();

        assert_eq!(store.get("agent-a", "B1", &ConversationKey::new("C1")).await.unwrap().unwrap().agent_id, "agent-a");
        assert_eq!(store.get("agent-b", "B2", &ConversationKey::new("C1")).await.unwrap().unwrap().agent_id, "agent-b");
    }

    /// Lazy provisioning: the first inbound message for a conversation
    /// creates exactly one row, and every subsequent inbound message for the
    /// *same* conversation reuses it rather than minting a new thread id —
    /// while a *different* key mints its own, distinct thread.
    #[tokio::test]
    async fn get_or_create_hit_reuses_thread_id_miss_mints_a_new_one() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();
        let calls = Arc::new(AtomicUsize::new(0));

        let make_id = {
            let calls = Arc::clone(&calls);
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                "thread-c1".to_string()
            }
        };
        let first = store
            .get_or_create("agent-a", "B1", ConversationKey::new("C1"), now, make_id)
            .await
            .unwrap();

        let make_id_again = {
            let calls = Arc::clone(&calls);
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                "thread-should-never-be-used".to_string()
            }
        };
        let second = store
            .get_or_create(
                "agent-a",
                "B1",
                ConversationKey::new("C1"),
                now + Duration::seconds(30),
                make_id_again,
            )
            .await
            .unwrap();

        assert_eq!(first.thread_id, "thread-c1");
        assert_eq!(second.thread_id, "thread-c1", "second inbound for the same key must reuse the row created by the first");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "mint must be called exactly once across both same-key calls");
        assert_eq!(second.last_seen_at, now + Duration::seconds(30), "last_seen_at must advance on reuse");

        // A different key in the same binding mints its own, distinct thread.
        let third = store
            .get_or_create("agent-a", "B1", ConversationKey::new("C2"), now, || "thread-c2".to_string())
            .await
            .unwrap();
        assert_eq!(third.thread_id, "thread-c2");
        assert_ne!(third.thread_id, first.thread_id);
    }

    /// Proves the registry cannot grow unbounded: once a binding is at
    /// `MAX_CONVERSATIONS_PER_BINDING`, the next new conversation evicts the
    /// least-recently-active row rather than growing past the cap, and hands
    /// that evicted row back to the caller.
    #[tokio::test]
    async fn upsert_evicts_the_least_recently_active_row_once_over_the_cap() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let base = Utc::now();

        for i in 0..MAX_CONVERSATIONS_PER_BINDING {
            let at = base + Duration::seconds(i as i64);
            let evicted = store
                .upsert("agent-a", "B1", ConversationKey::new(format!("C{i}")), row("agent-a", &format!("thread-{i}"), at), at)
                .await
                .unwrap();
            assert!(evicted.is_empty(), "must not evict anything while at or under the cap");
        }
        assert!(store.get("agent-a", "B1", &ConversationKey::new("C0")).await.unwrap().is_some());

        // One more, strictly more recent than every existing row — pushes
        // the binding over the cap and must evict exactly the oldest (C0),
        // returning it in the evicted vec.
        let overflow_at = base + Duration::seconds(MAX_CONVERSATIONS_PER_BINDING as i64 + 100);
        let evicted = store
            .upsert(
                "agent-a",
                "B1",
                ConversationKey::new("C-overflow"),
                row("agent-a", "thread-overflow", overflow_at),
                overflow_at,
            )
            .await
            .unwrap();

        assert_eq!(evicted.len(), 1, "exactly one row must be evicted once over the cap");
        assert_eq!(evicted[0].thread_id, "thread-0", "the least-recently-active row must be the one evicted");

        assert!(store.get("agent-a", "B1", &ConversationKey::new("C0")).await.unwrap().is_none(), "the evicted row must be gone from the store");
        assert!(store.get("agent-a", "B1", &ConversationKey::new("C1")).await.unwrap().is_some(), "the next-oldest row must survive");
        assert!(store.get("agent-a", "B1", &ConversationKey::new("C-overflow")).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn gc_evicts_rows_idle_past_the_threshold_regardless_of_cap() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        let stale_at = now - Duration::days(IDLE_EVICT_AFTER_DAYS + 1);
        store
            .upsert("agent-a", "B1", ConversationKey::new("C-stale"), row("agent-a", "thread-stale", stale_at), stale_at)
            .await
            .unwrap();

        // A second, unrelated write to the same binding is what actually
        // triggers GC (there is no background sweep — see the module doc).
        let evicted = store
            .upsert("agent-a", "B1", ConversationKey::new("C-fresh"), row("agent-a", "thread-fresh", now), now)
            .await
            .unwrap();

        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].thread_id, "thread-stale");
        assert!(store.get("agent-a", "B1", &ConversationKey::new("C-stale")).await.unwrap().is_none(), "idle-past-threshold row must be GC'd");
        assert!(store.get("agent-a", "B1", &ConversationKey::new("C-fresh")).await.unwrap().is_some());
    }

    /// The standalone `gc()` entry point behaves the same as the pass
    /// embedded in `upsert`/`get_or_create`, but is directly callable (e.g.
    /// by a caller that needs the evicted rows to release other in-memory
    /// state) without itself writing a new row.
    #[tokio::test]
    async fn standalone_gc_returns_evicted_rows_and_persists_the_result() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        let stale_at = now - Duration::days(IDLE_EVICT_AFTER_DAYS + 1);
        store
            .upsert("agent-a", "B1", ConversationKey::new("C-stale"), row("agent-a", "thread-stale", stale_at), stale_at)
            .await
            .unwrap();
        // upsert's own opportunistic GC already ran against `stale_at` as
        // `now`, which is not yet past the cutoff relative to itself — bump
        // the clock forward and invoke `gc` directly to observe the pass.
        let evicted = store.gc("agent-a", "B1", now).await.unwrap();

        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].thread_id, "thread-stale");
        assert!(store.get("agent-a", "B1", &ConversationKey::new("C-stale")).await.unwrap().is_none());

        // A second call has nothing left to evict.
        let evicted_again = store.gc("agent-a", "B1", now).await.unwrap();
        assert!(evicted_again.is_empty());
    }

    /// The bug this whole phase exists to fix: `binding_id` alone is not a
    /// safe registry key. Two DIFFERENT agents ("marketing", "sales") using
    /// the SAME binding_id ("B1") and colliding on the exact SAME
    /// `ConversationKey` (mirrors a Telegram private chat's `chat_id`, which
    /// is the human's own user id and is identical no matter which agent's
    /// bot they're talking to) must each still get their own distinct
    /// thread, and each agent must resolve back to its own thread — never
    /// the other's — on a second lookup. Before the fix, both `upsert` calls
    /// below wrote into the same `B1.json` file and the second silently
    /// clobbered the first.
    #[tokio::test]
    async fn two_agents_can_each_own_a_distinct_thread_under_the_same_binding() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        let shared_key = ConversationKey::new("C-growth-shared-chat-id");
        let marketing = row("agent-marketing", "thread-marketing", now);
        let sales = row("agent-sales", "thread-sales", now);

        store.upsert("agent-marketing", "B1", shared_key.clone(), marketing, now).await.unwrap();
        store.upsert("agent-sales", "B1", shared_key.clone(), sales, now).await.unwrap();

        let looked_up_marketing = store.get("agent-marketing", "B1", &shared_key).await.unwrap().unwrap();
        let looked_up_sales = store.get("agent-sales", "B1", &shared_key).await.unwrap().unwrap();

        assert_eq!(looked_up_marketing.agent_id, "agent-marketing");
        assert_eq!(looked_up_sales.agent_id, "agent-sales");
        assert_ne!(
            looked_up_marketing.thread_id, looked_up_sales.thread_id,
            "two agents sharing a binding_id and a ConversationKey must never collide onto one thread"
        );
    }
}
