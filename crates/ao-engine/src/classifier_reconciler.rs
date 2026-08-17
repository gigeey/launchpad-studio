//! Periodic reconciler that keeps every agent-owned task with a runner
//! assignment.
//!
//! Replaces the old 6-hour boot sweep. Each tick walks every agent's tasklists,
//! collects every task with `assignment.is_none()`, and spawns a retrying
//! classifier attempt for each one — with claim-based dedup so the event-driven
//! spawn sites (`TodoCreate`/`TodoAdd`/`TodoUpdate`, the chat-input HTTP routes)
//! can keep their low-latency first-touch path without the reconciler ever
//! double-spawning.
//!
//! Three reliability properties this gives us that the boot sweep did not:
//!
//!   * **No "stuck on Classifying" surface.** Anything that fails the
//!     event-driven spawn (rate limit, transient model error, retry budget
//!     exhausted) gets re-attempted within one [`RECONCILE_INTERVAL`] instead
//!     of waiting for the next process restart.
//!   * **Permanent failures are not terminal.** Conditions can change between
//!     ticks — the parent agent's address book might be repopulated, a missing
//!     profile reloaded. `classify_with_retry` exits cleanly on `Permanent`
//!     and the reconciler picks the task up next tick.
//!   * **No thundering herd on the model.** Orphans inside one tick are
//!     dispatched with a per-task stagger so 20 unowned tasks don't all hit
//!     the classifier semaphore in the same tokio instant.
//!
//! Scope is deliberately narrow: this runner only assigns runners to unowned
//! tasks. Task lifecycle (success / failure / escalation) stays in
//! `task_feeder`. The two runners are independent so a classifier bug cannot
//! cascade into a tasklist-completion bug.

use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::{ClassifierHandle, ClassifierInFlight, TasklistServiceHandle};
use ao_engine_tools_engine::classify_with_retry;
use ao_persistence::PersistenceLayer;
use tokio::sync::watch;

/// How often the reconciler sweeps for unassigned tasks. Picked at 30s — fast
/// enough that a missed first-touch (whatever caused it) recovers within a
/// single user turn; slow enough that an idle process with no orphans costs
/// effectively one disk walk per minute.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Background runner that periodically re-classifies any agent-owned task with
/// `assignment: None`. Construct with [`Self::new`], spawn with [`Self::run`].
pub struct ClassifierReconciler {
    classifier: Arc<dyn ClassifierHandle + Send + Sync>,
    persistence: Arc<PersistenceLayer>,
    svc: Arc<dyn TasklistServiceHandle + Send + Sync>,
    in_flight: Arc<ClassifierInFlight>,
    interval: Duration,
}

impl ClassifierReconciler {
    pub fn new(
        classifier: Arc<dyn ClassifierHandle + Send + Sync>,
        persistence: Arc<PersistenceLayer>,
        svc: Arc<dyn TasklistServiceHandle + Send + Sync>,
        in_flight: Arc<ClassifierInFlight>,
    ) -> Self {
        Self {
            classifier,
            persistence,
            svc,
            in_flight,
            interval: RECONCILE_INTERVAL,
        }
    }

