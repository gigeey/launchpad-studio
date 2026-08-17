use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::RwLock;
use tracing;
use uuid::Uuid;

use ao_engine_tools_core::terminal_report::{
    TerminalCounts, TerminalReport, TerminalTaskEntry, TerminalWatcherGuard,
    TerminalWatcherRegistry,
};
use ao_persistence::changelog::ChangelogStore;
use ao_persistence::paths::DataRoot;
use ao_persistence::progress_log::{append_progress_block, ProgressBlock};
use ao_persistence::task_meta::{read_task_meta, write_task_meta, TaskMeta};
use ao_persistence::tasklist_store::{ReclaimDispatchOutcome, TasklistStore};
use ao_persistence::transcript::TranscriptStore;
use ao_protocol::agent::AgentId;
use ao_protocol::changelog::ChangelogEntry;
use ao_protocol::error::AoError;
use ao_protocol::event::{AgentEventPayload, TodoListCompleteTask, TodoListTerminalCounts};
use ao_protocol::message::QueuedMessage;
use ao_protocol::tasklist::{
    Task, TaskComment, TaskCommentAuthorKind, TaskGroup, TaskGroupMode, TaskId, TaskStatus,
    Tasklist, TasklistId, TasklistOwner, TasklistStatus,
};
use ao_protocol::team::TeamId;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

use crate::agent_routing::{AgentRoutingChannel, AgentRoutingRequest};
use crate::event_bus::EventBus;
use crate::instance_registry::InstanceRegistry;
use crate::queue_manager::NotificationDispatcher;

// The `impl TaskFeeder` block below is continued across these sibling modules.
// They are private and contain nothing but `impl TaskFeeder` blocks, so this
// split changes no public API: inherent methods resolve through the type, not
// through the module path. Shared helpers, constants and `TaskLiveness` stay in
// this file because they are used from more than one of them.
mod dispatch;
mod events;
mod lifecycle;
mod terminal;
mod watchdog;

/// Abstract project-channel dispatch surface. When an agent-owned tasklist is
/// tagged with a `project_id`, completion summaries are routed here so the
/// project's main agent receives the message on the project channel — ensuring
/// responses stream to the project page and the project orchestration system
/// prompt is injected.
///
/// Production: [`crate::project_queue_manager::ProjectQueueManagerRegistry`].
/// Tests substitute a recording mock.
#[async_trait]
pub trait ProjectDispatcher: Send + Sync {
    async fn submit_to_project(
        &self,
        project_id: &str,
        message: QueuedMessage,
    ) -> Result<(), AoError>;
}

/// Abstract dispatch surface for the TaskFeeder.
///
/// Production: [`crate::tasklist_queue_manager::TasklistQueueDispatcher`]
/// resolves the tasklist's `workspace_dir` and submits a
/// [`crate::tasklist_queue_manager::TasklistMessage::Dispatch`] to the
/// per-tasklist [`crate::tasklist_queue_manager::TasklistQueueManager`].
/// Tests substitute an in-memory recorder.
#[async_trait]
pub trait TaskDispatcher: Send + Sync {
    /// Dispatch the prompt for `task_id` (owned by `owner_agent_id`) for the
    /// given tasklist. Implementations are fire-and-forget — the feeder does
    /// not block on the resulting agent run; downstream stories observe
    /// completion via `<task action="complete">` tags or run lifecycle events.
    async fn dispatch_task(
        &self,
        owner_agent_id: &AgentId,
        prompt: String,
        owner: &TasklistOwner,
        tasklist_id: &TasklistId,
        task_id: &TaskId,
    ) -> Result<(), AoError>;
}

