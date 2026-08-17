use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ao_engine_tools_core::background_agents::BackgroundAgentRegistry;
use ao_engine_tools_core::background_commands::BackgroundCommandRegistry;
use ao_engine_tools_core::context::{DEFAULT_BACKGROUND_AGENT_CAP, DEFAULT_BACKGROUND_COMMAND_CAP};
use ao_engine_tools_core::ReadFileState;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Contextual info about the parent session when registering a delegated child.
/// Set by SubagentSpawner/Delegate tool; None for top-level sessions.
pub struct ParentSessionInfo {
    pub session_id: String,
    pub agent_id: String,
    pub current_cwd: PathBuf,
}

/// Per-invocation MCP session state keyed by session_id (UUID).
/// Concurrent spawns of the same agent profile each get an isolated entry.
pub struct McpAgentSession {
    pub agent_id: String,
    /// Uses `std::sync::RwLock` (not tokio) so the Arc can be shared with
    /// `RunnerContext.cwd` (also `std::sync::RwLock`) enabling Bash-cd writes
    /// to propagate back to the session entry without extra bookkeeping.
    pub cwd: Arc<std::sync::RwLock<PathBuf>>,
    /// Session-scoped read snapshots, shared with every per-request
    /// `RunnerContext` the MCP HTTP route builds for this session.
    ///
    /// The native runner keeps one long-lived context for an entire run, so its
    /// default `ReadFileState` already persists across all of that run's tool
    /// calls. The MCP HTTP path is different: each JSON-RPC call builds a fresh
    /// context and drops it on return. Without a session home for these
    /// snapshots, a `Read` performed in one call would be invisible to an
    /// `Edit`/`Write` in the next call, so the read-before-write guard would
    /// reject every edit a CLI-spawned agent attempts. Binding each per-request
    /// context to this Arc (via `RunnerContext::with_read_file_state_arc`) lets
    /// the read snapshot survive between calls — the same Arc-sharing trick used
    /// for `cwd` above.
    pub read_file_state: Arc<ReadFileState>,
    pub window_floor_ts: Arc<RwLock<Option<DateTime<Utc>>>>,
    pub parent_session_id: Option<String>,
    pub parent_agent_id: Option<String>,
    /// Snapshot of the parent's current_cwd at delegation time; used by the memory
    /// scope resolver so project-scope writes default to the parent's project.
    pub parent_current_cwd: Option<PathBuf>,
    pub delegation_depth: u32,
    /// Updated on every successful MCP request and at registration.
    /// Used by the TTL sweep to evict orphaned entries.
    pub last_seen_at: RwLock<Instant>,
    /// Session-scoped registry of in-flight background agents.
    ///
    /// The MCP route builds a fresh RunnerContext per JSON-RPC call and drops it
    /// on return. Without a session home, handles inserted by Delegate mode=async
    /// in request N are gone before DelegateOutput can find them
    /// in request N+1. Storing the registry here and binding it into each
    /// per-request context via RunnerContext::with_background_agents mirrors the
    /// cwd and read_file_state patterns already used for cross-request state.
    pub background_agents: Arc<BackgroundAgentRegistry>,
    /// Session-scoped registry of live background shell commands.
    ///
    /// Exactly the same defect as `background_agents` above, one tool family
    /// over: `Bash { run_in_background: true }` registers a handle in the
    /// per-request context, that context is dropped when the JSON-RPC call
    /// returns, and the `BashStatus`/`BashKill` call that arrives in request
    /// N+1 searches a freshly-minted empty registry. The tools then reject
    /// the very `process_id` the `Bash` call just handed the model, so the
    /// subprocess runs to completion with nothing able to poll or stop it.
    ///
    /// Bound into each per-request context via
    /// `RunnerContext::with_background_commands`, mirroring `cwd`,
    /// `read_file_state`, and `background_agents`.
    pub background_commands: Arc<BackgroundCommandRegistry>,
    /// Shared counter of this session's tool calls currently suspended on a
    /// synchronous `AskUserQuestionWithForm` answer. Created once per session
    /// (same Arc-sharing pattern as `background_agents`/`read_file_state`
    /// above) and handed to both:
    /// - every per-request `LiveFormBridge` the MCP route handler builds for
    ///   this session (the writer — see `LiveFormBridge::with_suspension_counter`
    ///   in `ao_engine_tools_runner::prompt_bridge`), and
    /// - the CLI continuation loop's `SpawnInput.form_suspended` for this
    ///   step's subprocess spawn (the reader — see
    ///   `ao_process::default_supervisor`'s overall-timeout deadline loop).
    ///
    /// Distinct from `tools_in_flight` (owned separately, per-spawn, by the
    /// output normalizer): a long `Bash` call also holds that counter above
    /// zero but must NOT pause the overall wall-clock deadline — only a
    /// genuine blocked-on-human form wait does.
    pub form_suspended: Arc<AtomicUsize>,
    /// Delegation chain carried from the AgentRunRequest that spawned this CLI
    /// subprocess. The MCP route handler reads these and sets them on every
    /// RunnerContext it builds so Delegate-tool depth/cycle checks see the full
    /// ancestry even when tools are dispatched via HTTP rather than in-process.
    pub delegate_chain: Vec<String>,
    pub spawn_chain: Vec<String>,
    /// When this session is serving a project-channel run, the project ID is
    /// stored here so the MCP route handler can inject it into every
    /// per-request RunnerContext built for this session.
    pub project_id: Option<String>,
    /// When this session was spawned from a specific (possibly non-default)
    /// thread, the thread ID is stored here so the MCP route handler can
    /// inject it into every per-request RunnerContext built for this session,
    /// mirroring `project_id` above.
    pub thread_id: Option<String>,
    /// Cancellation token for this session's lifetime, minted fresh at
    /// registration and never reused across sessions.
    ///
    /// The MCP route handler binds every per-request `RunnerContext` it
    /// builds for this session to a clone of this token (via
    /// `RunnerContext::with_cancel`) instead of leaving `ctx.cancel` at its
    /// own private default — a context that nobody else can ever cancel.
    /// `McpSessionStore::remove` (and the eviction paths that funnel through
    /// it — subprocess-exit's `McpSessionGuard::drop`, the `/sessions`
    /// DELETE route, and the TTL sweep) cancels this token as part of
    /// tearing the session down, so a tool call genuinely suspended on this
    /// session (e.g. a synchronous `AskUserQuestionWithForm` wait) resolves
    /// as cancelled instead of hanging until its own deadline.
    pub cancel: CancellationToken,
}

