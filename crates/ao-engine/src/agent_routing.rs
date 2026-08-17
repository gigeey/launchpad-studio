//! Per-agent routing queue manager for agent-owned tasklists.
//!
//! When an agent-owned tasklist has an unowned Pending task,
//! `TaskFeeder::submit_routing_for` hands it off here via the
//! `AgentRoutingChannel` trait. This module then:
//!
//! - **Leaf-agent fast path**: if `agent.delegates_to` is empty, stamps
//!   `owner_agent_id = owning_agent_id` immediately — no LLM call.
//! - **Non-leaf path**: builds a classifier prompt from the delegate
//!   roster plus a synthesised self-entry, runs a one-shot LLM call,
//!   and parses `<task_owner>`. Falls back to self on parse failure.
//! - **LLM error/timeout**: leaves `owner_agent_id` empty and appends a
//!   `<task_comment>` to the task so the user can assign manually.
//!
//! The manager is keyed per owning agent (one queue per agent_id) and
//! processes requests serially within each agent's queue, parallel across
//! agents — mirroring `RoutingQueueManager` which is keyed per team.

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

use ao_normalizer::registry::NormalizerRegistry;
use ao_persistence::PersistenceLayer;
use ao_process::supervisor::{ProcessSupervisor, SpawnInput};
use ao_protocol::agent::{AgentId, AgentProfile, InputMode, ProviderConfig};
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::tasklist::{
    AssignmentMode, Task, TaskAssignment, TaskComment, TaskCommentAuthorKind, TaskGroupMode,
    TasklistOwner,
};
use ao_protocol::team::TeamId;
use ao_protocol::team::TeamMember;

use crate::agent_runner::CliAgentRunner;
use crate::task_feeder::TaskFeeder;
use crate::task_owner_extraction::extract_task_owner;

// ──────────────────────────────────────────────────────────────────────
// Public discriminator type
// ──────────────────────────────────────────────────────────────────────

/// Discriminates whether a routing roster is drawn from a Team's members
/// or an Agent's delegates_to address book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterSource {
    Team(TeamId),
    Agent(AgentId),
}

// ──────────────────────────────────────────────────────────────────────
// Request / channel types
// ──────────────────────────────────────────────────────────────────────

/// One routing request: classify ownership of `task_id` within
/// `tasklist_id` for the given owning agent.
#[derive(Debug, Clone)]
pub struct AgentRoutingRequest {
    pub agent_id: AgentId,
    pub tasklist_id: String,
    pub task_id: String,
}

/// Submit-only surface for the agent routing channel. `TaskFeeder` holds
/// this behind a trait object so the reference cycle
/// `TaskFeeder → Registry → TaskFeeder` stays Send/Sync-clean.
#[async_trait]
pub trait AgentRoutingChannel: Send + Sync {
    async fn submit_agent_routing(
        &self,
        agent_id: &AgentId,
        request: AgentRoutingRequest,
    ) -> Result<(), AoError>;
}

/// Sender-side handle for a single agent's routing queue.
#[derive(Clone)]
pub struct AgentRoutingQueueManagerHandle {
    pub request_tx: mpsc::Sender<AgentRoutingRequest>,
}

// ──────────────────────────────────────────────────────────────────────
// Per-agent queue manager
// ──────────────────────────────────────────────────────────────────────

/// Drives one routing request at a time for one owning agent.
pub struct AgentRoutingQueueManager {
    agent_id: AgentId,
    request_rx: mpsc::Receiver<AgentRoutingRequest>,
    persistence: Arc<PersistenceLayer>,
    process_supervisor: Arc<dyn ProcessSupervisor>,
    normalizer_registry: Arc<NormalizerRegistry>,
    task_feeder: Arc<TaskFeeder>,
}

impl AgentRoutingQueueManager {
    pub fn new(
        agent_id: AgentId,
        request_rx: mpsc::Receiver<AgentRoutingRequest>,
        persistence: Arc<PersistenceLayer>,
        process_supervisor: Arc<dyn ProcessSupervisor>,
        normalizer_registry: Arc<NormalizerRegistry>,
        task_feeder: Arc<TaskFeeder>,
    ) -> Self {
        Self {
            agent_id,
            request_rx,
            persistence,
            process_supervisor,
            normalizer_registry,
            task_feeder,
        }
    }

    /// Main loop: handle one request at a time.
    pub async fn run(mut self) {
        while let Some(request) = self.request_rx.recv().await {
            self.handle(request).await;
        }
        tracing::debug!(
            agent_id = %self.agent_id,
            "Agent routing queue manager shutting down (channel closed)",
        );
    }

