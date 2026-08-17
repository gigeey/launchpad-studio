//! Unit tests for the CLI agent runner.
//!
//! Declared from the parent module as `#[cfg(test)] mod tests;` — this is
//! the same module as the inline `mod tests` block it replaces, so private
//! items of the parent remain in scope here via `use super::*`.

use super::*;
use crate::agent_runner::shared::{build_memory_block, build_workflow_block};
use ao_protocol::memory::{MemoryScope, MemorySource};
use chrono::Utc;

fn make_memory(content: &str) -> MemoryEntry {
    let now = Utc::now();
    MemoryEntry {
        id: "test-id".to_string(),
        content: content.to_string(),
        created_at: now,
        source: Some(MemorySource::Manual),
        scope: MemoryScope::Agent,
        scope_key: None,
        updated_at: now,
        deleted_at: None,
        confidence: 1.0,
        status: Default::default(),
        superseded_by: None,
        pinned: false,
        decay_score: 1.0,
    }
}

#[test]
fn test_build_memory_block_all_empty() {
    assert_eq!(build_memory_block(&[], &[], &[]), None);
}

#[test]
fn test_build_memory_block_agent_only() {
    let agent = vec![make_memory("pref1"), make_memory("pref2")];
    let result = build_memory_block(&agent, &[], &[]).unwrap();
    assert_eq!(result, "[Agent Memories]\n- pref1\n- pref2");
}

#[test]
fn test_build_memory_block_project_only() {
    let project = vec![make_memory("proj-decision1")];
    let result = build_memory_block(&[], &project, &[]).unwrap();
    assert_eq!(result, "[Project Memories]\n- proj-decision1");
}

#[test]
fn test_build_memory_block_global_only() {
    let global = vec![make_memory("global1")];
    let result = build_memory_block(&[], &[], &global).unwrap();
    assert_eq!(result, "[Global Memories]\n- global1");
}

#[test]
fn test_build_memory_block_all_scopes() {
    let agent = vec![make_memory("agent1")];
    let project = vec![make_memory("project1")];
    let global = vec![make_memory("global1")];
    let result = build_memory_block(&agent, &project, &global).unwrap();
    assert_eq!(
        result,
        "[Agent Memories]\n- agent1\n\n[Project Memories]\n- project1\n\n[Global Memories]\n- global1"
    );
}

#[test]
fn test_build_memory_block_agent_global() {
    let agent = vec![make_memory("agent1")];
    let global = vec![make_memory("global1")];
    let result = build_memory_block(&agent, &[], &global).unwrap();
    assert_eq!(
        result,
        "[Agent Memories]\n- agent1\n\n[Global Memories]\n- global1"
    );
}

#[test]
fn test_memory_injection_with_existing_system_prompt() {
    let agent_mems = vec![make_memory("user likes Rust")];
    let global_mems = vec![make_memory("project uses tokio")];
    let mem_block = build_memory_block(&agent_mems, &[], &global_mems).unwrap();

    let existing_prompt = "You are a helpful assistant.".to_string();
    let result = format!("{}\n\n{}", existing_prompt, mem_block);

    assert!(result.starts_with("You are a helpful assistant."));
    assert!(result.contains("[Agent Memories]"));
    assert!(result.contains("- user likes Rust"));
    assert!(result.contains("[Global Memories]"));
    assert!(result.contains("- project uses tokio"));
}

#[test]
fn test_memory_injection_without_system_prompt() {
    let agent_mems = vec![make_memory("user likes Rust")];
    let mem_block = build_memory_block(&agent_mems, &[], &[]).unwrap();

    // When no system prompt, the memory block becomes the system prompt
    assert_eq!(mem_block, "[Agent Memories]\n- user likes Rust");
}

#[test]
fn test_memory_save_instruction_uses_tools() {
    assert!(MEMORY_SAVE_INSTRUCTION.contains("MemoryWrite"));
    assert!(MEMORY_SAVE_INSTRUCTION.contains("MemoryList"));
    assert!(MEMORY_SAVE_INSTRUCTION.contains("MemoryDelete"));
    assert!(MEMORY_SAVE_INSTRUCTION.contains("MemoryEdit"));
}

#[test]
fn test_memory_save_instruction_contains_guidance() {
    // Should describe the three scopes
    assert!(MEMORY_SAVE_INSTRUCTION.contains("agent"));
    assert!(MEMORY_SAVE_INSTRUCTION.contains("project"));
    assert!(MEMORY_SAVE_INSTRUCTION.contains("global"));
    // Should tell agent what to save and what not to
    assert!(MEMORY_SAVE_INSTRUCTION.contains("user preferences"));
    assert!(MEMORY_SAVE_INSTRUCTION.contains("corrections"));
    assert!(MEMORY_SAVE_INSTRUCTION.contains("Do NOT save"));
    assert!(MEMORY_SAVE_INSTRUCTION.contains("transient task details"));
}

// === augment_prompt_with_attachments tests ===

fn make_attachment(id: &str, path: &str, mime: &str, atype: AttachmentType) -> Attachment {
    Attachment {
        id: id.to_string(),
        file_path: path.to_string(),
        mime_type: mime.to_string(),
        original_filename: "test.txt".to_string(),
        size_bytes: 100,
        attachment_type: atype,
    }
}

#[test]
fn test_augment_no_attachments() {
    let result = augment_prompt_with_attachments("hello", &[], None);
    assert_eq!(result, "hello");
}

#[test]
fn test_augment_image_attachment_default_template() {
    let attachments = vec![make_attachment(
        "a1",
        "/tmp/photo.png",
        "image/png",
        AttachmentType::Image,
    )];
    let result = augment_prompt_with_attachments("describe this", &attachments, None);
    assert!(result.contains("[Attached image: /tmp/photo.png]"));
    assert!(result.contains("Please view and analyze this image."));
    assert!(result.ends_with("describe this"));
}

#[test]
fn test_augment_folder_attachment_default_template() {
    let attachments = vec![make_attachment(
        "a1",
        "/tmp/project",
        "inode/directory",
        AttachmentType::Folder,
    )];
    let result = augment_prompt_with_attachments("work on this", &attachments, None);
    assert!(result.contains("[Attached folder: /tmp/project]"));
    assert!(result.contains("Please explore this directory"));
    assert!(result.ends_with("work on this"));
}

#[test]
fn test_augment_document_attachment_default_template() {
    let attachments = vec![make_attachment(
        "a1",
        "/tmp/readme.md",
        "text/markdown",
        AttachmentType::Document,
    )];
    let result = augment_prompt_with_attachments("read this", &attachments, None);
    assert!(result.contains("[Attached file (text/markdown): /tmp/readme.md]"));
    assert!(result.contains("Please read and analyze this file."));
    assert!(result.ends_with("read this"));
}

#[test]
fn test_augment_multiple_attachments() {
    let attachments = vec![
        make_attachment("a1", "/tmp/photo.png", "image/png", AttachmentType::Image),
        make_attachment("a2", "/tmp/doc.pdf", "application/pdf", AttachmentType::Document),
    ];
    let result = augment_prompt_with_attachments("analyze these", &attachments, None);
    assert!(result.contains("[Attached image: /tmp/photo.png]"));
    assert!(result.contains("[Attached file (application/pdf): /tmp/doc.pdf]"));
    assert!(result.ends_with("analyze these"));
}

#[test]
fn test_augment_with_custom_template() {
    let file_cap = FileCapability {
        supported: true,
        max_file_size_bytes: 10 * 1024 * 1024,
        max_attachments_per_message: 5,
        allowed_mime_types: vec![],
        image_mode: ImageMode::FileReference {
            instruction_template: "FILE: {path} (type: {mime_type})".to_string(),
        },
    };
    let attachments = vec![make_attachment(
        "a1",
        "/tmp/photo.png",
        "image/png",
        AttachmentType::Image,
    )];
    let result =
        augment_prompt_with_attachments("describe", &attachments, Some(&file_cap));
    assert!(result.contains("FILE: /tmp/photo.png (type: image/png)"));
    assert!(result.ends_with("describe"));
}

#[test]
fn test_augment_format_references_then_blank_line_then_prompt() {
    let attachments = vec![make_attachment(
        "a1",
        "/tmp/photo.png",
        "image/png",
        AttachmentType::Image,
    )];
    let result = augment_prompt_with_attachments("hello", &attachments, None);
    // Format: references\n\nprompt
    assert!(result.contains("\n\nhello"));
}

// === build_workflow_block tests ===

fn make_summary(id: &str, name: &str, desc: &str) -> WorkflowSummary {
    WorkflowSummary {
        id: id.to_string(),
        name: name.to_string(),
        version: Some("1.0".to_string()),
        description: Some(desc.to_string()),
        phase_count: 3,
        source: Default::default(),
        updated_on: None,
        last_run: None,
    }
}

fn make_definition(id: &str, name: &str) -> WorkflowDefinition {
    use ao_protocol::workflow::{PhaseDefinition, PhaseInput, PhaseOutput};
    WorkflowDefinition {
        id: id.to_string(),
        name: name.to_string(),
        version: Some("1.0".to_string()),
        description: Some("A test workflow".to_string()),
        phases: vec![
            PhaseDefinition {
                id: "phase-1".to_string(),
                name: "Research".to_string(),
                intent: Some("Gather information".to_string()),
                path: "phases/research/prompt.md".to_string(),
                phase_type: None,
                auto_advance: true,
                schema: None,
                inputs: vec![],
                outputs: vec![PhaseOutput {
                    id: "research_output".to_string(),
                    filename: Some("research.json".to_string()),
                    description: Some("Research results".to_string()),
                }],
                fields: vec![],
            },
            PhaseDefinition {
                id: "phase-2".to_string(),
                name: "Implement".to_string(),
                intent: Some("Write the code".to_string()),
                path: "phases/implement/prompt.md".to_string(),
                phase_type: None,
                auto_advance: true,
                schema: None,
                inputs: vec![PhaseInput {
                    id: "research_data".to_string(),
                    from_phase: Some("phase-1".to_string()),
                    from_output: Some("research_output".to_string()),
                }],
                outputs: vec![],
                fields: vec![],
            },
        ],
    }
}

#[test]
fn test_build_workflow_block_generalist_all() {
    let s1 = make_summary("wf-1", "Loop", "Run a loop");
    let s2 = make_summary("wf-2", "Build", "Build things");
    let summaries: Vec<&WorkflowSummary> = vec![&s1, &s2];

    let binding = Some(WorkflowBinding::All);
    let result = build_workflow_block(&binding, &summaries, &[]).unwrap();

    assert!(result.contains("## Available Workflows"));
    assert!(result.contains("| wf-1 | Loop | Run a loop |"));
    assert!(result.contains("| wf-2 | Build | Build things |"));
}

#[test]
fn test_build_workflow_block_specialist_list() {
    let def = make_definition("wf-1", "Loop");
    let definitions: Vec<&WorkflowDefinition> = vec![&def];

    let binding = Some(WorkflowBinding::List(vec!["wf-1".to_string()]));
    let result = build_workflow_block(&binding, &[], &definitions).unwrap();

    assert!(result.contains("## Workflows"));
    assert!(result.contains("### Loop (wf-1)"));
    assert!(result.contains("**Research** (`phase-1`)"));
    assert!(result.contains("Gather information"));
    assert!(result.contains("**Implement** (`phase-2`)"));
    assert!(result.contains("research_data"));
    assert!(result.contains("research_output"));
}

#[test]
fn test_build_workflow_block_none_binding() {
    let binding = Some(WorkflowBinding::None);
    assert!(build_workflow_block(&binding, &[], &[]).is_none());
}

#[test]
fn test_build_workflow_block_absent_binding() {
    let binding: Option<WorkflowBinding> = None;
    assert!(build_workflow_block(&binding, &[], &[]).is_none());
}

#[test]
fn test_build_workflow_block_all_empty_summaries() {
    let binding = Some(WorkflowBinding::All);
    assert!(build_workflow_block(&binding, &[], &[]).is_none());
}

#[test]
fn test_build_workflow_block_list_empty_ids() {
    let binding = Some(WorkflowBinding::List(vec![]));
    assert!(build_workflow_block(&binding, &[], &[]).is_none());
}