/// Default cap on how many times a single task may be (re)dispatched before
/// transitioning to Failed. The output-validation reprompt loop and the
/// stale-run reprompt loop share this counter.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Default grace period before the watchdog considers an InProgress task with
/// zero active runs "stuck". Buffers the gap between `dispatch_one` marking the
/// task InProgress and the agent runner actually spawning (and registering) the
/// run.
///
/// This window must comfortably exceed real run-startup latency, because the
/// run-registration key is per-(tasklist, agent) rather than per-task: while a
/// task's run is still cold-starting or queued behind the same executor agent's
/// previous task, `running_count` reads zero even though the task is healthy
/// and about to produce work. Reaping during that window is a false positive —
/// it transitions a task to a terminal state while its run is mid-flight, and
/// the run can then never report completion (terminal states are final), which
/// stalls the whole SEQ group.
///
/// Observed startup latencies on a busy single-executor SEQ tasklist reached
/// ~130s, so 60s was far too tight. The watchdog is only a backstop: runs that
/// genuinely end without reporting are caught immediately and event-driven by
/// `on_run_ended`, so a generous grace here costs only delayed recovery of the
/// rare silently-dropped run, not correctness.
pub const DEFAULT_WATCHDOG_GRACE: Duration = Duration::from_secs(300);

/// Appended to the async completion message so the owning agent treats the
/// message as the result of a tasklist it launched and knows its options for
/// following up. Deliberately terse — the per-item summaries above carry the
/// substance; this just frames what to do with them.
const TASKLIST_COMPLETION_GUIDANCE: &str = "\
This message is the result of a tasklist you or the user launched. Decide how to follow up before responding to anyone:\n\
- If every item succeeded and the goal is met, synthesize the item summaries above into one coherent result for the user rather than relaying them verbatim.\n\
- For each failed item, read its output file (path shown above) to learn what went wrong, then either retry it by re-adding the item or surface the blocker to the user. Never silently drop a failure.\n\
- For each skipped item, decide whether it still needs doing.\n\
- Open an item's full output file only when its one-line summary isn't enough to act on.";

/// Appended to project-scoped tasklist completion messages. Directs the project
/// agent to validate deliverables against the declared goal and close the loop
/// by creating follow-up tasklists for any remaining gaps.
const PROJECT_COMPLETION_GUIDANCE: &str = "\
You are the main agent for this project. Now that this tasklist has finished:\n\
1. Compare the deliverables above against the project goal.\n\
2. If the goal is fully met, call ProjectComplete to mark the project done and summarise the outcome for the user.\n\
3. If gaps remain, create a new tasklist via TodoCreate that addresses only those gaps — keep each item focused and avoid re-doing work already completed.\n\
4. If any item failed, diagnose it (read its output file) before deciding whether to retry or surface the blocker.";

