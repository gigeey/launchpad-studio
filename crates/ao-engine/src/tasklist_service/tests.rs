//! Unit tests for the tasklist service.
//!
//! Declared from the parent module as `#[cfg(test)] mod tests;` — this is the
//! same module as the inline test blocks it replaces, so private items of
//! `tasklist_service` remain in scope here via `use super::*`.
//!
//! The three groups below stay as separate nested modules rather than being
//! flattened: each carries its own regression rationale in a `//!` header, and
//! keeping them nested preserves the original test paths
//! (`tasklist_service::terminal_watcher::…`) that CI failure output refers to.
//! The glob below is what lets their own `use super::*` reach the parent.

use super::*;

#[cfg(test)]
mod terminal_watcher {
    use super::*;
    use std::sync::Arc;

    use ao_persistence::{paths::DataRoot, tasklist_store::TasklistStore};
    use ao_protocol::tasklist::{Task, TaskGroup, TaskGroupMode, TasklistOwner, TasklistStatus};
    use chrono::Utc;
    use tempfile::TempDir;

    use crate::task_feeder::TaskFeeder;

    /// Minimal no-op task dispatcher for unit tests.
    struct NoopDispatcher;
    #[async_trait::async_trait]
    impl crate::task_feeder::TaskDispatcher for NoopDispatcher {
        async fn dispatch_task(
            &self,
            _owner_agent_id: &ao_protocol::agent::AgentId,
            _prompt: String,
            _owner: &TasklistOwner,
            _tasklist_id: &ao_protocol::tasklist::TasklistId,
            _task_id: &ao_protocol::tasklist::TaskId,
        ) -> Result<(), AoError> {
            Ok(())
        }
    }

    /// Setup just store + feeder — enough to test the watcher without the full
    /// service stack (PersistenceLayer + RoutingQueue are not needed here).
    async fn setup_feeder() -> (TempDir, DataRoot, Arc<TasklistStore>, Arc<TaskFeeder>) {
        let tmp = TempDir::new().unwrap();
        let data_root = DataRoot::new(tmp.path());
        data_root.ensure_directories().await.unwrap();
        let store = Arc::new(TasklistStore::new(data_root.clone()));
        let feeder = Arc::new(TaskFeeder::new(Arc::clone(&store), Arc::new(NoopDispatcher)));
        (tmp, data_root, store, feeder)
    }

    fn make_task(id: &str, owner: &str) -> Task {
        Task {
            id: id.to_string(),
            group_id: "g1".to_string(),
            prompt: format!("Do {id}"),
            owner_agent_id: owner.to_string(),
            status: ao_protocol::tasklist::TaskStatus::Pending,
            expected_outputs: vec![],
            error_log: vec![],
            attempt_count: 0,
            comments: vec![],
            attachments: vec![],
            notification_parse_retry_count: 0,
            parse_failed: false,
            remind_me: None,
            assignment: None,
            classifier_token: 0,
            dispatch_token: 0,
        }
    }

