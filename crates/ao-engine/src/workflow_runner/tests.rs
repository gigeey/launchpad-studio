//! Unit tests for the workflow runner.
//!
//! Declared from the parent module as `#[cfg(test)] mod tests;` — this is
//! the same module as the inline `mod tests` block it replaces, so private
//! items of the parent remain in scope here via `use super::*`.

use super::*;
use ao_protocol::workflow::{PhaseInput, PhaseOutput, WorkflowDefinition};

/// Helper: write a dummy output file so complete_phase validation passes.
async fn write_phase1_output(runner: &WorkflowRunner, task_id: &str) {
    runner
        .write_phase_output(task_id, "analysis.json", r#"{"test": true}"#)
        .await
        .unwrap();
}

async fn write_phase2_output(runner: &WorkflowRunner, task_id: &str) {
    runner
        .write_phase_output(task_id, "result.json", r#"{"test": true}"#)
        .await
        .unwrap();
}

async fn setup_test_env() -> (
    WorkflowRunner,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let workflows_tmp = tempfile::tempdir().unwrap();
    let tasks_tmp = tempfile::tempdir().unwrap();

    // Create a test workflow
    let wf_dir = workflows_tmp.path().join("test-wf");
    tokio::fs::create_dir_all(&wf_dir).await.unwrap();

    let phase1_dir = wf_dir.join("phase1");
    tokio::fs::create_dir_all(&phase1_dir).await.unwrap();
    tokio::fs::write(phase1_dir.join("prompt.md"), "# Phase 1\nDo the first thing.")
        .await
        .unwrap();

    let phase2_dir = wf_dir.join("phase2");
    tokio::fs::create_dir_all(&phase2_dir).await.unwrap();
    tokio::fs::write(phase2_dir.join("prompt.md"), "# Phase 2\nDo the second thing.")
        .await
        .unwrap();
    tokio::fs::write(
        phase2_dir.join("schema.json"),
        r#"{"type": "object", "properties": {"result": {"type": "string"}}}"#,
    )
    .await
    .unwrap();

    let workflow = WorkflowDefinition {
        id: "test-wf".to_string(),
        name: "Test Workflow".to_string(),
        version: Some("1.0".to_string()),
        description: Some("A test workflow".to_string()),
        phases: vec![
            PhaseDefinition {
                id: "phase-1".to_string(),
                name: "Phase One".to_string(),
                intent: Some("Do the first thing".to_string()),
                path: "phase1/prompt.md".to_string(),
                phase_type: None,
                auto_advance: true,
                schema: None,
                inputs: vec![],
                outputs: vec![PhaseOutput {
                    id: "analysis".to_string(),
                    filename: Some("analysis.json".to_string()),
                    description: Some("Analysis results".to_string()),
                }],
                fields: vec![],
            },
            PhaseDefinition {
                id: "phase-2".to_string(),
                name: "Phase Two".to_string(),
                intent: Some("Do the second thing".to_string()),
                path: "phase2/prompt.md".to_string(),
                phase_type: None,
                auto_advance: true,
                schema: Some("phase2/schema.json".to_string()),
                inputs: vec![PhaseInput {
                    id: "prev_analysis".to_string(),
                    from_phase: Some("phase-1".to_string()),
                    from_output: Some("analysis".to_string()),
                }],
                outputs: vec![PhaseOutput {
                    id: "final_result".to_string(),
                    filename: Some("result.json".to_string()),
                    description: Some("Final result".to_string()),
                }],
                fields: vec![],
            },
        ],
    };

    let yaml = serde_yaml::to_string(&workflow).unwrap();
    tokio::fs::write(wf_dir.join("workflow.yaml"), yaml)
        .await
        .unwrap();

    let workflow_store = WorkflowStore::new(workflows_tmp.path());
    let task_store = TaskStore::new(tasks_tmp.path());
    let event_bus = Arc::new(EventBus::new(256));
    let workflow_store_for_runner = WorkflowStore::new(workflows_tmp.path());

    let registry = WorkflowRegistry::new(workflow_store).await.unwrap();
    let workflow_registry = Arc::new(RwLock::new(registry));

    let runner = WorkflowRunner::new(
        workflow_registry,
        task_store,
        workflow_store_for_runner,
        event_bus,
    );

    (runner, workflows_tmp, tasks_tmp)
}

#[tokio::test]
async fn test_create_task_produces_valid_directory_and_snapshot() {
    let (runner, _wf_tmp, tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "My Project", Some("/tmp/work".to_string()), None)
        .await
        .unwrap();

    // Verify task ID format
    assert!(task_id.starts_with("test-wf_"));

    // Verify task directory exists
    let task_dir = tasks_tmp.path().join(&task_id);
    assert!(tokio::fs::try_exists(&task_dir).await.unwrap());
    assert!(tokio::fs::try_exists(task_dir.join("output")).await.unwrap());

    // Verify snapshot
    let snapshot = runner.get_task_state(&task_id).await.unwrap();
    assert_eq!(snapshot.workflow, "test-wf");
    assert_eq!(snapshot.project_name, "My Project");
    assert_eq!(
        snapshot.working_directory,
        Some("/tmp/work".to_string())
    );
    assert!(snapshot.phases.is_empty());
}

#[tokio::test]
async fn test_create_task_with_context() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task(
            "test-wf",
            "My Project",
            None,
            Some("Build a web app".to_string()),
        )
        .await
        .unwrap();

    let snapshot = runner.get_task_state(&task_id).await.unwrap();
    assert_eq!(
        snapshot.context.get("user_context"),
        Some(&"Build a web app".to_string())
    );
}

#[tokio::test]
async fn test_create_task_unknown_workflow() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;
    let result = runner
        .create_task("nonexistent", "Project", None, None)
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_get_next_phase_returns_first_phase() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    let next = runner.get_next_phase(&task_id).await.unwrap();
    assert!(next.is_some());
    assert_eq!(next.unwrap().id, "phase-1");
}