/// Walks tasklist groups in order, dispatching tasks via a [`TaskDispatcher`].
///
/// PAR groups dispatch one task per distinct owner agent at a time (the
/// `registry` enforces a single in-flight slot per agent — multiple PAR tasks
/// owned by the same agent serialize via `on_task_terminal`). SEQ groups
/// dispatch one at a time. Group N+1 starts only when every task in group N is
/// in a terminal state (Completed | Failed). Coordinator self-assigned tasks
/// dispatch identically to member tasks — there is no special-case path.
pub struct TaskFeeder {
    tasklist_store: Arc<TasklistStore>,
    dispatcher: Arc<dyn TaskDispatcher>,
    registry: Arc<RwLock<HashMap<TasklistId, HashMap<AgentId, TaskId>>>>,
    /// Per-(tasklist, task) wall-clock dispatch timestamp, populated by
    /// `dispatch_one` and cleared by `on_task_terminal`. The watchdog uses this
    /// to apply a grace period before declaring an InProgress task stuck.
    dispatched_at: Arc<RwLock<HashMap<(TasklistId, TaskId), Instant>>>,
    /// Per-(tasklist, task) "a run actually started" flag. Set via
    /// `mark_run_observed` the moment a task's agent run registers in the
    /// [`InstanceRegistry`]; cleared when the task reaches a terminal state or
    /// is re-dispatched for recovery. Lets the watchdog tell two zero-run cases
    /// apart: a run that registered then vanished (genuine drop — recover
    /// immediately) versus one that has simply not started yet (cold start —
    /// keep honouring the dispatch grace window). Keyed by task, not by the
    /// coarse `tasklist:{id}:{agent}` registry key, so a lingering sibling run
    /// can never falsely mark a still-starting task as observed.
    run_observed: Arc<RwLock<HashSet<(TasklistId, TaskId)>>>,
    max_attempts: u32,
    event_bus: Option<Arc<EventBus>>,
    instance_registry: Option<Arc<InstanceRegistry>>,
    watchdog_grace: Duration,
    /// Late-bound agent routing channel for agent-owned tasklists. Dispatches
    /// to the per-agent classifier that consults `delegates_to`. Team-owned
    /// tasklists have no equivalent channel in this build — see
    /// `submit_routing_for` and `note_team_routing_unsupported`.
    agent_routing_queue: OnceLock<Arc<dyn AgentRoutingChannel>>,
    /// Tasklist IDs for which `note_team_routing_unsupported` has already
    /// logged its `warn`, so repeated `dispatch_group` ticks against the same
    /// stuck team-owned tasklist don't spam the log once per poll.
    team_routing_unsupported_warned: RwLock<HashSet<TasklistId>>,
    /// Late-bound dispatcher used to post a completion-summary message to the
    /// owning agent's queue when the last item of an agent-owned tasklist
    /// reaches a terminal state. Without this wiring the summary is
    /// silently skipped — existing test fixtures that don't wire it stay green.
    notification_dispatcher: OnceLock<Arc<dyn NotificationDispatcher>>,
    /// Late-bound project-channel dispatcher. When set and a completing tasklist
    /// carries a `project_id`, the completion summary is submitted here (routes
    /// through the project queue manager so the run streams to the project page)
    /// instead of the personal agent queue. Without this wiring, project-tagged
    /// tasklists fall back to the personal queue — existing tests stay green.
    project_dispatcher: OnceLock<Arc<dyn ProjectDispatcher>>,
    /// Late-bound shared thread store, used to resolve a tasklist's
    /// `thread_id` to its `Thread` record (transcript path, kind) when
    /// persisting the `todo_list.complete` completion marker. Sharing the
    /// long-lived, already-cached instance (rather than constructing a fresh
    /// `ThreadStore::load(...)` per lookup) avoids re-reading `threads.json`
    /// off disk on every tasklist completion. Without this wiring, completion
    /// markers fall back to the legacy agent/project-keyed transcript path —
    /// existing tests that don't exercise thread scoping stay green.
    threads: OnceLock<Arc<ao_persistence::thread_store::ThreadStore>>,
    /// Oneshot senders waiting for a tasklist to reach a terminal state.
    /// Populated by `register_terminal_watcher`; fired by `fire_terminal_watcher`.
    /// Uses std::sync::Mutex so it can be locked from both async and Drop contexts.
    terminal_watchers: TerminalWatcherRegistry,
}

/// Liveness verdict for an `InProgress` tasklist task during a watchdog or
/// reconcile sweep. Computed by [`TaskFeeder::task_liveness`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TaskLiveness {
    /// A run is currently registered for this task — leave it alone.
    Live,
    /// No run registered yet, the task was dispatched within the grace window,
    /// and its run has never been observed registering — still cold-starting.
    Starting,
    /// No run registered, and either the task's run was previously observed
    /// alive (registered then vanished) or the grace window has elapsed —
    /// recover it.
    Stuck,
}

