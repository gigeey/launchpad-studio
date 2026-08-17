use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::handle::{BackgroundAgentHandle, BackgroundAgentId};

/// Error returned by [`BackgroundAgentRegistry::insert`].
#[derive(Debug, Error)]
pub enum RegistryError {
    /// Refused because the live count is at or above the configured cap.
    #[error("concurrency cap of {cap} reached ({live} agents live)")]
    ConcurrencyCapExceeded { live: usize, cap: usize },
}

/// Snapshot of observable (cloneable) metadata for a live background agent.
///
/// Returned by [`BackgroundAgentRegistry::get`] because [`BackgroundAgentHandle`]
/// contains non-`Clone` fields (`broadcast::Receiver`, `JoinHandle`).
#[derive(Debug, Clone)]
pub struct BackgroundAgentSnapshot {
    pub id: BackgroundAgentId,
    pub subagent_name: String,
    pub spawned_at: DateTime<Utc>,
    pub cancel: CancellationToken,
}

/// Per-parent registry of live [`BackgroundAgentHandle`]s.
///
/// Enforces a concurrency cap on [`insert`](Self::insert), exposes snapshot-based
/// [`get`](Self::get), and cancels every live child on
/// [`cancel_all`](Self::cancel_all).
pub struct BackgroundAgentRegistry {
    inner: RwLock<HashMap<BackgroundAgentId, BackgroundAgentHandle>>,
    cap: usize,
}