#[tokio::test]
async fn test_get_next_phase_returns_second_after_first_complete() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    write_phase1_output(&runner, &task_id).await;
    runner.complete_phase(&task_id, "phase-1").await.unwrap();

    let next = runner.get_next_phase(&task_id).await.unwrap();
    assert!(next.is_some());
    assert_eq!(next.unwrap().id, "phase-2");
}

#[tokio::test]
async fn test_get_next_phase_returns_none_when_all_done() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    write_phase1_output(&runner, &task_id).await;
    runner.complete_phase(&task_id, "phase-1").await.unwrap();
    write_phase2_output(&runner, &task_id).await;
    runner.complete_phase(&task_id, "phase-2").await.unwrap();

    let next = runner.get_next_phase(&task_id).await.unwrap();
    assert!(next.is_none());
}

#[tokio::test]
async fn test_get_next_phase_skipped_counts_as_done() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    runner
        .skip_phase(&task_id, "phase-1", "Not needed")
        .await
        .unwrap();

    let next = runner.get_next_phase(&task_id).await.unwrap();
    assert!(next.is_some());
    assert_eq!(next.unwrap().id, "phase-2");
}

#[tokio::test]
async fn test_build_phase_context_includes_all_sections() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task(
            "test-wf",
            "My Project",
            Some("/tmp/work".to_string()),
            Some("Build something".to_string()),
        )
        .await
        .unwrap();

    let phase = PhaseDefinition {
        id: "phase-1".to_string(),
        name: "Phase One".to_string(),
        intent: Some("Do the first thing".to_string()),
        path: "phase1/prompt.md".to_string(),
        phase_type: None,
        auto_advance: true,
        schema: None,
        inputs: vec![],
        outputs: vec![PhaseOutput {
            id: "analysis".to_string(),
            filename: Some("analysis.json".to_string()),
            description: Some("Analysis results".to_string()),
        }],
        fields: vec![],
    };

    let context = runner
        .build_phase_context(&task_id, &phase)
        .await
        .unwrap();

    // Verify all sections present
    assert!(context.contains("# Workflow: Test Workflow"));
    assert!(context.contains("## Task: My Project"));
    assert!(context.contains("Phase 1 of 2: Phase One"));
    assert!(context.contains("## Intent"));
    assert!(context.contains("Do the first thing"));
    assert!(context.contains("## Expected Outputs"));
    assert!(context.contains("analysis.json"));
    assert!(context.contains("## Instructions"));
    assert!(context.contains("# Phase 1"));
    assert!(context.contains("## Project Context"));
    assert!(context.contains("Build something"));
    assert!(context.contains("## Working Directory"));
    assert!(context.contains("/tmp/work"));
    assert!(context.contains("## Guidance"));
}

#[tokio::test]
async fn test_build_phase_context_with_inputs_and_schema() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    // Write output from phase-1 so phase-2 can read it
    runner
        .write_phase_output(&task_id, "analysis.json", r#"{"score": 42}"#)
        .await
        .unwrap();
    runner.complete_phase(&task_id, "phase-1").await.unwrap();

    let phase = PhaseDefinition {
        id: "phase-2".to_string(),
        name: "Phase Two".to_string(),
        intent: Some("Do the second thing".to_string()),
        path: "phase2/prompt.md".to_string(),
        phase_type: None,
        auto_advance: true,
        schema: Some("phase2/schema.json".to_string()),
        inputs: vec![PhaseInput {
            id: "prev_analysis".to_string(),
            from_phase: Some("phase-1".to_string()),
            from_output: Some("analysis".to_string()),
        }],
        outputs: vec![PhaseOutput {
            id: "final_result".to_string(),
            filename: Some("result.json".to_string()),
            description: Some("Final result".to_string()),
        }],
        fields: vec![],
    };

    let context = runner
        .build_phase_context(&task_id, &phase)
        .await
        .unwrap();

    // Verify inputs section with content
    assert!(context.contains("## Inputs"));
    assert!(context.contains("prev_analysis"));
    assert!(context.contains(r#"{"score": 42}"#));

    // Verify schema section
    assert!(context.contains("## Output Schema"));
    assert!(context.contains(r#""type": "object""#));
}

#[tokio::test]
async fn test_complete_phase_updates_snapshot() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    write_phase1_output(&runner, &task_id).await;
    runner.complete_phase(&task_id, "phase-1").await.unwrap();

    let snapshot = runner.get_task_state(&task_id).await.unwrap();
    let phase_state = snapshot.phases.get("phase-1").unwrap();
    assert!(matches!(phase_state.status, PhaseStatus::Completed));
    assert!(phase_state.completed_at.is_some());
}

#[tokio::test]
async fn test_skip_phase_updates_snapshot() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    runner
        .skip_phase(&task_id, "phase-1", "Not relevant")
        .await
        .unwrap();

    let snapshot = runner.get_task_state(&task_id).await.unwrap();
    let phase_state = snapshot.phases.get("phase-1").unwrap();
    assert!(matches!(phase_state.status, PhaseStatus::Skipped));
    assert!(phase_state.skipped_at.is_some());
    assert_eq!(phase_state.reason.as_deref(), Some("Not relevant"));
}

#[tokio::test]
async fn test_complete_phase_rejects_missing_outputs() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    // Mark phase-1 as Running so fail_phase fires on missing output
    runner.start_phase(&task_id, "phase-1").await.unwrap();

    // Try to complete phase-1 without writing analysis.json
    let result = runner.complete_phase(&task_id, "phase-1").await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("missing expected output"),
        "Error should mention missing output, got: {}",
        err_msg
    );
    assert!(
        err_msg.contains("Found in output dir"),
        "Error should include output dir listing, got: {}",
        err_msg
    );

    // Phase should be marked as failed
    let snapshot = runner.get_task_state(&task_id).await.unwrap();
    let phase_state = snapshot.phases.get("phase-1").unwrap();
    assert!(matches!(phase_state.status, PhaseStatus::Failed));
}