    fn make_tasklist(id: &str, owner_agent: &str, data_root: &DataRoot) -> Tasklist {
        let workspace = data_root.agent_tasklist_workspace_dir(owner_agent, id);
        let transcripts = data_root.agent_tasklist_transcripts_dir(owner_agent, id);
        Tasklist {
            id: id.to_string(),
            owner: TasklistOwner::Agent { agent_id: owner_agent.to_string() },
            team_id: None,
            title: format!("TL {id}"),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: "g1".to_string(),
                mode: TaskGroupMode::Seq,
                tasks: vec![make_task("t1", owner_agent), make_task("t2", owner_agent)],
            }],
            workspace_dir: workspace.to_string_lossy().to_string(),
            transcripts_dir: transcripts.to_string_lossy().to_string(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            }
    }

    // ---- tasklist_service::terminal_watcher tests ----------------------------

    /// Mark a task InProgress then Completed — simulates the real dispatch path.
    async fn complete_task(
        store: &TasklistStore,
        owner: &TasklistOwner,
        tl_id: &str,
        task_id: &str,
    ) {
        use ao_protocol::tasklist::TaskStatus;
        store.set_task_status_by_owner(owner, tl_id, task_id, TaskStatus::InProgress)
            .await.unwrap();
        store.set_task_status_by_owner(owner, tl_id, task_id, TaskStatus::Completed)
            .await.unwrap();
    }

    /// Happy path: watcher fires with status "completed" when all tasks complete.
    #[tokio::test]
    async fn terminal_watcher_happy_path_completes() {
        let (_tmp, data_root, store, feeder) = setup_feeder().await;
        let agent_id = "agent-watcher-ok";
        let tl_id = "tl-watcher-ok";

        let tl = make_tasklist(tl_id, agent_id, &data_root);
        store.create_for_agent(&tl).await.unwrap();

        let guard = feeder.register_terminal_watcher(tl_id);

        let owner = TasklistOwner::Agent { agent_id: agent_id.to_string() };

        // SEQ: t1 completes → t2 dispatches, then t2 completes → Completed.
        complete_task(&store, &owner, tl_id, "t1").await;
        feeder.on_task_terminal(&owner, &tl_id.to_string(), &"t1".to_string()).await.unwrap();

        complete_task(&store, &owner, tl_id, "t2").await;
        feeder.on_task_terminal(&owner, &tl_id.to_string(), &"t2".to_string()).await.unwrap();

        let report = guard.wait().await.unwrap();
        assert_eq!(report.status, "completed");
        assert_eq!(report.counts.succeeded, 2);
        assert_eq!(report.counts.failed, 0);
        assert_eq!(report.tasks.len(), 2);
    }

    /// When the receiver (guard) is dropped, the sender detects is_closed()
    /// and skips silently when the tasklist later reaches a terminal state.
    #[tokio::test]
    async fn terminal_watcher_receiver_dropped_sender_skips() {
        let (_tmp, data_root, store, feeder) = setup_feeder().await;
        let agent_id = "agent-watcher-drop";
        let tl_id = "tl-watcher-drop";

        // Single-task tasklist so it completes after one terminal.
        let workspace = data_root.agent_tasklist_workspace_dir(agent_id, tl_id);
        let transcripts = data_root.agent_tasklist_transcripts_dir(agent_id, tl_id);
        let tl = Tasklist {
            id: tl_id.to_string(),
            owner: TasklistOwner::Agent { agent_id: agent_id.to_string() },
            team_id: None,
            title: "drop-test".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: "g1".to_string(),
                mode: TaskGroupMode::Seq,
                tasks: vec![make_task("t1", agent_id)],
            }],
            workspace_dir: workspace.to_string_lossy().to_string(),
            transcripts_dir: transcripts.to_string_lossy().to_string(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        store.create_for_agent(&tl).await.unwrap();

        {
            let _guard = feeder.register_terminal_watcher(tl_id);
            // guard dropped here — Drop removes the sender from registry
        }

        let owner = TasklistOwner::Agent { agent_id: agent_id.to_string() };
        complete_task(&store, &owner, tl_id, "t1").await;
        // Must not panic — the sender was removed by Drop.
        let result = feeder.on_task_terminal(&owner, &tl_id.to_string(), &"t1".to_string()).await;
        assert!(result.is_ok(), "should not error when guard was dropped: {:?}", result);
    }

    /// When no watcher is registered for a tasklist, terminal transitions must
    /// not error or panic.
    #[tokio::test]
    async fn terminal_watcher_no_watcher_registered_no_error() {
        let (_tmp, data_root, store, feeder) = setup_feeder().await;
        let agent_id = "agent-no-watcher";
        let tl_id = "tl-no-watcher";

        let workspace = data_root.agent_tasklist_workspace_dir(agent_id, tl_id);
        let transcripts = data_root.agent_tasklist_transcripts_dir(agent_id, tl_id);
        let tl = Tasklist {
            id: tl_id.to_string(),
            owner: TasklistOwner::Agent { agent_id: agent_id.to_string() },
            team_id: None,
            title: "no-watcher-test".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: "g1".to_string(),
                mode: TaskGroupMode::Seq,
                tasks: vec![make_task("t1", agent_id)],
            }],
            workspace_dir: workspace.to_string_lossy().to_string(),
            transcripts_dir: transcripts.to_string_lossy().to_string(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        store.create_for_agent(&tl).await.unwrap();

        let owner = TasklistOwner::Agent { agent_id: agent_id.to_string() };
        complete_task(&store, &owner, tl_id, "t1").await;
        let result = feeder.on_task_terminal(&owner, &tl_id.to_string(), &"t1".to_string()).await;
        assert!(result.is_ok(), "no-watcher terminal must not error: {:?}", result);
    }
}

#[cfg(test)]
mod cancel_propagation {
    //! Regression tests: cancelling a tasklist must transition InProgress tasks
    //! to Skipped (not leave them as zombies). End-to-end CLI kill coverage is
    //! in the integration tests; here we verify the DB state transitions.