#[test]
fn test_workflow_actions_not_injected_for_none_binding() {
    // None binding should produce no block at all (no actions instruction)
    let binding_none = Some(WorkflowBinding::None);
    assert!(build_workflow_block(&binding_none, &[], &[]).is_none());

    let binding_absent: Option<WorkflowBinding> = None;
    assert!(build_workflow_block(&binding_absent, &[], &[]).is_none());
}

// === <task> / <tasklist> tag dispatch tests ===

mod tasklist_tag_dispatch {
    use super::*;
    use crate::task_feeder::{TaskDispatcher, TaskFeeder};
    use crate::tasklist_extraction::{
        TaskTagAction, TasklistGroupSpec, TasklistTagAction, TasklistTaskSpec,
    };
    use ao_persistence::tasklist_store::TasklistStore;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::agent::{AgentProfile, ProviderConfig};
    use ao_protocol::tasklist::{
        Task, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistStatus,
    };
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct NoopDispatcher {
        calls: Mutex<Vec<(String, String)>>,
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
            owner_agent_id: &String,
            _prompt: String,
            _owner: &ao_protocol::tasklist::TasklistOwner,
            _tasklist_id: &String,
            task_id: &String,
        ) -> Result<(), AoError> {
            self.calls
                .lock()
                .unwrap()
                .push((owner_agent_id.clone(), task_id.clone()));
            Ok(())
        }
    }

    // Each test must hold a unique tmp data dir to isolate persistence.
    // We mutate the process-wide LAUNCHPAD_STUDIO_DATA_DIR, so every test
    // serializes through the single crate-wide env lock for its full
    // duration — a per-module mutex would let parallel tests in other
    // modules stomp this one's temp root.
    use crate::plugin_paths::tests::ENV_LOCK as ENV_MUTEX;
    static TEST_AGENT_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn make_agent_profile(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("agent {id}"),
            description: "test".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(ao_protocol::agent::CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: ao_protocol::agent::OutputFormat::Text,
                input_mode: ao_protocol::agent::InputMode::Arg,
                model_arg: None,
                model_aliases: std::collections::HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: std::collections::HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
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

    async fn setup() -> (
        Arc<CliAgentRunner>,
        Arc<PersistenceLayer>,
        Arc<NoopDispatcher>,
        tempfile::TempDir,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("temp dir");
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let persistence = Arc::new(PersistenceLayer::init().await.expect("persistence init"));
        let event_bus = Arc::new(EventBus::new(64));
        let supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(MockProcessSupervisor::new(vec![]));
        let normalizer_registry = Arc::new(NormalizerRegistry::new());
        let command_queue = Arc::new(CommandQueue::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let running_agents = Arc::new(crate::agent_runner::RunningAgents::new());
        let runner = Arc::new(CliAgentRunner::new(
            supervisor,
            normalizer_registry,
            event_bus,
            Arc::clone(&persistence),
            command_queue,
            instance_registry,
            running_agents,
            Arc::new(Registry::new()),
        ));
        let dispatcher = NoopDispatcher::new();
        let feeder = Arc::new(TaskFeeder::new(
            Arc::new(TasklistStore::new(persistence.data_root.clone())),
            dispatcher.clone(),
        ));
        runner.set_task_feeder(feeder);
        (runner, persistence, dispatcher, tmp, guard)
    }

    async fn seed_active_tasklist(
        persistence: &PersistenceLayer,
        team_id: &str,
        owner_agent_id: &str,
    ) -> (Tasklist, String) {
        let tl_id = format!(
            "tl-{}",
            TEST_AGENT_COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let group_id = "g1".to_string();
        let task_id = "task-1".to_string();
        let tasklist = Tasklist {
            id: tl_id.clone(),
            owner: ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() },
            team_id: Some(team_id.to_string()),
            title: "t".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: group_id.clone(),
                mode: TaskGroupMode::Seq,
                tasks: vec![Task {
                    id: task_id.clone(),
                    owner_agent_id: owner_agent_id.to_string(),
                    prompt: "do work".to_string(),
                    expected_outputs: vec![],
                    status: TaskStatus::InProgress,
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
                .tasklist_workspace_dir(team_id, &tl_id)
                .to_string_lossy()
                .to_string(),
            transcripts_dir: persistence
                .data_root
                .tasklist_transcripts_dir(team_id, &tl_id)
                .to_string_lossy()
                .to_string(),
            created_at: chrono::Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        persistence
            .tasklists
            .create(&tasklist)
            .await
            .expect("seed tasklist");
        (tasklist, task_id)
    }

    #[tokio::test]
    async fn complete_transitions_task_to_completed() {
        let (runner, persistence, _dispatcher, _tmp, _guard) = setup().await;
        let team_id = "team-a";
        persistence
            .agents
            .create(&make_agent_profile("coord-a"))
            .await
            .expect("create coord");
        let (tl, task_id) = seed_active_tasklist(&persistence, team_id, "coord-a").await;

        let result = process_task_tag_action(
            &runner,
            &tl.owner,
            &tl.id,
            &task_id,
            "coord-a",
            &TaskTagAction::Complete {
                task_id: task_id.clone(),
            },
        )
        .await
        .expect("complete should succeed");
        assert!(result.is_none());

        let updated = persistence
            .tasklists
            .get(tl.team_id.as_deref().unwrap_or_default(), &tl.id)
            .await
            .unwrap()
            .unwrap();
        let t = updated
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == task_id)
            .unwrap();
        assert_eq!(t.status, TaskStatus::Completed);
        assert!(t.error_log.is_empty());
    }

    #[tokio::test]
    async fn fail_transitions_to_failed_and_logs_reason() {
        let (runner, persistence, _dispatcher, _tmp, _guard) = setup().await;
        let team_id = "team-b";
        persistence
            .agents
            .create(&make_agent_profile("coord-b"))
            .await
            .expect("create coord");
        let (tl, task_id) = seed_active_tasklist(&persistence, team_id, "coord-b").await;

        process_task_tag_action(
            &runner,
            &tl.owner,
            &tl.id,
            &task_id,
            "coord-b",
            &TaskTagAction::Fail {
                task_id: task_id.clone(),
                reason: "missing input file".to_string(),
            },
        )
        .await
        .expect("fail should succeed");

        let updated = persistence
            .tasklists
            .get(tl.team_id.as_deref().unwrap_or_default(), &tl.id)
            .await
            .unwrap()
            .unwrap();
        let t = updated
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == task_id)
            .unwrap();
        assert_eq!(t.status, TaskStatus::Failed);
        assert_eq!(t.error_log, vec!["missing input file".to_string()]);
    }

    #[tokio::test]
    async fn mismatched_task_id_is_validation_error() {
        let (runner, persistence, _dispatcher, _tmp, _guard) = setup().await;
        let team_id = "team-c";
        persistence
            .agents
            .create(&make_agent_profile("coord-c"))
            .await
            .expect("create coord");
        let (tl, assigned_task_id) =
            seed_active_tasklist(&persistence, team_id, "coord-c").await;

        let err = process_task_tag_action(
            &runner,
            &tl.owner,
            &tl.id,
            &assigned_task_id,
            "coord-c",
            &TaskTagAction::Complete {
                task_id: "some-other-task".to_string(),
            },
        )
        .await
        .expect_err("mismatched task_id must be rejected");
        assert!(matches!(err, AoError::ValidationError(_)));

        // State unchanged.
        let updated = persistence
            .tasklists
            .get(tl.team_id.as_deref().unwrap_or_default(), &tl.id)
            .await
            .unwrap()
            .unwrap();
        let t = updated
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == assigned_task_id)
            .unwrap();
        assert_eq!(t.status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn request_clarification_logs_no_state_change() {
        let (runner, persistence, _dispatcher, _tmp, _guard) = setup().await;
        let team_id = "team-d";
        persistence
            .agents
            .create(&make_agent_profile("coord-d"))
            .await
            .expect("create coord");
        let (tl, task_id) = seed_active_tasklist(&persistence, team_id, "coord-d").await;

        let followup = process_task_tag_action(
            &runner,
            &tl.owner,
            &tl.id,
            &task_id,
            "coord-d",
            &TaskTagAction::RequestClarification {
                task_id: task_id.clone(),
                question: "which file?".to_string(),
            },
        )
        .await
        .expect("clarification should succeed");
        let f = followup.expect("clarification returns a followup note");
        assert!(f.context.contains("which file?"));

        let updated = persistence
            .tasklists
            .get(tl.team_id.as_deref().unwrap_or_default(), &tl.id)
            .await
            .unwrap()
            .unwrap();
        let t = updated
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == task_id)
            .unwrap();
        assert_eq!(t.status, TaskStatus::InProgress);
    }

    /// `<tasklist action="create">` was team-scoped. Teams were removed, so
    /// the action can no longer succeed for any agent; it must reject
    /// explicitly (not error, not silently no-op) and create nothing.
    #[tokio::test]
    async fn tasklist_create_is_rejected_now_that_teams_are_gone() {
        let (runner, persistence, dispatcher, _tmp, _guard) = setup().await;

        let action = TasklistTagAction::Create {
            team: "any-team".to_string(),
            title: "Investigate".to_string(),
            description: None,
            groups: vec![TasklistGroupSpec {
                mode: TaskGroupMode::Par,
                tasks: vec![TasklistTaskSpec {
                    owner_agent_id: "someone".to_string(),
                    prompt: "research".to_string(),
                    expected_outputs: vec![],
                }],
            }],
        };

        let followup = process_tasklist_tag_action(&runner, "any-agent", action)
            .await
            .expect("rejection returns Ok with a follow-up, not Err");
        let f = followup.expect("expected a rejection follow-up note");
        assert!(
            f.context.contains("no longer supported"),
            "rejection must say why, got: {}",
            f.context
        );

        // Nothing was created and the feeder was never started.
        assert!(persistence
            .tasklists
            .find_active("any-team")
            .await
            .unwrap()
            .is_none());
        assert!(dispatcher.calls.lock().unwrap().is_empty());
    }

    /// Seed an Active, agent-owned tasklist with no groups yet and a co-pilot
    /// bound to it — the shape the live project co-pilot route produces
    /// (`routes/projects.rs` binds via `mutate_by_owner` on a
    /// `TasklistOwner::Agent`).
    async fn seed_agent_owned_tasklist_with_copilot(
        persistence: &PersistenceLayer,
        owner_agent_id: &str,
        copilot_agent_id: &str,
        project_id: Option<&str>,
    ) -> Tasklist {
        let tl_id = format!("tl-{}", TEST_AGENT_COUNTER.fetch_add(1, Ordering::SeqCst));
        let tasklist = Tasklist {
            id: tl_id.clone(),
            owner: ao_protocol::tasklist::TasklistOwner::Agent {
                agent_id: owner_agent_id.to_string(),
            },
            team_id: None,
            title: "project work".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![],
            workspace_dir: persistence
                .data_root
                .agent_tasklist_workspace_dir(owner_agent_id, &tl_id)
                .to_string_lossy()
                .to_string(),
            transcripts_dir: persistence
                .data_root
                .agent_tasklist_transcripts_dir(owner_agent_id, &tl_id)
                .to_string_lossy()
                .to_string(),
            created_at: chrono::Utc::now(),
            last_active_at: None,
            copilot_agent_id: Some(copilot_agent_id.to_string()),
            last_opened_at: None,
            project_id: project_id.map(|s| s.to_string()),
            thread_id: None,
        };
        persistence
            .tasklists
            .create_for_agent(&tasklist)
            .await
            .expect("seed agent-owned tasklist");
        tasklist
    }

    /// End-to-end reachability for `<tasklist action="append">` on the
    /// ownership the live project co-pilot actually binds.
    ///
    /// This is the assertion the feature was missing: the tag handler resolves
    /// the binding, writes through the agent-keyed store, and the appended
    /// tasks reach the dispatcher. Before the reverse lookup walked the agent
    /// tree this returned "not bound to a tasklist as its co-pilot" and wrote
    /// nothing at all.
    ///
    /// The dispatch assertion is load-bearing beyond "the write landed":
    /// agent-owned tasklists dispatch on `task.assignment`, so if the handler
    /// left it `None` the feeder would emit `awaiting_classification` and hand
    /// the task to the routing queue — which is not wired in this harness, so
    /// the task would sit Pending and `calls` would be empty.
    #[tokio::test]
    async fn append_lands_on_an_agent_owned_tasklist_and_dispatches() {
        let (runner, persistence, dispatcher, _tmp, _guard) = setup().await;
        for id in ["proj-owner", "copilot-p", "worker-1", "worker-2"] {
            persistence
                .agents
                .create(&make_agent_profile(id))
                .await
                .expect("create agent");
        }
        let tl = seed_agent_owned_tasklist_with_copilot(
            &persistence,
            "proj-owner",
            "copilot-p",
            Some("project-42"),
        )
        .await;

        let action = TasklistTagAction::Append {
            groups: vec![TasklistGroupSpec {
                mode: TaskGroupMode::Par,
                tasks: vec![
                    TasklistTaskSpec {
                        owner_agent_id: "worker-1".to_string(),
                        prompt: "echo one".to_string(),
                        expected_outputs: vec![],
                    },
                    TasklistTaskSpec {
                        owner_agent_id: "worker-2".to_string(),
                        prompt: "echo two".to_string(),
                        expected_outputs: vec![],
                    },
                ],
            }],
        };

        let followup = process_tasklist_tag_action(&runner, "copilot-p", action)
            .await
            .expect("append should succeed");
        let f = followup.expect("append returns a follow-up note");
        assert!(
            f.context.contains("Appended 2 tasks"),
            "expected an append confirmation, got: {}",
            f.context
        );

        // The write landed in the AGENT tree, not the team tree.
        let updated = persistence
            .tasklists
            .get_for_agent("proj-owner", &tl.id)
            .await
            .unwrap()
            .expect("agent-owned tasklist still readable");
        assert_eq!(updated.groups.len(), 1, "one new group appended");
        let tasks = &updated.groups[0].tasks;
        assert_eq!(tasks.len(), 2);

        for (task, expected_owner) in tasks.iter().zip(["worker-1", "worker-2"]) {
            assert_eq!(task.owner_agent_id, expected_owner);
            // Pinned to the agent the co-pilot named, so the classifier can
            // never reassign it.
            let assignment = task
                .assignment
                .as_ref()
                .unwrap_or_else(|| panic!("task {} must carry an assignment", task.id));
            assert_eq!(assignment.owner_agent_id, expected_owner);
            assert_eq!(
                assignment.mode,
                ao_protocol::tasklist::AssignmentMode::Pinned
            );
            // The appending co-pilot is woken when each task completes.
            assert_eq!(task.remind_me.as_deref(), Some("copilot-p"));
        }

        // ...and the feeder actually dispatched them.
        let calls = dispatcher.calls.lock().unwrap().clone();
        let dispatched: std::collections::HashSet<&str> =
            calls.iter().map(|(agent, _)| agent.as_str()).collect();
        assert!(
            dispatched.contains("worker-1") && dispatched.contains("worker-2"),
            "both appended tasks should reach the dispatcher, got: {calls:?}"
        );
    }

    /// The same tag against a team-owned binding still works — the agent-tree
    /// walk added to the reverse lookup must not shadow the team path.
    #[tokio::test]
    async fn append_still_lands_on_a_team_owned_tasklist() {
        let (runner, persistence, dispatcher, _tmp, _guard) = setup().await;
        for id in ["worker-t", "copilot-t"] {
            persistence
                .agents
                .create(&make_agent_profile(id))
                .await
                .expect("create agent");
        }
        let (tl, _task_id) = seed_active_tasklist(&persistence, "team-append", "worker-t").await;
        persistence
            .tasklists
            .mutate("team-append", &tl.id, |t| {
                t.copilot_agent_id = Some("copilot-t".to_string());
                Ok(())
            })
            .await
            .expect("bind co-pilot");

        let action = TasklistTagAction::Append {
            groups: vec![TasklistGroupSpec {
                mode: TaskGroupMode::Seq,
                tasks: vec![TasklistTaskSpec {
                    owner_agent_id: "worker-t".to_string(),
                    prompt: "follow-up".to_string(),
                    expected_outputs: vec![],
                }],
            }],
        };

        let followup = process_tasklist_tag_action(&runner, "copilot-t", action)
            .await
            .expect("append should succeed");
        assert!(followup
            .expect("append returns a follow-up note")
            .context
            .contains("Appended 1 task"));

        let updated = persistence
            .tasklists
            .get("team-append", &tl.id)
            .await
            .unwrap()
            .expect("team tasklist readable");
        assert_eq!(updated.groups.len(), 2, "seeded group + appended group");
        assert_eq!(updated.groups[1].tasks[0].owner_agent_id, "worker-t");
        assert_eq!(
            updated.groups[1].tasks[0].remind_me.as_deref(),
            Some("copilot-t")
        );
        // Team-owned dispatch reads owner_agent_id, so the seeded InProgress
        // task still holds worker-t's slot — the assertion here is that the
        // append persisted, not that it dispatched.
        let _ = dispatcher;
    }

    /// An agent with no co-pilot binding is still rejected, and the rejection
    /// must not be mistaken for a missing tasklist tree.
    #[tokio::test]
    async fn append_without_a_binding_is_rejected() {
        let (runner, persistence, dispatcher, _tmp, _guard) = setup().await;
        persistence
            .agents
            .create(&make_agent_profile("worker-1"))
            .await
            .expect("create agent");

        let action = TasklistTagAction::Append {
            groups: vec![TasklistGroupSpec {
                mode: TaskGroupMode::Par,
                tasks: vec![TasklistTaskSpec {
                    owner_agent_id: "worker-1".to_string(),
                    prompt: "work".to_string(),
                    expected_outputs: vec![],
                }],
            }],
        };

        let followup = process_tasklist_tag_action(&runner, "unbound-agent", action)
            .await
            .expect("rejection returns Ok with a follow-up, not Err");
        assert!(followup
            .expect("expected a rejection follow-up")
            .context
            .contains("not bound to a tasklist"));
        assert!(dispatcher.calls.lock().unwrap().is_empty());
    }
}

// === <task-item-notification> parse-success path tests ===

mod task_item_notification_dispatch {
    use super::*;
    use crate::queue_manager::NotificationDispatcher;
    use crate::task_feeder::{TaskDispatcher, TaskFeeder};
    use crate::tasklist_extraction::TaskItemNotification;
    use ao_persistence::tasklist_store::TasklistStore;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::message::QueuedMessage;
    use ao_protocol::tasklist::{
        Task, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistStatus,
    };
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// In-memory `NotificationDispatcher` that records every dispatch
    /// call. Tests assert on the captured `(target_agent_id, message)`
    /// pairs to verify the `remind_me` routing without standing up the
    /// full QueueManagerRegistry pipeline.
    struct RecordingNotificationDispatcher {
        calls: Mutex<Vec<(String, QueuedMessage)>>,
    }
    impl RecordingNotificationDispatcher {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }
        fn calls(&self) -> Vec<(String, QueuedMessage)> {
            self.calls.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl NotificationDispatcher for RecordingNotificationDispatcher {
        async fn submit_to_agent(
            &self,
            target_agent_id: &str,
            message: QueuedMessage,
        ) -> Result<(), AoError> {
            self.calls
                .lock()
                .unwrap()
                .push((target_agent_id.to_string(), message));
            Ok(())
        }
    }

    // Each test mutates LAUNCHPAD_STUDIO_DATA_DIR, so we serialize through
    // the single crate-wide env lock (aliased here) for the whole test.
    use crate::plugin_paths::tests::ENV_LOCK as ENV_MUTEX;
    static TL_COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Build an `AgentRunner` with a real `PersistenceLayer` (rooted in a
    /// temp dir), a no-op TaskFeeder dispatcher, and a recording
    /// notification dispatcher. The recording handle is returned so the
    /// caller can assert on dispatched payloads.
    #[allow(clippy::type_complexity)]
    async fn setup() -> (
        Arc<CliAgentRunner>,
        Arc<PersistenceLayer>,
        Arc<RecordingNotificationDispatcher>,
        tempfile::TempDir,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let persistence = Arc::new(PersistenceLayer::init().await.expect("persistence init"));
        let event_bus = Arc::new(EventBus::new(64));
        let supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(MockProcessSupervisor::new(vec![]));
        let normalizer_registry = Arc::new(NormalizerRegistry::new());
        let command_queue = Arc::new(CommandQueue::new());
        let instance_registry = Arc::new(InstanceRegistry::new());
        let running_agents = Arc::new(crate::agent_runner::RunningAgents::new());
        let runner = Arc::new(CliAgentRunner::new(
            supervisor,
            normalizer_registry,
            event_bus,
            Arc::clone(&persistence),
            command_queue,
            instance_registry,
            running_agents,
            Arc::new(Registry::new()),
        ));

        // TaskFeeder is wired with a no-op dispatcher because the tests
        // don't drive any agent runs — they exercise
        // `record_task_item_changelog` / `dispatch_task_item_remind_me`
        // directly.
        struct NoopTaskDispatcher;
        #[async_trait]
        impl TaskDispatcher for NoopTaskDispatcher {
            async fn dispatch_task(
                &self,
                _owner_agent_id: &String,
                _prompt: String,
                _owner: &ao_protocol::tasklist::TasklistOwner,
                _tasklist_id: &String,
                _task_id: &String,
            ) -> Result<(), AoError> {
                Ok(())
            }
        }
        let feeder = Arc::new(TaskFeeder::new(
            Arc::new(TasklistStore::new(persistence.data_root.clone())),
            Arc::new(NoopTaskDispatcher) as Arc<dyn TaskDispatcher>,
        ));
        runner.set_task_feeder(feeder);

        let dispatcher = RecordingNotificationDispatcher::new();
        runner.set_notification_dispatcher(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
        );

        (runner, persistence, dispatcher, tmp, guard)
    }

    async fn seed_tasklist_with_remind_me(
        persistence: &PersistenceLayer,
        team_id: &str,
        owner_agent_id: &str,
        remind_me: Option<&str>,
    ) -> (String, String) {
        let tl_id = format!("tl-{}", TL_COUNTER.fetch_add(1, Ordering::SeqCst));
        let task_id = "task-1".to_string();
        let tasklist = Tasklist {
            id: tl_id.clone(),
            owner: ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() },
            team_id: Some(team_id.to_string()),
            title: "t".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: "g1".to_string(),
                mode: TaskGroupMode::Seq,
                tasks: vec![Task {
                    id: task_id.clone(),
                    owner_agent_id: owner_agent_id.to_string(),
                    prompt: "do work".to_string(),
                    expected_outputs: vec![],
                    status: TaskStatus::Completed,
                    group_id: "g1".to_string(),
                    attempt_count: 0,
                    error_log: vec![],
                    comments: vec![],
                    attachments: vec![],
                    remind_me: remind_me.map(|s| s.to_string()),
                    parse_failed: false,
                    notification_parse_retry_count: 0,
                    assignment: None,
                    classifier_token: 0,
                    dispatch_token: 0,
                }],
            }],
            workspace_dir: persistence
                .data_root
                .tasklist_workspace_dir(team_id, &tl_id)
                .to_string_lossy()
                .to_string(),
            transcripts_dir: persistence
                .data_root
                .tasklist_transcripts_dir(team_id, &tl_id)
                .to_string_lossy()
                .to_string(),
            created_at: chrono::Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        persistence
            .tasklists
            .create(&tasklist)
            .await
            .expect("seed tasklist");
        (tl_id, task_id)
    }

    /// `remind_me` set + valid notification → ChangelogEntry
    /// appended AND the formatted XML is dispatched to the named agent.
    #[tokio::test]
    async fn parse_success_with_remind_me_appends_changelog_and_dispatches() {
        let (runner, persistence, dispatcher, _tmp, _guard) = setup().await;
        let team_id = "team-9";
        let owner = "producer";
        let reminder = "reminder";
        let (tl_id, task_id) =
            seed_tasklist_with_remind_me(&persistence, team_id, owner, Some(reminder)).await;

        let notification = TaskItemNotification {
            status: "complete".to_string(),
            summary: "wrote analysis.md".to_string(),
            details: Some("checked three signals".to_string()),
        };

        let owner_team =
            ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() };
        record_task_item_changelog(&runner, &owner_team, &tl_id, &task_id, owner, &notification)
            .await;
        dispatch_task_item_remind_me(&runner, &owner_team, &tl_id, &task_id, &notification)
            .await;

        // Changelog persisted with the producer's ids and the
        // notification's status/summary/details fields verbatim.
        let recent = persistence
            .changelogs
            .read_recent(&owner_team, &tl_id, 100)
            .await
            .expect("read_recent");
        assert_eq!(recent.len(), 1);
        let entry = &recent[0];
        assert_eq!(entry.task_id, task_id);
        assert_eq!(entry.tasklist_id, tl_id);
        assert_eq!(entry.agent_id, owner);
        assert_eq!(entry.status, "complete");
        assert_eq!(entry.summary, "wrote analysis.md");
        assert_eq!(entry.details.as_deref(), Some("checked three signals"));

        // Dispatcher saw exactly one call addressed to the reminder, and
        // the payload round-trips back to the original TaskItemNotification.
        let calls = dispatcher.calls();
        assert_eq!(calls.len(), 1, "expected one dispatch to remind_me");
        let (target, msg) = &calls[0];
        assert_eq!(target, reminder);
        assert!(msg.content.contains("<task-item-notification>"));
        assert!(msg.content.contains("</task-item-notification>"));
        assert!(msg.content.contains("<status>complete</status>"));
        assert!(msg.content.contains("<summary>wrote analysis.md</summary>"));
        assert!(msg.content.contains("<details>checked three signals</details>"));
    }

    /// `remind_me` UNSET → ChangelogEntry still appended, but no
    /// dispatch call is made.
    #[tokio::test]
    async fn parse_success_without_remind_me_appends_changelog_no_dispatch() {
        let (runner, persistence, dispatcher, _tmp, _guard) = setup().await;
        let team_id = "team-9b";
        let owner = "producer-b";
        let (tl_id, task_id) =
            seed_tasklist_with_remind_me(&persistence, team_id, owner, None).await;

        let notification = TaskItemNotification {
            status: "complete".to_string(),
            summary: "no notify".to_string(),
            details: None,
        };

        let owner_team =
            ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() };
        record_task_item_changelog(&runner, &owner_team, &tl_id, &task_id, owner, &notification)
            .await;
        dispatch_task_item_remind_me(&runner, &owner_team, &tl_id, &task_id, &notification)
            .await;

        // Changelog appended (independent of remind_me).
        let recent = persistence
            .changelogs
            .read_recent(&owner_team, &tl_id, 100)
            .await
            .expect("read_recent");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].agent_id, owner);
        assert!(recent[0].details.is_none());

        // No mailbox dispatch since remind_me was None.
        assert!(
            dispatcher.calls().is_empty(),
            "no remind_me ⇒ no dispatch"
        );
    }

    /// Defensive: if the notification dispatcher is never bound (the
    /// `set_notification_dispatcher` call is skipped), the changelog is
    /// still written and the helper does NOT panic. Ensures the parse-
    /// success path is best-effort end-to-end.
    #[tokio::test]
    async fn parse_success_when_dispatcher_unbound_still_writes_changelog() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let persistence = Arc::new(PersistenceLayer::init().await.unwrap());
        let event_bus = Arc::new(EventBus::new(64));
        let supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(MockProcessSupervisor::new(vec![]));
        let running_agents = Arc::new(crate::agent_runner::RunningAgents::new());
        let runner = Arc::new(CliAgentRunner::new(
            supervisor,
            Arc::new(NormalizerRegistry::new()),
            event_bus,
            Arc::clone(&persistence),
            Arc::new(CommandQueue::new()),
            Arc::new(InstanceRegistry::new()),
            running_agents,
            Arc::new(Registry::new()),
        ));
        // Notification dispatcher INTENTIONALLY not set.
        let team_id = "team-9c";
        let owner = "producer-c";
        let (tl_id, task_id) =
            seed_tasklist_with_remind_me(&persistence, team_id, owner, Some("ghost")).await;

        let notification = TaskItemNotification {
            status: "failed".to_string(),
            summary: "boom".to_string(),
            details: None,
        };

        let owner_team =
            ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() };
        record_task_item_changelog(&runner, &owner_team, &tl_id, &task_id, owner, &notification)
            .await;
        dispatch_task_item_remind_me(&runner, &owner_team, &tl_id, &task_id, &notification)
            .await;

        let recent = persistence
            .changelogs
            .read_recent(&owner_team, &tl_id, 100)
            .await
            .expect("read_recent");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].status, "failed");
    }

    // ---------- parse-failure path ----------

    /// Seeds a tasklist whose single task is `InProgress` (so we can
    /// assert the parse-failure path leaves it non-terminal).
    async fn seed_active_tasklist_with_remind_me(
        persistence: &PersistenceLayer,
        team_id: &str,
        owner_agent_id: &str,
        remind_me: Option<&str>,
    ) -> (String, String) {
        let tl_id = format!("tl-{}", TL_COUNTER.fetch_add(1, Ordering::SeqCst));
        let task_id = "task-1".to_string();
        let tasklist = Tasklist {
            id: tl_id.clone(),
            owner: ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() },
            team_id: Some(team_id.to_string()),
            title: "t".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: "g1".to_string(),
                mode: TaskGroupMode::Seq,
                tasks: vec![Task {
                    id: task_id.clone(),
                    owner_agent_id: owner_agent_id.to_string(),
                    prompt: "do work".to_string(),
                    expected_outputs: vec![],
                    status: TaskStatus::InProgress,
                    group_id: "g1".to_string(),
                    attempt_count: 0,
                    error_log: vec![],
                    comments: vec![],
                    attachments: vec![],
                    remind_me: remind_me.map(|s| s.to_string()),
                    parse_failed: false,
                    notification_parse_retry_count: 0,
                    assignment: None,
                    classifier_token: 0,
                    dispatch_token: 0,
                }],
            }],
            workspace_dir: persistence
                .data_root
                .tasklist_workspace_dir(team_id, &tl_id)
                .to_string_lossy()
                .to_string(),
            transcripts_dir: persistence
                .data_root
                .tasklist_transcripts_dir(team_id, &tl_id)
                .to_string_lossy()
                .to_string(),
            created_at: chrono::Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            };
        persistence
            .tasklists
            .create(&tasklist)
            .await
            .expect("seed tasklist");
        (tl_id, task_id)
    }

    /// The whole parse-failure path in one scenario: a malformed
    /// notification triggers `handle_task_item_notification_parse_failure`
    /// which routes a re-prompt directly to the producing agent's
    /// mailbox, bumps the persisted retry counter to 1, leaves the
    /// task in its prior non-terminal status, and writes NO changelog
    /// entry. (Earlier versions routed to the team coordinator; that
    /// caused the coordinator to parrot the structured key/value body
    /// back instead of re-dispatching the worker, so the routing target
    /// is now the worker that actually emitted the bad message.)
    #[tokio::test]
    async fn parse_failure_routes_followup_to_producer_and_bumps_retry_counter() {
        let (runner, persistence, dispatcher, _tmp, _guard) = setup().await;
        let team_id = "team-pf";
        let coordinator_id = "coord-pf";
        let producer_id = "producer-pf";
        let (tl_id, task_id) =
            seed_active_tasklist_with_remind_me(&persistence, team_id, producer_id, None).await;

        let original_output =
            "<task action=\"complete\" id=\"task-1\"/>\n<task-item-notification>\nUNCLOSED";
        let reason = "the closing </task-item-notification> tag was not found";

        handle_task_item_notification_parse_failure(
            &runner,
            &ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() },
            &tl_id,
            &task_id,
            producer_id,
            original_output,
            reason,
            "complete",
        )
        .await;

        // Persisted retry counter == 1; task still non-terminal.
        let tl = persistence
            .tasklists
            .get(team_id, &tl_id)
            .await
            .expect("get tasklist")
            .expect("tasklist present");
        let task = &tl.groups[0].tasks[0];
        assert_eq!(task.id, task_id);
        assert_eq!(
            task.notification_parse_retry_count, 1,
            "expected retry counter to be bumped to 1"
        );
        assert_eq!(
            task.status,
            TaskStatus::InProgress,
            "task must remain non-terminal after parse failure"
        );
        assert!(!task.parse_failed, "parse_failed flips only after retry exhaustion");

        // NO changelog entry written on parse failure.
        let recent = persistence
            .changelogs
            .read_recent(&ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() }, &tl_id, 100)
            .await
            .expect("read_recent");
        assert!(
            recent.is_empty(),
            "no changelog entry should be written on parse failure"
        );

        // Followup routed DIRECTLY to the producing agent (the worker
        // that emitted the bad message), NOT the coordinator. Payload
        // contains the original agent output, the parse-failure reason,
        // and the corrected XML schema example.
        let calls = dispatcher.calls();
        assert_eq!(calls.len(), 1, "expected exactly one dispatch to producer");
        let (target, msg) = &calls[0];
        assert_eq!(
            target, producer_id,
            "followup must be addressed to the producing agent (the worker), not the coordinator"
        );
        assert_ne!(
            target, coordinator_id,
            "followup must NOT be routed through the coordinator anymore"
        );
        assert!(
            msg.content.contains(original_output),
            "followup must include the producing agent's original output"
        );
        assert!(
            msg.content.contains(reason),
            "followup must include the parse-failure reason"
        );
        assert!(
            msg.content.contains("<task-item-notification>"),
            "followup must reference the block tag the agent failed to emit"
        );
        assert!(
            msg.content.contains("<status>"),
            "followup must show the corrected XML schema with <status> tag"
        );
        assert!(
            msg.content.contains("<summary>"),
            "followup must show the corrected XML schema with <summary> tag"
        );
        assert!(
            msg.content.contains(&task_id),
            "followup must identify which task to re-emit the notification for"
        );
    }

    /// A second parse-failure for the same task increments the persisted
    /// counter to 2 — verifies the saturating add walks forward across
    /// repeated failures (the retry-exhaustion path caps it; this story
    /// just bumps).
    #[tokio::test]
    async fn parse_failure_retry_counter_increments_across_repeated_failures() {
        let (runner, persistence, dispatcher, _tmp, _guard) = setup().await;
        let team_id = "team-pf2";
        let producer_id = "producer-pf2";
        let (tl_id, task_id) =
            seed_active_tasklist_with_remind_me(&persistence, team_id, producer_id, None).await;

        for _ in 0..2 {
            handle_task_item_notification_parse_failure(
                &runner,
                &ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() },
                &tl_id,
                &task_id,
                producer_id,
                "<task action=\"complete\" id=\"task-1\"/>",
                "the block was not present in the message",
                "complete",
            )
            .await;
        }

        let tl = persistence
            .tasklists
            .get(team_id, &tl_id)
            .await
            .expect("get tasklist")
            .expect("tasklist present");
        assert_eq!(tl.groups[0].tasks[0].notification_parse_retry_count, 2);
        // Each failure routes one followup, so two failures ⇒ two dispatches.
        assert_eq!(dispatcher.calls().len(), 2);
    }

    /// The producer-routing path submits straight to the producing agent's
    /// mailbox, so it needs no team-level lookup at all. The counter still
    /// bumps and the dispatch still lands on the producer.
    #[tokio::test]
    async fn parse_failure_when_team_missing_still_dispatches_to_producer() {
        let (runner, persistence, dispatcher, _tmp, _guard) = setup().await;
        let team_id = "team-pf-missing";
        let producer_id = "producer-pf-missing";
        let (tl_id, task_id) =
            seed_active_tasklist_with_remind_me(&persistence, team_id, producer_id, None).await;

        handle_task_item_notification_parse_failure(
            &runner,
            &ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() },
            &tl_id,
            &task_id,
            producer_id,
            "raw output",
            "missing block",
            "complete",
        )
        .await;

        let tl = persistence
            .tasklists
            .get(team_id, &tl_id)
            .await
            .expect("get tasklist")
            .expect("tasklist present");
        assert_eq!(tl.groups[0].tasks[0].notification_parse_retry_count, 1);
        let calls = dispatcher.calls();
        assert_eq!(
            calls.len(),
            1,
            "missing team profile must NOT prevent the producer-routed re-prompt"
        );
        assert_eq!(
            calls[0].0, producer_id,
            "re-prompt must land on the producing agent"
        );
    }

    // ---------- retry-exhaustion path ----------

    /// The whole retry-exhaustion path in one scenario:
    /// 3 consecutive malformed notifications → the 3rd call lands in the
    /// retry-exhaustion path, which (a) appends a synthesized
    /// ChangelogEntry whose `status` mirrors the producing agent's
    /// completion tag, (b) flips `Task.parse_failed` to true, and (c)
    /// suppresses any QueuedMessage dispatch for that 3rd call — even
    /// when `task.remind_me` is set. The first two failures still route
    /// coordinator followups, so total dispatcher
    /// calls across the three attempts is exactly 2.
    #[tokio::test]
    async fn retry_exhaustion_synthesizes_entry_flips_parse_failed_and_suppresses_dispatch() {
        let (runner, persistence, dispatcher, _tmp, _guard) = setup().await;
        let team_id = "team-pf-exhaust";
        let coordinator_id = "coord-pf-exhaust";
        let producer_id = "producer-pf-exhaust";
        let reminder_id = "reminder-pf-exhaust";
        // remind_me is set so we can verify that no remind_me notification
        // is dispatched on the synthesized entry: even with remind_me
        // set, the 3rd attempt MUST NOT add a dispatcher call.
        let (tl_id, task_id) = seed_active_tasklist_with_remind_me(
            &persistence,
            team_id,
            producer_id,
            Some(reminder_id),
        )
        .await;

        for attempt in 1..=MAX_NOTIFICATION_PARSE_RETRIES {
            handle_task_item_notification_parse_failure(
                &runner,
                &ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() },
                &tl_id,
                &task_id,
                producer_id,
                &format!("<task action=\"complete\" id=\"task-1\"/>\n[attempt {attempt}]"),
                "the block was not present in the message",
                "complete",
            )
            .await;
        }

        // Counter reflects every attempt; parse_failed is set.
        let tl = persistence
            .tasklists
            .get(team_id, &tl_id)
            .await
            .expect("get tasklist")
            .expect("tasklist present");
        let task = &tl.groups[0].tasks[0];
        assert_eq!(
            task.notification_parse_retry_count, MAX_NOTIFICATION_PARSE_RETRIES,
            "all three failures must be counted"
        );
        assert!(
            task.parse_failed,
            "parse_failed must flip true on the cap-hitting attempt"
        );
        // The terminal task action was never processed, so the task
        // status remains whatever it was before the parse failures.
        assert_eq!(
            task.status,
            TaskStatus::InProgress,
            "task must remain non-terminal — synthesis is a give-up, not a completion"
        );

        // Exactly one synthesized ChangelogEntry exists, with status
        // from the completion tag and a clearly-flagged summary.
        let recent = persistence
            .changelogs
            .read_recent(&ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() }, &tl_id, 100)
            .await
            .expect("read_recent");
        assert_eq!(
            recent.len(),
            1,
            "exactly one synthesized entry on retry exhaustion"
        );
        let entry = &recent[0];
        assert_eq!(entry.task_id, task_id);
        assert_eq!(entry.tasklist_id, tl_id);
        assert_eq!(entry.agent_id, producer_id);
        assert_eq!(
            entry.status, "complete",
            "synthesized status must mirror the producing agent's completion tag"
        );
        assert!(
            entry.summary.contains("auto-synthesized"),
            "summary must clearly mark the entry as a parse-failure fallback"
        );
        // Details surfaces the parse-failure reason for downstream review.
        let details = entry
            .details
            .as_deref()
            .expect("synthesized entry must carry details");
        assert!(
            details.contains("the block was not present in the message"),
            "synthesized details must echo the parse-failure reason"
        );

        // Only the first two attempts dispatched re-prompts;
        // the cap-hitting third attempt MUST suppress both the producer
        // re-prompt AND any remind_me dispatch.
        let calls = dispatcher.calls();
        assert_eq!(
            calls.len(),
            (MAX_NOTIFICATION_PARSE_RETRIES - 1) as usize,
            "the exhausting attempt must NOT add a dispatcher call"
        );
        for (target, _) in &calls {
            assert_eq!(
                target, producer_id,
                "every pre-exhaustion re-prompt goes to the producing agent (worker)"
            );
            assert_ne!(
                target, coordinator_id,
                "re-prompts no longer route through the coordinator"
            );
            assert_ne!(
                target, reminder_id,
                "even with remind_me set, the exhaustion path emits no notification"
            );
        }
    }

    /// Status string round-trip: when the producing agent's terminal
    /// action is `<task action="fail">`, the synthesized fallback entry
    /// must carry `"failed"` (matching the wire format the parse-success
    /// path uses), not `"complete"` or the literal action name.
    #[tokio::test]
    async fn retry_exhaustion_synthesized_status_mirrors_fail_action() {
        let (runner, persistence, _dispatcher, _tmp, _guard) = setup().await;
        let team_id = "team-pf-exhaust-fail";
        let producer_id = "producer-pf-exhaust-fail";
        let (tl_id, task_id) =
            seed_active_tasklist_with_remind_me(&persistence, team_id, producer_id, None)
                .await;

        for _ in 0..MAX_NOTIFICATION_PARSE_RETRIES {
            handle_task_item_notification_parse_failure(
                &runner,
                &ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() },
                &tl_id,
                &task_id,
                producer_id,
                "<task action=\"fail\" id=\"task-1\" reason=\"x\"/>",
                "the block was not present in the message",
                "failed",
            )
            .await;
        }

        let recent = persistence
            .changelogs
            .read_recent(&ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() }, &tl_id, 100)
            .await
            .expect("read_recent");
        assert_eq!(recent.len(), 1);
        assert_eq!(
            recent[0].status, "failed",
            "synthesized status must mirror the Fail action variant"
        );
    }

    /// A single parse failure (count=1) must NOT trigger the exhaustion
    /// path: no synthesized ChangelogEntry, parse_failed stays false,
    /// and the coordinator followup still dispatches as before. Pins
    /// the cap so a future tweak that drops it to 1 doesn't silently
    /// regress the retry budget.
    #[tokio::test]
    async fn single_parse_failure_does_not_synthesize_or_flip_parse_failed() {
        let (runner, persistence, dispatcher, _tmp, _guard) = setup().await;
        let team_id = "team-pf-single";
        let producer_id = "producer-pf-single";
        let (tl_id, task_id) =
            seed_active_tasklist_with_remind_me(&persistence, team_id, producer_id, None)
                .await;

        handle_task_item_notification_parse_failure(
            &runner,
            &ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() },
            &tl_id,
            &task_id,
            producer_id,
            "<task action=\"complete\" id=\"task-1\"/>",
            "the block was not present in the message",
            "complete",
        )
        .await;

        let tl = persistence
            .tasklists
            .get(team_id, &tl_id)
            .await
            .expect("get tasklist")
            .expect("tasklist present");
        let task = &tl.groups[0].tasks[0];
        assert_eq!(task.notification_parse_retry_count, 1);
        assert!(
            !task.parse_failed,
            "parse_failed must stay false until the cap is hit"
        );

        let recent = persistence
            .changelogs
            .read_recent(&ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() }, &tl_id, 100)
            .await
            .expect("read_recent");
        assert!(
            recent.is_empty(),
            "no synthesized entry until the retry cap is hit"
        );
        assert_eq!(
            dispatcher.calls().len(),
            1,
            "below the cap, the coordinator followup still dispatches"
        );
    }
}