#[tokio::test]
async fn test_complete_phase_error_lists_actual_files_on_name_mismatch() {
    // Write a file with the WRONG name — error must list what's actually present
    // so the agent can spot the mismatch without a separate directory read.
    let (runner, _wf_tmp, tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    // phase-1 requires "analysis.json" but we write "analysis_results.json"
    let output_dir = tasks_tmp.path().join(&task_id).join("output");
    tokio::fs::write(
        output_dir.join("analysis_results.json"),
        r#"{"wrong": "filename"}"#,
    )
    .await
    .unwrap();

    let result = runner.complete_phase(&task_id, "phase-1").await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("analysis.json"),
        "Error should contain expected filename, got: {}",
        err_msg
    );
    assert!(
        err_msg.contains("analysis_results.json"),
        "Error should list actual file to show mismatch, got: {}",
        err_msg
    );
}

#[tokio::test]
async fn test_write_phase_output() {
    let (runner, _wf_tmp, tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    runner
        .write_phase_output(&task_id, "result.json", r#"{"done": true}"#)
        .await
        .unwrap();

    // Verify file exists
    let output_path = tasks_tmp
        .path()
        .join(&task_id)
        .join("output")
        .join("result.json");
    let content = tokio::fs::read_to_string(&output_path).await.unwrap();
    assert_eq!(content, r#"{"done": true}"#);
}

#[tokio::test]
async fn test_workflow_completed_event_emitted() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let mut rx = runner.event_bus.subscribe();

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    write_phase1_output(&runner, &task_id).await;
    runner.complete_phase(&task_id, "phase-1").await.unwrap();
    write_phase2_output(&runner, &task_id).await;
    runner.complete_phase(&task_id, "phase-2").await.unwrap();

    // Collect events and look for WorkflowCompleted
    let mut found_completed = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(
            event.payload,
            AgentEventPayload::WorkflowCompleted { .. }
        ) {
            found_completed = true;
        }
    }
    assert!(
        found_completed,
        "Should have emitted WorkflowCompleted event"
    );
}

#[tokio::test]
async fn test_events_emitted_on_create_and_complete() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let mut rx = runner.event_bus.subscribe();

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    write_phase1_output(&runner, &task_id).await;
    runner.complete_phase(&task_id, "phase-1").await.unwrap();

    let mut event_types = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event.payload {
            AgentEventPayload::WorkflowTaskCreated { .. } => {
                event_types.push("WorkflowTaskCreated");
            }
            AgentEventPayload::PhaseCompleted { .. } => {
                event_types.push("PhaseCompleted");
            }
            _ => {}
        }
    }

    assert!(event_types.contains(&"WorkflowTaskCreated"));
    assert!(event_types.contains(&"PhaseCompleted"));
}

/// Helper to set up a workflow with a folder phase (run.sh) that writes an output file.
async fn setup_folder_phase_env() -> (
    WorkflowRunner,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let workflows_tmp = tempfile::tempdir().unwrap();
    let tasks_tmp = tempfile::tempdir().unwrap();

    let wf_dir = workflows_tmp.path().join("folder-wf");
    tokio::fs::create_dir_all(&wf_dir).await.unwrap();

    // Phase 1 is a folder phase with run.sh
    let phase1_dir = wf_dir.join("phase1");
    tokio::fs::create_dir_all(&phase1_dir).await.unwrap();
    // run.sh writes output using WORKFLOW_OUTPUT_DIR env var
    let run_script = r#"#!/bin/bash
echo '{"result": "computed"}' > "$WORKFLOW_OUTPUT_DIR/analysis.json"
"#;
    tokio::fs::write(phase1_dir.join("run.sh"), run_script)
        .await
        .unwrap();
    // Make run.sh executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(phase1_dir.join("run.sh"), perms).unwrap();
    }

    // Phase 2 is a file phase (prompt.md)
    let phase2_dir = wf_dir.join("phase2");
    tokio::fs::create_dir_all(&phase2_dir).await.unwrap();
    tokio::fs::write(phase2_dir.join("prompt.md"), "# Phase 2\nUse the analysis.")
        .await
        .unwrap();

    let workflow = WorkflowDefinition {
        id: "folder-wf".to_string(),
        name: "Folder Workflow".to_string(),
        version: Some("1.0".to_string()),
        description: Some("Workflow with folder phase".to_string()),
        phases: vec![
            PhaseDefinition {
                id: "phase-1".to_string(),
                name: "Compute Phase".to_string(),
                intent: Some("Run computation script".to_string()),
                path: "phase1".to_string(), // directory, not a file
                phase_type: None,
                auto_advance: true,
                schema: None,
                inputs: vec![],
                outputs: vec![PhaseOutput {
                    id: "analysis".to_string(),
                    filename: Some("analysis.json".to_string()),
                    description: Some("Analysis output".to_string()),
                }],
                fields: vec![],
            },
            PhaseDefinition {
                id: "phase-2".to_string(),
                name: "Review Phase".to_string(),
                intent: Some("Review the analysis".to_string()),
                path: "phase2/prompt.md".to_string(), // file
                phase_type: None,
                auto_advance: true,
                schema: None,
                inputs: vec![PhaseInput {
                    id: "prev_analysis".to_string(),
                    from_phase: Some("phase-1".to_string()),
                    from_output: Some("analysis".to_string()),
                }],
                outputs: vec![],
                fields: vec![],
            },
        ],
    };

    let yaml = serde_yaml::to_string(&workflow).unwrap();
    tokio::fs::write(wf_dir.join("workflow.yaml"), yaml)
        .await
        .unwrap();

    let workflow_store = WorkflowStore::new(workflows_tmp.path());
    let task_store = TaskStore::new(tasks_tmp.path());
    let event_bus = Arc::new(EventBus::new(256));
    let workflow_store_for_runner = WorkflowStore::new(workflows_tmp.path());

    let registry = WorkflowRegistry::new(workflow_store).await.unwrap();
    let workflow_registry = Arc::new(RwLock::new(registry));

    let runner = WorkflowRunner::new(
        workflow_registry,
        task_store,
        workflow_store_for_runner,
        event_bus,
    );

    (runner, workflows_tmp, tasks_tmp)
}

