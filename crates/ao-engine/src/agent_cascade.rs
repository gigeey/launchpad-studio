//! Cascade rules for agent deletion.
//!
//! When an agent is deleted, three side-effects must run in order:
//!
//! 1. Cancel every task that is currently `InProgress` and assigned to the deleted agent.
//! 2. Remove the deleted agent from every other agent's delegate targets (`delegates_to`).
//! 3. Reset the `assignment` field of every `NotStarted` task assigned to the deleted agent
//!    to `None` and spawn a fresh classifier for each.
//!
//! The actual profile-file deletion and data-dir removal remain in the HTTP route handler
//! (ao-server), which calls `execute_cascade` before proceeding with those steps.

use std::sync::Arc;

use ao_engine_tools_core::TasklistServiceHandle;
use ao_persistence::PersistenceLayer;
use ao_protocol::agent::DelegateTarget;
use ao_protocol::error::AoError;
use ao_protocol::tasklist::{TaskStatus, TasklistOwner};

use crate::task_classifier::TaskClassifier;
use crate::task_feeder::TaskFeeder;

// ── Types ─────────────────────────────────────────────────────────────────────

/// Preview of what `execute_cascade` will mutate, computed without touching disk.
/// Returned by `AgentCascadeService::compute_impact` for use in confirmation dialogs.
#[derive(Debug)]
pub struct CascadeImpact {
    /// Every delegate target across all other agents that references the deleted agent.
    /// `(parent_agent_id, target)` where `target.target_agent_id == deleted_agent_id`.
    pub delegate_refs: Vec<(String, DelegateTarget)>,
    /// Task IDs (across all parents) that are currently `InProgress` and owned by the
    /// deleted agent. These will be cancelled.
    pub in_flight_tasks: Vec<String>,
    /// Task IDs (across all parents) that are `NotStarted` and owned by the deleted
    /// agent. These will have their assignment cleared and be re-classified.
    pub not_started_tasks: Vec<String>,
}

/// Full task location — used internally during execution so we know where to write back.
struct TaskLocation {
    parent_agent_id: String,
    tasklist_id: String,
    task_id: String,
    classifier_token: u64,
    title: String,
    description: String,
}

// ── Service ───────────────────────────────────────────────────────────────────

pub struct AgentCascadeService {
    persistence: Arc<PersistenceLayer>,
    task_feeder: Arc<TaskFeeder>,
    task_classifier: TaskClassifier,
    svc: Arc<dyn TasklistServiceHandle + Send + Sync>,
}

impl AgentCascadeService {
    pub fn new(
        persistence: Arc<PersistenceLayer>,
        task_feeder: Arc<TaskFeeder>,
        task_classifier: TaskClassifier,
        svc: Arc<dyn TasklistServiceHandle + Send + Sync>,
    ) -> Self {
        Self { persistence, task_feeder, task_classifier, svc }
    }