    /// Classify ownership of one task:
    /// 1. Load agent profile + tasklist + task.
    /// 2. Idempotency: skip if already owned.
    /// 3. Leaf-agent fast path (delegates_to empty): stamp self, advance.
    /// 4. Non-leaf: classifier LLM call → stamp result; parse failure → self.
    /// 5. LLM error → persist failure comment, leave unowned.
    async fn handle(&self, request: AgentRoutingRequest) {
        let AgentRoutingRequest {
            agent_id,
            tasklist_id,
            task_id,
        } = &request;

        // ── Load agent profile ───────────────────────────────────────
        let agent = match self.persistence.agents.get(agent_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    "Agent routing aborted: owning agent profile not found",
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    "Agent routing aborted: failed to load agent profile: {}",
                    e
                );
                return;
            }
        };

        // ── Load tasklist ────────────────────────────────────────────
        let owner = TasklistOwner::Agent { agent_id: agent_id.clone() };
        let tasklist = match self
            .persistence
            .tasklists
            .get_by_owner(&owner, tasklist_id)
            .await
        {
            Ok(Some(tl)) => tl,
            Ok(None) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    tasklist_id = %tasklist_id,
                    "Agent routing aborted: tasklist not found",
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    tasklist_id = %tasklist_id,
                    "Agent routing aborted: failed to load tasklist: {}",
                    e
                );
                return;
            }
        };

        // ── Find task ────────────────────────────────────────────────
        let task = match tasklist
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == *task_id)
            .cloned()
        {
            Some(t) => t,
            None => {
                tracing::warn!(
                    agent_id = %agent_id,
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    "Agent routing aborted: task not found in tasklist",
                );
                return;
            }
        };

        // ── Idempotency guard ────────────────────────────────────────
        if task.assignment.is_some() || !task.owner_agent_id.is_empty() {
            tracing::debug!(
                agent_id = %agent_id,
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                "Agent routing skipped: task already classified",
            );
            return;
        }

        // ── Leaf-agent fast path ─────────────────────────────────────
        if agent.delegates_to.is_empty() {
            tracing::info!(
                agent_id = %agent_id,
                tasklist_id = %tasklist_id,
                task_id = %task_id,
                "Agent routing: leaf-agent fast path — stamping self, no LLM call",
            );
            let agent_id_owned = agent_id.clone();
            let task_id_owned = task_id.clone();
            let updated = match self
                .persistence
                .tasklists
                .mutate_by_owner(&owner, tasklist_id, move |tl| {
                    let task = tl
                        .groups
                        .iter_mut()
                        .flat_map(|g| g.tasks.iter_mut())
                        .find(|t| t.id == task_id_owned)
                        .ok_or_else(|| AoError::TaskNotFound(task_id_owned.clone()))?;
                    task.owner_agent_id = agent_id_owned.clone();
                    task.assignment = Some(TaskAssignment {
                        owner_agent_id: agent_id_owned,
                        mode: AssignmentMode::Classified,
                    });
                    Ok(())
                })
                .await
            {
                Ok(tl) => tl,
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        tasklist_id = %tasklist_id,
                        task_id = %task_id,
                        "Agent routing: failed to persist self-assignment: {}",
                        e
                    );
                    return;
                }
            };

            self.task_feeder
                .emit_task_updated(&owner, &tasklist_id.to_string(), &task_id.to_string())
                .await;

            if let Err(e) = self.task_feeder.advance(&updated).await {
                tracing::warn!(
                    agent_id = %agent_id,
                    "Agent routing: feeder advance after self-assignment failed: {}",
                    e
                );
            }
            return;
        }

        // ── Non-leaf: build roster ───────────────────────────────────
        // Roster = all delegates + synthesised self-entry.
        let routable_members: Vec<TeamMember> = agent
            .delegates_to
            .iter()
            .map(|d| TeamMember {
                agent_id: d.target_agent_id.clone(),
                role_description: d.purpose.clone(),
                working_dir: None,
            })
            .chain(std::iter::once(TeamMember {
                agent_id: agent_id.clone(),
                role_description: "Self (this agent handles the task directly)".to_string(),
                working_dir: None,
            }))
            .collect();

        let (system_prompt, user_prompt) =
            build_agent_routing_classifier_prompt(&tasklist, &task, &agent);

        // ── One-shot LLM call ────────────────────────────────────────
        let output = match self
            .one_shot_classify(&agent, &system_prompt, &user_prompt)
            .await
        {
            Ok(out) => out,
            Err(e) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    "Agent routing: classifier LLM call failed: {}",
                    e
                );
                let body = format!("Routing failed: LLM error — {}", e);
                self.persist_failure_comment(&owner, tasklist_id, task_id, agent_id, body)
                    .await;
                return;
            }
        };

        // ── Parse output: valid delegate → stamp; parse failure → self ──
        let chosen_id =
            if let Some(decision) = extract_task_owner(&output, &routable_members) {
                tracing::info!(
                    agent_id = %agent_id,
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    chosen = %decision.agent_id,
                    "Agent routing: classifier picked owner",
                );
                decision.agent_id
            } else {
                tracing::info!(
                    agent_id = %agent_id,
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    "Agent routing: classifier parse failure — falling back to self",
                );
                agent_id.clone()
            };

        // ── Persist owner_agent_id and assignment ────────────────────
        let chosen_for_mutation = chosen_id.clone();
        let task_id_for_mutation = task_id.clone();
        let updated = match self
            .persistence
            .tasklists
            .mutate_by_owner(&owner, tasklist_id, move |tl| {
                let task = tl
                    .groups
                    .iter_mut()
                    .flat_map(|g| g.tasks.iter_mut())
                    .find(|t| t.id == task_id_for_mutation)
                    .ok_or_else(|| AoError::TaskNotFound(task_id_for_mutation.clone()))?;
                task.owner_agent_id = chosen_for_mutation.clone();
                task.assignment = Some(TaskAssignment {
                    owner_agent_id: chosen_for_mutation,
                    mode: AssignmentMode::Classified,
                });
                Ok(())
            })
            .await
        {
            Ok(tl) => tl,
            Err(e) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    "Agent routing: failed to persist owner_agent_id: {}",
                    e
                );
                return;
            }
        };

        self.task_feeder
            .emit_task_updated(&owner, &tasklist_id.to_string(), &task_id.to_string())
            .await;

        if let Err(e) = self.task_feeder.advance(&updated).await {
            tracing::warn!(
                agent_id = %agent_id,
                "Agent routing: feeder advance after owner mutation failed: {}",
                e
            );
        }
    }

    /// Persist a failure `<task_comment>` and leave `owner_agent_id` empty
    /// when the LLM call itself errors out.
    async fn persist_failure_comment(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        task_id: &str,
        author_id: &str,
        body: String,
    ) {
        let comment = TaskComment {
            id: Uuid::new_v4().to_string(),
            author_id: author_id.to_string(),
            author_kind: TaskCommentAuthorKind::Agent,
            body,
            created_at: Utc::now(),
        };
        let task_id_owned = task_id.to_string();
        let stored = comment.clone();
        let result = self
            .persistence
            .tasklists
            .mutate_by_owner(owner, tasklist_id, move |tl| {
                let task = tl
                    .groups
                    .iter_mut()
                    .flat_map(|g| g.tasks.iter_mut())
                    .find(|t| t.id == task_id_owned)
                    .ok_or_else(|| AoError::TaskNotFound(task_id_owned.clone()))?;
                task.comments.push(stored);
                Ok(())
            })
            .await;

        match result {
            Ok(_) => {
                self.task_feeder
                    .emit_task_updated(owner, &tasklist_id.to_string(), &task_id.to_string())
                    .await;
                tracing::info!(
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    "Agent routing: persisted failure comment; task left unowned",
                );
            }
            Err(e) => {
                tracing::error!(
                    tasklist_id = %tasklist_id,
                    task_id = %task_id,
                    "Agent routing: failed to persist failure comment: {}",
                    e
                );
            }
        }
    }

    /// Issue a single direct CLI invocation against the owning agent's
    /// provider, returning the accumulated text output. No transcript writes,
    /// no instance-registry registration, no event emission.
    async fn one_shot_classify(
        &self,
        agent: &AgentProfile,
        system_prompt: &str,
        user_prompt: &str,
    ) -> Result<String, AoError> {
        let mut profile = agent.clone();
        profile.system_prompt = Some(system_prompt.to_string());

        let argv = CliAgentRunner::build_argv(&profile, user_prompt, None, None);
        let ProviderConfig::Cli(ref cli_config) = profile.provider;

        let stdin_data = if cli_config.input_mode == InputMode::Stdin {
            if cli_config.system_prompt_arg.is_none() {
                Some(format!(
                    "[System Instructions]\n{}\n[End System Instructions]\n\n{}",
                    system_prompt, user_prompt
                ))
            } else {
                Some(user_prompt.to_string())
            }
        } else {
            None
        };

        let cwd = profile.working_dir.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".".to_string())
        });

        let tools_in_flight = Arc::new(AtomicUsize::new(0));

        let spawn_input = SpawnInput {
            run_id: None,
            backend_id: profile.id.clone(),
            scope_key: None,
            argv,
            cwd: Some(cwd),
            env: Some(profile.env.clone()),
            stdin_data,
            timeout_ms: Some(profile.timeout_seconds * 1000),
            no_output_timeout_ms: Some(cli_config.no_output_timeout_ms),
            tools_in_flight: Some(tools_in_flight.clone()),
            form_suspended: None,
        };

        let managed = self.process_supervisor.spawn(spawn_input).await?;
        let ao_process::supervisor::ManagedRun {
            mut stdout_rx,
            mut stderr_rx,
            wait_handle,
            ..
        } = managed;

        let mut normalizer = self
            .normalizer_registry
            .create(&cli_config.command, cli_config);
        normalizer.set_tools_in_flight_counter(tools_in_flight);

        let stderr_handle = tokio::spawn(async move {
            let mut buf = String::new();
            while let Some(line) = stderr_rx.recv().await {
                buf.push_str(&line);
                buf.push('\n');
            }
            buf
        });

        let mut delta_buf = String::new();
        let mut complete_text: Option<String> = None;

        while let Some(chunk) = stdout_rx.recv().await {
            for payload in normalizer.process_chunk(&chunk) {
                match payload {
                    AgentEventPayload::TextDelta { text } => delta_buf.push_str(&text),
                    AgentEventPayload::TextComplete { text } => complete_text = Some(text),
                    _ => {}
                }
            }
        }

        let exit = wait_handle.await.map_err(|e| {
            AoError::Internal(format!("Agent routing classify wait join error: {}", e))
        })?;

        let stderr_text = stderr_handle.await.unwrap_or_default();

        for payload in normalizer.finalize(exit.exit_code, &stderr_text) {
            match payload {
                AgentEventPayload::TextDelta { text } => delta_buf.push_str(&text),
                AgentEventPayload::TextComplete { text } => complete_text = Some(text),
                _ => {}
            }
        }

        Ok(complete_text.unwrap_or(delta_buf))
    }
}

