use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use ao_protocol::agent::AgentId;
use ao_protocol::error::AoError;
use ao_protocol::tasklist::{Task, TaskStatus, Tasklist, TasklistOwner, TasklistStatus};
use chrono::Utc;
use tokio::sync::Mutex as AsyncMutex;

use crate::paths::DataRoot;

/// On-disk store for tasklists under either ownership.
///
/// Layout is per-owner, and both flavours carry the same set of files
/// (`tasklist.json`, `workspace/`, `transcripts/`):
/// - agent-owned: `{root}/tasks/agents/{agent_id}/tasklists/{tasklist_id}/`
/// - team-owned: `{root}/teams/{team_id}/tasklists/{tasklist_id}/`
///
/// Only the agent-owned path is creatable — `TasklistService::create` rejects
/// `TasklistOwner::Team`. The team subtree is read-only legacy: the variant is
/// still deserializable so installs that predate the removal keep working, and
/// every reader of `teams/` guards on its absence.
///
/// ## Concurrency
///
/// Every mutator follows a read → mutate-in-memory → atomic-write cycle against
/// a single `tasklist.json`. The write swap (tmp file + rename) is atomic, but
/// the surrounding read-modify-write is not — so two concurrent callers can
/// each read the same snapshot and the later write silently clobbers the
/// earlier one (lost update). In parallel tasklist dispatch this manifested as
/// dropped classifier assignments and even tasks re-running after they had
/// already completed (a stale writer reverting a `Completed` status).
///
/// To make each read-modify-write atomic with respect to other writers, every
/// mutating method acquires a per-tasklist async lock keyed by the tasklist's
/// on-disk meta path before touching the file. Reads are not locked. The
/// owner-routing wrappers (`*_by_owner`) delegate to the leaf methods and do
/// NOT take the lock themselves, so the lock is never acquired re-entrantly.
pub struct TasklistStore {
    data_root: DataRoot,
    /// Per-tasklist write locks, keyed by the meta-file path. Entries are
    /// created on first use and intentionally never evicted — the count is
    /// bounded by the number of distinct tasklists the process touches, and
    /// each entry is a single empty-tuple mutex, so the footprint is trivial.
    write_locks: StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

/// Result of [`TasklistStore::try_reclaim_dispatch_by_owner`].
#[derive(Debug)]
pub enum ReclaimDispatchOutcome {
    /// The task is no longer `InProgress` — another actor already resolved
    /// it (completed, failed, cancelled, ...). The caller should no-op.
    ///
    /// `observed` is the status read INSIDE this call's locked section — the
    /// authoritative value the rejection was actually decided on. It is
    /// handed back deliberately: a caller's own pre-lock snapshot of the
    /// status may already be out of date by the time the lock is acquired, so
    /// a caller that logged its snapshot would report a status the store
    /// never acted on. Log this field instead of any snapshot the caller
    /// holds.
    ///
    /// INVARIANT for future writers: the no-op is only safe because every
    /// writer that moves a task off `InProgress` also advances it — either by
    /// driving the terminal hook that dispatches the next task, or by owning
    /// its own resume path. A writer that parks a task in some other status
    /// without advancing it would strand that task silently, because the
    /// caller here simply returns and the watchdog cannot rescue it: the
    /// watchdog only ever considers `InProgress` candidates, so a parked task
    /// is invisible to recovery. Any new off-`InProgress` transition must
    /// therefore carry its own advance or resume path.
    NotInProgress { observed: TaskStatus },
    /// `expected_token` no longer matched the task's live `dispatch_token` —
    /// a concurrent caller already won the reclaim race for this recovery
    /// cycle. The caller must NOT dispatch.
    Stale,
    /// The reclaim pushed `attempt_count` to (or past) `max_attempts` inside
    /// this call; the task was transitioned to `Failed` as part of the same
    /// locked write. The caller should drive the terminal hook, not dispatch.
    Exhausted { attempt_count: u32 },
    /// The reclaim won: `attempt_count` and `dispatch_token` were bumped and
    /// persisted atomically. `task` is the fresh post-bump snapshot — build
    /// the reprompt from it, not from any pre-lock read.
    Claimed {
        attempt_count: u32,
        dispatch_token: u64,
        task: Task,
    },
}

impl TasklistStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self {
            data_root,
            write_locks: StdMutex::new(HashMap::new()),
        }
    }

    /// Fetch (or lazily create) the write lock for a given tasklist meta path.
    /// The std mutex guarding the map is held only for the brief lookup/insert
    /// and never across an `.await`, so it cannot deadlock the async runtime.
    fn write_lock_for(&self, key: String) -> Arc<AsyncMutex<()>> {
        let mut map = self
            .write_locks
            .lock()
            .expect("tasklist write-lock map poisoned");
        map.entry(key)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Lock key for a team-owned tasklist (its on-disk meta path).
    fn team_lock_key(&self, team_id: &str, tasklist_id: &str) -> String {
        self.data_root
            .tasklist_meta_path(team_id, tasklist_id)
            .to_string_lossy()
            .into_owned()
    }

    /// Lock key for an agent-owned tasklist (its on-disk meta path).
    fn agent_lock_key(&self, agent_id: &str, tasklist_id: &str) -> String {
        self.data_root
            .agent_tasklist_meta_path(agent_id, tasklist_id)
            .to_string_lossy()
            .into_owned()
    }

    pub fn data_root(&self) -> &DataRoot {
        &self.data_root
    }

    fn validate_id(label: &str, id: &str) -> Result<(), AoError> {
        if id.is_empty() {
            return Err(AoError::ValidationError(format!("{label} cannot be empty")));
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(AoError::ValidationError(format!(
                "{label} '{id}' contains invalid characters; only alphanumeric, '-', '_' allowed"
            )));
        }
        Ok(())
    }

    async fn write_meta_atomic(&self, tasklist: &Tasklist) -> Result<(), AoError> {
        let path = match &tasklist.owner {
            TasklistOwner::Team { team_id } => {
                self.data_root.tasklist_meta_path(team_id, &tasklist.id)
            }
            TasklistOwner::Agent { agent_id } => {
                self.data_root.agent_tasklist_meta_path(agent_id, &tasklist.id)
            }
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(tasklist)
            .map_err(|e| AoError::Json(e.to_string()))?;
        let tmp = path.with_file_name(format!(
            "tasklist.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::write(&tmp, json).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    /// Create a new team-owned tasklist on disk. Returns `TasklistAlreadyActive`
    /// if the team already has an active tasklist — no auto-queue, no replace.
    ///
    /// Agent-owned tasklists must go through [`Self::create_for_agent`]. Passing
    /// one here used to be accepted silently: `team_id` is `None` for an agent
    /// owner, `unwrap_or_default()` turned that into `""`, and `Path::join("")`
    /// collapses — so the tasklist was created under `{root}/teams/` in a
    /// directory named for its own id. Rejecting it makes the misuse loud
    /// instead of producing a plausible-looking directory in the wrong tree.
    pub async fn create(&self, tasklist: &Tasklist) -> Result<(), AoError> {
        let team_id_str = match &tasklist.owner {
            TasklistOwner::Team { team_id } => team_id.as_str(),
            TasklistOwner::Agent { agent_id } => {
                return Err(AoError::ValidationError(format!(
                    "tasklist '{}' is owned by agent '{}'; use create_for_agent \
                     (TasklistStore::create only handles team-owned tasklists)",
                    tasklist.id, agent_id
                )));
            }
        };
        Self::validate_id("Team ID", team_id_str)?;
        Self::validate_id("Tasklist ID", &tasklist.id)?;

        if let Some(existing) = self.find_active(team_id_str).await? {
            // Report the id we actually checked the slot against, not the
            // legacy `team_id` mirror — the two can disagree, and naming the
            // mirror here would point the caller at the wrong team.
            return Err(AoError::TasklistAlreadyActive {
                team_id: team_id_str.to_string(),
                tasklist_id: existing.id,
            });
        }

        let dir = self
            .data_root
            .tasklist_dir(team_id_str, &tasklist.id);
        if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            return Err(AoError::ValidationError(format!(
                "Tasklist directory already exists: {}",
                dir.display()
            )));
        }

        tokio::fs::create_dir_all(
            self.data_root
                .tasklist_workspace_dir(team_id_str, &tasklist.id),
        )
        .await?;
        tokio::fs::create_dir_all(
            self.data_root
                .tasklist_transcripts_dir(team_id_str, &tasklist.id),
        )
        .await?;

        self.write_meta_atomic(tasklist).await?;
        Ok(())
    }

    /// Load a single tasklist by id.
    pub async fn get(
        &self,
        team_id: &str,
        tasklist_id: &str,
    ) -> Result<Option<Tasklist>, AoError> {
        let path = self.data_root.tasklist_meta_path(team_id, tasklist_id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(None);
        }
        let contents = tokio::fs::read_to_string(&path).await?;
        let tasklist: Tasklist =
            serde_json::from_str(&contents).map_err(|e| AoError::Json(e.to_string()))?;
        Ok(Some(tasklist))
    }

    /// List every tasklist for a team (any status), sorted by creation time descending.
    pub async fn list(&self, team_id: &str) -> Result<Vec<Tasklist>, AoError> {
        let dir = self.data_root.team_tasklists_dir(team_id);
        if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let mut entries = tokio::fs::read_dir(&dir).await?;
        let mut out = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let meta_path = entry.path().join("tasklist.json");
            if !tokio::fs::try_exists(&meta_path).await.unwrap_or(false) {
                continue;
            }
            let contents = tokio::fs::read_to_string(&meta_path).await?;
            match serde_json::from_str::<Tasklist>(&contents) {
                Ok(tl) => out.push(tl),
                Err(e) => {
                    tracing::warn!("Failed to parse tasklist {:?}: {}", meta_path, e);
                }
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    /// Walk every team's tasklist directory and return all tasklists currently
    /// in `Active` status. Used by the dispatch watchdog to find stuck tasks
    /// across the entire system on each tick.
    pub async fn list_active_across_teams(&self) -> Result<Vec<Tasklist>, AoError> {
        let teams_dir = self.data_root.teams_dir();
        if !tokio::fs::try_exists(&teams_dir).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let mut team_entries = tokio::fs::read_dir(&teams_dir).await?;
        let mut out = Vec::new();
        while let Some(team_entry) = team_entries.next_entry().await? {
            if !team_entry.file_type().await?.is_dir() {
                continue;
            }
            let Some(team_id) = team_entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            for tl in self.list(&team_id).await? {
                if tl.status == TasklistStatus::Active {
                    out.push(tl);
                }
            }
        }
        Ok(out)
    }

    /// Walk every agent's tasklist directory and return active (Active | Paused)
    /// tasklists. Mirrors `list_active_across_teams` for agent-owned tasklists so
    /// the watchdog and startup advance can cover both ownership paths.
    pub async fn list_active_across_agents(&self) -> Result<Vec<Tasklist>, AoError> {
        let agents_dir = self.data_root.tasks_agents_dir();
        if !tokio::fs::try_exists(&agents_dir).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let mut agent_entries = tokio::fs::read_dir(&agents_dir).await?;
        let mut out = Vec::new();
        while let Some(agent_entry) = agent_entries.next_entry().await? {
            if !agent_entry.file_type().await?.is_dir() {
                continue;
            }
            let Some(agent_id) = agent_entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            for tl in self.list_for_agent(&agent_id).await? {
                if matches!(tl.status, TasklistStatus::Active | TasklistStatus::Paused) {
                    out.push(tl);
                }
            }
        }
        Ok(out)
    }

    /// Walk every team's tasklist directory and return ALL tasklists, regardless
    /// of status. Used by the co-pilot mailbox poller to rebuild its enrolled
    /// set on startup — `is_tasklist_active` from the lifecycle module
    /// classifies "active" using a 24h heartbeat in addition to the
    /// `TasklistStatus::Active` check, so the poller needs to inspect every
    /// tasklist (a Completed tasklist with a recent overlay open is still
    /// active per that predicate). Returns `Ok(Vec::new())` on a fresh
    /// data root with no `teams/` directory yet.
    pub async fn list_all_across_teams(&self) -> Result<Vec<Tasklist>, AoError> {
        let teams_dir = self.data_root.teams_dir();
        if !tokio::fs::try_exists(&teams_dir).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let mut team_entries = tokio::fs::read_dir(&teams_dir).await?;
        let mut out = Vec::new();
        while let Some(team_entry) = team_entries.next_entry().await? {
            if !team_entry.file_type().await?.is_dir() {
                continue;
            }
            let Some(team_id) = team_entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            for tl in self.list(&team_id).await? {
                out.push(tl);
            }
        }
        Ok(out)
    }

    /// Return the team's currently-active tasklist, if any. A paused tasklist
    /// still occupies the "active slot" (a new tasklist cannot be created while
    /// the previous one is paused — the user must resume or cancel it first).
    pub async fn find_active(&self, team_id: &str) -> Result<Option<Tasklist>, AoError> {
        Ok(self
            .list(team_id)
            .await?
            .into_iter()
            .find(|t| {
                matches!(
                    t.status,
                    TasklistStatus::Active | TasklistStatus::Paused
                )
            }))
    }

    /// Replace a tasklist's status, validating the transition. Active is the only
    /// non-terminal state; once a tasklist leaves Active it cannot return.
    pub async fn set_status(
        &self,
        team_id: &str,
        tasklist_id: &str,
        next: TasklistStatus,
    ) -> Result<Tasklist, AoError> {
        let lock = self.write_lock_for(self.team_lock_key(team_id, tasklist_id));
        let _guard = lock.lock().await;

        let mut tasklist = self
            .get(team_id, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.to_string()))?;

        if tasklist.status == next {
            return Ok(tasklist);
        }

        let allowed = matches!(
            (tasklist.status, next),
            (TasklistStatus::Active, TasklistStatus::Completed)
                | (TasklistStatus::Active, TasklistStatus::Failed)
                | (TasklistStatus::Active, TasklistStatus::Cancelled)
                | (TasklistStatus::Active, TasklistStatus::Paused)
                | (TasklistStatus::Paused, TasklistStatus::Active)
                | (TasklistStatus::Paused, TasklistStatus::Cancelled)
                | (TasklistStatus::Paused, TasklistStatus::Failed)
                // Continue: user-initiated revival of a Failed tasklist after
                // they've fixed the underlying cause (permissions, missing
                // skill, etc.) and reset the failed tasks back to Pending.
                | (TasklistStatus::Failed, TasklistStatus::Active)
                // Discard a Failed tasklist (terminal-by-user-decision).
                | (TasklistStatus::Failed, TasklistStatus::Cancelled)
                // Append-to-terminal revival: adding a task to a
                // terminal tasklist flips it back to Paused. Never auto-Active
                // — the user resumes manually when the active slot is free.
                | (TasklistStatus::Completed, TasklistStatus::Paused)
                | (TasklistStatus::Failed, TasklistStatus::Paused)
                | (TasklistStatus::Cancelled, TasklistStatus::Paused)
        );
        if !allowed {
            return Err(AoError::InvalidTasklistTransition(format!(
                "{:?} -> {:?}",
                tasklist.status, next
            )));
        }

        // Stamp `last_active_at` whenever the tasklist leaves `Active`. The
        // append-task auto-resume window reads this to decide whether a newly
        // Completed tasklist can flip back to Active when a task is appended
        // shortly after completion. Captures all three production exit paths
        // (auto-complete, on_task_terminal failure, user pause) since they all
        // funnel through this chokepoint.
        if tasklist.status == TasklistStatus::Active {
            tasklist.last_active_at = Some(Utc::now());
        }

        tasklist.status = next;
        self.write_meta_atomic(&tasklist).await?;
        Ok(tasklist)
    }

    /// Replace a single task's status, validating the transition. Returns the
    /// updated tasklist for the caller to inspect.
    pub async fn set_task_status(
        &self,
        team_id: &str,
        tasklist_id: &str,
        task_id: &str,
        next: TaskStatus,
    ) -> Result<Tasklist, AoError> {
        let lock = self.write_lock_for(self.team_lock_key(team_id, tasklist_id));
        let _guard = lock.lock().await;

        let mut tasklist = self
            .get(team_id, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.to_string()))?;

        let task = find_task_mut(&mut tasklist, task_id)
            .ok_or_else(|| AoError::TaskNotFound(task_id.to_string()))?;

        if !is_valid_task_transition(task.status, next) {
            return Err(AoError::InvalidTasklistTransition(format!(
                "task {task_id}: {:?} -> {:?}",
                task.status, next
            )));
        }
        task.status = next;
        self.write_meta_atomic(&tasklist).await?;
        Ok(tasklist)
    }

    /// Return the co-pilot agent currently bound to this tasklist, if any.
    /// `None` means no binding yet (the overlay has never been opened) or the
    /// tasklist itself does not exist — callers that need to distinguish those
    /// cases should call `get` first.
    pub async fn get_copilot_agent_id(
        &self,
        team_id: &str,
        tasklist_id: &str,
    ) -> Result<Option<AgentId>, AoError> {
        Ok(self
            .get(team_id, tasklist_id)
            .await?
            .and_then(|tl| tl.copilot_agent_id))
    }

    /// Reverse lookup: return the tasklist whose `copilot_agent_id` is the
    /// supplied `agent_id`, walking team-owned tasklists first and then
    /// agent-owned ones. `Ok(None)` when no tasklist references the agent
    /// (including when the agent doesn't exist).
    ///
    /// Both trees must be walked. Team tasklists live under `teams/`, but the
    /// live project co-pilot route binds an *agent-owned* tasklist under
    /// `tasks/agents/`. Enumerating only `teams/` structurally returned
    /// `Ok(None)` for every project co-pilot, which silently disabled context
    /// injection, the `<tasklist action="append">` tag, and the mailbox
    /// poller's sleep sweep for them — the binding persisted but no reader
    /// could ever observe it.
    ///
    /// O(total tasklists across both trees). Intended for the per-message
    /// context-injection path which only fires for agents whose profile is
    /// marked as the co-pilot template, so the effective fan-out is small.
    /// Callers that want to gate by profile-template MUST do that check
    /// themselves — this helper does not filter by agent kind.
    pub async fn find_by_copilot_agent_id(
        &self,
        agent_id: &str,
    ) -> Result<Option<Tasklist>, AoError> {
        let teams_dir = self.data_root.teams_dir();
        if tokio::fs::try_exists(&teams_dir).await.unwrap_or(false) {
            let mut team_entries = tokio::fs::read_dir(&teams_dir).await?;
            while let Some(team_entry) = team_entries.next_entry().await? {
                if !team_entry.file_type().await?.is_dir() {
                    continue;
                }
                let Some(team_id) = team_entry.file_name().to_str().map(|s| s.to_string()) else {
                    continue;
                };
                for tl in self.list(&team_id).await? {
                    if tl.copilot_agent_id.as_deref() == Some(agent_id) {
                        return Ok(Some(tl));
                    }
                }
            }
        }

        let agents_dir = self.data_root.tasks_agents_dir();
        if tokio::fs::try_exists(&agents_dir).await.unwrap_or(false) {
            let mut agent_entries = tokio::fs::read_dir(&agents_dir).await?;
            while let Some(agent_entry) = agent_entries.next_entry().await? {
                if !agent_entry.file_type().await?.is_dir() {
                    continue;
                }
                // The directory name is the tasklist's OWNER agent, which is
                // not the co-pilot agent we're searching for — the co-pilot is
                // a separate agent recorded in each tasklist's meta.
                let Some(owner_agent_id) = agent_entry.file_name().to_str().map(|s| s.to_string())
                else {
                    continue;
                };
                for tl in self.list_for_agent(&owner_agent_id).await? {
                    if tl.copilot_agent_id.as_deref() == Some(agent_id) {
                        return Ok(Some(tl));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Idempotently bind a co-pilot agent to a tasklist. If the tasklist already
    /// has a `copilot_agent_id` set, returns the existing binding unchanged
    /// (the supplied `agent_id` is ignored). Otherwise persists `agent_id` and
    /// returns it. The first writer wins, which makes parallel first-call
    /// overlays race-safe: whichever request lands the write first owns the
    /// binding for the life of the tasklist.
    pub async fn bind_copilot_agent_id(
        &self,
        team_id: &str,
        tasklist_id: &str,
        agent_id: &str,
    ) -> Result<AgentId, AoError> {
        let mut tasklist = self
            .get(team_id, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.to_string()))?;

        if let Some(existing) = tasklist.copilot_agent_id.clone() {
            return Ok(existing);
        }

        tasklist.copilot_agent_id = Some(agent_id.to_string());
        self.write_meta_atomic(&tasklist).await?;
        Ok(agent_id.to_string())
    }

    /// Apply an arbitrary mutation to an existing tasklist, then persist atomically.
    /// Returns the new state. Useful for the feeder to bump attempt counts and
    /// append to error logs in one shot.
    pub async fn mutate<F>(
        &self,
        team_id: &str,
        tasklist_id: &str,
        f: F,
    ) -> Result<Tasklist, AoError>
    where
        F: FnOnce(&mut Tasklist) -> Result<(), AoError>,
    {
        let lock = self.write_lock_for(self.team_lock_key(team_id, tasklist_id));
        let _guard = lock.lock().await;

        let mut tasklist = self
            .get(team_id, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.to_string()))?;
        f(&mut tasklist)?;
        self.write_meta_atomic(&tasklist).await?;
        Ok(tasklist)
    }

    // --- Owner-aware helper methods ---

    /// Load a tasklist by owner — routes to the team or agent store based on
    /// the owner variant.
    pub async fn get_by_owner(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
    ) -> Result<Option<Tasklist>, AoError> {
        match owner {
            TasklistOwner::Team { team_id } => self.get(team_id, tasklist_id).await,
            TasklistOwner::Agent { agent_id } => self.get_for_agent(agent_id, tasklist_id).await,
        }
    }

    /// Transition a tasklist's status by owner — routes to team or agent store.
    pub async fn set_status_by_owner(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        next: TasklistStatus,
    ) -> Result<Tasklist, AoError> {
        match owner {
            TasklistOwner::Team { team_id } => {
                self.set_status(team_id, tasklist_id, next).await
            }
            TasklistOwner::Agent { agent_id } => {
                let lock = self.write_lock_for(self.agent_lock_key(agent_id, tasklist_id));
                let _guard = lock.lock().await;

                let mut tasklist = self
                    .get_for_agent(agent_id, tasklist_id)
                    .await?
                    .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.to_string()))?;
                if tasklist.status == next {
                    return Ok(tasklist);
                }
                let allowed = matches!(
                    (tasklist.status, next),
                    (TasklistStatus::Active, TasklistStatus::Completed)
                        | (TasklistStatus::Active, TasklistStatus::Failed)
                        | (TasklistStatus::Active, TasklistStatus::Cancelled)
                        | (TasklistStatus::Active, TasklistStatus::Paused)
                        | (TasklistStatus::Paused, TasklistStatus::Active)
                        | (TasklistStatus::Paused, TasklistStatus::Cancelled)
                        | (TasklistStatus::Paused, TasklistStatus::Failed)
                        | (TasklistStatus::Failed, TasklistStatus::Active)
                        | (TasklistStatus::Failed, TasklistStatus::Cancelled)
                        | (TasklistStatus::Completed, TasklistStatus::Paused)
                        | (TasklistStatus::Failed, TasklistStatus::Paused)
                        | (TasklistStatus::Cancelled, TasklistStatus::Paused)
                );
                if !allowed {
                    return Err(AoError::InvalidTasklistTransition(format!(
                        "{:?} -> {:?}",
                        tasklist.status, next
                    )));
                }
                if tasklist.status == TasklistStatus::Active {
                    tasklist.last_active_at = Some(Utc::now());
                }
                tasklist.status = next;
                self.write_meta_atomic(&tasklist).await?;
                Ok(tasklist)
            }
        }
    }

    /// Transition a single task's status by owner.
    pub async fn set_task_status_by_owner(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        task_id: &str,
        next: TaskStatus,
    ) -> Result<Tasklist, AoError> {
        match owner {
            TasklistOwner::Team { team_id } => {
                self.set_task_status(team_id, tasklist_id, task_id, next).await
            }
            TasklistOwner::Agent { agent_id } => {
                self.mutate_for_agent(agent_id, tasklist_id, |tl| {
                    let task = find_task_mut(tl, task_id)
                        .ok_or_else(|| AoError::TaskNotFound(task_id.to_string()))?;
                    if !is_valid_task_transition(task.status, next) {
                        return Err(AoError::InvalidTasklistTransition(format!(
                            "task {task_id}: {:?} -> {:?}",
                            task.status, next
                        )));
                    }
                    task.status = next;
                    Ok(())
                })
                .await
            }
        }
    }

    /// Atomically claim a task for dispatch: transition it to `InProgress`
    /// **only if** it is currently dispatchable (`Pending` or `Blocked`).
    ///
    /// Returns `Ok(true)` when this call performed the transition, and
    /// `Ok(false)` when the task was already `InProgress` (another dispatcher
    /// won the claim) or in a terminal state (it already finished). The
    /// check-and-set runs under the per-tasklist write lock, so two concurrent
    /// dispatch attempts for the same task can never both observe a
    /// dispatchable status — exactly one wins.
    ///
    /// This is the guard against re-dispatching a task that a stale in-memory
    /// snapshot still believes is `Pending`. Using a status-conditional claim
    /// (rather than an unconditional `Pending -> InProgress`) also closes the
    /// `InProgress -> InProgress` self-transition that the generic transition
    /// validator permits, which would otherwise let a duplicate dispatch slip
    /// through while the first run is still in flight.
    pub async fn try_begin_task_by_owner(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        task_id: &str,
    ) -> Result<bool, AoError> {
        let lock_key = match owner {
            TasklistOwner::Team { team_id } => self.team_lock_key(team_id, tasklist_id),
            TasklistOwner::Agent { agent_id } => self.agent_lock_key(agent_id, tasklist_id),
        };
        let lock = self.write_lock_for(lock_key);
        let _guard = lock.lock().await;

        let mut tasklist = match owner {
            TasklistOwner::Team { team_id } => self.get(team_id, tasklist_id).await?,
            TasklistOwner::Agent { agent_id } => {
                self.get_for_agent(agent_id, tasklist_id).await?
            }
        }
        .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.to_string()))?;

        let task = find_task_mut(&mut tasklist, task_id)
            .ok_or_else(|| AoError::TaskNotFound(task_id.to_string()))?;
        if !matches!(task.status, TaskStatus::Pending | TaskStatus::Blocked) {
            return Ok(false);
        }
        task.status = TaskStatus::InProgress;
        self.write_meta_atomic(&tasklist).await?;
        Ok(true)
    }

    /// Atomically reclaim an already-`InProgress` task for a reprompt or
    /// recovery dispatch (watchdog recovery, stale-run reprompt, output
    /// validation reprompt). Unlike [`Self::try_begin_task_by_owner`], the
    /// task's status does not change on the happy path — it stays
    /// `InProgress` throughout — so there is no status transition to
    /// serialize concurrent reclaimers on. `dispatch_token` fills that role
    /// instead: the caller passes the value it read *before* deciding the
    /// task needed recovery; this call re-reads the task fresh under the
    /// per-tasklist write lock and only proceeds if `expected_token` still
    /// matches the live value. A mismatch means a concurrent reclaimer
    /// already won this exact recovery race (and has already bumped the
    /// token), so this call backs off instead of dispatching a second time.
    ///
    /// The `attempt_count` bump and the `>= max_attempts -> Failed`
    /// evaluation both run against the value read INSIDE this same locked
    /// section — never a caller's pre-lock snapshot — so a task can never
    /// exceed `max_attempts` without transitioning to `Failed`, even when
    /// two callers reclaim it concurrently. `error_msg` is invoked with the
    /// fresh, post-bump `attempt_count` so callers can render "Attempt N: "
    /// text that matches what is actually persisted.
    pub async fn try_reclaim_dispatch_by_owner<F>(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        task_id: &str,
        expected_token: u64,
        max_attempts: u32,
        error_msg: F,
    ) -> Result<ReclaimDispatchOutcome, AoError>
    where
        F: FnOnce(u32) -> String,
    {
        let lock_key = match owner {
            TasklistOwner::Team { team_id } => self.team_lock_key(team_id, tasklist_id),
            TasklistOwner::Agent { agent_id } => self.agent_lock_key(agent_id, tasklist_id),
        };
        let lock = self.write_lock_for(lock_key);
        let _guard = lock.lock().await;

        let mut tasklist = match owner {
            TasklistOwner::Team { team_id } => self.get(team_id, tasklist_id).await?,
            TasklistOwner::Agent { agent_id } => {
                self.get_for_agent(agent_id, tasklist_id).await?
            }
        }
        .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.to_string()))?;

        let task = find_task_mut(&mut tasklist, task_id)
            .ok_or_else(|| AoError::TaskNotFound(task_id.to_string()))?;

        if task.status != TaskStatus::InProgress {
            return Ok(ReclaimDispatchOutcome::NotInProgress {
                observed: task.status,
            });
        }
        if task.dispatch_token != expected_token {
            return Ok(ReclaimDispatchOutcome::Stale);
        }

        let new_count = task.attempt_count.saturating_add(1);
        task.attempt_count = new_count;
        task.error_log.push(error_msg(new_count));
        task.dispatch_token = task.dispatch_token.wrapping_add(1);
        let dispatch_token = task.dispatch_token;

        if new_count >= max_attempts {
            task.status = TaskStatus::Failed;
            self.write_meta_atomic(&tasklist).await?;
            return Ok(ReclaimDispatchOutcome::Exhausted {
                attempt_count: new_count,
            });
        }

        let snapshot = task.clone();
        self.write_meta_atomic(&tasklist).await?;
        Ok(ReclaimDispatchOutcome::Claimed {
            attempt_count: new_count,
            dispatch_token,
            task: snapshot,
        })
    }

    /// Apply an arbitrary mutation to an existing tasklist by owner.
    pub async fn mutate_by_owner<F>(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        f: F,
    ) -> Result<Tasklist, AoError>
    where
        F: FnOnce(&mut Tasklist) -> Result<(), AoError>,
    {
        match owner {
            TasklistOwner::Team { team_id } => self.mutate(team_id, tasklist_id, f).await,
            TasklistOwner::Agent { agent_id } => {
                self.mutate_for_agent(agent_id, tasklist_id, f).await
            }
        }
    }

    // --- Agent-scope methods ---

    /// Create a new agent-owned tasklist on disk. Returns `TasklistAlreadyActive`
    /// if the agent already has an active tasklist.
    pub async fn create_for_agent(&self, tasklist: &Tasklist) -> Result<(), AoError> {
        let agent_id = match &tasklist.owner {
            TasklistOwner::Agent { agent_id } => agent_id.clone(),
            _ => {
                return Err(AoError::ValidationError(
                    "create_for_agent requires an agent-owned tasklist".to_string(),
                ))
            }
        };

        Self::validate_id("Agent ID", &agent_id)?;
        Self::validate_id("Tasklist ID", &tasklist.id)?;

        if let Some(existing) = self.active_for_agent(&agent_id).await? {
            return Err(AoError::TasklistAlreadyActive {
                team_id: agent_id.clone(),
                tasklist_id: existing.id,
            });
        }

        let dir = self.data_root.agent_tasklist_dir(&agent_id, &tasklist.id);
        if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            return Err(AoError::ValidationError(format!(
                "Tasklist directory already exists: {}",
                dir.display()
            )));
        }

        tokio::fs::create_dir_all(
            self.data_root
                .agent_tasklist_workspace_dir(&agent_id, &tasklist.id),
        )
        .await?;
        tokio::fs::create_dir_all(
            self.data_root
                .agent_tasklist_transcripts_dir(&agent_id, &tasklist.id),
        )
        .await?;

        self.write_meta_atomic(tasklist).await?;
        Ok(())
    }

    /// Load a single agent-owned tasklist by id.
    pub async fn get_for_agent(
        &self,
        agent_id: &str,
        tasklist_id: &str,
    ) -> Result<Option<Tasklist>, AoError> {
        let path = self
            .data_root
            .agent_tasklist_meta_path(agent_id, tasklist_id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(None);
        }
        let contents = tokio::fs::read_to_string(&path).await?;
        let tasklist: Tasklist =
            serde_json::from_str(&contents).map_err(|e| AoError::Json(e.to_string()))?;
        Ok(Some(tasklist))
    }

    /// List every tasklist for an agent (any status), sorted by creation time descending.
    pub async fn list_for_agent(&self, agent_id: &str) -> Result<Vec<Tasklist>, AoError> {
        let dir = self.data_root.agent_tasklists_dir(agent_id);
        if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let mut entries = tokio::fs::read_dir(&dir).await?;
        let mut out = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let meta_path = entry.path().join("tasklist.json");
            if !tokio::fs::try_exists(&meta_path).await.unwrap_or(false) {
                continue;
            }
            let contents = tokio::fs::read_to_string(&meta_path).await?;
            match serde_json::from_str::<Tasklist>(&contents) {
                Ok(tl) => out.push(tl),
                Err(e) => {
                    tracing::warn!("Failed to parse agent tasklist {:?}: {}", meta_path, e);
                }
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    /// Apply an arbitrary mutation to an agent-owned tasklist, then persist atomically.
    /// Uses the same last-write-wins file pattern as `mutate`.
    pub async fn mutate_for_agent<F>(
        &self,
        agent_id: &str,
        tasklist_id: &str,
        f: F,
    ) -> Result<Tasklist, AoError>
    where
        F: FnOnce(&mut Tasklist) -> Result<(), AoError>,
    {
        let lock = self.write_lock_for(self.agent_lock_key(agent_id, tasklist_id));
        let _guard = lock.lock().await;

        let mut tasklist = self
            .get_for_agent(agent_id, tasklist_id)
            .await?
            .ok_or_else(|| AoError::TasklistNotFound(tasklist_id.to_string()))?;
        f(&mut tasklist)?;
        self.write_meta_atomic(&tasklist).await?;
        Ok(tasklist)
    }

    /// Return the agent's currently-active (non-terminal) tasklist, or `None`.
    pub async fn active_for_agent(
        &self,
        agent_id: &str,
    ) -> Result<Option<Tasklist>, AoError> {
        Ok(self
            .list_for_agent(agent_id)
            .await?
            .into_iter()
            .find(|t| {
                matches!(t.status, TasklistStatus::Active | TasklistStatus::Paused)
            }))
    }
}

fn find_task_mut<'a>(tasklist: &'a mut Tasklist, task_id: &str) -> Option<&'a mut Task> {
    for group in &mut tasklist.groups {
        if let Some(t) = group.tasks.iter_mut().find(|t| t.id == task_id) {
            return Some(t);
        }
    }
    None
}

fn is_valid_task_transition(prev: TaskStatus, next: TaskStatus) -> bool {
    if prev == next {
        return true;
    }
    use TaskStatus::*;
    matches!(
        (prev, next),
        (Pending, InProgress)
            | (Pending, Blocked)
            | (Pending, Failed)
            | (Pending, Skipped)
            | (InProgress, Completed)
            | (InProgress, Failed)
            | (InProgress, Blocked)
            | (InProgress, Skipped)
            // TodoStopTask: halt a single in-flight task.
            | (InProgress, Stopped)
            | (Blocked, Pending)
            | (Blocked, InProgress)
            | (Blocked, Failed)
            | (Blocked, Skipped)
            // Continue (failed → pending) and Skip-failed (failed → skipped)
            // recovery paths. Driven only by user-facing recovery actions; the
            // feeder never auto-rewinds a Failed task.
            | (Failed, Pending)
            | (Failed, Skipped)
            // TodoResumeTask: return a stopped task to the dispatch queue.
            | (Stopped, Pending)
            // Runner completion/failure after a stop — the runner may not see
            // the stop before finishing its work; let the natural outcome land.
            | (Stopped, Completed)
            | (Stopped, Failed)
    )
}