#[tokio::test]
async fn test_is_folder_phase_detects_directory() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_folder_phase_env().await;

    let folder_phase = PhaseDefinition {
        id: "phase-1".to_string(),
        name: "Compute Phase".to_string(),
        intent: None,
        path: "phase1".to_string(),
        phase_type: None,
        auto_advance: true,
        schema: None,
        inputs: vec![],
        outputs: vec![],
        fields: vec![],
    };

    let file_phase = PhaseDefinition {
        id: "phase-2".to_string(),
        name: "Review Phase".to_string(),
        intent: None,
        path: "phase2/prompt.md".to_string(),
        phase_type: None,
        auto_advance: true,
        schema: None,
        inputs: vec![],
        outputs: vec![],
        fields: vec![],
    };

    assert!(runner.is_folder_phase("folder-wf", &folder_phase).await);
    assert!(!runner.is_folder_phase("folder-wf", &file_phase).await);
}

#[tokio::test]
async fn test_execute_folder_phase_runs_script_and_writes_output() {
    let (runner, _wf_tmp, tasks_tmp) = setup_folder_phase_env().await;

    let task_id = runner
        .create_task("folder-wf", "Test Project", None, None)
        .await
        .unwrap();

    let phase = PhaseDefinition {
        id: "phase-1".to_string(),
        name: "Compute Phase".to_string(),
        intent: Some("Run computation script".to_string()),
        path: "phase1".to_string(),
        phase_type: None,
        auto_advance: true,
        schema: None,
        inputs: vec![],
        outputs: vec![PhaseOutput {
            id: "analysis".to_string(),
            filename: Some("analysis.json".to_string()),
            description: Some("Analysis output".to_string()),
        }],
        fields: vec![],
    };

    runner
        .execute_folder_phase(&task_id, &phase)
        .await
        .unwrap();

    // Verify output file was written
    let output_path = tasks_tmp
        .path()
        .join(&task_id)
        .join("output")
        .join("analysis.json");
    let content = tokio::fs::read_to_string(&output_path).await.unwrap();
    assert!(content.contains("computed"));

    // Verify phase was marked as completed
    let snapshot = runner.get_task_state(&task_id).await.unwrap();
    let phase_state = snapshot.phases.get("phase-1").unwrap();
    assert!(matches!(phase_state.status, PhaseStatus::Completed));
}

#[tokio::test]
async fn test_execute_folder_phase_fails_on_missing_run_sh() {
    let (runner, wf_tmp, _tasks_tmp) = setup_folder_phase_env().await;

    // Create a directory without run.sh
    let no_script_dir = wf_tmp.path().join("folder-wf").join("no_script");
    tokio::fs::create_dir_all(&no_script_dir).await.unwrap();

    let task_id = runner
        .create_task("folder-wf", "Test Project", None, None)
        .await
        .unwrap();

    let phase = PhaseDefinition {
        id: "bad-phase".to_string(),
        name: "Bad Phase".to_string(),
        intent: None,
        path: "no_script".to_string(),
        phase_type: None,
        auto_advance: true,
        schema: None,
        inputs: vec![],
        outputs: vec![],
        fields: vec![],
    };

    let result = runner.execute_folder_phase(&task_id, &phase).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("missing run.sh"));
}

#[tokio::test]
async fn test_execute_folder_phase_fails_on_script_error() {
    let (runner, wf_tmp, _tasks_tmp) = setup_folder_phase_env().await;

    // Create a phase with a failing script
    let fail_dir = wf_tmp.path().join("folder-wf").join("fail_phase");
    tokio::fs::create_dir_all(&fail_dir).await.unwrap();
    tokio::fs::write(fail_dir.join("run.sh"), "#!/bin/bash\nexit 1\n")
        .await
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(fail_dir.join("run.sh"), perms).unwrap();
    }

    let task_id = runner
        .create_task("folder-wf", "Test Project", None, None)
        .await
        .unwrap();

    let phase = PhaseDefinition {
        id: "fail-phase".to_string(),
        name: "Fail Phase".to_string(),
        intent: None,
        path: "fail_phase".to_string(),
        phase_type: None,
        auto_advance: true,
        schema: None,
        inputs: vec![],
        outputs: vec![],
        fields: vec![],
    };

    let result = runner.execute_folder_phase(&task_id, &phase).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("failed with exit code"));
}

#[tokio::test]
async fn test_execute_folder_phase_fails_on_missing_output() {
    let (runner, wf_tmp, _tasks_tmp) = setup_folder_phase_env().await;

    // Create a phase that succeeds but doesn't write the expected output
    let no_output_dir = wf_tmp.path().join("folder-wf").join("no_output");
    tokio::fs::create_dir_all(&no_output_dir).await.unwrap();
    tokio::fs::write(no_output_dir.join("run.sh"), "#!/bin/bash\nexit 0\n")
        .await
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(no_output_dir.join("run.sh"), perms).unwrap();
    }

    let task_id = runner
        .create_task("folder-wf", "Test Project", None, None)
        .await
        .unwrap();

    let phase = PhaseDefinition {
        id: "no-output-phase".to_string(),
        name: "No Output Phase".to_string(),
        intent: None,
        path: "no_output".to_string(),
        phase_type: None,
        auto_advance: true,
        schema: None,
        inputs: vec![],
        outputs: vec![PhaseOutput {
            id: "expected".to_string(),
            filename: Some("expected.json".to_string()),
            description: Some("Expected output".to_string()),
        }],
        fields: vec![],
    };

    let result = runner.execute_folder_phase(&task_id, &phase).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("missing expected output"),
        "Error should mention missing output, got: {}",
        err_msg
    );
    assert!(
        err_msg.contains("Found in output dir"),
        "Error should include output dir listing, got: {}",
        err_msg
    );
}