// ──────────────────────────────────────────────────────────────────────
// Classifier prompt builder
// ──────────────────────────────────────────────────────────────────────

/// Build the (system_prompt, user_prompt) pair for the agent-scope routing
/// classifier. The roster lists all entries from `agent.delegates_to` plus
/// a synthesised self-entry. The model must emit a single
/// `<task_owner>agent_id</task_owner>` tag.
pub fn build_agent_routing_classifier_prompt(
    tasklist: &ao_protocol::tasklist::Tasklist,
    task: &Task,
    agent: &AgentProfile,
) -> (String, String) {
    let system = r#"You are a task-routing assistant for an agent that manages a team of delegates.
Your job is to decide which delegate (or the agent itself) should handle a given task.

Output format — you MUST emit exactly one tag:
  <task_owner>{agent_id}</task_owner>

Rules:
- Pick the delegate whose name/purpose best matches the task.
- If the task is best handled by the coordinating agent itself, output its agent_id.
- Output only the agent_id inside the tag, exactly as listed in the roster.
- Do not add any text after the closing tag.

Spreading parallel work:
- When two or more delegates are an equally good fit (for example, their purposes are interchangeable) AND this task belongs to a parallel group, prefer a delegate that is NOT already handling a sibling task in that group. Each agent works its tasks one at a time, so co-assigning equally-suited parallel tasks to a single delegate forces them to run back-to-back; spreading them across distinct delegates lets them run concurrently. Only use this to break a genuine tie — never pass over a clearly better-matched delegate just to spread work.
"#
    .to_string();

    let mut user = String::new();

    // Roster section
    user.push_str("## Delegate Roster\n\n");
    for d in &agent.delegates_to {
        user.push_str(&format!(
            "- agent_id: `{}` | name: {} | purpose: {}\n",
            d.target_agent_id, d.name, d.purpose
        ));
    }
    // Self entry
    user.push_str(&format!(
        "- agent_id: `{}` | name: {} (Self) | purpose: Handles tasks directly\n",
        agent.id, agent.name,
    ));

    user.push_str("\n## Task to Route\n\n");
    user.push_str(&format!("tasklist_id: {}\n", tasklist.id));
    user.push_str(&format!("task_id: {}\n", task.id));
    user.push_str(&format!("prompt: {}\n", task.prompt));

    // When this task sits in a parallel group, surface the current owners of
    // its sibling tasks so the classifier can spread equally-suited work
    // across distinct delegates. Routing runs serially per owning agent and
    // re-reads fresh tasklist state, so earlier siblings already carry an
    // owner by the time a later sibling is classified — giving the model a
    // real signal to round-robin genuine ties. Each agent runs its tasks one
    // at a time, so co-assigning parallel siblings to one delegate serializes
    // them; distributing them restores the concurrency the parallel group
    // asked for.
    if let Some(group) = tasklist
        .groups
        .iter()
        .find(|g| g.tasks.iter().any(|t| t.id == task.id))
    {
        if group.mode == TaskGroupMode::Par {
            let siblings: Vec<&Task> = group.tasks.iter().filter(|t| t.id != task.id).collect();
            if !siblings.is_empty() {
                user.push_str("\n## Parallel Group Siblings\n\n");
                user.push_str(
                    "This task is in a PARALLEL group. Current owners of its sibling tasks:\n",
                );
                for s in siblings {
                    let owner = if s.owner_agent_id.is_empty() {
                        "(unassigned)"
                    } else {
                        s.owner_agent_id.as_str()
                    };
                    user.push_str(&format!("- task_id: {} | owner: {}\n", s.id, owner));
                }
                user.push_str(
                    "\nIf fit is otherwise equal, prefer a delegate not already listed above so the parallel tasks run concurrently.\n",
                );
            }
        }
    }

    (system, user)
}