impl BackgroundAgentRegistry {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            cap,
        }
    }

    /// Insert `handle`, returning [`RegistryError::ConcurrencyCapExceeded`]
    /// if the live count is already at the cap.
    pub async fn insert(&self, handle: BackgroundAgentHandle) -> Result<(), RegistryError> {
        let mut map = self.inner.write().await;
        if map.len() >= self.cap {
            return Err(RegistryError::ConcurrencyCapExceeded {
                live: map.len(),
                cap: self.cap,
            });
        }
        map.insert(handle.id.clone(), handle);
        Ok(())
    }

    /// Remove and return the handle for `id`, or `None` if not present.
    pub async fn remove(&self, id: &BackgroundAgentId) -> Option<BackgroundAgentHandle> {
        self.inner.write().await.remove(id)
    }

    /// Return a cloneable snapshot of metadata for `id`, or `None` if not present.
    pub async fn get(&self, id: &BackgroundAgentId) -> Option<BackgroundAgentSnapshot> {
        let map = self.inner.read().await;
        map.get(id).map(|h| BackgroundAgentSnapshot {
            id: h.id.clone(),
            subagent_name: h.subagent_name.clone(),
            spawned_at: h.spawned_at,
            cancel: h.cancel.clone(),
        })
    }

    /// Current number of in-flight handles.
    pub async fn live_count(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Snapshot of every handle that is *still running* — cheap, `Clone`-only
    /// fields, no borrow of the non-`Clone` `events`/`join`. Used by the
    /// `/system/stream` connect-time replay to reconfirm in-flight async
    /// delegations that survived a mere reconnect (as opposed to a server
    /// restart, which drops this registry along with everything else).
    ///
    /// Entries whose join handle has already finished are excluded. Presence in
    /// the map does **not** imply "running": a cancelled or naturally-completed
    /// delegation deliberately keeps its handle until a `DelegateOutput` poll
    /// reaps it, so the map is a superset of what is actually in flight. Since
    /// a terminal delegation has already emitted its completion event and will
    /// never emit another, replaying it as started would strand a permanently
    /// "running" indicator in the UI. Filtering here — at the read site — keeps
    /// that reaping contract intact while still answering the question callers
    /// are really asking.
    ///
    /// `spawned_at` is carried through so a replayed indicator can show elapsed
    /// time measured from the real start rather than restarting its clock at
    /// the moment of reconnect.
    pub async fn active(&self) -> Vec<BackgroundAgentSnapshot> {
        self.inner
            .read()
            .await
            .values()
            .filter(|h| !h.join.is_finished())
            .map(|h| BackgroundAgentSnapshot {
                id: h.id.clone(),
                subagent_name: h.subagent_name.clone(),
                spawned_at: h.spawned_at,
                cancel: h.cancel.clone(),
            })
            .collect()
    }

    /// The configured concurrency cap for this registry.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Fire every live agent's [`CancellationToken`], await each join up to
    /// `grace_period`, then drop all handles. After this call the registry is empty.
    pub async fn cancel_all(&self, grace_period: Duration) {
        let handles: Vec<BackgroundAgentHandle> = {
            let mut map = self.inner.write().await;
            map.drain().map(|(_, h)| h).collect()
        };

        for BackgroundAgentHandle { cancel, join, .. } in handles {
            cancel.cancel();
            let _ = timeout(grace_period, join).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    use crate::background_agents::handle::{BackgroundAgentHandle, BackgroundAgentId, TaskFinalReport};

    fn make_handle(name: &str) -> BackgroundAgentHandle {
        let (_tx, rx) = broadcast::channel(1);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let join = tokio::spawn(async move {
            cancel_clone.cancelled().await;
            Ok::<TaskFinalReport, ao_protocol::error::AoError>(TaskFinalReport::cancelled())
        });
        BackgroundAgentHandle {
            id: BackgroundAgentId::new(),
            subagent_name: name.to_string(),
            spawned_at: chrono::Utc::now(),
            cancel,
            events: rx,
            join,
        }
    }

    /// A handle whose task has already run to completion, so `join.is_finished()`
    /// is guaranteed true by the time this returns.
    async fn make_finished_handle(name: &str) -> BackgroundAgentHandle {
        let h = make_handle(name);
        // `make_handle`'s task parks on the cancel token; fire it so the body
        // returns, then wait until the join handle actually observes that.
        h.cancel.cancel();
        while !h.join.is_finished() {
            tokio::task::yield_now().await;
        }
        h
    }

    #[tokio::test]
    async fn insert_under_cap_succeeds() {
        let registry = BackgroundAgentRegistry::new(2);
        let h = make_handle("alpha");
        let id = h.id.clone();
        assert!(registry.insert(h).await.is_ok());
        assert_eq!(registry.live_count().await, 1);
        assert!(registry.get(&id).await.is_some());
    }

    #[tokio::test]
    async fn insert_at_cap_returns_concurrency_cap_exceeded() {
        let registry = BackgroundAgentRegistry::new(1);
        let h1 = make_handle("first");
        let h2 = make_handle("second");
        assert!(registry.insert(h1).await.is_ok());
        let result = registry.insert(h2).await;
        assert!(
            matches!(result, Err(RegistryError::ConcurrencyCapExceeded { live: 1, cap: 1 })),
            "expected ConcurrencyCapExceeded, got: {result:?}",
        );
    }

    #[tokio::test]
    async fn active_snapshots_id_and_name_for_every_live_handle() {
        let registry = BackgroundAgentRegistry::new(4);
        let h1 = make_handle("alpha");
        let h2 = make_handle("beta");
        let id1 = h1.id.clone();
        let id2 = h2.id.clone();
        registry.insert(h1).await.unwrap();
        registry.insert(h2).await.unwrap();

        let mut active = registry.active().await;
        active.sort_by(|a, b| a.subagent_name.cmp(&b.subagent_name));
        let pairs: Vec<_> = active
            .iter()
            .map(|s| (s.id.clone(), s.subagent_name.clone()))
            .collect();
        assert_eq!(pairs, vec![(id1, "alpha".to_string()), (id2, "beta".to_string())]);
    }

    #[tokio::test]
    async fn active_excludes_finished_handles_but_keeps_running_ones() {
        let registry = BackgroundAgentRegistry::new(4);
        let running = make_handle("still-running");
        let finished = make_finished_handle("already-done").await;
        let running_id = running.id.clone();
        let finished_id = finished.id.clone();
        registry.insert(running).await.unwrap();
        registry.insert(finished).await.unwrap();

        let ids: Vec<_> = registry.active().await.into_iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec![running_id],
            "active() must report exactly the handles still in flight",
        );
        assert!(
            !ids.contains(&finished_id),
            "a finished handle must not be replayed as a running delegation",
        );

        // Filtered at the read, not evicted from the map: a terminal handle has
        // to stay put until a DelegateOutput poll reaps it.
        assert_eq!(registry.live_count().await, 2);
        assert!(registry.get(&finished_id).await.is_some());
    }

    #[tokio::test]
    async fn active_carries_spawned_at_through_the_round_trip() {
        let registry = BackgroundAgentRegistry::new(2);
        let h = make_handle("alpha");
        let id = h.id.clone();
        let spawned_at = h.spawned_at;
        registry.insert(h).await.unwrap();

        let active = registry.active().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, id);
        assert_eq!(
            active[0].spawned_at, spawned_at,
            "spawned_at must survive active() so a replayed indicator can show true elapsed time",
        );
    }

    #[tokio::test]
    async fn active_is_empty_for_a_fresh_registry() {
        let registry = BackgroundAgentRegistry::new(2);
        assert!(registry.active().await.is_empty());
    }

    #[tokio::test]
    async fn remove_drops_entry() {
        let registry = BackgroundAgentRegistry::new(2);
        let h = make_handle("beta");
        let id = h.id.clone();
        registry.insert(h).await.unwrap();
        assert_eq!(registry.live_count().await, 1);

        let removed = registry.remove(&id).await;
        assert!(removed.is_some(), "remove should return the handle");
        assert_eq!(registry.live_count().await, 0);
        assert!(registry.get(&id).await.is_none(), "entry should be gone");
    }

    #[tokio::test]
    async fn cancel_all_fires_tokens_and_reaps() {
        let registry = BackgroundAgentRegistry::new(3);
        let h1 = make_handle("gamma");
        let h2 = make_handle("delta");

        let cancel1 = h1.cancel.clone();
        let cancel2 = h2.cancel.clone();

        registry.insert(h1).await.unwrap();
        registry.insert(h2).await.unwrap();
        assert_eq!(registry.live_count().await, 2);

        registry.cancel_all(Duration::from_millis(500)).await;

        assert_eq!(registry.live_count().await, 0, "registry must be empty after cancel_all");
        assert!(cancel1.is_cancelled(), "first agent's token must be fired");
        assert!(cancel2.is_cancelled(), "second agent's token must be fired");
    }
}