    /// Test-only interval override. Production callers should leave the
    /// default [`RECONCILE_INTERVAL`] in place.
    #[cfg(test)]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Spawn the runner as a detached tokio task. Fires its first sweep
    /// immediately at startup — replacing the explicit boot sweep step — and
    /// then re-sweeps every [`Self::interval`]. Returns a `watch::Sender`
    /// whose drop (or `send(())`) stops the loop.
    pub fn run(self) -> watch::Sender<()> {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(());
        tracing::info!(interval = ?self.interval, "ClassifierReconciler starting");

        tokio::spawn(async move {
            // Fire the first sweep immediately so a fresh process catches up
            // on anything left orphaned by the previous one.
            self.tick().await;

            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        tracing::info!("ClassifierReconciler shutting down");
                        break;
                    }
                    _ = tokio::time::sleep(self.interval) => {
                        self.tick().await;
                    }
                }
            }
        });

        shutdown_tx
    }

    /// One sweep pass. Public for tests that drive the runner manually.
    pub async fn tick(&self) {
        let orphans = match self.collect_orphans().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "reconciler: orphan scan failed");
                return;
            }
        };

        if orphans.is_empty() {
            tracing::debug!("reconciler: no orphans this tick");
            return;
        }

        // Stagger dispatch across ~80% of the tick window so concurrent
        // orphans don't all reach the classifier semaphore in the same
        // tokio instant. With `interval=30s` and 20 orphans that's a 1.2s
        // gap between spawns — small enough to drain promptly, large enough
        // that the bounded model-call pool (CLASSIFY_POOL_SIZE=4) doesn't
        // immediately backlog.
        let stagger = self
            .interval
            .mul_f64(0.8)
            .checked_div(orphans.len() as u32)
            .unwrap_or(Duration::ZERO);

        tracing::info!(
            count = orphans.len(),
            stagger_ms = stagger.as_millis() as u64,
            "reconciler: dispatching unassigned tasks",
        );

        let total = orphans.len();
        for (idx, orphan) in orphans.into_iter().enumerate() {
            // Spawn first, then sleep — the spawn itself takes microseconds
            // and the claim happens inside `classify_with_retry`. The sleep
            // gates the NEXT spawn so the stagger spreads dispatches over
            // the tick window rather than the spawn loop. Skip the sleep
            // after the final spawn so `tick().await` returns promptly.
            let classifier = Arc::clone(&self.classifier);
            let svc = Arc::clone(&self.svc);
            let in_flight = Arc::clone(&self.in_flight);
            tokio::spawn(classify_with_retry(
                classifier,
                svc,
                Some(in_flight),
                orphan.agent_id,
                orphan.tasklist_id,
                orphan.task_id,
                orphan.parent_agent_id,
                orphan.title,
                orphan.description,
                orphan.expected_token,
            ));
            if idx + 1 < total && !stagger.is_zero() {
                tokio::time::sleep(stagger).await;
            }
        }
    }

    /// Walk every agent's tasklists and return one entry per task with
    /// `assignment.is_none()`. Status filter is intentionally absent —
    /// classification's job is to assign a runner, never to read or mutate
    /// task lifecycle state. If a Completed or Cancelled row somehow lost
    /// its assignment, claiming and re-classifying is harmless (the result
    /// is either a no-op CAS or a clean overwrite of a historical record).
    async fn collect_orphans(&self) -> Result<Vec<Orphan>, ao_protocol::error::AoError> {
        use ao_protocol::tasklist::TasklistStatus;

        let agents = self.persistence.agents.list().await?;
        let mut out = Vec::new();

        for agent in &agents {
            let tasklists = match self.persistence.tasklists.list_for_agent(&agent.id).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent.id,
                        error = %e,
                        "reconciler: tasklist list failed; skipping agent",
                    );
                    continue;
                }
            };
            for tl in &tasklists {
                // Skip terminal tasklists — re-classifying a task on a
                // Cancelled/Completed list would queue work the feeder will
                // never read. Active and Paused both stay in scope so
                // paused-with-orphan recovers as soon as the user resumes.
                if matches!(
                    tl.status,
                    TasklistStatus::Completed | TasklistStatus::Cancelled | TasklistStatus::Failed
                ) {
                    continue;
                }
                for group in &tl.groups {
                    for task in &group.tasks {
                        if task.assignment.is_some() {
                            continue;
                        }
                        // Same prompt split as TodoCreate uses on emit. The
                        // classifier tolerates an empty title.
                        let mut parts = task.prompt.splitn(2, ": ");
                        let title = parts.next().unwrap_or("").to_string();
                        let description = parts.next().unwrap_or("").to_string();
                        out.push(Orphan {
                            agent_id: agent.id.clone(),
                            tasklist_id: tl.id.clone(),
                            task_id: task.id.clone(),
                            parent_agent_id: agent.id.clone(),
                            title,
                            description,
                            expected_token: task.classifier_token,
                        });
                    }
                }
            }
        }

        Ok(out)
    }
}