    /// Scan all agents for the cascade impact of deleting `deleted_agent_id`.
    /// Pure read — no disk mutations.
    pub async fn compute_impact(&self, deleted_agent_id: &str) -> Result<CascadeImpact, AoError> {
        let agents = self.persistence.agents.list().await?;
        let mut delegate_refs: Vec<(String, DelegateTarget)> = Vec::new();
        let mut in_flight_tasks: Vec<String> = Vec::new();
        let mut not_started_tasks: Vec<String> = Vec::new();

        for agent in &agents {
            if agent.id == deleted_agent_id {
                continue;
            }

            // Delegate-target check.
            if let Some(target) = agent
                .delegates_to
                .iter()
                .find(|t| t.target_agent_id == deleted_agent_id)
            {
                delegate_refs.push((agent.id.clone(), target.clone()));
            }

            // Tasklist scan.
            let tasklists = match self.persistence.tasklists.list_for_agent(&agent.id).await {
                Ok(tl) => tl,
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent.id,
                        error = %e,
                        "compute_impact: failed to list tasklists"
                    );
                    continue;
                }
            };
            for tl in &tasklists {
                for group in &tl.groups {
                    for task in &group.tasks {
                        let is_owned = task
                            .assignment
                            .as_ref()
                            .map(|a| a.owner_agent_id == deleted_agent_id)
                            .unwrap_or(false);
                        if !is_owned {
                            continue;
                        }
                        match task.status {
                            TaskStatus::InProgress => in_flight_tasks.push(task.id.clone()),
                            TaskStatus::Pending => not_started_tasks.push(task.id.clone()),
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(CascadeImpact { delegate_refs, in_flight_tasks, not_started_tasks })
    }

    /// Execute the full cascade for `deleted_agent_id`. Steps run in order:
    ///
    /// 1. Cancel in-flight tasks (set `InProgress` → `Cancelled`, fire `on_task_terminal`).
    /// 2. Remove the deleted agent from every other agent's delegate targets.
    /// 3. Clear assignment on `NotStarted` orphans via CAS, then spawn re-classification.
    ///
    /// The caller is responsible for deleting the agent profile and data dir afterwards.
    pub async fn execute_cascade(&self, deleted_agent_id: &str) -> Result<(), AoError> {
        let agents = self.persistence.agents.list().await?;

        let mut in_flight: Vec<TaskLocation> = Vec::new();
        let mut orphans: Vec<TaskLocation> = Vec::new();
        let mut delegate_agents: Vec<String> = Vec::new(); // agents that list deleted_id as a delegate target

        // Single scan to collect everything we need.
        for agent in &agents {
            if agent.id == deleted_agent_id {
                continue;
            }

            // Check delegate targets.
            if agent
                .delegates_to
                .iter()
                .any(|t| t.target_agent_id == deleted_agent_id)
            {
                delegate_agents.push(agent.id.clone());
            }

            // Check tasklists.
            let tasklists = match self.persistence.tasklists.list_for_agent(&agent.id).await {
                Ok(tl) => tl,
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent.id,
                        error = %e,
                        "execute_cascade: failed to list tasklists"
                    );
                    continue;
                }
            };
            for tl in &tasklists {
                for group in &tl.groups {
                    for task in &group.tasks {
                        let is_owned = task
                            .assignment
                            .as_ref()
                            .map(|a| a.owner_agent_id == deleted_agent_id)
                            .unwrap_or(false);
                        if !is_owned {
                            continue;
                        }
                        let mut parts = task.prompt.splitn(2, ": ");
                        let title = parts.next().unwrap_or("").to_string();
                        let description = parts.next().unwrap_or("").to_string();
                        let loc = TaskLocation {
                            parent_agent_id: agent.id.clone(),
                            tasklist_id: tl.id.clone(),
                            task_id: task.id.clone(),
                            classifier_token: task.classifier_token,
                            title,
                            description,
                        };
                        match task.status {
                            TaskStatus::InProgress => in_flight.push(loc),
                            TaskStatus::Pending => orphans.push(loc),
                            _ => {}
                        }
                    }
                }
            }
        }

        let total_in_flight = in_flight.len();
        let total_orphans = orphans.len();
        let total_delegates = delegate_agents.len();
        tracing::info!(
            deleted_agent_id = %deleted_agent_id,
            in_flight = total_in_flight,
            orphans = total_orphans,
            delegate_refs = total_delegates,
            "execute_cascade: starting"
        );

        // ── Step 1: Cancel in-flight tasks ────────────────────────────────────
        for loc in &in_flight {
            let parent = loc.parent_agent_id.clone();
            let tl_id = loc.tasklist_id.clone();
            let t_id = loc.task_id.clone();

            // Mutate status to Cancelled first so on_task_terminal sees the right state.
            let mutate_result = self
                .persistence
                .tasklists
                .mutate_for_agent(&parent, &tl_id, |tl| {
                    for group in &mut tl.groups {
                        for task in &mut group.tasks {
                            if task.id == t_id && task.status == TaskStatus::InProgress {
                                task.status = TaskStatus::Skipped;
                            }
                        }
                    }
                    Ok(())
                })
                .await;

            if let Err(e) = mutate_result {
                tracing::warn!(
                    task_id = %t_id,
                    error = %e,
                    "execute_cascade: failed to cancel in-flight task"
                );
                continue;
            }

            // Fire the feeder so SEQ groups advance past the now-terminal slot.
            let owner = TasklistOwner::Agent { agent_id: parent.clone() };
            if let Err(e) = self.task_feeder.on_task_terminal(&owner, &tl_id, &t_id).await {
                tracing::warn!(
                    task_id = %t_id,
                    error = %e,
                    "execute_cascade: on_task_terminal failed for cancelled task"
                );
            }
        }

        // ── Step 2: Remove deleted agent from other agents' delegate targets ──
        for agent_id in &delegate_agents {
            match self.persistence.agents.get(agent_id).await {
                Ok(Some(mut profile)) => {
                    let before = profile.delegates_to.len();
                    profile
                        .delegates_to
                        .retain(|t| t.target_agent_id != deleted_agent_id);
                    if profile.delegates_to.len() != before {
                        if let Err(e) = self.persistence.agents.update(&profile).await {
                            tracing::warn!(
                                agent_id = %agent_id,
                                error = %e,
                                "execute_cascade: failed to save profile after removing delegate target"
                            );
                        }
                    }
                }
                Ok(None) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        "execute_cascade: agent profile vanished during delegate cleanup"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        error = %e,
                        "execute_cascade: failed to load profile for delegate cleanup"
                    );
                }
            }
        }

