//! Durable conversation→thread registry persistence.
//!
//! Storage layout: **one JSON file per Slack workspace (`team_id`)**,
//! holding a map from a composed `channel_id[:thread_ts]` string to a
//! [`SlackConversationRow`]. `team_id` is the sharding key — every Slack
//! event that ever triggers a lookup already carries its own `team_id`, so
//! reads and writes never need to touch any other workspace's file, and one
//! busy workspace's rewrite churn can't contend with a quiet one's. This
//! differs from [`crate::channel_cursor_store::ChannelCursorStore`]'s
//! one-file-per-`(agent_id, binding_id)` layout on purpose: the whole point
//! of the workspace-wide key is that this registry is *not* scoped to a
//! binding, so there
//! is no `(agent_id, binding_id)` pair to key a file by in the first place.
//! Consolidating per team (rather than one file per individual conversation)
//! is what makes the cap + GC policy below a cheap read-evict-write of one
//! small file instead of a directory scan across potentially hundreds of
//! per-conversation files just to find the oldest one.
//!
//! # Cap and GC policy
//!
//! See [`MAX_CONVERSATIONS_PER_TEAM`] and [`IDLE_EVICT_AFTER_DAYS`] for the
//! chosen values and their reasoning. Both are enforced by [`gc`], run
//! opportunistically at the end of every [`SlackConversationRegistryStore::set`]
//! and [`SlackConversationRegistryStore::get_or_create`] call — there is no
//! separate background sweep. Idle eviction runs first (drops anything
//! stale regardless of the cap), then cap eviction trims whatever remains
//! down to the cap, oldest `last_seen_at` first. This ordering means a
//! workspace that never comes close to the cap still has its dead
//! conversations reclaimed, and a workspace that does hit the cap always
//! evicts the *least recently active* conversation, never an arbitrary one.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use ao_protocol::error::AoError;
use ao_protocol::slack_conversation_registry::SlackConversationRow;

use crate::paths::DataRoot;

/// Max conversation rows retained per Slack workspace (`team_id`) before the
/// least-recently-active ones are evicted to make room for a new one.
///
/// Chosen to comfortably outlive realistic usage: even a workspace running
/// several channels rarely has more than a few dozen genuinely live threads
/// at once, so 300 leaves an order of magnitude of headroom. It also bounds
/// the per-team JSON file's size — each row serializes to well under 200
/// bytes, so 300 rows is tens of KB, cheap to read, GC, and rewrite on every
/// single inbound message rather than needing its own indexing scheme.
pub const MAX_CONVERSATIONS_PER_TEAM: usize = 300;

/// A row untouched for this many days is evicted regardless of whether its
/// workspace is anywhere near the cap. A conversation nobody has replied to
/// in a month is presumed dead, and reclaiming it early keeps a slow trickle
/// of one-off threads — individually never enough to hit the cap — from
/// silently accumulating forever in a workspace that's simply quiet rather
/// than busy.
pub const IDLE_EVICT_AFTER_DAYS: i64 = 30;

/// Composes the in-file map key from `(channel_id, thread_ts)`. `thread_ts`
/// is `None` for a DM, whose conversation key is the channel id alone;
/// `Some` for a channel `@mention` or reply, keyed on the channel
/// plus the Slack thread's root `ts`. `:` is unambiguous here — Slack
/// channel ids and `ts` values are never used together in a way that could
/// collide (a `ts` always contains a `.`, never lands as a bare channel id).
fn compose_key(channel_id: &str, thread_ts: Option<&str>) -> String {
    match thread_ts {
        Some(ts) => format!("{channel_id}:{ts}"),
        None => channel_id.to_string(),
    }
}

type TeamMap = BTreeMap<String, SlackConversationRow>;

/// On-disk store for the conversation→thread registry, one [`TeamMap`] per
/// Slack workspace `team_id`.
pub struct SlackConversationRegistryStore {
    data_root: DataRoot,
}