impl TaskFeeder {
    pub fn new(tasklist_store: Arc<TasklistStore>, dispatcher: Arc<dyn TaskDispatcher>) -> Self {
        Self {
            tasklist_store,
            dispatcher,
            registry: Arc::new(RwLock::new(HashMap::new())),
            dispatched_at: Arc::new(RwLock::new(HashMap::new())),
            run_observed: Arc::new(RwLock::new(HashSet::new())),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            event_bus: None,
            instance_registry: None,
            watchdog_grace: DEFAULT_WATCHDOG_GRACE,
            agent_routing_queue: OnceLock::new(),
            team_routing_unsupported_warned: RwLock::new(HashSet::new()),
            notification_dispatcher: OnceLock::new(),
            project_dispatcher: OnceLock::new(),
            threads: OnceLock::new(),
            terminal_watchers: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Late-bind the agent routing channel so `dispatch_group` can submit
    /// unowned tasks in agent-owned tasklists for per-agent classification.
    /// Idempotent. Without this wiring, agent-owned unowned tasks remain
    /// Pending until manually assigned.
    pub fn set_agent_routing_queue(&self, channel: Arc<dyn AgentRoutingChannel>) {
        let _ = self.agent_routing_queue.set(channel);
    }

    /// Late-bind the notification dispatcher used to post a completion-summary
    /// message to the owning agent when the last item of an agent-owned
    /// tasklist reaches a terminal state. Idempotent.
    pub fn set_notification_dispatcher(&self, dispatcher: Arc<dyn NotificationDispatcher>) {
        let _ = self.notification_dispatcher.set(dispatcher);
    }

    /// Late-bind the project-channel dispatcher so completion summaries for
    /// project-tagged agent-owned tasklists are routed via the project queue
    /// manager. Idempotent.
    pub fn set_project_dispatcher(&self, dispatcher: Arc<dyn ProjectDispatcher>) {
        let _ = self.project_dispatcher.set(dispatcher);
    }

    /// Late-bind the shared thread store used to resolve a completing
    /// tasklist's `thread_id` to its `Thread` record when persisting the
    /// `todo_list.complete` transcript marker. Idempotent.
    pub fn set_thread_store(&self, threads: Arc<ao_persistence::thread_store::ThreadStore>) {
        let _ = self.threads.set(threads);
    }

    /// Return a clone of the terminal watcher registry so `TasklistService`
    /// can share the same registry for registration and cancellation paths.
    pub fn terminal_watchers(&self) -> TerminalWatcherRegistry {
        Arc::clone(&self.terminal_watchers)
    }

    /// Register a one-shot watcher for `tasklist_id`. Returns a guard whose
    /// `wait()` method resolves when the tasklist reaches a terminal state.
    /// Call this *before* starting the tasklist to avoid the race where the
    /// tasklist completes before the await is established.
    pub fn register_terminal_watcher(&self, tasklist_id: &str) -> TerminalWatcherGuard {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.terminal_watchers
            .lock()
            .expect("terminal_watchers mutex poisoned")
            .insert(tasklist_id.to_owned(), tx);
        TerminalWatcherGuard::new(
            rx,
            Arc::clone(&self.terminal_watchers),
            tasklist_id.to_owned(),
        )
    }

    /// Fire the watcher for `tasklist_id` if one is registered. Checks
    /// `is_closed()` before sending and removes the entry either way.
    /// Silently skips if no watcher is registered or the receiver was dropped.
    /// Returns `true` if a live watcher was found and the report was sent —
    /// callers use this to suppress `post_completion_summary` for sync runs.
    ///
    /// Async because building the report reads the tasklist changelog to
    /// attach each task's notification summary. The registry lock is released
    /// (the sender is moved out) before any `.await`, so no guard is held
    /// across the suspension point.
    pub async fn fire_terminal_watcher(&self, tasklist: &Tasklist) -> bool {
        let tasklist_id = &tasklist.id;
        let maybe_tx = self
            .terminal_watchers
            .lock()
            .expect("terminal_watchers mutex poisoned")
            .remove(tasklist_id);
        if let Some(tx) = maybe_tx {
            if !tx.is_closed() {
                let summaries = self.load_task_summaries(tasklist).await;
                let report =
                    build_terminal_report(tasklist, &self.tasklist_store.data_root(), &summaries);
                let _ = tx.send(report);
                return true;
            }
        }
        false
    }

    /// Load the tasklist's changelog and index the most recent entry per task.
    ///
    /// Each parsed `<task-item-notification>` a subagent emits is appended to
    /// the changelog (see `cli::record_task_item_changelog`). A task
    /// can have several entries across retries; we keep the last one because
    /// `read_recent` returns entries oldest-first and later inserts win. The
    /// changelog is keyed by `(owner, tasklist_id)`, resolving to the
    /// tasklist's own workspace under either ownership tree. Best-effort: a
    /// read failure logs and yields an empty map so the report still ships.
    async fn load_task_summaries(&self, tasklist: &Tasklist) -> HashMap<TaskId, ChangelogEntry> {
        let store = ChangelogStore::new(self.tasklist_store.data_root().clone());
        match store
            .read_recent(&tasklist.owner, &tasklist.id, usize::MAX)
            .await
        {
            Ok(entries) => {
                let mut map = HashMap::with_capacity(entries.len());
                for entry in entries {
                    map.insert(entry.task_id.clone(), entry);
                }
                map
            }
            Err(e) => {
                tracing::warn!(
                    tasklist_id = %tasklist.id,
                    "load_task_summaries: failed to read changelog: {}",
                    e
                );
                HashMap::new()
            }
        }
    }

    /// Walk every team's Active tasklists and call [`Self::advance`] on each.
    /// Recovers tasklists that were left in an Active-but-not-dispatching state
    /// across a server restart — in particular, tasklists with seeded unowned
    /// tasks whose initial routing dispatch never fired (see
    /// `state.rs::AppState::new`). Best-effort: per-tasklist failures are
    /// logged and skipped so a single broken tasklist can't block startup.
    pub async fn advance_all_active(&self) -> Result<(), AoError> {
        let mut active = self.tasklist_store.list_active_across_teams().await?;
        let agent_active = self.tasklist_store.list_active_across_agents().await?;
        active.extend(agent_active);
        if active.is_empty() {
            return Ok(());
        }
        tracing::info!(count = active.len(), "TaskFeeder: startup advance scan");
        for tasklist in &active {
            if let Err(e) = self.advance(tasklist).await {
                tracing::warn!(
                    team_id = %tasklist.team_id.as_deref().unwrap_or_default(),
                    tasklist_id = %tasklist.id,
                    "TaskFeeder: startup advance failed: {}",
                    e
                );
            }
        }
        Ok(())
    }

    /// On engine restart, any task left `InProgress` has no live runner —
    /// `dispatched_at` is empty and `instance_registry` has no entries.
    /// This method runs a watchdog tick immediately (zero grace period) to
    /// recover those orphaned tasks before the regular 30-second watchdog fires.
    /// Covers both team-owned and agent-owned tasklists.
    pub async fn reconcile_zombies_on_start(&self) -> Result<usize, AoError> {
        let Some(instance_registry) = self.instance_registry.as_ref() else {
            return Ok(0);
        };
        let mut active = self.tasklist_store.list_active_across_teams().await?;
        let agent_active = self.tasklist_store.list_active_across_agents().await?;
        active.extend(agent_active);
        if active.is_empty() {
            return Ok(0);
        }
        let mut recovered = 0usize;
        for tasklist in &active {
            let candidates: Vec<(AgentId, TaskId)> = tasklist
                .groups
                .iter()
                .flat_map(|g| g.tasks.iter())
                .filter(|t| t.status == TaskStatus::InProgress)
                .map(|t| (resolve_executor_agent_id(&tasklist.owner, t), t.id.clone()))
                .collect();
            for (agent_id, task_id) in candidates {
                let registry_key = format!("tasklist:{}:{}", tasklist.id, agent_id);
                let running = instance_registry.running_count(&registry_key).await;
                if running > 0 {
                    // A run is actually alive (e.g. reconcile called mid-flight).
                    continue;
                }
                tracing::warn!(
                    tasklist_id = %tasklist.id,
                    task_id = %task_id,
                    agent_id = %agent_id,
                    "reconcile_zombies_on_start: InProgress task has no live runner; recovering",
                );
                if self
                    .recover_stuck_task(&tasklist.owner, &tasklist.id, &agent_id, &task_id)
                    .await?
                {
                    recovered += 1;
                }
            }
        }
        if recovered > 0 {
            tracing::info!(recovered, "reconcile_zombies_on_start: recovered orphaned tasks");
        }
        Ok(recovered)
    }

    /// Override the validation/reprompt attempt cap. Default is
    /// [`DEFAULT_MAX_ATTEMPTS`]; tests use a smaller value to keep loops short.
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// Attach an [`EventBus`] so task and tasklist transitions emit SSE-bound
    /// events (`tasklist.task_updated`, `tasklist.completed`, `tasklist.failed`).
    /// Optional so unit tests that don't need event observation can skip wiring.
    pub fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    /// Attach the [`InstanceRegistry`] so the watchdog can detect tasks that
    /// are still `InProgress` despite the owning agent having no active run.
    /// Without this wiring `watchdog_tick` is a no-op.
    pub fn with_instance_registry(mut self, registry: Arc<InstanceRegistry>) -> Self {
        self.instance_registry = Some(registry);
        self
    }

    /// Override the watchdog grace period. Default is
    /// [`DEFAULT_WATCHDOG_GRACE`]; tests use a much shorter value.
    pub fn with_watchdog_grace(mut self, grace: Duration) -> Self {
        self.watchdog_grace = grace;
        self
    }

    /// Return the dispatch timestamp recorded for `(tasklist_id, task_id)`, or
    /// `None` if no timestamp exists (e.g. the task was never dispatched in
    /// this process lifetime or the in-memory map was cleared after a restart).
    pub async fn dispatch_timestamp_for(
        &self,
        tasklist_id: &TasklistId,
        task_id: &TaskId,
    ) -> Option<Instant> {
        let times = self.dispatched_at.read().await;
        times.get(&(tasklist_id.clone(), task_id.clone())).copied()
    }

}

fn group_is_terminal(group: &TaskGroup) -> bool {
    group.tasks.is_empty() || group.tasks.iter().all(|t| t.status.is_terminal())
}

/// Resolve the agent that actually *executes* a task — i.e. the id its run
/// registers under in the [`InstanceRegistry`] (`tasklist:{id}:{executor}`).
///
/// For agent-owned tasklists a classifier-assigned task carries an empty
/// `owner_agent_id` and stores the chosen executor in `assignment.owner_agent_id`
/// instead. Reading `owner_agent_id` alone therefore yields the wrong (often
/// empty) key, so any liveness probe keyed off it can never match the run the
/// agent runner registered — the probe reads zero live runs and a healthy,
/// still-working task looks "stuck".
///
/// This mirrors the executor resolution in `dispatch_one` and `on_task_terminal`
/// exactly, so the watchdog/reconcile sweeps query the same key the dispatch
/// path used to spawn (and the runner used to register) the run.
fn resolve_executor_agent_id(owner: &TasklistOwner, task: &Task) -> AgentId {
    match owner {
        TasklistOwner::Agent {
            agent_id: parent_id,
        } => task
            .assignment
            .as_ref()
            .map(|a| a.owner_agent_id.clone())
            .filter(|id| !id.is_empty())
            .or_else(|| Some(task.owner_agent_id.clone()).filter(|id| !id.is_empty()))
            .unwrap_or_else(|| parent_id.clone()),
        TasklistOwner::Team { .. } => task.owner_agent_id.clone(),
    }
}

/// Render the prompt sent to the executing agent. With no comments this is
/// `task.prompt` byte-for-byte (regression-preserving). With one or more
/// comments, a clearly-delimited "Additional context" block is appended in
/// chronological order, attributing each comment to its author.
fn build_dispatch_prompt(task: &Task) -> String {
    if task.comments.is_empty() {
        return task.prompt.clone();
    }
    let mut out = String::with_capacity(task.prompt.len() + 128);
    out.push_str(&task.prompt);
    out.push_str("\n\n---\nAdditional context (in chronological order):\n");
    for comment in &task.comments {
        let kind = match comment.author_kind {
            TaskCommentAuthorKind::User => "user",
            TaskCommentAuthorKind::Agent => "agent",
        };
        out.push_str(&format!(
            "- [{kind}: {author}] {body}\n",
            kind = kind,
            author = comment.author_id,
            body = comment.body,
        ));
    }
    out
}

fn tasklist_status_to_str(status: TasklistStatus) -> &'static str {
    match status {
        TasklistStatus::Active => "active",
        TasklistStatus::Paused => "paused",
        TasklistStatus::Completed => "completed",
        TasklistStatus::Cancelled => "cancelled",
        TasklistStatus::Failed => "failed",
    }
}

/// Derive the SSE event channel from a `TasklistOwner`.
/// Team-owned → `"team:{team_id}"`, Agent-owned → `"{agent_id}"`.
fn owner_event_channel(owner: &TasklistOwner) -> String {
    match owner {
        TasklistOwner::Team { team_id } => format!("team:{}", team_id),
        TasklistOwner::Agent { agent_id } => agent_id.clone(),
    }
}

/// Return the team_id string to embed in backward-compat event payloads that
/// still carry a `team_id: String` field. For Agent owners returns an empty
/// string — the payload's `owner` field is the canonical identifier.
fn owner_team_id_str(owner: &TasklistOwner) -> String {
    match owner {
        TasklistOwner::Team { team_id } => team_id.clone(),
        TasklistOwner::Agent { .. } => String::new(),
    }
}

/// Build a `TerminalReport` from a terminal tasklist snapshot.
/// `data_root` is used to resolve per-task output paths for agent-owned tasklists.
/// `summaries` maps task id → the latest changelog entry for that task, used to
/// attach each subagent's notification summary/details to its report entry.
fn build_terminal_report(
    tasklist: &Tasklist,
    data_root: &DataRoot,
    summaries: &HashMap<TaskId, ChangelogEntry>,
) -> TerminalReport {
    let status_str = match tasklist.status {
        TasklistStatus::Completed => "completed",
        TasklistStatus::Failed => "failed",
        TasklistStatus::Cancelled => "cancelled",
        _ => "unknown",
    };
    let owner_agent_id = match &tasklist.owner {
        TasklistOwner::Agent { agent_id } => Some(agent_id.clone()),
        TasklistOwner::Team { .. } => None,
    };
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut tasks: Vec<TerminalTaskEntry> = Vec::new();
    for group in &tasklist.groups {
        for t in &group.tasks {
            let task_status = match t.status {
                TaskStatus::Completed => {
                    succeeded += 1;
                    "completed"
                }
                TaskStatus::Failed => {
                    failed += 1;
                    "failed"
                }
                TaskStatus::Skipped => {
                    skipped += 1;
                    "skipped"
                }
                TaskStatus::Pending => "pending",
                TaskStatus::InProgress => "in_progress",
                TaskStatus::Blocked => "blocked",
                TaskStatus::Stopped => "stopped",
            };
            let output_path = match &owner_agent_id {
                Some(agent_id) => {
                    data_root.agent_tasklist_task_output_path(agent_id, &tasklist.id, &t.id)
                }
                None => std::path::PathBuf::new(),
            };
            let changelog_entry = summaries.get(&t.id);
            tasks.push(TerminalTaskEntry {
                id: t.id.clone(),
                title: t.prompt.lines().next().unwrap_or("").to_string(),
                status: task_status.to_string(),
                summary: changelog_entry.map(|e| e.summary.clone()),
                details: changelog_entry.and_then(|e| e.details.clone()),
                output_path,
                attempt_count: t.attempt_count,
            });
        }
    }
    TerminalReport {
        status: status_str.to_string(),
        counts: TerminalCounts {
            succeeded,
            failed,
            skipped,
        },
        tasks,
    }
}

#[cfg(test)]
mod tests;
