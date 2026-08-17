use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::RwLock;

use ao_protocol::agent::AgentId;

/// RAII guard that unregisters a `(agent_id, run_id)` pair from an
/// [`InstanceRegistry`] when it goes out of scope.
///
/// Drop fires on every exit path — normal return, `?` propagation, early
/// `return Err(_)`, **and panic unwind**. The previous pattern of pairing
/// `register_run` with a manual `unregister_run` left the runtime overlay
/// (`has_active_run` derived from `running_count` > 0) wedged "active"
/// forever whenever the runner task panicked between the two calls.
/// Symptom: sidebar typing indicator stuck on an agent that's no longer
/// running; only a server restart cleared it.
///
/// Because `InstanceRegistry::unregister_run` is async (it acquires a
/// `tokio::sync::RwLock`), `Drop` cannot await it directly. Instead the
/// guard captures the registry handle + identifiers cheaply and spawns
/// the cleanup onto the current Tokio runtime via
/// [`tokio::runtime::Handle::try_current`]. If no runtime is available
/// (process shutdown), the cleanup is a best-effort no-op — the next
/// process boot starts with an empty registry anyway since it's pure
/// runtime state, never persisted to disk.
pub struct InstanceRegistryGuard {
    registry: Arc<InstanceRegistry>,
    agent_id: AgentId,
    run_id: String,
}

impl InstanceRegistryGuard {
    /// Registers `(agent_id, run_id)` and returns a guard that will
    /// unregister it on Drop. Callers must keep the guard alive for the
    /// full lifetime of the run.
    pub async fn register(
        registry: Arc<InstanceRegistry>,
        agent_id: AgentId,
        run_id: String,
    ) -> Self {
        Self::register_with_thread(registry, agent_id, run_id, None).await
    }

    /// Same as `register`, but also records the run's thread so it can be
    /// recovered later via `InstanceRegistry::thread_for_run`. Use this at
    /// any registration site where the thread being run on is already known
    /// — see `native.rs`'s `run_session` for the caller.
    pub async fn register_with_thread(
        registry: Arc<InstanceRegistry>,
        agent_id: AgentId,
        run_id: String,
        thread_id: Option<String>,
    ) -> Self {
        registry.register_run_with_thread(&agent_id, &run_id, thread_id).await;
        Self { registry, agent_id, run_id }
    }

    /// Wrap a registration that has already been performed elsewhere so
    /// the unregister-on-Drop semantics apply to it. Useful when the
    /// register call has to happen synchronously (before a spawn boundary
    /// to avoid a "no active run" race window at SSE-connect time) but
    /// the cleanup duty should live on the spawned task so a panic inside
    /// the task still triggers it.
    ///
    /// Caller must ensure `register_run(&agent_id, &run_id)` has already
    /// been invoked on the same registry — otherwise Drop will harmlessly
    /// unregister a key that isn't there (idempotent).
    pub fn wrap_existing(
        registry: Arc<InstanceRegistry>,
        agent_id: AgentId,
        run_id: String,
    ) -> Self {
        Self { registry, agent_id, run_id }
    }
}

impl Drop for InstanceRegistryGuard {
    fn drop(&mut self) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let registry = Arc::clone(&self.registry);
            let agent_id = self.agent_id.clone();
            let run_id = self.run_id.clone();
            handle.spawn(async move {
                registry.unregister_run(&agent_id, &run_id).await;
            });
        }
    }
}

pub struct InstanceRegistry {
    runs: Arc<RwLock<HashMap<AgentId, HashSet<String>>>>,
    /// `run_id -> thread_id`, populated alongside `runs` so a reconnecting SSE
    /// client can be told which thread a still-active run belongs to (see
    /// `thread_for_run`). Absent entries (never registered via
    /// `register_run_with_thread`) and `Some(None)` entries both resolve to
    /// "default thread" through `thread_for_run`'s `flatten()` — the same
    /// fallback every unresolved case used before this map existed, so
    /// call sites that only ever registered through the plain `register_run`
    /// keep their old (harmless) behavior.
    run_threads: Arc<RwLock<HashMap<String, Option<String>>>>,
    /// Agents with a non-terminal agent-owned tasklist. Used by `can_spawn` to
    /// bump the effective cap to at least 2 so tasklist dispatch and user chat
    /// can run in parallel without extra queue plumbing.
    active_tasklist_agents: Arc<RwLock<HashSet<AgentId>>>,
}