impl SlackConversationRegistryStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Looks up the row for `(team_id, channel_id, thread_ts)`, or `None` if
    /// this conversation has never been provisioned.
    pub async fn get(
        &self,
        team_id: &str,
        channel_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<Option<SlackConversationRow>, AoError> {
        let map = Self::read_team(&self.data_root, team_id).await?;
        Ok(map.get(&compose_key(channel_id, thread_ts)).cloned())
    }

    /// Persists `row` for `(team_id, channel_id, thread_ts)`, overwriting any
    /// prior row, then runs GC (see the module doc) using `now`.
    pub async fn set(
        &self,
        team_id: &str,
        channel_id: &str,
        thread_ts: Option<&str>,
        row: &SlackConversationRow,
        now: DateTime<Utc>,
    ) -> Result<(), AoError> {
        let mut map = Self::read_team(&self.data_root, team_id).await?;
        map.insert(compose_key(channel_id, thread_ts), row.clone());
        gc(&mut map, now);
        Self::write_team(&self.data_root, team_id, &map).await
    }

    /// Lazily provisions the row for `(team_id, channel_id, thread_ts)`: if
    /// one is already registered, bumps its `last_seen_at` to `now` and
    /// returns it unchanged otherwise; if this is the first inbound message
    /// ever seen for this conversation, calls `make_thread_id` exactly once
    /// to mint a new bridge thread id, stores a fresh row, and returns that.
    ///
    /// This is the only place a row is created — there is no up-front
    /// provisioning step; a row is provisioned exactly once, on the first
    /// inbound message for that conversation.
    pub async fn get_or_create(
        &self,
        team_id: &str,
        channel_id: &str,
        thread_ts: Option<&str>,
        agent_id: &str,
        now: DateTime<Utc>,
        make_thread_id: impl FnOnce() -> String,
    ) -> Result<SlackConversationRow, AoError> {
        let mut map = Self::read_team(&self.data_root, team_id).await?;
        let key = compose_key(channel_id, thread_ts);

        let row = match map.get(&key) {
            Some(existing) => {
                let mut row = existing.clone();
                row.last_seen_at = now;
                row
            }
            None => SlackConversationRow {
                agent_id: agent_id.to_string(),
                thread_id: make_thread_id(),
                created_at: now,
                last_seen_at: now,
            },
        };

        map.insert(key, row.clone());
        gc(&mut map, now);
        Self::write_team(&self.data_root, team_id, &map).await?;
        Ok(row)
    }

    /// Every conversation row across every Slack workspace this data root
    /// has ever seen a message from, regardless of `team_id`. Used only by
    /// the one-time `channel_origin` backfill at startup
    /// (`PersistenceLayer::init_with_root`) — no runtime lookup path needs a
    /// cross-workspace view, which is exactly why the on-disk layout above
    /// stays sharded per `team_id` instead of this shape. Skips (rather than
    /// fails startup on) a team file that fails to parse, since a corrupt
    /// registry file is a pre-existing condition this backfill shouldn't be
    /// the thing that turns into a hard boot failure.
    pub async fn list_all(&self) -> Result<Vec<SlackConversationRow>, AoError> {
        let dir = self.data_root.slack_conversations_dir();
        if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let mut rows = Vec::new();
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = tokio::fs::read(&path).await else {
                continue;
            };
            let Ok(map) = serde_json::from_slice::<TeamMap>(&bytes) else {
                continue;
            };
            rows.extend(map.into_values());
        }
        Ok(rows)
    }

    async fn read_team(data_root: &DataRoot, team_id: &str) -> Result<TeamMap, AoError> {
        let path = data_root.slack_conversation_registry_path(team_id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(TeamMap::new());
        }
        let bytes = tokio::fs::read(&path).await?;
        let map: TeamMap = serde_json::from_slice(&bytes).map_err(|e| AoError::Json(e.to_string()))?;
        Ok(map)
    }

    async fn write_team(data_root: &DataRoot, team_id: &str, map: &TeamMap) -> Result<(), AoError> {
        let path = data_root.slack_conversation_registry_path(team_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(map).map_err(|e| AoError::Json(e.to_string()))?;
        let tmp = path.with_file_name(format!(
            "{}.{}.tmp",
            path.file_name().and_then(|f| f.to_str()).unwrap_or("slack_conversations"),
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::write(&tmp, &json).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }
}

/// Applies the cap + GC policy to `map` in place. Idle eviction runs first
/// (unconditional on size), then cap eviction trims whatever remains down to
/// [`MAX_CONVERSATIONS_PER_TEAM`], removing the lowest `last_seen_at` entries
/// first. Both passes are cheap at this scale (a few hundred rows at most).
fn gc(map: &mut TeamMap, now: DateTime<Utc>) {
    let idle_cutoff = now - Duration::days(IDLE_EVICT_AFTER_DAYS);
    map.retain(|_, row| row.last_seen_at > idle_cutoff);

    if map.len() > MAX_CONVERSATIONS_PER_TEAM {
        let mut by_age: Vec<(String, DateTime<Utc>)> =
            map.iter().map(|(key, row)| (key.clone(), row.last_seen_at)).collect();
        by_age.sort_by_key(|(_, last_seen_at)| *last_seen_at);

        let excess = map.len() - MAX_CONVERSATIONS_PER_TEAM;
        for (key, _) in by_age.into_iter().take(excess) {
            map.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn store_at(dir: &Path) -> SlackConversationRegistryStore {
        SlackConversationRegistryStore::new(DataRoot::new(dir))
    }

    fn row(agent_id: &str, thread_id: &str, at: DateTime<Utc>) -> SlackConversationRow {
        SlackConversationRow {
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
        assert_eq!(store.get("T1", "C1", None).await.unwrap(), None);
    }

    #[tokio::test]
    async fn conversation_row_round_trips_through_persistence() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();
        let r = row("agent-a", "thread-1", now);

        store.set("T1", "C1", Some("111.000"), &r, now).await.unwrap();
        let loaded = store.get("T1", "C1", Some("111.000")).await.unwrap();

        assert_eq!(loaded, Some(r));
    }

    #[tokio::test]
    async fn dm_conversation_keys_on_channel_id_alone() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();
        let r = row("agent-a", "thread-dm", now);

        // A DM has no thread_ts — the key collapses to just the channel id.
        store.set("T1", "D1", None, &r, now).await.unwrap();

        assert_eq!(store.get("T1", "D1", None).await.unwrap(), Some(r));
        // A channel/thread key that happens to share the same channel id
        // string but carries a thread_ts is still a distinct row.
        assert_eq!(store.get("T1", "D1", Some("111.000")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn different_teams_are_isolated_in_separate_files() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        store.set("T1", "C1", None, &row("agent-a", "thread-1", now), now).await.unwrap();
        store.set("T2", "C1", None, &row("agent-b", "thread-2", now), now).await.unwrap();

        assert_eq!(store.get("T1", "C1", None).await.unwrap().unwrap().agent_id, "agent-a");
        assert_eq!(store.get("T2", "C1", None).await.unwrap().unwrap().agent_id, "agent-b");
    }

    /// Lazy provisioning: the first inbound message for a conversation
    /// creates exactly one row, and every subsequent inbound message for the
    /// *same* conversation reuses it rather than minting a new thread id.
    #[tokio::test]
    async fn get_or_create_provisions_exactly_one_row_for_repeated_inbound() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();
        let calls = Arc::new(AtomicUsize::new(0));

        let make_id = {
            let calls = Arc::clone(&calls);
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                "thread-provisioned".to_string()
            }
        };
        let first = store.get_or_create("T1", "C1", Some("111.000"), "agent-a", now, make_id).await.unwrap();

        let make_id_again = {
            let calls = Arc::clone(&calls);
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                "thread-should-never-be-used".to_string()
            }
        };
        let second = store
            .get_or_create("T1", "C1", Some("111.000"), "agent-a", now + Duration::seconds(30), make_id_again)
            .await
            .unwrap();

        assert_eq!(first.thread_id, "thread-provisioned");
        assert_eq!(second.thread_id, "thread-provisioned", "second inbound must reuse the row created by the first");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "make_thread_id must be called exactly once across both calls");
        assert_eq!(second.last_seen_at, now + Duration::seconds(30), "last_seen_at must advance on reuse");

        // Exactly one row exists for this conversation's team — lazy
        // provisioning must not have created any extras.
        let map = SlackConversationRegistryStore::read_team(&DataRoot::new(tmp.path()), "T1").await.unwrap();
        assert_eq!(map.len(), 1);
    }

    /// Proves the registry cannot grow unbounded: once a workspace is at
    /// `MAX_CONVERSATIONS_PER_TEAM`, the next new conversation evicts the
    /// least-recently-active row rather than growing past the cap.
    #[tokio::test]
    async fn set_evicts_the_least_recently_active_row_once_over_the_cap() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let base = Utc::now();

        for i in 0..MAX_CONVERSATIONS_PER_TEAM {
            let at = base + Duration::seconds(i as i64);
            store
                .set("T1", &format!("C{i}"), None, &row("agent-a", &format!("thread-{i}"), at), at)
                .await
                .unwrap();
        }
        // At exactly the cap: nothing evicted yet, the oldest (C0) survives.
        assert!(store.get("T1", "C0", None).await.unwrap().is_some());
        let at_cap = SlackConversationRegistryStore::read_team(&DataRoot::new(tmp.path()), "T1").await.unwrap();
        assert_eq!(at_cap.len(), MAX_CONVERSATIONS_PER_TEAM);

        // One more, strictly more recent than every existing row — pushes
        // the team over the cap and must evict exactly the oldest (C0).
        let overflow_at = base + Duration::seconds(MAX_CONVERSATIONS_PER_TEAM as i64 + 100);
        store
            .set("T1", "C-overflow", None, &row("agent-a", "thread-overflow", overflow_at), overflow_at)
            .await
            .unwrap();

        assert!(store.get("T1", "C0", None).await.unwrap().is_none(), "the least-recently-active row must be evicted");
        assert!(store.get("T1", "C1", None).await.unwrap().is_some(), "the next-oldest row must survive");
        assert!(store.get("T1", "C-overflow", None).await.unwrap().is_some());

        let after = SlackConversationRegistryStore::read_team(&DataRoot::new(tmp.path()), "T1").await.unwrap();
        assert_eq!(after.len(), MAX_CONVERSATIONS_PER_TEAM, "eviction must keep the team at, not over, the cap");
    }

    #[tokio::test]
    async fn gc_evicts_rows_idle_past_the_threshold_regardless_of_cap() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        let stale_at = now - Duration::days(IDLE_EVICT_AFTER_DAYS + 1);
        store.set("T1", "C-stale", None, &row("agent-a", "thread-stale", stale_at), stale_at).await.unwrap();

        // A second, unrelated write to the same team is what actually
        // triggers GC (there is no background sweep — see the module doc).
        store.set("T1", "C-fresh", None, &row("agent-a", "thread-fresh", now), now).await.unwrap();

        assert!(store.get("T1", "C-stale", None).await.unwrap().is_none(), "idle-past-threshold row must be GC'd");
        assert!(store.get("T1", "C-fresh", None).await.unwrap().is_some());
    }

    /// The key/value shape must support a second agent owning a distinct
    /// conversation in the *same* Slack channel — i.e. nothing about the
    /// key collapses two threads in one channel into a single owner. This
    /// is what makes "a second agent in the same channel" a lookup rather
    /// than a schema change.
    #[tokio::test]
    async fn two_agents_can_each_own_a_distinct_thread_in_the_same_channel() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        let marketing = row("agent-marketing", "thread-marketing", now);
        let sales = row("agent-sales", "thread-sales", now);

        store.set("T1", "C-growth", Some("111.000"), &marketing, now).await.unwrap();
        store.set("T1", "C-growth", Some("222.000"), &sales, now).await.unwrap();

        let looked_up_marketing = store.get("T1", "C-growth", Some("111.000")).await.unwrap().unwrap();
        let looked_up_sales = store.get("T1", "C-growth", Some("222.000")).await.unwrap().unwrap();

        assert_eq!(looked_up_marketing.agent_id, "agent-marketing");
        assert_eq!(looked_up_sales.agent_id, "agent-sales");
        assert_ne!(looked_up_marketing.thread_id, looked_up_sales.thread_id);
    }

    #[tokio::test]
    async fn list_all_returns_empty_when_nothing_persisted_yet() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        assert_eq!(store.list_all().await.unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn list_all_flattens_rows_across_multiple_workspaces() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        // Two different team_id files, plus two conversations in one of them
        // — list_all must cross the per-team sharding boundary and return
        // every row from every file.
        store.set("T1", "C1", None, &row("agent-a", "thread-1", now), now).await.unwrap();
        store.set("T1", "C2", Some("111.000"), &row("agent-a", "thread-2", now), now).await.unwrap();
        store.set("T2", "C1", None, &row("agent-b", "thread-3", now), now).await.unwrap();

        let mut all = store.list_all().await.unwrap();
        all.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
        let thread_ids: Vec<&str> = all.iter().map(|r| r.thread_id.as_str()).collect();
        assert_eq!(thread_ids, vec!["thread-1", "thread-2", "thread-3"]);
    }

    #[tokio::test]
    async fn list_all_skips_a_corrupt_team_file_instead_of_failing() {
        let tmp = tempdir().unwrap();
        let store = store_at(tmp.path());
        let now = Utc::now();

        store.set("T1", "C1", None, &row("agent-a", "thread-good", now), now).await.unwrap();

        // A sibling team file that isn't valid JSON at all.
        let dir = DataRoot::new(tmp.path()).slack_conversations_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("T-corrupt.json"), b"not json").await.unwrap();

        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 1, "the corrupt file must be skipped, not fail the whole call");
        assert_eq!(all[0].thread_id, "thread-good");
    }
}