impl McpAgentSession {
    fn new(
        agent_id: String,
        cwd: PathBuf,
        parent_info: Option<ParentSessionInfo>,
        delegation_depth: u32,
        delegate_chain: Vec<String>,
        spawn_chain: Vec<String>,
        project_id: Option<String>,
        thread_id: Option<String>,
    ) -> Arc<Self> {
        let (parent_session_id, parent_agent_id, parent_current_cwd) = match parent_info {
            Some(p) => (Some(p.session_id), Some(p.agent_id), Some(p.current_cwd)),
            None => (None, None, None),
        };
        Arc::new(Self {
            agent_id,
            cwd: Arc::new(std::sync::RwLock::new(cwd)),
            read_file_state: Arc::new(ReadFileState::default()),
            window_floor_ts: Arc::new(RwLock::new(None)),
            parent_session_id,
            parent_agent_id,
            parent_current_cwd,
            delegation_depth,
            last_seen_at: RwLock::new(Instant::now()),
            background_agents: Arc::new(BackgroundAgentRegistry::new(DEFAULT_BACKGROUND_AGENT_CAP)),
            background_commands: Arc::new(BackgroundCommandRegistry::new(
                DEFAULT_BACKGROUND_COMMAND_CAP,
            )),
            form_suspended: Arc::new(AtomicUsize::new(0)),
            delegate_chain,
            spawn_chain,
            project_id,
            thread_id,
            cancel: CancellationToken::new(),
        })
    }
}