// ──────────────────────────────────────────────────────────────────────
// Registry
// ──────────────────────────────────────────────────────────────────────

/// Per-agent routing queue manager registry. Spawns each manager lazily on
/// first `submit_agent_routing` for that agent.
pub struct AgentRoutingQueueManagerRegistry {
    handles: Arc<RwLock<HashMap<AgentId, AgentRoutingQueueManagerHandle>>>,
    persistence: Arc<PersistenceLayer>,
    process_supervisor: Arc<dyn ProcessSupervisor>,
    normalizer_registry: Arc<NormalizerRegistry>,
    task_feeder: Arc<TaskFeeder>,
}

impl AgentRoutingQueueManagerRegistry {
    pub fn new(
        persistence: Arc<PersistenceLayer>,
        process_supervisor: Arc<dyn ProcessSupervisor>,
        normalizer_registry: Arc<NormalizerRegistry>,
        task_feeder: Arc<TaskFeeder>,
    ) -> Self {
        Self {
            handles: Arc::new(RwLock::new(HashMap::new())),
            persistence,
            process_supervisor,
            normalizer_registry,
            task_feeder,
        }
    }

    /// Get or lazily create the per-agent loop handle.
    async fn get_or_create(&self, agent_id: &AgentId) -> AgentRoutingQueueManagerHandle {
        {
            let handles = self.handles.read().await;
            if let Some(handle) = handles.get(agent_id) {
                return handle.clone();
            }
        }

        let mut handles = self.handles.write().await;
        if let Some(handle) = handles.get(agent_id) {
            return handle.clone();
        }

        let (request_tx, request_rx) = mpsc::channel::<AgentRoutingRequest>(128);
        let handle = AgentRoutingQueueManagerHandle { request_tx };

        let manager = AgentRoutingQueueManager::new(
            agent_id.clone(),
            request_rx,
            Arc::clone(&self.persistence),
            Arc::clone(&self.process_supervisor),
            Arc::clone(&self.normalizer_registry),
            Arc::clone(&self.task_feeder),
        );
        tokio::spawn(manager.run());

        handles.insert(agent_id.clone(), handle.clone());
        handle
    }