/// One unassigned task located by a reconciler sweep.
struct Orphan {
    agent_id: String,
    tasklist_id: String,
    task_id: String,
    parent_agent_id: String,
    title: String,
    description: String,
    expected_token: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use ao_engine_tools_core::{
        CancelOutcome, ClassifyOutcome, TerminalWatcherGuard,
    };
    use ao_persistence::paths::DataRoot;
    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };
    use ao_protocol::error::AoError;
    use ao_protocol::tasklist::{
        AssignmentMode, Task, TaskAssignment, TaskGroup, TaskGroupMode, TaskStatus,
        Tasklist, TasklistOwner, TasklistStatus,
    };
    use async_trait::async_trait;
    use chrono::Utc;

    // ── Test fixtures ────────────────────────────────────────────────────────

    fn agent_profile(id: &str) -> AgentProfile {
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
            serialize: true,
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

    fn unassigned_task(id: &str, group_id: &str) -> Task {
        Task {
            id: id.to_string(),
            owner_agent_id: String::new(),
            prompt: format!("Title {id}: Description {id}"),
            expected_outputs: vec![],
            status: TaskStatus::Pending,
            group_id: group_id.to_string(),
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
        }
    }

    fn tasklist(
        data_root: &DataRoot,
        agent_id: &str,
        tl_id: &str,
        status: TasklistStatus,
        tasks: Vec<Task>,
    ) -> Tasklist {
        Tasklist {
            id: tl_id.to_string(),
            owner: TasklistOwner::Agent {
                agent_id: agent_id.to_string(),
            },
            team_id: None,
            title: "test list".to_string(),
            description: String::new(),
            status,
            groups: vec![TaskGroup {
                id: "g1".to_string(),
                mode: TaskGroupMode::Seq,
                tasks,
            }],
            workspace_dir: data_root
                .agent_tasklist_workspace_dir(agent_id, tl_id)
                .to_string_lossy()
                .into_owned(),
            transcripts_dir: data_root
                .agent_tasklist_transcripts_dir(agent_id, tl_id)
                .to_string_lossy()
                .into_owned(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            }
    }

    // ── Mock classifier ──────────────────────────────────────────────────────

    struct CountingClassifier {
        owner: String,
        calls: AtomicU32,
    }

    #[async_trait]
    impl ClassifierHandle for CountingClassifier {
        async fn classify(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
        ) -> ClassifyOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ClassifyOutcome::Assigned(TaskAssignment {
                owner_agent_id: self.owner.clone(),
                mode: AssignmentMode::Classified,
            })
        }
    }

    // ── Mock tasklist service ────────────────────────────────────────────────

    #[derive(Default)]
    struct RecordingSvc {
        set_assignment_calls:
            Mutex<Vec<(String, String, String, Option<TaskAssignment>, u64)>>,
    }

    #[async_trait]
    impl TasklistServiceHandle for RecordingSvc {
        async fn agent_active(&self, _: &str) -> Result<Option<Tasklist>, AoError> {
            unimplemented!()
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
        async fn terminal_watcher(
            &self,
            _: &str,
        ) -> Result<TerminalWatcherGuard, AoError> {
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
            self.set_assignment_calls.lock().unwrap().push((
                agent_id.to_string(),
                tasklist_id.to_string(),
                task_id.to_string(),
                assignment,
                expected_token,
            ));
            Ok(true)
        }
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    /// A single tick assigns every unassigned task on an Active tasklist.
    #[tokio::test]
    async fn tick_assigns_every_unassigned_task() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let persistence =
            Arc::new(PersistenceLayer::init_with_root(data_root.clone()).await.unwrap());

        persistence.agents.create(&agent_profile("parent")).await.unwrap();
        let tl = tasklist(
            &data_root,
            "parent",
            "tl1",
            TasklistStatus::Active,
            vec![
                unassigned_task("t1", "g1"),
                unassigned_task("t2", "g1"),
                unassigned_task("t3", "g1"),
            ],
        );
        persistence.tasklists.create_for_agent(&tl).await.unwrap();

        let classifier = Arc::new(CountingClassifier {
            owner: "parent".to_string(),
            calls: AtomicU32::new(0),
        });
        let svc = Arc::new(RecordingSvc::default());
        let in_flight = Arc::new(ClassifierInFlight::new());

        let reconciler = ClassifierReconciler::new(
            Arc::clone(&classifier) as Arc<dyn ClassifierHandle + Send + Sync>,
            Arc::clone(&persistence),
            Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>,
            Arc::clone(&in_flight),
        )
        .with_interval(Duration::from_millis(10));

        reconciler.tick().await;

        // Spawned classify_with_retry tasks are detached; give them a chance
        // to complete (the mock classifier never sleeps).
        tokio::time::sleep(Duration::from_millis(200)).await;

        let calls = svc.set_assignment_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 3, "expected 3 assignments, got {calls:?}");
        for (_, _, task_id, assignment, _) in &calls {
            assert!(matches!(task_id.as_str(), "t1" | "t2" | "t3"));
            assert!(assignment.is_some());
        }
    }

    /// Terminal tasklists are skipped — a Completed list with an unassigned
    /// task is not re-classified.
    #[tokio::test]
    async fn tick_skips_terminal_tasklists() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let persistence =
            Arc::new(PersistenceLayer::init_with_root(data_root.clone()).await.unwrap());

        persistence.agents.create(&agent_profile("parent")).await.unwrap();
        let tl = tasklist(
            &data_root,
            "parent",
            "tl1",
            TasklistStatus::Completed,
            vec![unassigned_task("t1", "g1")],
        );
        persistence.tasklists.create_for_agent(&tl).await.unwrap();

        let classifier = Arc::new(CountingClassifier {
            owner: "parent".to_string(),
            calls: AtomicU32::new(0),
        });
        let svc = Arc::new(RecordingSvc::default());
        let in_flight = Arc::new(ClassifierInFlight::new());

        let reconciler = ClassifierReconciler::new(
            Arc::clone(&classifier) as Arc<dyn ClassifierHandle + Send + Sync>,
            Arc::clone(&persistence),
            Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>,
            in_flight,
        );

        reconciler.tick().await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(classifier.calls.load(Ordering::SeqCst), 0);
        assert!(svc.set_assignment_calls.lock().unwrap().is_empty());
    }

    /// A second tick fired while the first tick's spawn is still in-flight
    /// must not double-spawn — the dedup claim blocks the duplicate.
    #[tokio::test]
    async fn second_tick_does_not_double_spawn_in_flight_task() {
        struct SlowClassifier {
            calls: AtomicU32,
        }
        #[async_trait]
        impl ClassifierHandle for SlowClassifier {
            async fn classify(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &str,
            ) -> ClassifyOutcome {
                self.calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(300)).await;
                ClassifyOutcome::Assigned(TaskAssignment {
                    owner_agent_id: "parent".to_string(),
                    mode: AssignmentMode::Classified,
                })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let persistence =
            Arc::new(PersistenceLayer::init_with_root(data_root.clone()).await.unwrap());

        persistence.agents.create(&agent_profile("parent")).await.unwrap();
        let tl = tasklist(
            &data_root,
            "parent",
            "tl1",
            TasklistStatus::Active,
            vec![unassigned_task("t1", "g1")],
        );
        persistence.tasklists.create_for_agent(&tl).await.unwrap();

        let classifier = Arc::new(SlowClassifier {
            calls: AtomicU32::new(0),
        });
        let svc = Arc::new(RecordingSvc::default());
        let in_flight = Arc::new(ClassifierInFlight::new());

        let reconciler = ClassifierReconciler::new(
            Arc::clone(&classifier) as Arc<dyn ClassifierHandle + Send + Sync>,
            Arc::clone(&persistence),
            Arc::clone(&svc) as Arc<dyn TasklistServiceHandle + Send + Sync>,
            Arc::clone(&in_flight),
        )
        .with_interval(Duration::from_millis(10));

        // First tick spawns the slow classify. Give the spawn a moment to
        // claim the in-flight slot, then fire a second tick.
        reconciler.tick().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(in_flight.contains("parent", "tl1", "t1"));

        reconciler.tick().await;

        // Let both ticks resolve fully.
        tokio::time::sleep(Duration::from_millis(400)).await;

        // Classifier should have been called exactly once — the second tick
        // saw the slot already claimed and skipped silently.
        assert_eq!(
            classifier.calls.load(Ordering::SeqCst),
            1,
            "expected exactly one classifier call, got {}",
            classifier.calls.load(Ordering::SeqCst),
        );
    }
}