    use super::*;
    use std::sync::Arc;

    use ao_persistence::{paths::DataRoot, tasklist_store::TasklistStore, PersistenceLayer};
    use ao_protocol::tasklist::{Task, TaskGroup, TaskGroupMode, TasklistOwner, TasklistStatus};
    use chrono::Utc;
    use tempfile::TempDir;

    use crate::event_bus::EventBus;
    use crate::task_feeder::TaskFeeder;

    struct NoopDispatcher;
    #[async_trait::async_trait]
    impl crate::task_feeder::TaskDispatcher for NoopDispatcher {
        async fn dispatch_task(
            &self,
            _owner_agent_id: &ao_protocol::agent::AgentId,
            _prompt: String,
            _owner: &TasklistOwner,
            _tasklist_id: &ao_protocol::tasklist::TasklistId,
            _task_id: &ao_protocol::tasklist::TaskId,
        ) -> Result<(), AoError> {
            Ok(())
        }
    }

    async fn setup() -> (TempDir, DataRoot, Arc<TasklistStore>, Arc<TasklistService>) {
        let tmp = TempDir::new().unwrap();
        let data_root = DataRoot::new(tmp.path());
        data_root.ensure_directories().await.unwrap();
        let persistence = Arc::new(
            PersistenceLayer::init_with_root(data_root.clone()).await.unwrap()
        );
        let store = Arc::new(TasklistStore::new(data_root.clone()));
        let feeder = Arc::new(TaskFeeder::new(
            Arc::clone(&store),
            Arc::new(NoopDispatcher),
        ));
        let event_bus = Arc::new(EventBus::new(256));
        let svc = Arc::new(TasklistService::new(
            Arc::clone(&persistence),
            Arc::clone(&feeder),
            Arc::clone(&event_bus),
        ));
        (tmp, data_root, store, svc)
    }

    fn make_task_with_status(id: &str, owner: &str, status: TaskStatus) -> Task {
        Task {
            id: id.to_string(),
            group_id: "g1".to_string(),
            prompt: format!("Task {id}"),
            owner_agent_id: owner.to_string(),
            status,
            expected_outputs: vec![],
            error_log: vec![],
            attempt_count: 0,
            comments: vec![],
            attachments: vec![],
            notification_parse_retry_count: 0,
            parse_failed: false,
            remind_me: None,
            assignment: None,
            classifier_token: 0,
            dispatch_token: 0,
        }
    }