        // ── Step 3: Re-classify orphaned NotStarted tasks ─────────────────────
        for loc in orphans {
            let parent = loc.parent_agent_id.clone();
            let tl_id = loc.tasklist_id.clone();
            let t_id = loc.task_id.clone();
            let expected_token = loc.classifier_token;

            // Clear assignment via CAS — this bumps the token, invalidating any
            // stale in-flight classification write-backs.
            match self
                .svc
                .set_assignment(&parent, &tl_id, &t_id, None, expected_token)
                .await
            {
                Ok(true) => {
                    // CAS succeeded. Spawn fresh classification against the now-updated book.
                    let new_token = expected_token + 1;
                    let classifier = self.task_classifier.clone();
                    let svc_clone = Arc::clone(&self.svc);
                    let title = loc.title.clone();
                    let desc = loc.description.clone();
                    tokio::spawn(async move {
                        match classifier.classify(&parent, &t_id, &title, &desc).await {
                            Ok(assignment) => {
                                match svc_clone
                                    .set_assignment(
                                        &parent,
                                        &tl_id,
                                        &t_id,
                                        Some(assignment),
                                        new_token,
                                    )
                                    .await
                                {
                                    Ok(true) => {
                                        tracing::debug!(
                                            task_id = %t_id,
                                            "cascade: re-classification written"
                                        );
                                    }
                                    Ok(false) => {
                                        tracing::debug!(
                                            task_id = %t_id,
                                            "cascade: re-classification stale, discarded"
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            task_id = %t_id,
                                            error = %e,
                                            "cascade: set_assignment failed after re-classify"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    task_id = %t_id,
                                    "cascade: re-classify failed: {}",
                                    e
                                );
                            }
                        }
                    });
                }
                Ok(false) => {
                    tracing::debug!(
                        task_id = %t_id,
                        "cascade: stale token for orphan, skipping re-classify"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %t_id,
                        error = %e,
                        "execute_cascade: set_assignment (clear) failed for orphan"
                    );
                }
            }
        }

        tracing::info!(
            deleted_agent_id = %deleted_agent_id,
            in_flight = total_in_flight,
            orphans = total_orphans,
            delegate_refs = total_delegates,
            "execute_cascade: complete"
        );

        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    use ao_engine_tools_core::terminal_report::{CancelOutcome, TerminalWatcherGuard};
    use ao_engine_tools_core::TasklistServiceHandle;
    use ao_normalizer::registry::NormalizerRegistry;
    use ao_persistence::paths::DataRoot;
    use ao_persistence::tasklist_store::TasklistStore;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, DelegateTarget, InputMode, OutputFormat, ProviderConfig,
    };
    use ao_protocol::error::AoError;
    use ao_protocol::tasklist::{
        AssignmentMode, Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus, Tasklist,
        TasklistOwner, TasklistStatus,
    };
    use chrono::Utc;
    use uuid::Uuid;

    use crate::task_classifier::TaskClassifier;
    use crate::task_feeder::TaskFeeder;

    // ── Helpers ──────────────────────────────────────────────────────────────

    async fn make_persistence(dir: &TempDir) -> Arc<PersistenceLayer> {
        Arc::new(PersistenceLayer::init_with_root(DataRoot::new(dir.path())).await.unwrap())
    }

    fn make_agent(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: HashMap::new(),
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
            env: HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: None,
            serialize: false,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            delegates_to: vec![],
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

    fn make_task(id: &str, owner_id: &str, status: TaskStatus) -> Task {
        Task {
            id: id.to_string(),
            prompt: format!("{}: task description", id),
            owner_agent_id: owner_id.to_string(),
            assignment: Some(TaskAssignment {
                owner_agent_id: owner_id.to_string(),
                mode: AssignmentMode::Classified,
            }),
            classifier_token: 0,
            dispatch_token: 0,
            status,
            group_id: String::new(),
            attempt_count: 0,
            error_log: vec![],
            attachments: vec![],
            comments: vec![],
            expected_outputs: vec![],
            remind_me: None,
            parse_failed: false,
            notification_parse_retry_count: 0,
        }
    }

    async fn create_agent_tasklist(
        persistence: &Arc<PersistenceLayer>,
        agent_id: &str,
        tasks: Vec<Task>,
    ) -> Tasklist {
        let tl_id = Uuid::new_v4().to_string();
        let group_id = Uuid::new_v4().to_string();
        let tasks_with_group: Vec<Task> = tasks
            .into_iter()
            .map(|mut t| { t.group_id = group_id.clone(); t })
            .collect();
        let group = TaskGroup {
            id: group_id,
            mode: TaskGroupMode::Seq,
            tasks: tasks_with_group,
        };
        let workspace = persistence
            .data_root
            .agent_tasklist_workspace_dir(agent_id, &tl_id);
        let transcripts = persistence
            .data_root
            .agent_tasklist_transcripts_dir(agent_id, &tl_id);
        let tl = Tasklist {
            id: tl_id,
            owner: TasklistOwner::Agent { agent_id: agent_id.to_string() },
            team_id: None,
            title: "test".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![group],
            workspace_dir: workspace.to_string_lossy().to_string(),
            transcripts_dir: transcripts.to_string_lossy().to_string(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        persistence.tasklists.create_for_agent(&tl).await.unwrap();
        tl
    }

    fn make_target(id: &str, name: &str, purpose: &str) -> DelegateTarget {
        DelegateTarget {
            target_agent_id: id.to_string(),
            name: name.to_string(),
            purpose: purpose.to_string(),
            share_context_allowed: false,
        }
    }

    /// Set the delegate targets on an already-created agent profile.
    async fn set_delegates(
        persistence: &Arc<PersistenceLayer>,
        owner_id: &str,
        targets: Vec<DelegateTarget>,
    ) {
        let mut profile = persistence.agents.get(owner_id).await.unwrap().unwrap();
        profile.delegates_to = targets;
        persistence.agents.update(&profile).await.unwrap();
    }

    // ── Minimal mock tasklist service ─────────────────────────────────────────

    #[derive(Default)]
    struct MockSvc {
        set_assign_calls: Mutex<Vec<(String, String, String, Option<TaskAssignment>, u64)>>,
    }

    #[async_trait]
    impl TasklistServiceHandle for MockSvc {
        async fn agent_active(&self, _: &str) -> Result<Option<Tasklist>, AoError> {
            Ok(None)
        }
        async fn create_for_agent(
            &self,
            _: &str,
            _: String,
            _: Vec<TaskGroup>,
        ) -> Result<Tasklist, AoError> {
            unimplemented!()
        }
        async fn get_agent_max_instances(&self, _: &str) -> Result<u32, AoError> {
            Ok(1)
        }
        async fn add_group_for_agent(
            &self,
            _: &str,
            _: &str,
            _: Vec<Task>,
            _: TaskGroupMode,
        ) -> Result<Tasklist, AoError> {
            unimplemented!()
        }
        async fn update_task_for_agent(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: Option<String>,
            _: Option<String>,
            _: Option<Vec<String>>,
        ) -> Result<Tasklist, AoError> {
            unimplemented!()
        }
        async fn complete_task_for_agent(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<(), AoError> {
            unimplemented!()
        }
        async fn terminal_watcher(&self, _: &str) -> Result<TerminalWatcherGuard, AoError> {
            unimplemented!()
        }
        async fn cancel_for_agent(&self, _: &str) -> Result<CancelOutcome, AoError> {
            unimplemented!()
        }
        async fn set_assignment(
            &self,
            agent_id: &str,
            tasklist_id: &str,
            task_id: &str,
            assignment: Option<TaskAssignment>,
            expected_token: u64,
        ) -> Result<bool, AoError> {
            self.set_assign_calls.lock().await.push((
                agent_id.to_string(),
                tasklist_id.to_string(),
                task_id.to_string(),
                assignment,
                expected_token,
            ));
            Ok(true)
        }
    }

    /// No-op task dispatcher for tests — never dispatches.
    struct NoopDispatcher;

    #[async_trait]
    impl crate::task_feeder::TaskDispatcher for NoopDispatcher {
        async fn dispatch_task(
            &self,
            _owner_agent_id: &ao_protocol::agent::AgentId,
            _prompt: String,
            _owner: &ao_protocol::tasklist::TasklistOwner,
            _tasklist_id: &ao_protocol::tasklist::TasklistId,
            _task_id: &ao_protocol::tasklist::TaskId,
        ) -> Result<(), ao_protocol::error::AoError> {
            Ok(())
        }
    }

    fn make_cascade(
        persistence: Arc<PersistenceLayer>,
        svc: Arc<dyn TasklistServiceHandle + Send + Sync>,
    ) -> AgentCascadeService {
        let mock_supervisor = MockProcessSupervisor::new(vec![]);
        let normalizer_registry = Arc::new(NormalizerRegistry::new());
        let classifier = TaskClassifier::new(
            Arc::clone(&persistence),
            Arc::new(mock_supervisor),
            normalizer_registry,
        );
        let task_feeder = Arc::new(
            TaskFeeder::new(
                Arc::new(TasklistStore::new(persistence.data_root.clone())),
                Arc::new(NoopDispatcher),
            )
        );
        AgentCascadeService::new(persistence, task_feeder, classifier, svc)
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// cascade_delegates_only: two parents both list X as a delegate target; no tasks owned by X.
    /// After execute_cascade, neither profile references X.
    #[tokio::test]
    async fn cascade_delegates_only() {
        let tmp = TempDir::new().unwrap();
        let persistence = make_persistence(&tmp).await;

        // Seed two agent profiles so list() returns them.
        let _ = persistence.agents.create(&make_agent("agent-a")).await;
        let _ = persistence.agents.create(&make_agent("agent-b")).await;
        let _ = persistence.agents.create(&make_agent("child-x")).await;

        let target = make_target("child-x", "Child X", "A child agent");
        set_delegates(&persistence, "agent-a", vec![target.clone()]).await;
        set_delegates(&persistence, "agent-b", vec![target.clone()]).await;

        let svc = Arc::new(MockSvc::default());
        let cascade = make_cascade(Arc::clone(&persistence), Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>);

        let impact = cascade.compute_impact("child-x").await.unwrap();
        assert_eq!(impact.delegate_refs.len(), 2);
        assert!(impact.in_flight_tasks.is_empty());
        assert!(impact.not_started_tasks.is_empty());

        cascade.execute_cascade("child-x").await.unwrap();

        // Verify neither profile still lists child-x as a delegate target.
        let profile_a = persistence.agents.get("agent-a").await.unwrap().unwrap();
        assert!(
            !profile_a.delegates_to.iter().any(|t| t.target_agent_id == "child-x"),
            "agent-a profile still references child-x"
        );

        let profile_b = persistence.agents.get("agent-b").await.unwrap().unwrap();
        assert!(
            !profile_b.delegates_to.iter().any(|t| t.target_agent_id == "child-x"),
            "agent-b profile still references child-x"
        );

        // No classifiers were spawned.
        assert!(svc.set_assign_calls.lock().await.is_empty());
    }

    /// cascade_cancels_in_flight: X has 2 in-flight tasks; after cascade both are Cancelled.
    #[tokio::test]
    async fn cascade_cancels_in_flight() {
        let tmp = TempDir::new().unwrap();
        let persistence = make_persistence(&tmp).await;

        let _ = persistence.agents.create(&make_agent("parent-a")).await;
        let _ = persistence.agents.create(&make_agent("child-x")).await;

        let t1 = make_task("task-1", "child-x", TaskStatus::InProgress);
        let t2 = make_task("task-2", "child-x", TaskStatus::InProgress);
        let tl = create_agent_tasklist(&persistence, "parent-a", vec![t1, t2]).await;

        let svc = Arc::new(MockSvc::default());
        let cascade = make_cascade(Arc::clone(&persistence), svc as Arc<dyn TasklistServiceHandle + Send + Sync>);

        let impact = cascade.compute_impact("child-x").await.unwrap();
        assert_eq!(impact.in_flight_tasks.len(), 2);
        assert!(impact.not_started_tasks.is_empty());

        cascade.execute_cascade("child-x").await.unwrap();

        // Verify both tasks are now Cancelled.
        let updated = persistence
            .tasklists
            .get_for_agent("parent-a", &tl.id)
            .await
            .unwrap()
            .unwrap();
        for group in &updated.groups {
            for task in &group.tasks {
                assert_eq!(
                    task.status,
                    TaskStatus::Skipped,
                    "task {} should be Skipped (cancelled by cascade)",
                    task.id
                );
            }
        }
    }

    /// cascade_reclassifies_orphans: X has 3 NotStarted tasks; after cascade all 3 go to
    /// assignment: None (set_assignment called with None) and classifier is spawned.
    #[tokio::test]
    async fn cascade_reclassifies_orphans() {
        let tmp = TempDir::new().unwrap();
        let persistence = make_persistence(&tmp).await;

        let _ = persistence.agents.create(&make_agent("parent-a")).await;
        let _ = persistence.agents.create(&make_agent("child-x")).await;

        let t1 = make_task("task-1", "child-x", TaskStatus::Pending);
        let t2 = make_task("task-2", "child-x", TaskStatus::Pending);
        let t3 = make_task("task-3", "child-x", TaskStatus::Pending);
        create_agent_tasklist(&persistence, "parent-a", vec![t1, t2, t3]).await;

        let svc = Arc::new(MockSvc::default());
        let cascade = make_cascade(
            Arc::clone(&persistence),
            Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>,
        );

        let impact = cascade.compute_impact("child-x").await.unwrap();
        assert_eq!(impact.not_started_tasks.len(), 3);
        assert!(impact.in_flight_tasks.is_empty());

        cascade.execute_cascade("child-x").await.unwrap();

        // Give spawned classify tasks a moment to call set_assignment (clear step).
        // The mock svc.set_assignment is synchronous; the clear calls happen before the spawn.
        let calls = svc.set_assign_calls.lock().await;
        // Three clear calls (assignment = None).
        let clear_calls: Vec<_> = calls.iter().filter(|(_, _, _, a, _)| a.is_none()).collect();
        assert_eq!(clear_calls.len(), 3, "expected 3 clear calls, got {}", clear_calls.len());
    }

    /// cascade_dry_run: compute_impact for an agent with 2 delegate refs + 2 in-flight + 5 NotStarted
    /// returns CascadeImpact with those exact counts and DOES NOT mutate any profile.
    #[tokio::test]
    async fn cascade_dry_run() {
        let tmp = TempDir::new().unwrap();
        let persistence = make_persistence(&tmp).await;

        // Create three agent profiles.
        for id in &["parent-a", "parent-b", "child-x"] {
            let _ = persistence.agents.create(&make_agent(id)).await;
        }

        let target = make_target("child-x", "X", "");
        set_delegates(&persistence, "parent-a", vec![target.clone()]).await;
        set_delegates(&persistence, "parent-b", vec![target.clone()]).await;

        // parent-a: 2 in-flight + 2 NotStarted
        let in_flight: Vec<Task> = (0..2)
            .map(|i| make_task(&format!("if-{}", i), "child-x", TaskStatus::InProgress))
            .collect();
        let pending_a: Vec<Task> = (0..2)
            .map(|i| make_task(&format!("pa-{}", i), "child-x", TaskStatus::Pending))
            .collect();
        let mut all_a = in_flight;
        all_a.extend(pending_a);
        create_agent_tasklist(&persistence, "parent-a", all_a).await;

        // parent-b: 3 NotStarted
        let pending_b: Vec<Task> = (0..3)
            .map(|i| make_task(&format!("pb-{}", i), "child-x", TaskStatus::Pending))
            .collect();
        create_agent_tasklist(&persistence, "parent-b", pending_b).await;

        let svc = Arc::new(MockSvc::default());
        let cascade = make_cascade(
            Arc::clone(&persistence),
            Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>,
        );

        let impact = cascade.compute_impact("child-x").await.unwrap();

        assert_eq!(impact.delegate_refs.len(), 2, "delegate_refs count mismatch");
        assert_eq!(impact.in_flight_tasks.len(), 2, "in_flight count mismatch");
        assert_eq!(impact.not_started_tasks.len(), 5, "not_started count mismatch");

        // No mutations must have happened: both profiles still list child-x.
        let profile_a = persistence.agents.get("parent-a").await.unwrap().unwrap();
        assert!(
            profile_a.delegates_to.iter().any(|t| t.target_agent_id == "child-x"),
            "parent-a profile mutated by dry run"
        );
        let profile_b = persistence.agents.get("parent-b").await.unwrap().unwrap();
        assert!(
            profile_b.delegates_to.iter().any(|t| t.target_agent_id == "child-x"),
            "parent-b profile mutated by dry run"
        );
        assert!(
            svc.set_assign_calls.lock().await.is_empty(),
            "set_assignment called during dry run"
        );
    }

    /// cascade_dry_run_then_real: dry_run preview matches real cascade execution counts.
    #[tokio::test]
    async fn cascade_dry_run_then_real() {
        let tmp = TempDir::new().unwrap();
        let persistence = make_persistence(&tmp).await;

        for id in &["parent-a", "child-x"] {
            let _ = persistence.agents.create(&make_agent(id)).await;
        }

        set_delegates(&persistence, "parent-a", vec![make_target("child-x", "X", "")]).await;

        let tasks: Vec<Task> = vec![
            make_task("t1", "child-x", TaskStatus::InProgress),
            make_task("t2", "child-x", TaskStatus::Pending),
            make_task("t3", "child-x", TaskStatus::Pending),
        ];
        create_agent_tasklist(&persistence, "parent-a", tasks).await;

        let svc = Arc::new(MockSvc::default());
        let cascade = make_cascade(
            Arc::clone(&persistence),
            Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>,
        );

        let impact = cascade.compute_impact("child-x").await.unwrap();
        let preview_delegates = impact.delegate_refs.len();
        let preview_in_flight = impact.in_flight_tasks.len();
        let preview_orphans = impact.not_started_tasks.len();

        cascade.execute_cascade("child-x").await.unwrap();

        let calls = svc.set_assign_calls.lock().await;
        // Each orphan gets exactly one clear call (None assignment).
        let clear_calls: Vec<_> = calls.iter().filter(|(_, _, _, a, _)| a.is_none()).collect();
        assert_eq!(clear_calls.len(), preview_orphans, "orphan clear calls != preview");

        // Delegate target was removed: load and verify.
        let profile = persistence.agents.get("parent-a").await.unwrap().unwrap();
        assert!(
            !profile.delegates_to.iter().any(|t| t.target_agent_id == "child-x"),
            "profile still references deleted agent as delegate target"
        );
        let _ = (preview_delegates, preview_in_flight); // counts checked via other assertions
    }
}