// === Fresh-thread reply routing (regression) ===
//
// Pins the contract that a Standalone run targeting a non-default thread
// persists the AGENT'S reply to that thread's own transcript file, not
// the agent-keyed personal transcript. The user's message already routed
// correctly (see `messages_and_stream::test_send_message_with_thread_id_writes_to_thread_file`
// at the HTTP-route level) — this test covers the runner's write side,
// where `transcript_path_override` previously only accounted for
// delegate overrides and tasklist scope, silently dropping `thread_id`
// and letting every reply on a fresh/branch thread fall through to the
// agent-keyed file.
mod fresh_thread_reply_routing {
    use super::*;
    use ao_normalizer::registry::NormalizerRegistry;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::{MockProcessSupervisor, MockScenario};
    use ao_protocol::agent::{AgentProfile, ProviderConfig};

    fn make_profile(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("agent {id}"),
            description: "test".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(ao_protocol::agent::CliProviderConfig {
                command: "claude-mock".to_string(),
                args: vec![],
                normalizer: None,
                output_format: ao_protocol::agent::OutputFormat::Text,
                input_mode: ao_protocol::agent::InputMode::Arg,
                model_arg: None,
                model_aliases: std::collections::HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: std::collections::HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
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

    #[tokio::test]
    async fn standalone_run_on_fresh_thread_writes_reply_to_thread_file() {
        let _guard = crate::plugin_paths::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("temp dir");
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let persistence = Arc::new(PersistenceLayer::init().await.expect("persistence init"));

        let supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(MockProcessSupervisor::new(vec![MockScenario {
                stdout_lines: vec!["REPLY_SENTINEL from the fresh thread".to_string()],
                stderr_lines: vec![],
                exit_code: 0,
                delay_per_line_ms: 0,
            }]));

        let runner = Arc::new(CliAgentRunner::new(
            supervisor,
            Arc::new(NormalizerRegistry::new()),
            Arc::new(EventBus::new(64)),
            Arc::clone(&persistence),
            Arc::new(CommandQueue::new()),
            Arc::new(InstanceRegistry::new()),
            Arc::new(crate::agent_runner::RunningAgents::new()),
            Arc::new(Registry::new()),
        ));

        let profile = make_profile("thread-reply-agent");
        persistence
            .agents
            .create(&profile)
            .await
            .expect("create agent");

        let thread = persistence
            .threads
            .build_fresh_thread(&profile.id, Some("alt".to_string()));
        let thread = persistence
            .threads
            .create(thread)
            .await
            .expect("create thread");

        let (tx, mut rx) = mpsc::channel(1);
        runner
            .run_with_scope_inner(
                &profile,
                "hello from the fresh thread",
                &[],
                tx,
                RunScope::Standalone,
                None,
                None,
                Some(thread.id.clone()),
                vec![],
                vec![],
                false,
                None,
                None,
                false,
            )
            .await
            .expect("run must start");

        tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("run must finish within timeout")
            .expect("completion must arrive");

        let thread_path = std::path::PathBuf::from(&thread.transcript_path);
        let in_thread = persistence
            .transcripts
            .read_all_at(&thread_path)
            .await
            .expect("read thread transcript");
        assert!(
            in_thread
                .iter()
                .any(|e| e.content.contains("REPLY_SENTINEL")),
            "agent reply should land in the fresh thread's transcript file; got: {:?}",
            in_thread
        );

        let in_agent = persistence
            .transcripts
            .read_all(&profile.id)
            .await
            .expect("read agent transcript");
        assert!(
            !in_agent.iter().any(|e| e.content.contains("REPLY_SENTINEL")),
            "agent reply must NOT leak into the agent-keyed personal transcript; got: {:?}",
            in_agent
        );
    }
}

// === `AgentRunRequest::bypass_instance_cap` (agent-watch max_instances bypass) ===
//
// Confirms `bypass_instance_cap` does what its doc comment promises for
// the CLI runner: a bypassed run never occupies the agent's
// `InstanceRegistry` slot (so it can never be blocked/queued behind, or
// itself block, the agent's own live turn), while a normal (default)
// request still registers exactly as before.
mod bypass_instance_cap_tests {
    use super::*;
    use ao_normalizer::registry::NormalizerRegistry;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::{MockProcessSupervisor, MockScenario};
    use ao_protocol::agent::{AgentProfile, ProviderConfig};

    fn make_profile(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("agent {id}"),
            description: "test".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(ao_protocol::agent::CliProviderConfig {
                command: "claude-mock".to_string(),
                args: vec![],
                normalizer: None,
                output_format: ao_protocol::agent::OutputFormat::Text,
                input_mode: ao_protocol::agent::InputMode::Arg,
                model_arg: None,
                model_aliases: std::collections::HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: std::collections::HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
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

    /// Drives one `CliAgentRunner::run` call through a `MockProcessSupervisor`
    /// scenario deliberately held "in flight" for `delay_per_line_ms` before its
    /// one stdout line lands, so a mid-run poll of `InstanceRegistry::running_count`
    /// deterministically observes the run while it's still active — no real
    /// subprocess or wall-clock race involved. Returns `(running_count while
    /// in flight, can_spawn while in flight under max_instances: 1,
    /// running_count after completion)`.
    async fn run_and_sample_registry(bypass_instance_cap: bool) -> (usize, bool, usize) {
        let _guard = crate::plugin_paths::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("temp dir");
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let persistence = Arc::new(PersistenceLayer::init().await.expect("persistence init"));

        let supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(MockProcessSupervisor::new(vec![MockScenario {
                stdout_lines: vec!["ok".to_string()],
                stderr_lines: vec![],
                exit_code: 0,
                delay_per_line_ms: 200,
            }]));

        let instance_registry = Arc::new(InstanceRegistry::new());
        let runner: Arc<dyn AgentRunner> = Arc::new(CliAgentRunner::new(
            supervisor,
            Arc::new(NormalizerRegistry::new()),
            Arc::new(EventBus::new(64)),
            Arc::clone(&persistence),
            Arc::new(CommandQueue::new()),
            Arc::clone(&instance_registry),
            Arc::new(crate::agent_runner::RunningAgents::new()),
            Arc::new(Registry::new()),
        ));

        let profile = make_profile("watch-poll-agent");
        persistence.agents.create(&profile).await.expect("create agent");
        let agent_id = profile.id.clone();

        let (tx, _rx) = mpsc::channel(1);
        let request = AgentRunRequest {
            agent: profile,
            prompt: "check for anything new".to_string(),
            run_complete_tx: tx,
            bypass_instance_cap,
            ..Default::default()
        };

        let run_task = tokio::spawn(async move { runner.run(request).await });

        // The mock scenario sleeps `delay_per_line_ms` (200ms) before its
        // stdout line lands; sampling well before that deadline reliably
        // catches the run mid-flight without a real-process race.
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let mid_flight = instance_registry.running_count(&agent_id).await;
        // `max_instances: 1` on `make_profile` — this is exactly the
        // check the per-agent queue manager's `pump()` makes before
        // dispatching the next queued message for this agent.
        let can_spawn_mid_flight = instance_registry.can_spawn(&agent_id, 1).await;

        run_task
            .await
            .expect("run task must not panic")
            .expect("run must complete successfully");
        let after = instance_registry.running_count(&agent_id).await;

        (mid_flight, can_spawn_mid_flight, after)
    }

    #[tokio::test]
    async fn normal_request_registers_and_blocks_a_capped_profile_while_running() {
        let (mid_flight, can_spawn_mid_flight, after) = run_and_sample_registry(false).await;
        assert_eq!(
            mid_flight, 1,
            "a normal (bypass_instance_cap: false) request must occupy the agent's \
                 InstanceRegistry slot while running, exactly as before this change"
        );
        assert!(
            !can_spawn_mid_flight,
            "with max_instances: 1, a normal in-flight run must still block a \
                 second dispatch — the cap must stay enforced for every other caller"
        );
        assert_eq!(after, 0, "the slot must be released once the run completes");
    }

    #[tokio::test]
    async fn bypassed_request_never_registers_and_never_blocks_a_capped_profile() {
        let (mid_flight, can_spawn_mid_flight, after) = run_and_sample_registry(true).await;
        assert_eq!(
            mid_flight, 0,
            "bypass_instance_cap: true must skip InstanceRegistry registration \
                 entirely, so a watch poll never occupies — or is blocked behind — \
                 the agent's max_instances slot"
        );
        assert!(
            can_spawn_mid_flight,
            "with max_instances: 1, a bypassed poll must NOT count against the \
                 cap — a real user turn dispatched by the queue manager while the \
                 poll is in flight must still be allowed to spawn"
        );
        assert_eq!(after, 0);
    }
}

// === CLI-mode tool_use/tool_result transcript persistence (regression) ===
//
// Pins the fix for a bug where a CLI-mode agent's MCP tool calls (e.g.
// `ArtifactWrite`) never landed in the persisted transcript — only the
// live SSE bus saw them (via the generic `ToolCallStarted`/
// `ToolCallCompleted` fallthrough), so a page reload lost any inline
// rendering that depends on replaying `tool_use`/`tool_result` pairs
// (`extractArtifactWriteResults` on the frontend). Drives a real
// `ClaudeNormalizer` (StreamJson) through a scripted tool-call exchange
// and asserts the persisted transcript now carries both halves, matched
// by `tool_use_id` and sharing a `turn_id`.
mod cli_tool_use_persistence {
    use super::*;
    use ao_normalizer::registry::NormalizerRegistry;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::{MockProcessSupervisor, MockScenario};
    use ao_protocol::agent::{AgentProfile, OutputFormat, ProviderConfig};

    fn make_profile(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("agent {id}"),
            description: "test".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(ao_protocol::agent::CliProviderConfig {
                command: "claude".to_string(),
                args: vec![],
                // Force the real Claude stream-json normalizer rather than
                // relying on command-name matching.
                normalizer: Some("claude".to_string()),
                output_format: OutputFormat::StreamJson,
                input_mode: ao_protocol::agent::InputMode::Arg,
                model_arg: None,
                model_aliases: std::collections::HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: std::collections::HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
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

    #[tokio::test]
    async fn tool_use_and_result_persist_with_matching_ids() {
        let _guard = crate::plugin_paths::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("temp dir");
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let persistence = Arc::new(PersistenceLayer::init().await.expect("persistence init"));

        // Scripted exchange mirroring real Claude CLI stream-json output:
        // narration text, a tool_use block (id + streamed input), its
        // close, the tool_result fed back as a top-level "user" event,
        // then trailing narration.
        let lines = vec![
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Creating the artifact."}}"#,
            r#"{"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_test1","name":"mcp__launchpad__ArtifactWrite"}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"title\":\"Test\"}"}}"#,
            r#"{"type":"content_block_stop"}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_test1","content":"{\"id\":\"artifact-abc\",\"renderer\":\"table\"}"}]}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":" Done."}}"#,
        ];
        let stdout_lines: Vec<String> = lines.into_iter().map(|l| format!("{l}\n")).collect();

        let supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(MockProcessSupervisor::new(vec![MockScenario {
                stdout_lines,
                stderr_lines: vec![],
                exit_code: 0,
                delay_per_line_ms: 0,
            }]));

        let runner = Arc::new(CliAgentRunner::new(
            supervisor,
            Arc::new(NormalizerRegistry::new()),
            Arc::new(EventBus::new(64)),
            Arc::clone(&persistence),
            Arc::new(CommandQueue::new()),
            Arc::new(InstanceRegistry::new()),
            Arc::new(crate::agent_runner::RunningAgents::new()),
            Arc::new(Registry::new()),
        ));

        let profile = make_profile("tool-persist-agent");
        persistence
            .agents
            .create(&profile)
            .await
            .expect("create agent");

        let (tx, mut rx) = mpsc::channel(1);
        runner
            .run(&profile, "make a table artifact", &[], tx, None)
            .await
            .expect("run must start");

        tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("run must finish within timeout")
            .expect("completion must arrive");

        let entries = persistence
            .transcripts
            .read_all(&profile.id)
            .await
            .expect("read agent transcript");

        let tool_use = entries
            .iter()
            .find(|e| e.event_type == "tool_use")
            .unwrap_or_else(|| panic!("expected a persisted tool_use entry; got: {:?}", entries));
        let tool_result = entries
            .iter()
            .find(|e| e.event_type == "tool_result")
            .unwrap_or_else(|| panic!("expected a persisted tool_result entry; got: {:?}", entries));

        let use_meta = tool_use.metadata.as_ref().expect("tool_use metadata");
        let result_meta = tool_result.metadata.as_ref().expect("tool_result metadata");

        assert_eq!(
            use_meta.get("tool_name").and_then(|v| v.as_str()),
            Some("mcp__launchpad__ArtifactWrite"),
        );
        let use_id = use_meta.get("tool_use_id").and_then(|v| v.as_str());
        assert_eq!(use_id, Some("toolu_test1"));
        assert_eq!(
            result_meta.get("tool_use_id").and_then(|v| v.as_str()),
            use_id,
            "tool_result must carry the same tool_use_id as its tool_use",
        );
        assert!(
            result_meta
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("artifact-abc"),
            "tool_result output must carry the ArtifactWrite response; got: {:?}",
            result_meta.get("output")
        );
        assert_eq!(result_meta.get("is_error"), Some(&serde_json::Value::Bool(false)));

        // Both entries must share a turn_id so the frontend's reload-path
        // extraction (`extractArtifactWriteResults`) can anchor the
        // resulting artifact card to the right chat bubble.
        assert!(use_meta.get("turn_id").is_some());
        assert_eq!(use_meta.get("turn_id"), result_meta.get("turn_id"));
    }
}

// === Project-scoped system prompt injection (regression) ===
//
// Pins the contract that a `RunScope::Project` run's child process
// receives the project context block (goal + status role section) even
// when the profile carries `persona`/`special_instructions`. The
// system-prompt composer rebuilds the prompt from those fields and
// discards the legacy `system_prompt` field, so the block must be
// appended post-compose by the runner itself — stuffing it into the
// profile before the run silently delivers nothing.
mod project_scope_prompt {
    use super::*;
    use ao_normalizer::registry::NormalizerRegistry;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::{MockProcessSupervisor, MockScenario};
    use ao_process::registry::RunRecord;
    use ao_process::supervisor::{ManagedRun, ProcessSupervisor, SpawnInput};
    use ao_protocol::agent::{AgentProfile, ProviderConfig};
    use ao_protocol::project::{Project, ProjectStatus};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// Wraps the mock supervisor and records every `SpawnInput` so the
    /// test can assert on the argv the runner actually built for the
    /// child process.
    struct CapturingSupervisor {
        inner: MockProcessSupervisor,
        spawns: Arc<Mutex<Vec<SpawnInput>>>,
    }

    #[async_trait]
    impl ProcessSupervisor for CapturingSupervisor {
        async fn spawn(&self, input: SpawnInput) -> Result<ManagedRun, AoError> {
            self.spawns.lock().unwrap().push(input.clone());
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

    fn make_profile(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("agent {id}"),
            description: "test".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(ao_protocol::agent::CliProviderConfig {
                // Deliberately NOT `echo`: leak-prone commands strip the
                // system prompt from argv, which would mask the regression
                // this test exists to catch.
                command: "claude-mock".to_string(),
                args: vec![],
                normalizer: None,
                output_format: ao_protocol::agent::OutputFormat::Text,
                input_mode: ao_protocol::agent::InputMode::Arg,
                model_arg: None,
                model_aliases: std::collections::HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: std::collections::HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
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
            delegates_to: vec![],
            // Persona-bearing profile: the exact regression trigger. With
            // persona set the composer ignores the legacy `system_prompt`
            // field entirely, so only the runner's post-compose append can
            // deliver project context.
            persona: Some("PERSONA_SENTINEL collaborative reviewer".to_string()),
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

    #[tokio::test]
    async fn project_scope_run_injects_project_context_post_compose() {
        let _guard = crate::plugin_paths::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("temp dir");
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let persistence = Arc::new(PersistenceLayer::init().await.expect("persistence init"));

        let spawns: Arc<Mutex<Vec<SpawnInput>>> = Arc::new(Mutex::new(Vec::new()));
        let supervisor: Arc<dyn ProcessSupervisor> = Arc::new(CapturingSupervisor {
            inner: MockProcessSupervisor::new(vec![MockScenario {
                stdout_lines: vec!["ack".to_string()],
                stderr_lines: vec![],
                exit_code: 0,
                delay_per_line_ms: 0,
            }]),
            spawns: Arc::clone(&spawns),
        });

        let runner = Arc::new(CliAgentRunner::new(
            supervisor,
            Arc::new(NormalizerRegistry::new()),
            Arc::new(EventBus::new(64)),
            Arc::clone(&persistence),
            Arc::new(CommandQueue::new()),
            Arc::new(InstanceRegistry::new()),
            Arc::new(crate::agent_runner::RunningAgents::new()),
            Arc::new(Registry::new()),
        ));

        let profile = make_profile("proj-prompt-agent");
        persistence
            .agents
            .create(&profile)
            .await
            .expect("create agent");

        let now = chrono::Utc::now();
        let project = Project {
            id: "proj-prompt-test".to_string(),
            name: "Prompt Injection Probe".to_string(),
            emoji: None,
            goal: "GOAL_SENTINEL build the widget".to_string(),
            spec: None,
            agent_id: profile.id.clone(),
            working_dir: None,
            attachments: vec![],
            status: ProjectStatus::Interviewing,
            summary: None,
            verifications: vec![],
            created_at: now,
            updated_at: now,
        };
        persistence
            .projects
            .create(&project)
            .await
            .expect("create project");

        let (tx, mut rx) = mpsc::channel(1);
        runner
            .run_with_scope(
                &profile,
                "kick off the interview",
                &[],
                tx,
                RunScope::Project {
                    project_id: project.id.clone(),
                },
                None,
            )
            .await
            .expect("run must start");

        tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("run must finish within timeout")
            .expect("completion must arrive");

        let joined = spawns
            .lock()
            .unwrap()
            .iter()
            .flat_map(|s| s.argv.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("PERSONA_SENTINEL"),
            "composed prompt (persona) must reach the child argv; got:\n{joined}"
        );
        assert!(
            joined.contains("## Project Context"),
            "project context block must be appended post-compose; got:\n{joined}"
        );
        assert!(
            joined.contains("GOAL_SENTINEL"),
            "project goal must reach the child argv"
        );
        assert!(
            joined.contains("Do not do the work yourself"),
            "interviewing role block must reach the child argv"
        );
    }
}

// === CLI [Conversation history] byte-stability test ===

#[test]
fn test_cli_conversation_history_byte_stability() {
    use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
    use chrono::{Duration, TimeZone, Utc};
    use std::sync::Arc;
    use crate::history::anchor::{AnchorKey, FloorMarker, WindowAnchor, WindowAnchorRegistry};
    use crate::context::compute_message_count;

    let base_ts = Utc.with_ymd_and_hms(2026, 2, 25, 10, 0, 0).unwrap();

    let make_entry = |idx: usize, is_user: bool| TranscriptEntry {
        ts: base_ts + Duration::minutes(idx as i64),
        role: if is_user {
            TranscriptRole::System("user".to_string())
        } else {
            TranscriptRole::Agent { agent: "assistant".to_string() }
        },
        content: format!("Message {}", idx),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    };

    // Seed 25 entries (within active window → target=20, floor at index 5).
    let mut entries: Vec<TranscriptEntry> = (0..25)
        .map(|i| make_entry(i, i % 2 == 0))
        .collect();

    // build_prompt_with_context is a pure formatting function — caller supplies the stable slice.
    let config = ContextConfig::default();
    let registry = Arc::new(WindowAnchorRegistry::new());
    let key = AnchorKey::Personal("test-agent".to_string());

    // Turn N: compute floor (target=20, start_n=5), pin anchor.
    let now = base_ts + Duration::minutes(30);
    let target = compute_message_count(entries.last().map(|e| e.ts), now, &config);
    assert_eq!(target, 20, "25 entries within active window → target should be 20");
    let start_n = entries.len().saturating_sub(target); // 25 - 20 = 5

    registry.set(
        key.clone(),
        WindowAnchor {
            floor_marker: FloorMarker::for_entry(&entries[start_n]),
            pinned_target: target,
            pinned_at: Utc::now(),
        },
    );
    let system_prompt_n =
        build_prompt_with_context(&entries[start_n..], "turn N message", &config);

    // Append a User + Assistant pair to simulate the next turn landing.
    entries.push(make_entry(25, true));
    entries.push(make_entry(26, false));

    // Turn N+1: locate anchor floor in the extended slice (CACHE HIT path).
    let stored = registry.get(&key).expect("anchor must be set from turn N");
    let floor_idx = entries
        .iter()
        .position(|e| FloorMarker::for_entry(e) == stored.floor_marker)
        .expect("floor entry must be locatable after two appends");

    // Verify the window hasn't exceeded max_window (so this IS a cache hit).
    let max_window = stored.pinned_target * 2 + config.anchor_grace; // 20*2+4 = 44
    assert!(
        entries.len() - floor_idx <= max_window,
        "27 - 5 = 22 must be ≤ max_window {}",
        max_window
    );

    let system_prompt_n1 =
        build_prompt_with_context(&entries[floor_idx..], "turn N+1 message", &config);

    // Extract the [Conversation history] block (from header to [Current message] separator).
    let extract_history = |prompt: &str| -> String {
        let start = prompt
            .find("[Conversation history]")
            .expect("prompt must contain [Conversation history] block");
        let end = prompt.find("\n\n[Current message]").unwrap_or(prompt.len());
        prompt[start..end].to_string()
    };

    let block_n = extract_history(&system_prompt_n);
    let block_n1 = extract_history(&system_prompt_n1);

    // Byte-prefix stability: turn N+1's history block must start with turn N's block.
    // The floor is stable (same anchor), so only the two new tail entries are appended.
    assert!(
        block_n1.starts_with(&block_n),
        "Turn N+1 [Conversation history] block must be a byte-prefix extension of turn N.\n\
             Turn N block:\n{}\n\nTurn N+1 block:\n{}",
        block_n,
        block_n1
    );
    assert!(
        block_n1.len() > block_n.len(),
        "Turn N+1 block must be longer (2 new entries appended)"
    );
}

#[tokio::test]
async fn prepare_mcp_session_writes_json_and_updates_store() {
    use ao_normalizer::registry::NormalizerRegistry;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_process::supervisor::ProcessSupervisor;

    let _env_guard = crate::plugin_paths::tests::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("temp dir");
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
    let persistence = Arc::new(PersistenceLayer::init().await.expect("persistence"));
    let event_bus = Arc::new(EventBus::new(8));
    let supervisor: Arc<dyn ProcessSupervisor> = Arc::new(MockProcessSupervisor::new(vec![]));
    let runner = Arc::new(CliAgentRunner::new(
        supervisor,
        Arc::new(NormalizerRegistry::new()),
        event_bus,
        Arc::clone(&persistence),
        Arc::new(CommandQueue::new()),
        Arc::new(InstanceRegistry::new()),
        Arc::new(crate::agent_runner::RunningAgents::new()),
        Arc::new(ao_engine_tools_core::Registry::new()),
    ));

    let agent_id = "test-agent-us007";
    let cwd = std::path::PathBuf::from("/tmp/test-cwd");
    let floor_ts = chrono::Utc::now();

    let session_id1 = runner
        .prepare_mcp_session(agent_id, cwd.clone(), floor_ts)
        .await
        .expect("prepare_mcp_session should succeed");

    // Each spawn writes a per-session mcp-{session_id}.json with the session_id in the URL.
    let mcp_path1 = persistence
        .data_root
        .agents_dir()
        .join(agent_id)
        .join(format!("mcp-{}.json", session_id1));
    assert!(mcp_path1.exists(), "per-session mcp json must be created");
    let raw = std::fs::read_to_string(&mcp_path1).expect("read mcp json");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert!(
        parsed["mcpServers"]["launchpad"]["type"] == "http",
        "type must be http"
    );
    let url = parsed["mcpServers"]["launchpad"]["url"]
        .as_str()
        .expect("url string");
    assert!(
        url.contains(agent_id),
        "URL must contain agent_id; got: {url}"
    );
    assert!(
        url.contains(&session_id1),
        "URL must contain session_id; got: {url}"
    );

    // McpAgentSession must be registered with updated floor (looked up by session_id).
    let session1 = runner
        .mcp_sessions
        .get_by_session_id(&session_id1)
        .expect("session must be registered");
    let stored_floor = *session1.window_floor_ts.read().await;
    assert_eq!(stored_floor, Some(floor_ts), "window_floor_ts must match");

    // Second call writes a different mcp-{session_id2}.json; first session is NOT evicted.
    let floor_ts2 = chrono::Utc::now();
    let session_id2 = runner
        .prepare_mcp_session(agent_id, std::path::PathBuf::from("/tmp/other"), floor_ts2)
        .await
        .expect("second call should succeed");
    assert_ne!(session_id1, session_id2, "session IDs must be distinct");
    let mcp_path2 = persistence
        .data_root
        .agents_dir()
        .join(agent_id)
        .join(format!("mcp-{}.json", session_id2));
    assert!(mcp_path2.exists(), "second per-session mcp json must be created");
    let raw2 = std::fs::read_to_string(&mcp_path2).expect("read second mcp json");
    assert!(
        raw2.contains(&session_id2),
        "second mcp json must embed session_id2"
    );
    // Both sessions coexist — no eviction on registration (caller manages cleanup).
    assert!(
        runner.mcp_sessions.get_by_session_id(&session_id1).is_some(),
        "first session must still be present (no auto-eviction on second register)"
    );
    let session2 = runner
        .mcp_sessions
        .get_by_session_id(&session_id2)
        .expect("second session must be registered");
    let stored_floor2 = *session2.window_floor_ts.read().await;
    assert_eq!(stored_floor2, Some(floor_ts2), "floor must be refreshed");
}

#[tokio::test]
async fn two_concurrent_spawns_produce_distinct_session_entries() {
    use ao_normalizer::registry::NormalizerRegistry;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_process::supervisor::ProcessSupervisor;

    let _env_guard = crate::plugin_paths::tests::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("temp dir");
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
    let persistence = Arc::new(PersistenceLayer::init().await.expect("persistence"));
    let event_bus = Arc::new(EventBus::new(8));
    let supervisor: Arc<dyn ProcessSupervisor> = Arc::new(MockProcessSupervisor::new(vec![]));
    let runner = Arc::new(CliAgentRunner::new(
        supervisor,
        Arc::new(NormalizerRegistry::new()),
        event_bus,
        Arc::clone(&persistence),
        Arc::new(CommandQueue::new()),
        Arc::new(InstanceRegistry::new()),
        Arc::new(crate::agent_runner::RunningAgents::new()),
        Arc::new(ao_engine_tools_core::Registry::new()),
    ));

    let agent_id = "test-agent-concurrent";
    let cwd = std::path::PathBuf::from("/tmp/concurrent-cwd");
    let ts = chrono::Utc::now();

    // Simulate two concurrent spawns of the same agent profile.
    let sid1 = runner
        .prepare_mcp_session(agent_id, cwd.clone(), ts)
        .await
        .expect("first spawn ok");
    let sid2 = runner
        .prepare_mcp_session(agent_id, cwd.clone(), ts)
        .await
        .expect("second spawn ok");

    assert_ne!(sid1, sid2, "concurrent spawns must get distinct session IDs");

    // Both sessions must coexist in the store (no cross-spawn eviction).
    assert!(
        runner.mcp_sessions.get_by_session_id(&sid1).is_some(),
        "first session must survive the second spawn"
    );
    assert!(
        runner.mcp_sessions.get_by_session_id(&sid2).is_some(),
        "second session must be registered"
    );

    // Each spawn got its own isolated mcp config file.
    let mcp1 = persistence
        .data_root
        .agents_dir()
        .join(agent_id)
        .join(format!("mcp-{}.json", sid1));
    let mcp2 = persistence
        .data_root
        .agents_dir()
        .join(agent_id)
        .join(format!("mcp-{}.json", sid2));
    assert!(mcp1.exists(), "first spawn config must exist");
    assert!(mcp2.exists(), "second spawn config must exist");

    // McpSessionGuard drop removes the session and its config file.
    {
        let guard = McpSessionGuard {
            sessions: Arc::clone(&runner.mcp_sessions),
            session_id: sid1.clone(),
            mcp_json_path: mcp1.clone(),
        };
        drop(guard);
    }
    assert!(
        runner.mcp_sessions.get_by_session_id(&sid1).is_none(),
        "session must be removed after guard drop"
    );
    assert!(!mcp1.exists(), "mcp config must be removed after guard drop");
    // Second session is unaffected by dropping the first guard.
    assert!(
        runner.mcp_sessions.get_by_session_id(&sid2).is_some(),
        "second session must survive first guard drop"
    );
}

#[test]
fn event_agent_id_for_agent_tasklist_returns_tasklist_channel() {
    let scope = RunScope::Tasklist {
        scope: TasklistScope::Agent("agent-42".to_string()),
        tasklist_id: "tl-abc123".to_string(),
        task_id: "task-1".to_string(),
    };
    assert_eq!(
        scope.event_agent_id("agent-42"),
        "tasklist:tl-abc123",
        "agent-owned tasklist must emit on tasklist:{{tasklist_id}}"
    );
}

#[test]
fn event_agent_id_for_team_tasklist_unchanged() {
    let scope = RunScope::Tasklist {
        scope: TasklistScope::Team("team-xyz".to_string()),
        tasklist_id: "tl-def456".to_string(),
        task_id: "task-2".to_string(),
    };
    assert_eq!(
        scope.event_agent_id("agent-99"),
        "team:team-xyz",
        "team-owned tasklist must still emit on team:{{team_id}}"
    );
}

#[test]
fn merge_cursor_mcp_config_creates_file_when_absent() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let url = "http://localhost:3101/mcp/cursor-agent/session-1";
    merge_cursor_mcp_config(tmp.path(), url).expect("write should succeed");

    let config_path = tmp.path().join(".cursor").join("mcp.json");
    let raw = std::fs::read_to_string(&config_path).expect("config must exist");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(parsed["mcpServers"]["launchpad"]["url"], url);
    // No `type` key — cursor-agent infers the transport from `url` alone;
    // adding an unrecognized key risks the CLI's schema rejecting the file.
    assert!(parsed["mcpServers"]["launchpad"].get("type").is_none());
}

#[test]
fn merge_cursor_mcp_config_preserves_other_servers() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let cursor_dir = tmp.path().join(".cursor");
    std::fs::create_dir_all(&cursor_dir).expect("mkdir");
    std::fs::write(
        cursor_dir.join("mcp.json"),
        r#"{"mcpServers":{"other-server":{"command":"some-tool","args":[]}}}"#,
    )
    .expect("seed file");

    let url = "http://localhost:3101/mcp/cursor-agent/session-2";
    merge_cursor_mcp_config(tmp.path(), url).expect("write should succeed");

    let raw = std::fs::read_to_string(cursor_dir.join("mcp.json")).expect("read back");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(
        parsed["mcpServers"]["other-server"]["command"], "some-tool",
        "pre-existing project MCP servers must survive the merge"
    );
    assert_eq!(parsed["mcpServers"]["launchpad"]["url"], url);
}

#[test]
fn merge_cursor_mcp_config_overwrites_stale_launchpad_url() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let old_url = "http://localhost:3101/mcp/cursor-agent/session-old";
    merge_cursor_mcp_config(tmp.path(), old_url).expect("first write");

    let new_url = "http://localhost:3101/mcp/cursor-agent/session-new";
    merge_cursor_mcp_config(tmp.path(), new_url).expect("second write");

    let raw = std::fs::read_to_string(tmp.path().join(".cursor").join("mcp.json"))
        .expect("read back");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(
        parsed["mcpServers"]["launchpad"]["url"], new_url,
        "a later spawn's session URL must replace the previous one"
    );
}

#[test]
fn merge_agy_mcp_config_creates_file_when_absent() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let url = "http://localhost:3101/mcp/agy/session-1";
    merge_agy_mcp_config_at(tmp.path(), url).expect("write should succeed");

    let config_path = agy_global_mcp_config_path(tmp.path());
    assert_eq!(
        config_path,
        tmp.path().join(".gemini").join("config").join("mcp_config.json"),
        "must resolve to agy's global config path, not a per-project file"
    );
    let raw = std::fs::read_to_string(&config_path).expect("config must exist");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(
        parsed["mcpServers"]["launchpad"]["url"], url,
        "agy's remote-server key is `url`, not `serverUrl`"
    );
}

#[test]
fn merge_agy_mcp_config_creates_file_when_zero_byte() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let config_path = agy_global_mcp_config_path(tmp.path());
    std::fs::create_dir_all(config_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&config_path, "").expect("seed empty file");

    let url = "http://localhost:3101/mcp/agy/session-empty";
    merge_agy_mcp_config_at(tmp.path(), url).expect("write should succeed on 0-byte file");

    let raw = std::fs::read_to_string(&config_path).expect("read back");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(parsed["mcpServers"]["launchpad"]["url"], url);
}

#[test]
fn merge_agy_mcp_config_handles_malformed_file_without_panicking() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let config_path = agy_global_mcp_config_path(tmp.path());
    std::fs::create_dir_all(config_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&config_path, "{not valid json at all").expect("seed malformed file");

    let url = "http://localhost:3101/mcp/agy/session-malformed";
    merge_agy_mcp_config_at(tmp.path(), url)
        .expect("malformed existing file must not fail the merge");

    let raw = std::fs::read_to_string(&config_path).expect("read back");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(parsed["mcpServers"]["launchpad"]["url"], url);
}

#[test]
fn merge_agy_mcp_config_preserves_other_servers() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let config_path = agy_global_mcp_config_path(tmp.path());
    std::fs::create_dir_all(config_path.parent().unwrap()).expect("mkdir");
    std::fs::write(
        &config_path,
        r#"{"mcpServers":{"other-server":{"command":"some-tool","args":[]}}}"#,
    )
    .expect("seed file");

    let url = "http://localhost:3101/mcp/agy/session-2";
    merge_agy_mcp_config_at(tmp.path(), url).expect("write should succeed");

    let raw = std::fs::read_to_string(&config_path).expect("read back");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(
        parsed["mcpServers"]["other-server"]["command"], "some-tool",
        "the user's own global MCP servers must survive the merge"
    );
    assert_eq!(parsed["mcpServers"]["launchpad"]["url"], url);
}

#[test]
fn merge_agy_mcp_config_is_idempotent_on_repeat_runs() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let url = "http://localhost:3101/mcp/agy/session-3";
    merge_agy_mcp_config_at(tmp.path(), url).expect("first write should succeed");
    merge_agy_mcp_config_at(tmp.path(), url).expect("second write should succeed");
    merge_agy_mcp_config_at(tmp.path(), url).expect("third write should succeed");

    let config_path = agy_global_mcp_config_path(tmp.path());
    let raw = std::fs::read_to_string(&config_path).expect("read back");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    let servers = parsed["mcpServers"]
        .as_object()
        .expect("mcpServers must be an object");
    assert_eq!(
        servers.len(),
        1,
        "re-running the merge must not duplicate the launchpad entry: {servers:?}"
    );
    assert_eq!(parsed["mcpServers"]["launchpad"]["url"], url);
}

/// `build_argv` provider-dispatch coverage: proves the `agy` addition
/// doesn't perturb Claude/Codex/cursor-agent argv (byte-for-byte), and
/// pins down the new `agy` argv shape.
mod build_argv_provider_dispatch {
    use super::*;
    use ao_protocol::agent::{CliProviderConfig, OutputFormat};

    /// Base args mirrored from
    /// `ao_engine_tools_engine::agent_author::template_provider` as of
    /// this change. Copied rather than imported: that function is
    /// private to its own crate, so this is a manual pin that won't
    /// auto-track future template edits — acceptable for a regression
    /// test whose job is to freeze *today's* shape.
    fn make_agent(
        command: &str,
        args: Vec<&str>,
        model_arg: Option<&str>,
        output_format: OutputFormat,
        normalizer: &str,
        session_id_fields: Vec<&str>,
    ) -> AgentProfile {
        AgentProfile {
            id: "a".to_string(),
            name: "a".to_string(),
            description: "test".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: command.to_string(),
                args: args.into_iter().map(str::to_string).collect(),
                normalizer: Some(normalizer.to_string()),
                output_format,
                input_mode: InputMode::Arg,
                model_arg: model_arg.map(str::to_string),
                model_aliases: std::collections::HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: session_id_fields.into_iter().map(str::to_string).collect(),
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: Some("test-model".to_string()),
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: std::collections::HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
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

    // Every case below passes both an mcp_config_path and an
    // mcp_server_url — mirroring the real continuation-loop call site
    // (`run_with_scope_inner`, which always has both by the time it
    // calls `build_argv`) — so the assertions prove not just "argv is
    // right" but "each provider picks the right MCP delivery mechanism
    // out of the two it's handed."
    fn mcp_args() -> (std::path::PathBuf, &'static str) {
        (std::path::PathBuf::from("/tmp/mcp-session.json"), "http://localhost:3001/mcp/a/s")
    }

    #[test]
    fn claude_argv_is_unchanged() {
        let agent = make_agent(
            "claude",
            vec![
                "--print",
                "--output-format",
                "stream-json",
                "--verbose",
                "--dangerously-skip-permissions",
                "--include-partial-messages",
            ],
            Some("--model"),
            OutputFormat::StreamJson,
            "claude",
            vec![],
        );
        let (mcp_path, mcp_url) = mcp_args();
        let argv = CliAgentRunner::build_argv(&agent, "hello", Some(&mcp_path), Some(mcp_url));
        assert_eq!(
            argv,
            vec![
                "claude",
                "--print",
                "--output-format",
                "stream-json",
                "--verbose",
                "--dangerously-skip-permissions",
                "--include-partial-messages",
                "--model",
                "test-model",
                "--mcp-config",
                "/tmp/mcp-session.json",
                "hello",
            ]
        );
    }

    #[test]
    fn codex_argv_with_mcp_adds_approval_policy_never() {
        let agent = make_agent(
            "codex",
            vec!["exec", "--json", "--sandbox", "workspace-write", "--skip-git-repo-check"],
            Some("--model"),
            OutputFormat::StreamJsonl,
            "codex",
            vec!["thread_id"],
        );
        let (mcp_path, mcp_url) = mcp_args();
        let argv = CliAgentRunner::build_argv(&agent, "hello", Some(&mcp_path), Some(mcp_url));

        assert_eq!(
            argv,
            vec![
                "codex",
                "exec",
                "--json",
                "--sandbox",
                "workspace-write",
                "--skip-git-repo-check",
                "--model",
                "test-model",
                "-c",
                r#"mcp_servers.launchpad.url="http://localhost:3001/mcp/a/s""#,
                "-c",
                r#"approval_policy="never""#,
                "hello",
            ],
            "when MCP is wired, codex needs approval_policy=never or every MCP tool call gets silently cancelled"
        );
    }

    #[test]
    fn codex_argv_without_mcp_has_no_approval_policy_override() {
        let agent = make_agent(
            "codex",
            vec!["exec", "--json", "--sandbox", "workspace-write", "--skip-git-repo-check"],
            Some("--model"),
            OutputFormat::StreamJsonl,
            "codex",
            vec!["thread_id"],
        );
        let argv = CliAgentRunner::build_argv(&agent, "hello", None, None);
        assert_eq!(
            argv,
            vec![
                "codex",
                "exec",
                "--json",
                "--sandbox",
                "workspace-write",
                "--skip-git-repo-check",
                "--model",
                "test-model",
                "hello",
            ],
            "non-MCP codex agents must keep their sandbox and must not get an approval_policy override"
        );
    }

    #[test]
    fn cursor_agent_argv_is_unchanged() {
        let agent = make_agent(
            "cursor-agent",
            vec![
                "--print",
                "--output-format",
                "stream-json",
                "--force",
                "--approve-mcps",
                "--trust",
                "--stream-partial-output",
            ],
            Some("--model"),
            OutputFormat::StreamJson,
            "cursor-agent",
            vec![],
        );
        let (mcp_path, mcp_url) = mcp_args();
        let argv = CliAgentRunner::build_argv(&agent, "hello", Some(&mcp_path), Some(mcp_url));
        assert_eq!(
            argv,
            vec![
                "cursor-agent",
                "--print",
                "--output-format",
                "stream-json",
                "--force",
                "--approve-mcps",
                "--trust",
                "--stream-partial-output",
                "--model",
                "test-model",
                "hello",
            ],
            "cursor-agent must take neither --mcp-config nor a -c override — it's file-based"
        );
    }

    #[test]
    fn agy_argv_headless_model_output_format_and_prompt_flag() {
        let agent = make_agent(
            "agy",
            vec!["--dangerously-skip-permissions", "--output-format", "stream-json"],
            Some("--model"),
            OutputFormat::StreamJson,
            "agy",
            vec!["conversation_id"],
        );
        let (mcp_path, mcp_url) = mcp_args();
        let argv = CliAgentRunner::build_argv(&agent, "hello", Some(&mcp_path), Some(mcp_url));
        assert_eq!(
            argv,
            vec![
                "agy",
                "--dangerously-skip-permissions",
                "--output-format",
                "stream-json",
                "--model",
                "test-model",
                "-p",
                "hello"
            ],
            "agy must take neither --mcp-config nor a -c override (file-based, like cursor-agent), \
                 and its prompt must be preceded by -p"
        );
    }
}