    fn make_tasklist_with_tasks(id: &str, agent_id: &str, data_root: &DataRoot, tasks: Vec<Task>) -> Tasklist {
        let workspace = data_root.agent_tasklist_workspace_dir(agent_id, id);
        let transcripts = data_root.agent_tasklist_transcripts_dir(agent_id, id);
        Tasklist {
            id: id.to_string(),
            owner: TasklistOwner::Agent { agent_id: agent_id.to_string() },
            team_id: None,
            title: format!("TL {id}"),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: "g1".to_string(),
                mode: TaskGroupMode::Seq,
                tasks,
            }],
            workspace_dir: workspace.to_string_lossy().to_string(),
            transcripts_dir: transcripts.to_string_lossy().to_string(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            }
    }

    /// Regression: after cancelling an agent-owned tasklist, an InProgress task
    /// must transition to Skipped — previously it stayed InProgress (zombie).
    #[tokio::test]
    async fn cancel_agent_tasklist_skips_in_progress_tasks() {
        let (_tmp, data_root, store, svc) = setup().await;
        let agent_id = "cp-cancel-agent";

        let t1 = make_task_with_status("t1", agent_id, TaskStatus::InProgress);
        let t2 = make_task_with_status("t2", agent_id, TaskStatus::Pending);
        let tl = make_tasklist_with_tasks("tl-cp-cancel", agent_id, &data_root, vec![t1, t2]);
        store.create_for_agent(&tl).await.unwrap();

        let result = svc.cancel_for_agent(agent_id).await;
        assert!(result.is_ok(), "cancel_for_agent must succeed: {:?}", result);

        let updated = store.get_for_agent(agent_id, "tl-cp-cancel").await.unwrap().unwrap();
        assert_eq!(updated.status, TasklistStatus::Cancelled, "tasklist must be Cancelled");

        let t1_status = updated.groups[0].tasks.iter().find(|t| t.id == "t1").unwrap().status;
        let t2_status = updated.groups[0].tasks.iter().find(|t| t.id == "t2").unwrap().status;
        assert_eq!(t1_status, TaskStatus::Skipped, "InProgress task must become Skipped");
        assert_eq!(t2_status, TaskStatus::Skipped, "Pending task must become Skipped");
    }

    /// Extended setup that also returns the `PersistenceLayer` so tests can
    /// register agent profiles before calling service methods that look them up.
    async fn setup_with_persistence() -> (TempDir, DataRoot, Arc<TasklistStore>, Arc<TasklistService>, Arc<PersistenceLayer>) {
        let tmp = TempDir::new().unwrap();
        let data_root = DataRoot::new(tmp.path());
        data_root.ensure_directories().await.unwrap();
        let persistence = Arc::new(
            PersistenceLayer::init_with_root(data_root.clone()).await.unwrap()
        );
        let store = Arc::new(TasklistStore::new(data_root.clone()));
        let feeder = Arc::new(TaskFeeder::new(Arc::clone(&store), Arc::new(NoopDispatcher)));
        let event_bus = Arc::new(EventBus::new(256));
        let svc = Arc::new(TasklistService::new(
            Arc::clone(&persistence),
            Arc::clone(&feeder),
            Arc::clone(&event_bus),
        ));
        (tmp, data_root, store, svc, persistence)
    }

    fn make_minimal_agent(agent_id: &str) -> ao_protocol::agent::AgentProfile {
        use ao_protocol::agent::*;
        AgentProfile {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: String::new(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: Default::default(),
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
            env: Default::default(),
            max_instances: 1,
            timeout_seconds: 60,
            working_dir: None,
            home_dir: None,
            serialize: false,
            workflows: None,
            template: None,
            runner_mode: AgentRunnerMode::Cli,
            enabled_plugins: Default::default(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
            max_turns: None,
        }
    }

    /// `create_for_agent_with_project(None)` behaves identically to
    /// `create_for_agent` — no project_id is set.
    #[tokio::test]
    async fn create_for_agent_with_project_none_is_identical_to_plain() {
        let (_tmp, _data_root, _store, svc, persistence) = setup_with_persistence().await;
        let agent_id = "cp-proj-none";
        persistence.agents.create(&make_minimal_agent(agent_id)).await.unwrap();
        let groups = vec![TaskGroup {
            id: "g1".to_string(),
            mode: TaskGroupMode::Seq,
            tasks: vec![make_task_with_status("t1", agent_id, TaskStatus::Pending)],
        }];
        let tl = svc
            .create_for_agent_with_project(agent_id, "plain".to_string(), groups, None, None)
            .await
            .expect("create must succeed");
        assert!(tl.project_id.is_none(), "project_id must be None when not supplied");
    }

    /// `create_for_agent_with_project(Some(pid))` stamps the tasklist
    /// atomically — the returned value and the on-disk record both carry `pid`.
    #[tokio::test]
    async fn create_for_agent_with_project_some_stamps_atomically() {
        let (_tmp, _data_root, store, svc, persistence) = setup_with_persistence().await;
        let agent_id = "cp-proj-some";
        persistence.agents.create(&make_minimal_agent(agent_id)).await.unwrap();
        let pid = "proj-abc-123".to_string();
        let groups = vec![TaskGroup {
            id: "g1".to_string(),
            mode: TaskGroupMode::Seq,
            tasks: vec![make_task_with_status("t1", agent_id, TaskStatus::Pending)],
        }];
        let tl = svc
            .create_for_agent_with_project(
                agent_id,
                "stamped".to_string(),
                groups,
                Some(pid.clone()),
                None,
            )
            .await
            .expect("create must succeed");

        // Returned value is already stamped.
        assert_eq!(tl.project_id.as_deref(), Some(pid.as_str()),
            "returned tasklist must carry project_id");

        // On-disk record is also stamped — no stale window.
        let persisted = store
            .get_for_agent(agent_id, &tl.id)
            .await
            .expect("read must succeed")
            .expect("tasklist must exist");
        assert_eq!(persisted.project_id.as_deref(), Some(pid.as_str()),
            "persisted tasklist must carry project_id");
    }

    /// Regression: stopping an active task must transition it from InProgress
    /// to Stopped so it does not stay as a zombie.
    #[tokio::test]
    async fn stop_task_transitions_in_progress_to_stopped() {
        let (_tmp, data_root, store, svc) = setup().await;
        let agent_id = "cp-stop-agent";

        // Persist a minimal agent profile so stop_task_for_agent can find the tasklist.
        let t1 = make_task_with_status("t1", agent_id, TaskStatus::InProgress);
        let t2 = make_task_with_status("t2", agent_id, TaskStatus::Pending);
        let tl = make_tasklist_with_tasks("tl-cp-stop", agent_id, &data_root, vec![t1, t2]);
        store.create_for_agent(&tl).await.unwrap();

        let result = svc.stop_task_for_agent(agent_id, "tl-cp-stop", "t1").await;
        assert!(result.is_ok(), "stop_task_for_agent must succeed: {:?}", result);

        let updated = store.get_for_agent(agent_id, "tl-cp-stop").await.unwrap().unwrap();
        assert_eq!(updated.status, TasklistStatus::Active, "tasklist must stay Active");

        let t1_status = updated.groups[0].tasks.iter().find(|t| t.id == "t1").unwrap().status;
        let t2_status = updated.groups[0].tasks.iter().find(|t| t.id == "t2").unwrap().status;
        assert_eq!(t1_status, TaskStatus::Stopped, "stopped task must be Stopped");
        assert_eq!(t2_status, TaskStatus::Pending, "other task must stay Pending");
    }
}

#[cfg(test)]
mod add_group_fan_out {
    //! Regression tests: add_group_for_agent must fan out TasklistTaskAdded onto
    //! the agent channel AND the project channel (when project-scoped), not only
    //! the tasklist channel. Verifies the fix for "appended tasks don't appear
    //! live in the chat Todos panel / header indicator without re-hydration".

    use super::*;
    use std::sync::Arc;

    use ao_persistence::{paths::DataRoot, tasklist_store::TasklistStore, PersistenceLayer};
    use ao_protocol::event::AgentEventPayload;
    use ao_protocol::tasklist::{Task, TaskGroup, TaskGroupMode, TasklistOwner, TasklistStatus};
    use chrono::Utc;
    use tempfile::TempDir;

    use crate::event_bus::EventBus;
    use crate::task_feeder::TaskFeeder;

    struct NoopDispatcher;
    #[async_trait::async_trait]
    impl crate::task_feeder::TaskDispatcher for NoopDispatcher {
        async fn dispatch_task(
            &self,
            _owner_agent_id: &ao_protocol::agent::AgentId,
            _prompt: String,
            _owner: &TasklistOwner,
            _tasklist_id: &ao_protocol::tasklist::TasklistId,
            _task_id: &ao_protocol::tasklist::TaskId,
        ) -> Result<(), AoError> {
            Ok(())
        }
    }

    async fn setup() -> (TempDir, DataRoot, Arc<TasklistStore>, Arc<TasklistService>, Arc<EventBus>) {
        let tmp = TempDir::new().unwrap();
        let data_root = DataRoot::new(tmp.path());
        data_root.ensure_directories().await.unwrap();
        let persistence = Arc::new(
            PersistenceLayer::init_with_root(data_root.clone()).await.unwrap()
        );
        let store = Arc::new(TasklistStore::new(data_root.clone()));
        let feeder = Arc::new(TaskFeeder::new(Arc::clone(&store), Arc::new(NoopDispatcher)));
        let event_bus = Arc::new(EventBus::new(256));
        let svc = Arc::new(TasklistService::new(
            Arc::clone(&persistence),
            Arc::clone(&feeder),
            Arc::clone(&event_bus),
        ));
        (tmp, data_root, store, svc, event_bus)
    }

    fn make_task(id: &str, agent_id: &str) -> Task {
        Task {
            id: id.to_string(),
            group_id: String::new(),
            prompt: format!("Task {id}"),
            owner_agent_id: agent_id.to_string(),
            status: ao_protocol::tasklist::TaskStatus::Pending,
            expected_outputs: vec![],
            error_log: vec![],
            attempt_count: 0,
            comments: vec![],
            attachments: vec![],
            notification_parse_retry_count: 0,
            parse_failed: false,
            remind_me: None,
            assignment: None,
            classifier_token: 0,
            dispatch_token: 0,
        }
    }

    fn make_tasklist(id: &str, agent_id: &str, data_root: &DataRoot, project_id: Option<String>) -> Tasklist {
        let workspace = data_root.agent_tasklist_workspace_dir(agent_id, id);
        let transcripts = data_root.agent_tasklist_transcripts_dir(agent_id, id);
        Tasklist {
            id: id.to_string(),
            owner: TasklistOwner::Agent { agent_id: agent_id.to_string() },
            team_id: None,
            title: format!("TL {id}"),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: "g0".to_string(),
                mode: TaskGroupMode::Seq,
                tasks: vec![make_task("t0", agent_id)],
            }],
            workspace_dir: workspace.to_string_lossy().to_string(),
            transcripts_dir: transcripts.to_string_lossy().to_string(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id,
            thread_id: None,
            }
    }

    fn drain_task_added(
        rx: &mut tokio::sync::broadcast::Receiver<ao_protocol::event::AgentEvent>,
    ) -> Vec<ao_protocol::event::AgentEvent> {
        let mut events = vec![];
        loop {
            match rx.try_recv() {
                Ok(e) if matches!(e.payload, AgentEventPayload::TasklistTaskAdded { .. }) => {
                    events.push(e);
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        events
    }

    /// Appending to a personal (no project_id) tasklist must deliver
    /// TasklistTaskAdded on both the tasklist channel and the agent channel.
    /// The agent-channel event must carry project_id = None so useSSE.ts
    /// processes it and updates agentTasklistStore + the header indicator.
    #[tokio::test]
    async fn personal_tasklist_emits_on_agent_channel() {
        let (_tmp, data_root, store, svc, bus) = setup().await;
        let agent_id = "fan-out-personal-agent";
        let tl_id = "fan-out-personal-tl";

        let tl = make_tasklist(tl_id, agent_id, &data_root, None);
        store.create_for_agent(&tl).await.unwrap();

        let mut rx = bus.subscribe();

        svc.add_group_for_agent(agent_id, tl_id, vec![make_task("t1", agent_id)], TaskGroupMode::Seq)
            .await
            .unwrap();

        let events = drain_task_added(&mut rx);

        // Agent channel (bare agent_id) must have received the event.
        let on_agent: Vec<_> = events.iter().filter(|e| e.agent_id == agent_id).collect();
        assert!(!on_agent.is_empty(), "must emit TasklistTaskAdded on agent channel");

        // Agent-channel events must carry project_id = None (personal tasklist).
        for e in &on_agent {
            if let AgentEventPayload::TasklistTaskAdded { project_id, .. } = &e.payload {
                assert!(project_id.is_none(), "personal tasklist event must have project_id=None on agent channel");
            }
        }

        // Tasklist channel must also receive the event.
        let tasklist_ch = format!("tasklist:{}", tl_id);
        let on_tasklist: Vec<_> = events.iter().filter(|e| e.agent_id == tasklist_ch).collect();
        assert!(!on_tasklist.is_empty(), "must emit TasklistTaskAdded on tasklist channel");
    }

    /// Appending to a project-scoped tasklist must deliver TasklistTaskAdded on
    /// the project channel AND on the agent channel. Every emitted event must
    /// carry the project_id so the agent-channel handler skips it (no leak into
    /// per-agent chat). The tasklist channel still gets the event for the open
    /// TodoPanel's run-SSE.
    #[tokio::test]
    async fn project_tasklist_emits_on_project_and_agent_channels() {
        let (_tmp, data_root, store, svc, bus) = setup().await;
        let agent_id = "fan-out-proj-agent";
        let tl_id = "fan-out-proj-tl";
        let pid = "proj-test-123".to_string();

        let tl = make_tasklist(tl_id, agent_id, &data_root, Some(pid.clone()));
        store.create_for_agent(&tl).await.unwrap();

        let mut rx = bus.subscribe();

        svc.add_group_for_agent(agent_id, tl_id, vec![make_task("t1", agent_id)], TaskGroupMode::Seq)
            .await
            .unwrap();

        let events = drain_task_added(&mut rx);

        let project_ch = format!("project:{}", pid);

        // Project channel must have received the event.
        let on_project: Vec<_> = events.iter().filter(|e| e.agent_id == project_ch).collect();
        assert!(!on_project.is_empty(), "must emit TasklistTaskAdded on project channel");

        // Agent channel must have received the event.
        let on_agent: Vec<_> = events.iter().filter(|e| e.agent_id == agent_id).collect();
        assert!(!on_agent.is_empty(), "must emit TasklistTaskAdded on agent channel");

        // Every emitted TasklistTaskAdded must carry project_id = Some(pid).
        for e in &events {
            if let AgentEventPayload::TasklistTaskAdded { project_id, .. } = &e.payload {
                assert_eq!(
                    project_id.as_deref(),
                    Some(pid.as_str()),
                    "all TasklistTaskAdded events for project-scoped tasklist must carry project_id (channel: {})",
                    e.agent_id,
                );
            }
        }
    }
}