#[tokio::test]
async fn test_advance_to_next_agent_phase_skips_folder_phases() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_folder_phase_env().await;

    let task_id = runner
        .create_task("folder-wf", "Test Project", None, None)
        .await
        .unwrap();

    // advance_to_next_agent_phase should auto-execute folder phase-1
    // and return context for file phase-2
    let context = runner
        .advance_to_next_agent_phase(&task_id)
        .await
        .unwrap();

    assert!(context.is_some(), "Should return context for file phase-2");
    let ctx = context.unwrap();
    assert!(ctx.contains("Review Phase"));

    // Verify folder phase-1 was auto-completed
    let snapshot = runner.get_task_state(&task_id).await.unwrap();
    let phase_state = snapshot.phases.get("phase-1").unwrap();
    assert!(matches!(phase_state.status, PhaseStatus::Completed));

    // phase-2 should not be in the snapshot yet (agent needs to complete it)
    assert!(!snapshot.phases.contains_key("phase-2"));
}

#[tokio::test]
async fn test_execute_folder_phase_emits_started_event() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_folder_phase_env().await;

    let mut rx = runner.event_bus.subscribe();

    let task_id = runner
        .create_task("folder-wf", "Test Project", None, None)
        .await
        .unwrap();

    let phase = PhaseDefinition {
        id: "phase-1".to_string(),
        name: "Compute Phase".to_string(),
        intent: Some("Run computation script".to_string()),
        path: "phase1".to_string(),
        phase_type: None,
        auto_advance: true,
        schema: None,
        inputs: vec![],
        outputs: vec![PhaseOutput {
            id: "analysis".to_string(),
            filename: Some("analysis.json".to_string()),
            description: Some("Analysis output".to_string()),
        }],
        fields: vec![],
    };

    runner
        .execute_folder_phase(&task_id, &phase)
        .await
        .unwrap();

    let mut found_started = false;
    let mut found_completed = false;
    while let Ok(event) = rx.try_recv() {
        match &event.payload {
            AgentEventPayload::PhaseStarted { phase_id, .. } if phase_id == "phase-1" => {
                found_started = true;
            }
            AgentEventPayload::PhaseCompleted { phase_id, .. } if phase_id == "phase-1" => {
                found_completed = true;
            }
            _ => {}
        }
    }
    assert!(found_started, "Should emit PhaseStarted event");
    assert!(found_completed, "Should emit PhaseCompleted event");
}

#[tokio::test]
async fn test_execute_folder_phase_with_input_args() {
    let (runner, wf_tmp, tasks_tmp) = setup_folder_phase_env().await;

    // Create a phase that reads an input and copies it to output
    let input_phase_dir = wf_tmp.path().join("folder-wf").join("input_phase");
    tokio::fs::create_dir_all(&input_phase_dir).await.unwrap();
    let script = r#"#!/bin/bash
cp "$WORKFLOW_INPUT_PREV" "$WORKFLOW_OUTPUT_DIR/copied.json"
"#;
    tokio::fs::write(input_phase_dir.join("run.sh"), script)
        .await
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(input_phase_dir.join("run.sh"), perms).unwrap();
    }

    let task_id = runner
        .create_task("folder-wf", "Test Project", None, None)
        .await
        .unwrap();

    // Pre-write the input file using the actual output filename from phase-1
    runner
        .write_phase_output(&task_id, "analysis.json", r#"{"data": "hello"}"#)
        .await
        .unwrap();

    let phase = PhaseDefinition {
        id: "input-phase".to_string(),
        name: "Input Phase".to_string(),
        intent: None,
        path: "input_phase".to_string(),
        phase_type: None,
        auto_advance: true,
        schema: None,
        inputs: vec![PhaseInput {
            id: "prev".to_string(),
            from_phase: Some("phase-1".to_string()),
            from_output: Some("analysis".to_string()),
        }],
        outputs: vec![PhaseOutput {
            id: "copied".to_string(),
            filename: Some("copied.json".to_string()),
            description: None,
        }],
        fields: vec![],
    };

    runner
        .execute_folder_phase(&task_id, &phase)
        .await
        .unwrap();

    // Verify the input was copied to the output
    let output_path = tasks_tmp
        .path()
        .join(&task_id)
        .join("output")
        .join("copied.json");
    let content = tokio::fs::read_to_string(&output_path).await.unwrap();
    assert!(content.contains("hello"));
}

#[tokio::test]
async fn test_resolve_output_filename_with_json_extension() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    // phase-1 has output "analysis" with filename "analysis.json"
    let filename = runner
        .resolve_output_filename("test-wf", "phase-1", "analysis")
        .await;
    assert_eq!(filename, "analysis.json");
}

#[tokio::test]
async fn test_resolve_output_filename_fallback_to_txt() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    // Non-existent output ID should fall back to {id}.txt
    let filename = runner
        .resolve_output_filename("test-wf", "phase-1", "nonexistent")
        .await;
    assert_eq!(filename, "nonexistent.txt");

    // Non-existent phase should also fall back
    let filename = runner
        .resolve_output_filename("test-wf", "no-such-phase", "anything")
        .await;
    assert_eq!(filename, "anything.txt");

    // Non-existent workflow should also fall back
    let filename = runner
        .resolve_output_filename("no-such-workflow", "phase-1", "analysis")
        .await;
    assert_eq!(filename, "analysis.txt");
}

#[tokio::test]
async fn test_workflow_failed_event_on_failed_phase() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let mut rx = runner.event_bus.subscribe();

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    // Complete phase 1, fail phase 2
    write_phase1_output(&runner, &task_id).await;
    runner.complete_phase(&task_id, "phase-1").await.unwrap();
    runner
        .fail_phase(&task_id, "phase-2", "Script failed with exit code 1")
        .await
        .unwrap();

    // Collect events and look for WorkflowTaskFailed (not WorkflowCompleted)
    let mut found_failed = false;
    let mut found_completed = false;
    while let Ok(event) = rx.try_recv() {
        match &event.payload {
            AgentEventPayload::WorkflowTaskFailed { task_id: tid, error } => {
                assert_eq!(tid, &task_id);
                assert!(error.contains("phase-2"));
                found_failed = true;
            }
            AgentEventPayload::WorkflowCompleted { .. } => {
                found_completed = true;
            }
            _ => {}
        }
    }
    assert!(
        found_failed,
        "Should have emitted WorkflowTaskFailed event"
    );
    assert!(
        !found_completed,
        "Should NOT have emitted WorkflowCompleted when a phase failed"
    );
}