    /// Submit a routing request for the given agent.
    pub async fn submit(
        &self,
        agent_id: &AgentId,
        request: AgentRoutingRequest,
    ) -> Result<(), AoError> {
        let handle = self.get_or_create(agent_id).await;
        handle
            .request_tx
            .send(request)
            .await
            .map_err(|e| AoError::Internal(format!("Agent routing queue send error: {}", e)))
    }
}

#[async_trait]
impl AgentRoutingChannel for AgentRoutingQueueManagerRegistry {
    async fn submit_agent_routing(
        &self,
        agent_id: &AgentId,
        request: AgentRoutingRequest,
    ) -> Result<(), AoError> {
        self.submit(agent_id, request).await
    }
}

// ──────────────────────────────────────────────────────────────────────
// Unit tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;
    use std::time::Duration;

    use ao_persistence::paths::DataRoot;
    use ao_process::mock::{MockProcessSupervisor, MockScenario};
    use ao_process::registry::RunRecord;
    use ao_process::supervisor::ManagedRun;
    use ao_protocol::agent::{AgentProfile, CliProviderConfig, DelegateTarget, OutputFormat};
    use ao_protocol::tasklist::{
        Task, TaskGroup, TaskGroupMode, TaskId, TaskStatus, Tasklist, TasklistId, TasklistStatus,
    };

    use crate::task_feeder::TaskDispatcher;

    // ── Helpers ────────────────────────────────────────────────────────

    struct CountingSupervisor {
        inner: MockProcessSupervisor,
        spawn_count: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingSupervisor {
        fn new(scenarios: Vec<MockScenario>) -> (Arc<Self>, Arc<std::sync::atomic::AtomicUsize>) {
            let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let sup = Arc::new(Self {
                inner: MockProcessSupervisor::new(scenarios),
                spawn_count: Arc::clone(&counter),
            });
            (sup, counter)
        }
    }

    #[async_trait]
    impl ProcessSupervisor for CountingSupervisor {
        async fn spawn(&self, input: SpawnInput) -> Result<ManagedRun, AoError> {
            self.spawn_count.fetch_add(1, Ordering::SeqCst);
            self.inner.spawn(input).await
        }

        async fn cancel(&self, run_id: &str) -> Result<(), AoError> {
            self.inner.cancel(run_id).await
        }

        fn get_record(&self, run_id: &str) -> Option<RunRecord> {
            self.inner.get_record(run_id)
        }

        fn list_active(&self) -> Vec<RunRecord> {
            self.inner.list_active()
        }
    }

    struct NoopDispatcher {
        calls: Mutex<Vec<(AgentId, TaskId)>>,
    }

    impl NoopDispatcher {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl TaskDispatcher for NoopDispatcher {
        async fn dispatch_task(
            &self,
            owner_agent_id: &AgentId,
            _prompt: String,
            _owner: &ao_protocol::tasklist::TasklistOwner,
            _tasklist_id: &TasklistId,
            task_id: &TaskId,
        ) -> Result<(), AoError> {
            self.calls
                .lock()
                .unwrap()
                .push((owner_agent_id.clone(), task_id.clone()));
            Ok(())
        }
    }

    fn agent_profile(id: &str, delegates: Vec<DelegateTarget>) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: "test agent".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "test-cli".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: std::collections::HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30_000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: std::collections::HashMap::new(),
            max_instances: 2,
            timeout_seconds: 60,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            enabled_plugins: std::collections::HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            delegates_to: delegates,
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            max_turns: None,
}
    }

    fn scenario(stdout: &str) -> MockScenario {
        MockScenario {
            stdout_lines: vec![stdout.to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 0,
        }
    }

    /// Seed a persistence layer with an owning agent profile + an
    /// agent-owned tasklist containing one unowned Pending task. Returns
    /// all identifiers the test needs to drive routing requests.
    async fn setup_agent_routing_test(
        scenarios: Vec<MockScenario>,
        delegates: Vec<DelegateTarget>,
    ) -> (
        Arc<AgentRoutingQueueManagerRegistry>,
        Arc<PersistenceLayer>,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<NoopDispatcher>,
        String, // owning agent_id
        String, // tasklist_id
        String, // task_id
        tempfile::TempDir,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_root = DataRoot::new(tmp.path());
        let persistence = Arc::new(
            PersistenceLayer::init_with_root(data_root)
                .await
                .expect("init persistence"),
        );

        let (counting, counter) = CountingSupervisor::new(scenarios);
        let process_supervisor: Arc<dyn ProcessSupervisor> = counting;
        let normalizer_registry = Arc::new(NormalizerRegistry::new());
        let dispatcher = NoopDispatcher::new();
        let task_feeder = Arc::new(TaskFeeder::new(
            Arc::new(ao_persistence::tasklist_store::TasklistStore::new(
                persistence.data_root.clone(),
            )),
            Arc::clone(&dispatcher) as Arc<dyn TaskDispatcher>,
        ));
        let registry = Arc::new(AgentRoutingQueueManagerRegistry::new(
            Arc::clone(&persistence),
            Arc::clone(&process_supervisor),
            Arc::clone(&normalizer_registry),
            task_feeder,
        ));

        let owner_id = "owning-agent".to_string();
        let profile = agent_profile(&owner_id, delegates.clone());
        persistence
            .agents
            .create(&profile)
            .await
            .expect("create owning agent");

        // Delegates must exist in persistence so dispatch_one's safety check
        // doesn't fail tasks routed to them.
        for d in &delegates {
            let delegate_profile = agent_profile(&d.target_agent_id, vec![]);
            persistence
                .agents
                .create(&delegate_profile)
                .await
                .expect("create delegate agent");
        }

        let tasklist_id = "tl-agent-1".to_string();
        let task_id = "task-agent-1".to_string();
        let group_id = "g-1".to_string();
        let tasklist = Tasklist {
            id: tasklist_id.clone(),
            owner: TasklistOwner::Agent { agent_id: owner_id.clone() },
            team_id: None,
            title: "Agent routing test".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: group_id.clone(),
                mode: TaskGroupMode::Seq,
                tasks: vec![Task {
                    id: task_id.clone(),
                    owner_agent_id: String::new(),
                    prompt: "Write a haiku about Rust".to_string(),
                    expected_outputs: vec![],
                    status: TaskStatus::Pending,
                    group_id: group_id.clone(),
                    attempt_count: 0,
                    error_log: vec![],
                    comments: vec![],
                    attachments: vec![],
                    remind_me: None,
                    parse_failed: false,
                    notification_parse_retry_count: 0,
                    assignment: None,
                    classifier_token: 0,
                    dispatch_token: 0,
                }],
            }],
            workspace_dir: persistence
                .data_root
                .agent_tasklist_workspace_dir(&owner_id, &tasklist_id)
                .to_string_lossy()
                .to_string(),
            transcripts_dir: persistence
                .data_root
                .agent_tasklist_transcripts_dir(&owner_id, &tasklist_id)
                .to_string_lossy()
                .to_string(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        persistence
            .tasklists
            .create_for_agent(&tasklist)
            .await
            .expect("create agent tasklist");

        (registry, persistence, counter, dispatcher, owner_id, tasklist_id, task_id, tmp)
    }

    /// Poll persistence until `predicate(&task)` returns true or `timeout`
    /// elapses.
    async fn wait_for_agent_task<F>(
        persistence: &PersistenceLayer,
        owner_id: &str,
        tasklist_id: &str,
        task_id: &str,
        timeout: Duration,
        predicate: F,
    ) -> Task
    where
        F: Fn(&Task) -> bool,
    {
        let owner = TasklistOwner::Agent { agent_id: owner_id.to_string() };
        let start = std::time::Instant::now();
        loop {
            if let Some(tl) = persistence
                .tasklists
                .get_by_owner(&owner, tasklist_id)
                .await
                .expect("load tasklist")
            {
                if let Some(t) = tl
                    .groups
                    .iter()
                    .flat_map(|g| g.tasks.iter())
                    .find(|t| t.id == task_id)
                    .cloned()
                {
                    if predicate(&t) {
                        return t;
                    }
                }
            }
            if start.elapsed() > timeout {
                panic!(
                    "Timed out after {:?} waiting for agent task {} predicate",
                    timeout, task_id
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // ── Test 1: leaf-agent fast path ───────────────────────────────────

    /// Empty delegates_to → owner stamped to self immediately,
    /// zero LLM calls spawned.
    #[tokio::test]
    async fn test_leaf_agent_auto_self_no_llm_call() {
        // No mock scenarios supplied; any spawn would consume an absent
        // entry from the empty vec and panic.
        let (registry, persistence, counter, dispatcher, owner_id, tl_id, task_id, _tmp) =
            setup_agent_routing_test(vec![], vec![]).await;

        registry
            .submit(
                &owner_id,
                AgentRoutingRequest {
                    agent_id: owner_id.clone(),
                    tasklist_id: tl_id.clone(),
                    task_id: task_id.clone(),
                },
            )
            .await
            .expect("submit");

        let task = wait_for_agent_task(
            &persistence,
            &owner_id,
            &tl_id,
            &task_id,
            Duration::from_secs(5),
            |t| !t.owner_agent_id.is_empty(),
        )
        .await;

        // Give feeder.advance a tick to call the dispatcher before asserting.
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "leaf-agent path must not spawn any LLM process"
        );
        assert_eq!(
            task.owner_agent_id, owner_id,
            "owner_agent_id must be the owning agent itself"
        );
        assert!(
            task.comments.is_empty(),
            "leaf-agent path must not append any comments"
        );
        let calls = dispatcher.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "feeder should dispatch the self-assigned task once");
        assert_eq!(calls[0].0, owner_id);
        assert_eq!(calls[0].1, task_id);
    }

    // ── Test 2: non-leaf roster shape ──────────────────────────────────

    /// Non-empty delegates_to → prompt contains all delegate
    /// agent_ids and the self entry; when the mock LLM picks a valid
    /// delegate, that delegate is stamped as the owner.
    #[tokio::test]
    async fn test_non_leaf_roster_contains_all_delegates_and_self() {
        let delegate_a = DelegateTarget {
            target_agent_id: "delegate-alpha".to_string(),
            name: "Alpha".to_string(),
            purpose: "Writes poetry".to_string(),
            share_context_allowed: false,
        };
        let delegate_b = DelegateTarget {
            target_agent_id: "delegate-beta".to_string(),
            name: "Beta".to_string(),
            purpose: "Handles prose".to_string(),
            share_context_allowed: false,
        };
        let delegates = vec![delegate_a, delegate_b];

        // Verify the prompt builder includes all delegates + self before
        // running the full routing flow.
        let owning_agent = agent_profile("owning-agent", delegates.clone());
        let fake_tasklist = Tasklist {
            id: "tl-x".to_string(),
            owner: TasklistOwner::Agent { agent_id: "owning-agent".to_string() },
            team_id: None,
            title: "test".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![],
            workspace_dir: String::new(),
            transcripts_dir: String::new(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        let fake_task = Task {
            id: "t-x".to_string(),
            owner_agent_id: String::new(),
            prompt: "Write a haiku".to_string(),
            expected_outputs: vec![],
            status: TaskStatus::Pending,
            group_id: "g-x".to_string(),
            attempt_count: 0,
            error_log: vec![],
            comments: vec![],
            attachments: vec![],
            remind_me: None,
            parse_failed: false,
            notification_parse_retry_count: 0,
            assignment: None,
            classifier_token: 0,
            dispatch_token: 0,
        };

        let (_, user_prompt) =
            build_agent_routing_classifier_prompt(&fake_tasklist, &fake_task, &owning_agent);

        assert!(
            user_prompt.contains("delegate-alpha"),
            "user prompt must list delegate-alpha: {}",
            user_prompt
        );
        assert!(
            user_prompt.contains("delegate-beta"),
            "user prompt must list delegate-beta: {}",
            user_prompt
        );
        assert!(
            user_prompt.contains("owning-agent"),
            "user prompt must include the self (owning-agent) entry: {}",
            user_prompt
        );

        // Now run the full routing flow with a mock that picks delegate-alpha.
        let scenarios = vec![scenario("<task_owner>delegate-alpha</task_owner>")];
        let (registry, persistence, counter, dispatcher, owner_id, tl_id, task_id, _tmp) =
            setup_agent_routing_test(scenarios, delegates).await;

        registry
            .submit(
                &owner_id,
                AgentRoutingRequest {
                    agent_id: owner_id.clone(),
                    tasklist_id: tl_id.clone(),
                    task_id: task_id.clone(),
                },
            )
            .await
            .expect("submit");

        let task = wait_for_agent_task(
            &persistence,
            &owner_id,
            &tl_id,
            &task_id,
            Duration::from_secs(5),
            |t| !t.owner_agent_id.is_empty(),
        )
        .await;

        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(counter.load(Ordering::SeqCst), 1, "one LLM call expected");
        assert_eq!(
            task.owner_agent_id, "delegate-alpha",
            "classifier output must be respected"
        );
        assert!(task.comments.is_empty());
        let calls = dispatcher.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "delegate-alpha");
    }

    // ── Test 3: LLM error → comment + unowned ─────────────────────────

    /// A supervisor whose `spawn` always returns an error, simulating a
    /// network timeout, missing binary, or other fatal LLM failure.
    struct ErrorSupervisor;

    #[async_trait]
    impl ProcessSupervisor for ErrorSupervisor {
        async fn spawn(&self, _input: SpawnInput) -> Result<ManagedRun, AoError> {
            Err(AoError::Internal("simulated LLM spawn error".to_string()))
        }
        async fn cancel(&self, _run_id: &str) -> Result<(), AoError> {
            Ok(())
        }
        fn get_record(&self, _run_id: &str) -> Option<ao_process::registry::RunRecord> {
            None
        }
        fn list_active(&self) -> Vec<ao_process::registry::RunRecord> {
            vec![]
        }
    }

    /// When the one-shot LLM spawn itself fails (supervisor
    /// returns Err), `owner_agent_id` must remain empty and a failure
    /// comment must be appended to the task.
    #[tokio::test]
    async fn test_llm_failure_appends_comment_and_leaves_unowned() {
        let delegate = DelegateTarget {
            target_agent_id: "delegate-gamma".to_string(),
            name: "Gamma".to_string(),
            purpose: "Writes code".to_string(),
            share_context_allowed: false,
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let data_root = DataRoot::new(tmp.path());
        let persistence = Arc::new(
            PersistenceLayer::init_with_root(data_root)
                .await
                .expect("init persistence"),
        );

        let process_supervisor: Arc<dyn ProcessSupervisor> = Arc::new(ErrorSupervisor);
        let normalizer_registry = Arc::new(NormalizerRegistry::new());
        let dispatcher = NoopDispatcher::new();
        let task_feeder = Arc::new(TaskFeeder::new(
            Arc::new(ao_persistence::tasklist_store::TasklistStore::new(
                persistence.data_root.clone(),
            )),
            Arc::clone(&dispatcher) as Arc<dyn TaskDispatcher>,
        ));
        let registry = Arc::new(AgentRoutingQueueManagerRegistry::new(
            Arc::clone(&persistence),
            Arc::clone(&process_supervisor),
            Arc::clone(&normalizer_registry),
            task_feeder,
        ));

        let owner_id = "owning-agent".to_string();
        persistence
            .agents
            .create(&agent_profile(&owner_id, vec![delegate]))
            .await
            .expect("create owning agent");

        let tasklist_id = "tl-err-1".to_string();
        let task_id = "task-err-1".to_string();
        let group_id = "g-err".to_string();
        let tasklist = Tasklist {
            id: tasklist_id.clone(),
            owner: TasklistOwner::Agent { agent_id: owner_id.clone() },
            team_id: None,
            title: "Error test".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: group_id.clone(),
                mode: TaskGroupMode::Seq,
                tasks: vec![Task {
                    id: task_id.clone(),
                    owner_agent_id: String::new(),
                    prompt: "Write something".to_string(),
                    expected_outputs: vec![],
                    status: TaskStatus::Pending,
                    group_id: group_id.clone(),
                    attempt_count: 0,
                    error_log: vec![],
                    comments: vec![],
                    attachments: vec![],
                    remind_me: None,
                    parse_failed: false,
                    notification_parse_retry_count: 0,
                    assignment: None,
                    classifier_token: 0,
                    dispatch_token: 0,
                }],
            }],
            workspace_dir: persistence
                .data_root
                .agent_tasklist_workspace_dir(&owner_id, &tasklist_id)
                .to_string_lossy()
                .to_string(),
            transcripts_dir: persistence
                .data_root
                .agent_tasklist_transcripts_dir(&owner_id, &tasklist_id)
                .to_string_lossy()
                .to_string(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        persistence
            .tasklists
            .create_for_agent(&tasklist)
            .await
            .expect("create agent tasklist");

        registry
            .submit(
                &owner_id,
                AgentRoutingRequest {
                    agent_id: owner_id.clone(),
                    tasklist_id: tasklist_id.clone(),
                    task_id: task_id.clone(),
                },
            )
            .await
            .expect("submit");

        let task = wait_for_agent_task(
            &persistence,
            &owner_id,
            &tasklist_id,
            &task_id,
            Duration::from_secs(5),
            |t| !t.comments.is_empty(),
        )
        .await;

        assert_eq!(
            task.owner_agent_id, "",
            "owner_agent_id must remain empty on LLM failure"
        );
        assert_eq!(task.comments.len(), 1, "exactly one failure comment");
        assert_eq!(task.comments[0].author_kind, TaskCommentAuthorKind::Agent);
        assert!(
            task.comments[0].body.contains("Routing failed"),
            "failure comment body: {}",
            task.comments[0].body
        );
        assert!(
            dispatcher.calls.lock().unwrap().is_empty(),
            "no dispatch on LLM failure"
        );
    }
}