/// Thread-safe store of per-invocation MCP sessions, keyed by session_id (UUID string).
/// Use `register_session` to create entries and `get_by_session_id` to retrieve them.
/// `list_by_agent_id` provides an observability scan for tooling that knows agent_id only.
pub struct McpSessionStore {
    sessions: Arc<DashMap<String, Arc<McpAgentSession>>>,
}

impl McpSessionStore {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
        }
    }

    /// Register a new session explicitly. Returns the session Arc on success or
    /// `Err(())` if `session_id` is already registered (caller should treat as 409).
    ///
    /// `delegation_depth` is computed automatically: 0 for top-level sessions
    /// (parent_info is None), or parent.delegation_depth + 1 for children. If
    /// the parent session is not yet in the store, the child defaults to depth 1.
    pub fn register_session(
        &self,
        session_id: String,
        agent_id: String,
        cwd: PathBuf,
        parent_info: Option<ParentSessionInfo>,
    ) -> Result<Arc<McpAgentSession>, ()> {
        self.register_session_with_chains(session_id, agent_id, cwd, parent_info, vec![], vec![], None, None)
    }

    /// Like [`register_session`] but also stores the delegation chain vectors
    /// and optional project/thread scope so the MCP route handler can propagate
    /// them into `RunnerContext` for every tool-call request within this session.
    pub fn register_session_with_chains(
        &self,
        session_id: String,
        agent_id: String,
        cwd: PathBuf,
        parent_info: Option<ParentSessionInfo>,
        delegate_chain: Vec<String>,
        spawn_chain: Vec<String>,
        project_id: Option<String>,
        thread_id: Option<String>,
    ) -> Result<Arc<McpAgentSession>, ()> {
        // Reject duplicate session_id without touching the existing entry.
        if self.sessions.contains_key(&session_id) {
            return Err(());
        }
        // Resolve delegation_depth: look up parent's depth to compute child's.
        let delegation_depth = parent_info
            .as_ref()
            .map(|p| {
                self.sessions
                    .get(&p.session_id)
                    .map(|s| s.delegation_depth + 1)
                    .unwrap_or(1)
            })
            .unwrap_or(0);
        let session = McpAgentSession::new(
            agent_id,
            cwd,
            parent_info,
            delegation_depth,
            delegate_chain,
            spawn_chain,
            project_id,
            thread_id,
        );
        // Use entry().or_insert_with() so a concurrent insert wins and we detect the duplicate.
        let entry = self
            .sessions
            .entry(session_id)
            .or_insert_with(|| Arc::clone(&session));
        // If another thread inserted a different Arc, the value won't ptr_eq our session.
        if Arc::ptr_eq(&*entry, &session) {
            Ok(Arc::clone(&session))
        } else {
            Err(())
        }
    }

    /// Look up a session by its session_id. Returns None if not registered.
    pub fn get_by_session_id(&self, session_id: &str) -> Option<Arc<McpAgentSession>> {
        self.sessions.get(session_id).map(|e| Arc::clone(&*e))
    }

    /// Return all sessions associated with a given agent_id (for observability).
    /// This is a linear scan and not intended for hot paths.
    pub fn list_by_agent_id(&self, agent_id: &str) -> Vec<Arc<McpAgentSession>> {
        self.sessions
            .iter()
            .filter(|e| e.value().agent_id == agent_id)
            .map(|e| Arc::clone(e.value()))
            .collect()
    }

    /// Return every registered session, across every agent. Used by the
    /// `/system/stream` connect-time replay to find every session's
    /// `background_agents` registry and reconfirm any async delegation still
    /// live in this server process. Like `list_by_agent_id`, a linear scan —
    /// fine for a connect-time one-off, not a hot path.
    pub fn all_sessions(&self) -> Vec<Arc<McpAgentSession>> {
        self.sessions.iter().map(|e| Arc::clone(e.value())).collect()
    }

    /// Set the session's replay window floor.
    ///
    /// The `Arc` is cloned out and the DashMap `Ref` dropped before the
    /// `.await`. A `Ref` held across a suspension point keeps that shard's
    /// lock held for as long as the task is parked, so every other task
    /// touching the same shard blocks behind it. Cloning first makes the
    /// hazard impossible by construction rather than relying on the write
    /// lock happening to be uncontended.
    pub async fn update_floor(&self, session_id: &str, ts: DateTime<Utc>) {
        let Some(session) = self.get_by_session_id(session_id) else {
            return;
        };
        let mut floor = session.window_floor_ts.write().await;
        *floor = Some(ts);
    }

    /// Deregister `session_id` and cancel its [`McpAgentSession::cancel`]
    /// token. Cancellation is unconditional — a session that never had a
    /// live suspended tool call simply cancels a token nobody was waiting on,
    /// which is a no-op. This is the single funnel every session-teardown
    /// path (subprocess-exit's `McpSessionGuard::drop`, the `/sessions`
    /// DELETE route, and [`Self::sweep_expired_sessions`] below) goes
    /// through, so "the session's run ended" and "the session's cancel token
    /// fired" can never drift apart.
    pub fn remove(&self, session_id: &str) {
        if let Some((_, session)) = self.sessions.remove(session_id) {
            session.cancel.cancel();
        }
    }

    /// Evict sessions whose `last_seen_at` is older than `ttl`.
    /// Called by the background sweep task every 10 minutes.
    /// Uses `try_read()` on each session's lock: if the lock is held (session is
    /// being actively updated), the entry is skipped — it's not expired.
    /// Returns the number of sessions evicted.
    pub fn sweep_expired_sessions(&self, ttl: Duration) -> usize {
        let stale_ids: Vec<String> = self
            .sessions
            .iter()
            .filter_map(|e| {
                e.last_seen_at
                    .try_read()
                    .ok()
                    .filter(|guard| guard.elapsed() > ttl)
                    .map(|_| e.key().clone())
            })
            .collect();
        let count = stale_ids.len();
        for sid in stale_ids {
            self.remove(&sid);
        }
        count
    }

    /// Remove all sessions for a given agent_id. Used by `prepare_mcp_session` to
    /// clean up prior sessions before registering a fresh one per subprocess spawn.
    pub fn remove_all_for_agent_id(&self, agent_id: &str) {
        let sids: Vec<String> = self
            .sessions
            .iter()
            .filter(|e| e.value().agent_id == agent_id)
            .map(|e| e.key().clone())
            .collect();
        for sid in sids {
            self.remove(&sid);
        }
    }
}