#[tokio::test]
async fn test_failed_phase_is_terminal_in_completion_check() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    // Fail phase 1 — workflow should NOT be complete yet (phase-2 still pending)
    let mut rx = runner.event_bus.subscribe();
    runner
        .fail_phase(&task_id, "phase-1", "Something went wrong")
        .await
        .unwrap();

    let mut found_task_failed = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event.payload, AgentEventPayload::WorkflowTaskFailed { .. }) {
            found_task_failed = true;
        }
    }
    assert!(
        !found_task_failed,
        "Should NOT emit WorkflowTaskFailed when only one phase failed and others are pending"
    );

    // Now skip phase 2 — all phases are terminal, workflow should be marked failed
    let mut rx2 = runner.event_bus.subscribe();
    runner
        .skip_phase(&task_id, "phase-2", "Skipping due to prior failure")
        .await
        .unwrap();

    let mut found_task_failed2 = false;
    while let Ok(event) = rx2.try_recv() {
        if matches!(event.payload, AgentEventPayload::WorkflowTaskFailed { .. }) {
            found_task_failed2 = true;
        }
    }
    assert!(
        found_task_failed2,
        "Should emit WorkflowTaskFailed when all phases are terminal and at least one failed"
    );
}

#[tokio::test]
async fn test_pause_phase_on_missing_inputs() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    // Phase-2 has input from phase-1 output "analysis" (resolved to analysis.json)
    // We do NOT write phase-1 output, so inputs should be missing

    let missing = runner
        .check_inputs_available(
            &task_id,
            &PhaseDefinition {
                id: "phase-2".to_string(),
                name: "Phase Two".to_string(),
                intent: Some("Do the second thing".to_string()),
                path: "phase2/prompt.md".to_string(),
                phase_type: None,
                auto_advance: true,
                schema: Some("phase2/schema.json".to_string()),
                inputs: vec![PhaseInput {
                    id: "prev_analysis".to_string(),
                    from_phase: Some("phase-1".to_string()),
                    from_output: Some("analysis".to_string()),
                }],
                outputs: vec![],
                fields: vec![],
            },
        )
        .await
        .unwrap();

    assert_eq!(missing, vec!["prev_analysis"]);

    // Now pause the phase
    runner
        .pause_phase(&task_id, "phase-2", "Missing required inputs: prev_analysis")
        .await
        .unwrap();

    let snapshot = runner.get_task_state(&task_id).await.unwrap();
    let phase_state = snapshot.phases.get("phase-2").unwrap();
    assert!(matches!(phase_state.status, PhaseStatus::Paused));
    assert_eq!(
        phase_state.paused_reason,
        Some("Missing required inputs: prev_analysis".to_string())
    );
}

#[tokio::test]
async fn test_phase_proceeds_when_inputs_available() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    // Write the output that phase-2 depends on
    write_phase1_output(&runner, &task_id).await;

    let missing = runner
        .check_inputs_available(
            &task_id,
            &PhaseDefinition {
                id: "phase-2".to_string(),
                name: "Phase Two".to_string(),
                intent: None,
                path: "phase2/prompt.md".to_string(),
                phase_type: None,
                auto_advance: true,
                schema: None,
                inputs: vec![PhaseInput {
                    id: "prev_analysis".to_string(),
                    from_phase: Some("phase-1".to_string()),
                    from_output: Some("analysis".to_string()),
                }],
                outputs: vec![],
                fields: vec![],
            },
        )
        .await
        .unwrap();

    assert!(missing.is_empty(), "All inputs should be available");
}

#[tokio::test]
async fn test_get_next_phase_returns_paused_phase() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    // Complete phase-1
    write_phase1_output(&runner, &task_id).await;
    runner.complete_phase(&task_id, "phase-1").await.unwrap();

    // Pause phase-2
    runner
        .pause_phase(&task_id, "phase-2", "Missing inputs")
        .await
        .unwrap();

    // get_next_phase should return phase-2 (paused is re-evaluable)
    let next = runner.get_next_phase(&task_id).await.unwrap();
    assert!(next.is_some());
    assert_eq!(next.unwrap().id, "phase-2");
}

#[tokio::test]
async fn test_clear_paused_phases_removes_paused() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    // Pause phase-1
    runner
        .pause_phase(&task_id, "phase-1", "Missing inputs")
        .await
        .unwrap();

    // Verify it's paused
    let snapshot = runner.get_task_state(&task_id).await.unwrap();
    assert!(matches!(
        snapshot.phases.get("phase-1").unwrap().status,
        PhaseStatus::Paused
    ));

    // Clear paused phases
    let cleared = runner.clear_paused_phases(&task_id).await.unwrap();
    assert!(cleared);

    // Verify phase-1 is removed from phases map
    let snapshot = runner.get_task_state(&task_id).await.unwrap();
    assert!(!snapshot.phases.contains_key("phase-1"));

    // get_next_phase should return phase-1 again (it's no longer in the map)
    let next = runner.get_next_phase(&task_id).await.unwrap();
    assert!(next.is_some());
    assert_eq!(next.unwrap().id, "phase-1");
}

#[tokio::test]
async fn test_clear_paused_phases_returns_false_when_none_paused() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    // No phases are paused
    let cleared = runner.clear_paused_phases(&task_id).await.unwrap();
    assert!(!cleared);
}