impl InstanceRegistry {
    pub fn new() -> Self {
        Self {
            runs: Arc::new(RwLock::new(HashMap::new())),
            run_threads: Arc::new(RwLock::new(HashMap::new())),
            active_tasklist_agents: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Record that `agent_id` has a non-terminal agent-owned tasklist.
    pub async fn mark_has_active_tasklist(&self, agent_id: &AgentId) {
        self.active_tasklist_agents.write().await.insert(agent_id.clone());
    }

    /// Clear the active-tasklist flag for `agent_id` (called when the tasklist
    /// reaches a terminal state: Completed, Failed, or Cancelled).
    pub async fn clear_has_active_tasklist(&self, agent_id: &AgentId) {
        self.active_tasklist_agents.write().await.remove(agent_id);
    }

    pub async fn register_run(&self, agent_id: &AgentId, run_id: &str) {
        self.register_run_with_thread(agent_id, run_id, None).await;
    }

    /// Same as `register_run`, but also records which thread the run belongs
    /// to (`None` = the agent's default thread) so a later `thread_for_run`
    /// lookup — used at SSE-connect time to tag a synthetic `AgentBusy`
    /// replay with the right thread — resolves correctly instead of always
    /// falling back to the default thread. See `stream_events` in
    /// ao-server's stream route for the consumer.
    pub async fn register_run_with_thread(
        &self,
        agent_id: &AgentId,
        run_id: &str,
        thread_id: Option<String>,
    ) {
        let mut map = self.runs.write().await;
        map.entry(agent_id.clone())
            .or_default()
            .insert(run_id.to_string());
        drop(map);
        self.run_threads.write().await.insert(run_id.to_string(), thread_id);
    }

    pub async fn unregister_run(&self, agent_id: &AgentId, run_id: &str) {
        let mut map = self.runs.write().await;
        if let Some(set) = map.get_mut(agent_id) {
            set.remove(run_id);
            if set.is_empty() {
                map.remove(agent_id);
            }
        }
        drop(map);
        self.run_threads.write().await.remove(run_id);
    }

    /// Which thread `run_id` is running on, if it's currently registered and
    /// was registered via `register_run_with_thread`. `None` covers both
    /// "not registered" and "registered for the default thread" — callers
    /// that need to distinguish those already know whether the run is active
    /// from `active_runs`/`running_count`.
    pub async fn thread_for_run(&self, run_id: &str) -> Option<String> {
        self.run_threads.read().await.get(run_id).cloned().flatten()
    }

    pub async fn running_count(&self, agent_id: &AgentId) -> usize {
        let map = self.runs.read().await;
        map.get(agent_id).map_or(0, |set| set.len())
    }

    /// Which thread ids `agent_id` currently has an active run on, deduped
    /// and sorted for deterministic serialization. Runs registered with no
    /// thread (`thread_for_run` resolving to `None`) are excluded — they
    /// can't light up a specific thread row. Lets the Home sidebar show the
    /// running badge on the exact thread that's active, not just the agent.
    pub async fn running_thread_ids(&self, agent_id: &AgentId) -> Vec<String> {
        let run_ids = self.runs.read().await.get(agent_id).cloned().unwrap_or_default();
        let threads = self.run_threads.read().await;
        let mut thread_ids: Vec<String> = run_ids
            .iter()
            .filter_map(|run_id| threads.get(run_id).cloned().flatten())
            .collect();
        thread_ids.sort();
        thread_ids.dedup();
        thread_ids
    }

    pub async fn can_spawn(&self, agent_id: &AgentId, max_instances: u32) -> bool {
        let effective_cap = if self.active_tasklist_agents.read().await.contains(agent_id) {
            max_instances.max(2)
        } else {
            max_instances
        };
        self.running_count(agent_id).await < effective_cap as usize
    }

    /// Returns true if any agent (across all keys) currently has an active run.
    pub async fn is_any_active(&self) -> bool {
        let map = self.runs.read().await;
        map.values().any(|set| !set.is_empty())
    }

    /// Returns true if any agent currently has a non-terminal tasklist, even
    /// if no run is registered for it right now. Unlike `is_any_active`, this
    /// stays true across the gap between one dispatched task completing and
    /// the next one being registered — the window callers that need to treat
    /// a whole tasklist as "active" (not just its individual task runs) care
    /// about.
    pub async fn has_any_active_tasklist(&self) -> bool {
        !self.active_tasklist_agents.read().await.is_empty()
    }

    /// Return the set of active run_ids for an agent. Empty set if none.
    pub async fn active_runs(&self, agent_id: &AgentId) -> HashSet<String> {
        let map = self.runs.read().await;
        map.get(agent_id).cloned().unwrap_or_default()
    }

    /// Return all active run_ids whose registry key starts with `prefix`.
    /// Used by team SSE reconnect: keys are `team:{team_id}:{agent_id}` but
    /// the SSE endpoint only knows `team:{team_id}`.
    pub async fn active_runs_by_prefix(&self, prefix: &str) -> Vec<(String, HashSet<String>)> {
        let map = self.runs.read().await;
        let mut result = Vec::new();
        for (key, run_ids) in map.iter() {
            if key.starts_with(prefix) {
                result.push((key.clone(), run_ids.clone()));
            }
        }
        result
    }

    /// Return every `(key, run_ids)` pair currently tracked, regardless of
    /// key shape — plain agent id or a synthetic `team:` / `project:` /
    /// `task:…:phase:` / `tasklist:` key. Unlike `active_runs`/
    /// `active_runs_by_prefix`, which each per-entity SSE endpoint uses to
    /// replay its own slice of active runs on connect, this enumerates the
    /// whole registry so `/system/stream` can replay AgentBusy for every
    /// active run and be a true superset of every per-entity endpoint.
    pub async fn all_active_runs(&self) -> Vec<(String, HashSet<String>)> {
        let map = self.runs.read().await;
        map.iter()
            .map(|(key, run_ids)| (key.clone(), run_ids.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Happy path: guard dropped via normal scope exit unregisters the run.
    #[tokio::test]
    async fn guard_unregisters_on_normal_drop() {
        let registry = Arc::new(InstanceRegistry::new());
        let agent: AgentId = "agent-1".to_string();

        {
            let _guard = InstanceRegistryGuard::register(
                Arc::clone(&registry),
                agent.clone(),
                "run-1".to_string(),
            )
            .await;
            assert_eq!(registry.running_count(&agent).await, 1);
        }
        // Drop fired — give the spawned cleanup task one yield to run.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(registry.running_count(&agent).await, 0);
    }

    /// Panic path: when the future holding the guard panics, the guard's
    /// Drop still fires during unwinding and unregisters the run. This is
    /// the load-bearing test for the bug fix — without the guard, a panic
    /// in the runner task wedged the registry forever.
    #[tokio::test]
    async fn guard_unregisters_on_panic_unwind() {
        let registry = Arc::new(InstanceRegistry::new());
        let agent: AgentId = "agent-panicky".to_string();

        let work = {
            let registry = Arc::clone(&registry);
            let agent = agent.clone();
            tokio::spawn(async move {
                let _guard = InstanceRegistryGuard::register(
                    registry,
                    agent,
                    "run-panic".to_string(),
                )
                .await;
                panic!("simulated runner crash");
            })
        };

        let join = work.await;
        assert!(join.is_err(), "spawned task should observe a panic");
        assert!(join.unwrap_err().is_panic());

        // Give the Drop-spawned cleanup task time to run.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(
            registry.running_count(&agent).await,
            0,
            "InstanceRegistry must be cleared after a runner-task panic"
        );
    }

    /// `wrap_existing` does not double-register and Drop still unregisters.
    /// Pins the constructor used at the CLI spawn boundary where register
    /// has to land synchronously before the spawn returns.
    #[tokio::test]
    async fn wrap_existing_unregisters_on_drop_without_double_registering() {
        let registry = Arc::new(InstanceRegistry::new());
        let agent: AgentId = "agent-wrap".to_string();
        registry.register_run(&agent, "run-X").await;
        assert_eq!(registry.running_count(&agent).await, 1);

        {
            let _guard = InstanceRegistryGuard::wrap_existing(
                Arc::clone(&registry),
                agent.clone(),
                "run-X".to_string(),
            );
            // wrap_existing must NOT register again — count stays at 1.
            assert_eq!(registry.running_count(&agent).await, 1);
        }
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(registry.running_count(&agent).await, 0);
    }

    /// `thread_for_run` recovers the thread a run was registered under —
    /// the piece SSE reconnect needs to tag a replayed AgentBusy event with
    /// the right thread instead of always defaulting to the main thread
    /// (the bug this map was added to fix).
    #[tokio::test]
    async fn thread_for_run_resolves_registered_thread() {
        let registry = Arc::new(InstanceRegistry::new());
        let agent: AgentId = "agent-threaded".to_string();

        registry
            .register_run_with_thread(&agent, "run-non-default", Some("thread-123".to_string()))
            .await;
        registry.register_run_with_thread(&agent, "run-default", None).await;
        // Plain `register_run` (no thread awareness) must still resolve to
        // "default thread" rather than panicking or erroring — the old
        // fallback every call site had before this map existed.
        registry.register_run(&agent, "run-legacy").await;

        assert_eq!(
            registry.thread_for_run("run-non-default").await,
            Some("thread-123".to_string())
        );
        assert_eq!(registry.thread_for_run("run-default").await, None);
        assert_eq!(registry.thread_for_run("run-legacy").await, None);
        assert_eq!(registry.thread_for_run("run-never-registered").await, None);
    }

    /// `unregister_run` clears the thread mapping too, so a reused run_id
    /// (shouldn't happen in practice since they're UUIDs, but defensively)
    /// or a stale lookup after teardown doesn't resolve a thread that no
    /// longer has an active run.
    #[tokio::test]
    async fn unregister_run_clears_thread_mapping() {
        let registry = Arc::new(InstanceRegistry::new());
        let agent: AgentId = "agent-threaded-2".to_string();

        registry
            .register_run_with_thread(&agent, "run-1", Some("thread-abc".to_string()))
            .await;
        assert_eq!(registry.thread_for_run("run-1").await, Some("thread-abc".to_string()));

        registry.unregister_run(&agent, "run-1").await;
        assert_eq!(registry.thread_for_run("run-1").await, None);
    }

    /// `running_thread_ids` returns every distinct thread a given agent has
    /// an active run on — the piece the Home sidebar needs to badge a
    /// specific thread row instead of only the agent row.
    #[tokio::test]
    async fn running_thread_ids_returns_sorted_deduped_threads() {
        let registry = InstanceRegistry::new();
        let agent: AgentId = "agent-multithread".to_string();

        registry
            .register_run_with_thread(&agent, "run-1", Some("thread-b".to_string()))
            .await;
        registry
            .register_run_with_thread(&agent, "run-2", Some("thread-a".to_string()))
            .await;
        assert_eq!(
            registry.running_thread_ids(&agent).await,
            vec!["thread-a".to_string(), "thread-b".to_string()]
        );

        // A third run on a thread already covered by another run must not
        // produce a duplicate entry.
        registry
            .register_run_with_thread(&agent, "run-3", Some("thread-a".to_string()))
            .await;
        assert_eq!(
            registry.running_thread_ids(&agent).await,
            vec!["thread-a".to_string(), "thread-b".to_string()]
        );

        // A run with no thread_id can't light up a specific row — excluded.
        registry.register_run_with_thread(&agent, "run-none", None).await;
        assert_eq!(
            registry.running_thread_ids(&agent).await,
            vec!["thread-a".to_string(), "thread-b".to_string()]
        );

        // Unregistering the only run on thread-b drops it from the result;
        // thread-a survives because run-3 still holds it.
        registry.unregister_run(&agent, "run-1").await;
        assert_eq!(
            registry.running_thread_ids(&agent).await,
            vec!["thread-a".to_string()]
        );
    }

    /// `InstanceRegistryGuard::register_with_thread` threads the thread_id
    /// through to the registry exactly like the plain map API — pins the
    /// path `native.rs`'s auto-register call site actually uses.
    #[tokio::test]
    async fn guard_register_with_thread_is_queryable() {
        let registry = Arc::new(InstanceRegistry::new());
        let agent: AgentId = "agent-guard-threaded".to_string();

        let _guard = InstanceRegistryGuard::register_with_thread(
            Arc::clone(&registry),
            agent.clone(),
            "run-guarded".to_string(),
            Some("thread-xyz".to_string()),
        )
        .await;

        assert_eq!(
            registry.thread_for_run("run-guarded").await,
            Some("thread-xyz".to_string())
        );
    }

    /// Multiple guards under the same agent — each guard only unregisters
    /// its own run_id. Pins concurrent-run isolation.
    #[tokio::test]
    async fn multiple_guards_unregister_independently() {
        let registry = Arc::new(InstanceRegistry::new());
        let agent: AgentId = "agent-2".to_string();

        let g1 = InstanceRegistryGuard::register(
            Arc::clone(&registry),
            agent.clone(),
            "run-A".to_string(),
        )
        .await;
        let _g2 = InstanceRegistryGuard::register(
            Arc::clone(&registry),
            agent.clone(),
            "run-B".to_string(),
        )
        .await;
        assert_eq!(registry.running_count(&agent).await, 2);

        drop(g1);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(registry.running_count(&agent).await, 1);
        let active: HashSet<String> = registry.active_runs(&agent).await;
        assert!(active.contains("run-B"));
        assert!(!active.contains("run-A"));
    }

    /// Agent with max_instances=1 and an active tasklist: effective cap is 2,
    /// so a second spawn is allowed.
    #[tokio::test]
    async fn active_tasklist_bumps_cap_to_2() {
        let registry = Arc::new(InstanceRegistry::new());
        let agent: AgentId = "agent-tl".to_string();

        registry.mark_has_active_tasklist(&agent).await;
        // One run already in progress.
        registry.register_run(&agent, "run-1").await;

        // With max_instances=1, effective cap should be 2, so can_spawn is true.
        assert!(registry.can_spawn(&agent, 1).await);

        // Register the second run (simulates the second spawn).
        registry.register_run(&agent, "run-2").await;

        // Now 2 runs active; effective cap is still 2 → no third spawn.
        assert!(!registry.can_spawn(&agent, 1).await);
    }

    /// Agent with max_instances=3 and active tasklist: effective cap stays 3.
    #[tokio::test]
    async fn active_tasklist_does_not_lower_configured_cap() {
        let registry = Arc::new(InstanceRegistry::new());
        let agent: AgentId = "agent-big".to_string();

        registry.mark_has_active_tasklist(&agent).await;
        // Two runs already in progress.
        registry.register_run(&agent, "r1").await;
        registry.register_run(&agent, "r2").await;

        // max(3, 2) = 3, running_count=2 → can spawn.
        assert!(registry.can_spawn(&agent, 3).await);

        registry.register_run(&agent, "r3").await;
        // running_count=3 → cannot spawn.
        assert!(!registry.can_spawn(&agent, 3).await);
    }

    /// Agent with no active tasklist: effective cap equals configured_max unchanged.
    #[tokio::test]
    async fn no_active_tasklist_uses_configured_max() {
        let registry = Arc::new(InstanceRegistry::new());
        let agent: AgentId = "agent-plain".to_string();

        registry.register_run(&agent, "r1").await;
        // No active tasklist, max_instances=1, running_count=1 → cannot spawn.
        assert!(!registry.can_spawn(&agent, 1).await);
    }

    /// `all_active_runs` enumerates every channel key regardless of shape —
    /// plain agent ids alongside synthetic team:/project:/task:…:phase:/
    /// tasklist: keys — which is what lets `/system/stream` replay the same
    /// AgentBusy set every per-entity endpoint would replay on its own.
    #[tokio::test]
    async fn all_active_runs_enumerates_every_channel_key() {
        let registry = InstanceRegistry::new();

        registry.register_run(&"agent-1".to_string(), "run-agent").await;
        registry.register_run(&"team:t1:agent-2".to_string(), "run-team").await;
        registry.register_run(&"project:p1".to_string(), "run-project").await;
        registry.register_run(&"task:tk1:phase:0".to_string(), "run-task").await;
        registry.register_run(&"tasklist:tl1".to_string(), "run-tasklist").await;

        let all = registry.all_active_runs().await;
        let as_map: HashMap<String, HashSet<String>> = all.into_iter().collect();

        assert_eq!(as_map.len(), 5);
        assert_eq!(
            as_map.get("agent-1"),
            Some(&HashSet::from(["run-agent".to_string()]))
        );
        assert_eq!(
            as_map.get("team:t1:agent-2"),
            Some(&HashSet::from(["run-team".to_string()]))
        );
        assert_eq!(
            as_map.get("project:p1"),
            Some(&HashSet::from(["run-project".to_string()]))
        );
        assert_eq!(
            as_map.get("task:tk1:phase:0"),
            Some(&HashSet::from(["run-task".to_string()]))
        );
        assert_eq!(
            as_map.get("tasklist:tl1"),
            Some(&HashSet::from(["run-tasklist".to_string()]))
        );
    }

    /// Runs that unregister no longer appear in `all_active_runs`, and keys
    /// with multiple concurrent runs surface every run_id under one key.
    #[tokio::test]
    async fn all_active_runs_reflects_unregister_and_multi_run_keys() {
        let registry = InstanceRegistry::new();
        let agent: AgentId = "agent-multi".to_string();

        registry.register_run(&agent, "run-a").await;
        registry.register_run(&agent, "run-b").await;
        registry.register_run(&"agent-solo".to_string(), "run-solo").await;

        let all = registry.all_active_runs().await;
        let as_map: HashMap<String, HashSet<String>> = all.into_iter().collect();
        assert_eq!(
            as_map.get("agent-multi"),
            Some(&HashSet::from(["run-a".to_string(), "run-b".to_string()]))
        );
        assert_eq!(as_map.len(), 2);

        registry.unregister_run(&"agent-solo".to_string(), "run-solo").await;
        let all = registry.all_active_runs().await;
        let as_map: HashMap<String, HashSet<String>> = all.into_iter().collect();
        assert_eq!(as_map.len(), 1);
        assert!(!as_map.contains_key("agent-solo"));
    }

    /// Clearing the active-tasklist flag restores the original cap.
    #[tokio::test]
    async fn clear_active_tasklist_restores_cap() {
        let registry = Arc::new(InstanceRegistry::new());
        let agent: AgentId = "agent-cleared".to_string();

        registry.mark_has_active_tasklist(&agent).await;
        registry.register_run(&agent, "r1").await;
        // Active tasklist → effective cap 2, can spawn second.
        assert!(registry.can_spawn(&agent, 1).await);

        // Tasklist goes terminal.
        registry.clear_has_active_tasklist(&agent).await;
        // Now cap is back to 1, running_count=1 → cannot spawn.
        assert!(!registry.can_spawn(&agent, 1).await);
    }
}