impl Default for McpSessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn register_and_get_by_session_id() {
        let store = McpSessionStore::new();
        let cwd = PathBuf::from("/tmp/agent-a");
        let sid = "session-uuid-1".to_string();

        let s1 = store
            .register_session(sid.clone(), "agent-a".to_string(), cwd.clone(), None)
            .expect("first registration should succeed");

        // Same session_id returns Err
        let result = store.register_session(
            sid.clone(),
            "agent-a".to_string(),
            PathBuf::from("/tmp/other"),
            None,
        );
        assert!(result.is_err(), "duplicate session_id must return Err");

        // get_by_session_id returns the same Arc
        let s2 = store.get_by_session_id(&sid).expect("must be found");
        assert!(Arc::ptr_eq(&s1, &s2));

        // cwd is still the original value
        let current_cwd = s1.cwd.read().unwrap().clone();
        assert_eq!(current_cwd, cwd);
    }

    #[tokio::test]
    async fn all_sessions_returns_every_registered_session_across_agents() {
        let store = McpSessionStore::new();
        store
            .register_session("sid-all-1".to_string(), "agent-all-a".to_string(), PathBuf::from("/tmp/1"), None)
            .expect("ok");
        store
            .register_session("sid-all-2".to_string(), "agent-all-b".to_string(), PathBuf::from("/tmp/2"), None)
            .expect("ok");

        let all = store.all_sessions();
        assert_eq!(all.len(), 2);
        let agent_ids: Vec<&str> = all.iter().map(|s| s.agent_id.as_str()).collect();
        assert!(agent_ids.contains(&"agent-all-a"));
        assert!(agent_ids.contains(&"agent-all-b"));
    }

    #[tokio::test]
    async fn all_sessions_is_empty_for_a_fresh_store() {
        let store = McpSessionStore::new();
        assert!(store.all_sessions().is_empty());
    }

    #[tokio::test]
    async fn list_by_agent_id_returns_all_sessions_for_agent() {
        let store = McpSessionStore::new();

        store
            .register_session(
                "sid-1".to_string(),
                "agent-x".to_string(),
                PathBuf::from("/tmp/1"),
                None,
            )
            .expect("ok");
        store
            .register_session(
                "sid-2".to_string(),
                "agent-x".to_string(),
                PathBuf::from("/tmp/2"),
                None,
            )
            .expect("ok");
        store
            .register_session(
                "sid-3".to_string(),
                "agent-y".to_string(),
                PathBuf::from("/tmp/3"),
                None,
            )
            .expect("ok");

        let x_sessions = store.list_by_agent_id("agent-x");
        assert_eq!(x_sessions.len(), 2, "agent-x must have 2 sessions");

        let y_sessions = store.list_by_agent_id("agent-y");
        assert_eq!(y_sessions.len(), 1, "agent-y must have 1 session");

        let z_sessions = store.list_by_agent_id("agent-z");
        assert!(z_sessions.is_empty(), "unknown agent must return empty");
    }

    #[tokio::test]
    async fn update_floor_sets_timestamp() {
        let store = McpSessionStore::new();
        store
            .register_session(
                "sid-b".to_string(),
                "agent-b".to_string(),
                PathBuf::from("/tmp/b"),
                None,
            )
            .expect("ok");

        let ts = Utc::now();
        store.update_floor("sid-b", ts).await;

        let session = store.get_by_session_id("sid-b").unwrap();
        let floor = *session.window_floor_ts.read().await;
        assert_eq!(floor, Some(ts));
    }

    #[tokio::test]
    async fn remove_clears_session() {
        let store = McpSessionStore::new();
        store
            .register_session(
                "sid-c".to_string(),
                "agent-c".to_string(),
                PathBuf::from("/tmp/c"),
                None,
            )
            .expect("ok");
        assert!(store.get_by_session_id("sid-c").is_some());

        store.remove("sid-c");
        assert!(store.get_by_session_id("sid-c").is_none());
    }

    #[tokio::test]
    async fn remove_cancels_the_session_token() {
        let store = McpSessionStore::new();
        store
            .register_session(
                "sid-cancel".to_string(),
                "agent-cancel".to_string(),
                PathBuf::from("/tmp/cancel"),
                None,
            )
            .expect("ok");
        let session = store.get_by_session_id("sid-cancel").unwrap();
        assert!(
            !session.cancel.is_cancelled(),
            "a freshly registered session must start with a live token"
        );

        store.remove("sid-cancel");

        assert!(
            session.cancel.is_cancelled(),
            "removing a session must cancel its token so anything still \
             suspended on it (e.g. a synchronous form wait) unblocks"
        );
    }

    #[tokio::test]
    async fn sweep_cancels_tokens_of_evicted_sessions() {
        let store = McpSessionStore::new();
        store
            .register_session(
                "sid-sweep-stale".to_string(),
                "agent-sweep".to_string(),
                PathBuf::from("/tmp/sweep"),
                None,
            )
            .expect("ok");
        let session = store.get_by_session_id("sid-sweep-stale").unwrap();
        *session.last_seen_at.write().await = Instant::now() - Duration::from_secs(7200);

        let evicted = store.sweep_expired_sessions(Duration::from_secs(3600));
        assert_eq!(evicted, 1);
        assert!(
            session.cancel.is_cancelled(),
            "TTL-evicted sessions must have their token cancelled just like \
             an explicit remove()"
        );
    }

    #[tokio::test]
    async fn parent_info_propagated_correctly() {
        let store = McpSessionStore::new();
        let parent = ParentSessionInfo {
            session_id: "parent-sid".to_string(),
            agent_id: "parent-agent".to_string(),
            current_cwd: PathBuf::from("/tmp/parent"),
        };
        store
            .register_session(
                "child-sid".to_string(),
                "child-agent".to_string(),
                PathBuf::from("/tmp/child"),
                Some(parent),
            )
            .expect("ok");

        let session = store.get_by_session_id("child-sid").unwrap();
        assert_eq!(session.parent_session_id.as_deref(), Some("parent-sid"));
        assert_eq!(session.parent_agent_id.as_deref(), Some("parent-agent"));
        assert_eq!(
            session.parent_current_cwd.as_deref(),
            Some(std::path::Path::new("/tmp/parent"))
        );
        assert_eq!(session.delegation_depth, 1);
    }

    #[tokio::test]
    async fn sweep_evicts_expired_sessions_and_keeps_fresh_ones() {
        let store = McpSessionStore::new();

        store
            .register_session(
                "fresh".to_string(),
                "agent-fresh".to_string(),
                PathBuf::from("/tmp/fresh"),
                None,
            )
            .expect("ok");
        store
            .register_session(
                "stale1".to_string(),
                "agent-stale".to_string(),
                PathBuf::from("/tmp/stale1"),
                None,
            )
            .expect("ok");
        store
            .register_session(
                "stale2".to_string(),
                "agent-stale".to_string(),
                PathBuf::from("/tmp/stale2"),
                None,
            )
            .expect("ok");

        // Backdate last_seen_at for stale sessions to 2 hours ago.
        let two_hours_ago = Instant::now() - Duration::from_secs(7200);
        for sid in ["stale1", "stale2"] {
            let session = store.get_by_session_id(sid).unwrap();
            *session.last_seen_at.write().await = two_hours_ago;
        }

        let evicted = store.sweep_expired_sessions(Duration::from_secs(3600));
        assert_eq!(evicted, 2, "two stale sessions must be evicted");

        assert!(
            store.get_by_session_id("fresh").is_some(),
            "fresh session must survive the sweep"
        );
        assert!(
            store.get_by_session_id("stale1").is_none(),
            "stale1 must be evicted"
        );
        assert!(
            store.get_by_session_id("stale2").is_none(),
            "stale2 must be evicted"
        );
    }

    // The deadline deliberately lives outside the runtime under test, on a
    // plain std channel, and the runtime is multi-threaded to match
    // production (`ao-server/src/main.rs` builds via `Runtime::new()`).
    //
    // `tokio::time::timeout` cannot serve as the detector here. DashMap
    // shard locks are synchronous, so a task parked while holding a `Ref`
    // blocks whichever worker thread next touches that shard; once every
    // worker is blocked, nothing drives tokio's time driver and the timeout
    // never fires. Confirmed by experiment: reintroducing a suspension point
    // under a held `Ref` left the run at 0% CPU past 60s on both a
    // current_thread and a 4-worker multi_thread runtime, with the 5s
    // timeout never expiring. `recv_timeout` is an OS-level timed wait on a
    // thread that owns no part of that runtime, so runtime starvation cannot
    // stall it and the assert below stays reachable.
    #[test]
    fn concurrent_insert_update_remove_no_deadlock() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .expect("build test runtime");

            rt.block_on(async {
                let store = Arc::new(McpSessionStore::new());
                let mut handles = vec![];

                for i in 0..20usize {
                    let store_clone = Arc::clone(&store);
                    handles.push(tokio::spawn(async move {
                        let sid = format!("sid-{}", i % 5);
                        let agent_id = format!("agent-{}", i % 5);
                        // register_session may return Err for duplicate sid — that's fine.
                        let _ = store_clone.register_session(
                            sid.clone(),
                            agent_id,
                            PathBuf::from("/tmp"),
                            None,
                        );
                        store_clone.update_floor(&sid, Utc::now()).await;
                        if i % 3 == 0 {
                            store_clone.remove(&sid);
                        }
                    }));
                }

                for h in handles {
                    h.await.unwrap();
                }
            });

            // A deadlocked run never reaches this send, so the receiver below
            // times out instead of blocking forever.
            let _ = done_tx.send(());
        });

        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "concurrent insert/update/remove did not finish within 5s — deadlocked"
        );
    }
}