#[tokio::test]
async fn test_clear_paused_phases_preserves_completed() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    // Complete phase-1
    write_phase1_output(&runner, &task_id).await;
    runner.complete_phase(&task_id, "phase-1").await.unwrap();

    // Pause phase-2
    runner
        .pause_phase(&task_id, "phase-2", "Missing inputs")
        .await
        .unwrap();

    // Clear paused phases
    let cleared = runner.clear_paused_phases(&task_id).await.unwrap();
    assert!(cleared);

    // phase-1 should still be completed
    let snapshot = runner.get_task_state(&task_id).await.unwrap();
    assert!(matches!(
        snapshot.phases.get("phase-1").unwrap().status,
        PhaseStatus::Completed
    ));
    // phase-2 should be removed
    assert!(!snapshot.phases.contains_key("phase-2"));
}

#[tokio::test]
async fn test_phase_required_outputs_returns_filenames() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;
    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    let outputs = runner
        .phase_required_outputs(&task_id, "phase-1")
        .await
        .unwrap();
    assert_eq!(outputs, vec!["analysis.json"]);

    let outputs2 = runner
        .phase_required_outputs(&task_id, "phase-2")
        .await
        .unwrap();
    assert_eq!(outputs2, vec!["result.json"]);
}

#[tokio::test]
async fn test_phase_required_outputs_invalid_phase_returns_err() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;
    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    let result = runner
        .phase_required_outputs(&task_id, "no-such-phase")
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_phase_write_progress_summary_partial() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;
    let task_id = runner
        .create_task("test-wf", "Project", None, None)
        .await
        .unwrap();

    // phase-1 requires analysis.json; nothing written yet
    // After writing analysis.json → all present
    runner
        .write_phase_output(&task_id, "analysis.json", r#"{}"#)
        .await
        .unwrap();
    let summary = runner
        .phase_write_progress_summary(&task_id, "analysis.json")
        .await
        .unwrap();
    assert!(
        summary.contains("all 1 required output"),
        "got: {}",
        summary
    );
    assert!(summary.contains("WorkflowActionCompletePhase"));
}

#[tokio::test]
async fn test_phase_write_progress_summary_all_present_multiple() {
    // Build a workflow with two required outputs on phase-1
    let workflows_tmp = tempfile::tempdir().unwrap();
    let tasks_tmp = tempfile::tempdir().unwrap();

    let wf_dir = workflows_tmp.path().join("two-output-wf");
    tokio::fs::create_dir_all(&wf_dir).await.unwrap();
    let phase1_dir = wf_dir.join("phase1");
    tokio::fs::create_dir_all(&phase1_dir).await.unwrap();
    tokio::fs::write(phase1_dir.join("prompt.md"), "# Phase 1").await.unwrap();

    let workflow = WorkflowDefinition {
        id: "two-output-wf".to_string(),
        name: "Two Output Workflow".to_string(),
        version: None,
        description: None,
        phases: vec![PhaseDefinition {
            id: "phase-1".to_string(),
            name: "Phase One".to_string(),
            intent: None,
            path: "phase1/prompt.md".to_string(),
            phase_type: None,
            auto_advance: true,
            schema: None,
            inputs: vec![],
            outputs: vec![
                PhaseOutput {
                    id: "a".to_string(),
                    filename: Some("a.md".to_string()),
                    description: None,
                },
                PhaseOutput {
                    id: "b".to_string(),
                    filename: Some("b.md".to_string()),
                    description: None,
                },
            ],
            fields: vec![],
        }],
    };
    let yaml = serde_yaml::to_string(&workflow).unwrap();
    tokio::fs::write(wf_dir.join("workflow.yaml"), yaml).await.unwrap();

    let workflow_store = WorkflowStore::new(workflows_tmp.path());
    let task_store = TaskStore::new(tasks_tmp.path());
    let event_bus = Arc::new(EventBus::new(256));
    let ws2 = WorkflowStore::new(workflows_tmp.path());
    let registry = WorkflowRegistry::new(workflow_store).await.unwrap();
    let runner = WorkflowRunner::new(
        Arc::new(RwLock::new(registry)),
        task_store,
        ws2,
        event_bus,
    );

    let task_id = runner
        .create_task("two-output-wf", "P", None, None)
        .await
        .unwrap();

    // Write only a.md first — partial
    runner.write_phase_output(&task_id, "a.md", "content").await.unwrap();
    let summary = runner
        .phase_write_progress_summary(&task_id, "a.md")
        .await
        .unwrap();
    assert!(summary.contains("1/2"), "got: {}", summary);
    assert!(summary.contains("b.md"), "got: {}", summary);

    // Write b.md — all present
    runner.write_phase_output(&task_id, "b.md", "content").await.unwrap();
    let summary2 = runner
        .phase_write_progress_summary(&task_id, "b.md")
        .await
        .unwrap();
    assert!(summary2.contains("all 2 required outputs"), "got: {}", summary2);
}

#[tokio::test]
async fn test_phase_write_progress_summary_no_required_outputs_returns_none() {
    // Build a workflow with a free-form phase (no declared outputs)
    let workflows_tmp = tempfile::tempdir().unwrap();
    let tasks_tmp = tempfile::tempdir().unwrap();

    let wf_dir = workflows_tmp.path().join("free-form-wf");
    tokio::fs::create_dir_all(&wf_dir).await.unwrap();
    let phase1_dir = wf_dir.join("phase1");
    tokio::fs::create_dir_all(&phase1_dir).await.unwrap();
    tokio::fs::write(phase1_dir.join("prompt.md"), "# Phase 1").await.unwrap();

    let workflow = WorkflowDefinition {
        id: "free-form-wf".to_string(),
        name: "Free Form Workflow".to_string(),
        version: None,
        description: None,
        phases: vec![PhaseDefinition {
            id: "phase-1".to_string(),
            name: "Phase One".to_string(),
            intent: None,
            path: "phase1/prompt.md".to_string(),
            phase_type: None,
            auto_advance: true,
            schema: None,
            inputs: vec![],
            outputs: vec![], // no required outputs
            fields: vec![],
        }],
    };
    let yaml = serde_yaml::to_string(&workflow).unwrap();
    tokio::fs::write(wf_dir.join("workflow.yaml"), yaml).await.unwrap();

    let workflow_store = WorkflowStore::new(workflows_tmp.path());
    let task_store = TaskStore::new(tasks_tmp.path());
    let event_bus = Arc::new(EventBus::new(256));
    let ws2 = WorkflowStore::new(workflows_tmp.path());
    let registry = WorkflowRegistry::new(workflow_store).await.unwrap();
    let runner = WorkflowRunner::new(
        Arc::new(RwLock::new(registry)),
        task_store,
        ws2,
        event_bus,
    );

    let task_id = runner
        .create_task("free-form-wf", "P", None, None)
        .await
        .unwrap();
    runner.write_phase_output(&task_id, "anything.txt", "content").await.unwrap();

    let summary = runner
        .phase_write_progress_summary(&task_id, "anything.txt")
        .await;
    assert!(summary.is_none());
}

#[tokio::test]
async fn test_phase_write_progress_summary_bad_task_returns_none() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;
    // Non-existent task should return None gracefully (not panic or error)
    let summary = runner
        .phase_write_progress_summary("no-such-task", "file.txt")
        .await;
    assert!(summary.is_none());
}

// -----------------------------------------------------------------------
// reopen_task tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_reopen_completed_task_rewinds_phase_and_sets_pending() {
    let (runner, _wf_tmp, tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Proj", None, None)
        .await
        .unwrap();

    // Simulate task completed with phase-1 done
    let mut snap = runner.get_task_state(&task_id).await.unwrap();
    snap.status = TaskStatus::Completed;
    snap.phases.insert(
        "phase-1".to_string(),
        PhaseState { status: PhaseStatus::Completed, completed_at: Some(Utc::now()),
            skipped_at: None, started_at: None, reason: None, error: None,
            failed_at: None, paused_reason: None, input_tokens: None, output_tokens: None },
    );
    snap.phases.insert(
        "phase-2".to_string(),
        PhaseState { status: PhaseStatus::Completed, completed_at: Some(Utc::now()),
            skipped_at: None, started_at: None, reason: None, error: None,
            failed_at: None, paused_reason: None, input_tokens: None, output_tokens: None },
    );
    runner.task_store.write_task_snapshot(&task_id, &snap).await.unwrap();

    // Write an output file to verify outputs are preserved
    runner.write_phase_output(&task_id, "result.txt", "content").await.unwrap();

    let file_count = runner.reopen_task(&task_id, "phase-1").await.unwrap();

    let updated = runner.get_task_state(&task_id).await.unwrap();
    assert_eq!(updated.status, TaskStatus::Pending, "task should be Pending after reopen");
    assert!(!updated.phases.contains_key("phase-1"), "phase-1 should be removed (pending)");
    assert!(updated.phases.contains_key("phase-2"), "phase-2 should still be present");

    // Output files are preserved
    let output_dir = tasks_tmp.path().join(&task_id).join("output");
    assert!(output_dir.join("result.txt").exists(), "output file should be preserved");
    assert_eq!(file_count, 1, "reopen_task should return file count");
}

#[tokio::test]
async fn test_reopen_failed_task_succeeds() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Proj", None, None)
        .await
        .unwrap();

    let mut snap = runner.get_task_state(&task_id).await.unwrap();
    snap.status = TaskStatus::Failed;
    snap.phases.insert(
        "phase-1".to_string(),
        PhaseState { status: PhaseStatus::Failed, completed_at: None,
            skipped_at: None, started_at: None, reason: None,
            error: Some("script error".into()), failed_at: Some(Utc::now()),
            paused_reason: None, input_tokens: None, output_tokens: None },
    );
    runner.task_store.write_task_snapshot(&task_id, &snap).await.unwrap();

    runner.reopen_task(&task_id, "phase-1").await.unwrap();

    let updated = runner.get_task_state(&task_id).await.unwrap();
    assert_eq!(updated.status, TaskStatus::Pending);
    assert!(!updated.phases.contains_key("phase-1"));
}

#[tokio::test]
async fn test_reopen_stopped_task_succeeds() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Proj", None, None)
        .await
        .unwrap();

    let mut snap = runner.get_task_state(&task_id).await.unwrap();
    snap.status = TaskStatus::Stopped;
    runner.task_store.write_task_snapshot(&task_id, &snap).await.unwrap();

    runner.reopen_task(&task_id, "phase-1").await.unwrap();

    let updated = runner.get_task_state(&task_id).await.unwrap();
    assert_eq!(updated.status, TaskStatus::Pending);
}

#[tokio::test]
async fn test_reopen_running_task_returns_error() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Proj", None, None)
        .await
        .unwrap();

    let mut snap = runner.get_task_state(&task_id).await.unwrap();
    snap.status = TaskStatus::Running;
    runner.task_store.write_task_snapshot(&task_id, &snap).await.unwrap();

    let err = runner.reopen_task(&task_id, "phase-1").await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("terminal"), "expected terminal-state error, got: {}", msg);
}

#[tokio::test]
async fn test_reopen_invalid_phase_id_lists_valid_ids() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let task_id = runner
        .create_task("test-wf", "Proj", None, None)
        .await
        .unwrap();

    let mut snap = runner.get_task_state(&task_id).await.unwrap();
    snap.status = TaskStatus::Completed;
    runner.task_store.write_task_snapshot(&task_id, &snap).await.unwrap();

    let err = runner.reopen_task(&task_id, "no-such-phase").await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("no-such-phase"), "got: {}", msg);
    assert!(msg.contains("Valid phase IDs"), "got: {}", msg);
    assert!(msg.contains("phase-1"), "got: {}", msg);
}

#[tokio::test]
async fn test_reopen_nonexistent_task_returns_error() {
    let (runner, _wf_tmp, _tasks_tmp) = setup_test_env().await;

    let err = runner.reopen_task("no-such-task", "phase-1").await;
    assert!(err.is_err(), "expected error for unknown task");
}
