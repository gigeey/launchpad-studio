use super::WorkflowActionWriteOutput;
use super::super::tests::MockWorkflowRunner;
use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput, WorkflowRunnerHandle};
use ao_protocol::error::AoError;
use ao_protocol::workflow::{PhaseDefinition, PhaseState, PhaseStatus, TaskSnapshot};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

fn ctx_with_runner() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_workflow_runner(Arc::new(MockWorkflowRunner))
}

fn ctx_no_runner() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
}

#[tokio::test]
async fn happy_path_writes_output() {
    let ctx = ctx_with_runner();
    let out = WorkflowActionWriteOutput
        .invoke(
            json!({"task_id": "t1", "filename": "result.json", "content": "{}"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("result.json")),
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn missing_workflow_runner_returns_non_recoverable_error() {
    let ctx = ctx_no_runner();
    let out = WorkflowActionWriteOutput
        .invoke(
            json!({"task_id": "t1", "filename": "f.json", "content": "x"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(!recoverable),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[test]
fn cli_compatible_is_true() {
    assert!(WorkflowActionWriteOutput.cli_compatible());
}

// --- Mocks that exercise the progress-summary path ---

/// Runner that returns non-empty required outputs and a known progress string.
struct MockRunnerAllPresent;

#[async_trait]
impl WorkflowRunnerHandle for MockRunnerAllPresent {
    async fn create_task(&self, _: &str, _: &str, _: Option<String>, _: Option<String>) -> Result<String, AoError> { Ok("t".into()) }
    async fn build_create_summary(&self, _: &str, _: &str) -> Result<String, AoError> { Ok(String::new()) }
    async fn write_phase_output(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn complete_phase(&self, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn skip_phase(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn start_task(&self, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn delete_task(&self, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn get_task_state(&self, _: &str) -> Result<TaskSnapshot, AoError> {
        Ok(TaskSnapshot {
            status: ao_protocol::workflow::TaskStatus::Pending,
            workflow: "mock-wf".into(),
            workflow_version: None,
            created: Utc::now(),
            project_name: "p".into(),
            working_directory: None,
            context: HashMap::new(),
            phases: HashMap::new(),
        })
    }
    async fn get_next_phase(&self, _: &str) -> Result<Option<PhaseDefinition>, AoError> { Ok(None) }
    async fn notify_phase_completed(&self, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn get_workflow_summaries(&self, _: Option<&[String]>) -> Vec<ao_protocol::workflow::WorkflowSummary> { vec![] }
    async fn stop_task(&self, _: &str) -> Result<std::path::PathBuf, AoError> { Ok(std::path::PathBuf::from("/tmp/output")) }
    async fn phase_required_outputs(&self, _: &str, _: &str) -> Result<Vec<String>, AoError> {
        Ok(vec!["out.md".into()])
    }
    async fn phase_write_progress_summary(&self, _: &str, _: &str) -> Option<String> {
        Some("Phase 'p1' now has all 1 required output. Call WorkflowActionCompletePhase to advance.".into())
    }
    async fn reopen_task(&self, _: &str, _: &str) -> Result<usize, AoError> { Ok(0) }
}

/// Runner that returns a partial-progress summary.
struct MockRunnerPartial;

#[async_trait]
impl WorkflowRunnerHandle for MockRunnerPartial {
    async fn create_task(&self, _: &str, _: &str, _: Option<String>, _: Option<String>) -> Result<String, AoError> { Ok("t".into()) }
    async fn build_create_summary(&self, _: &str, _: &str) -> Result<String, AoError> { Ok(String::new()) }
    async fn write_phase_output(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn complete_phase(&self, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn skip_phase(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn start_task(&self, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn delete_task(&self, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn get_task_state(&self, _: &str) -> Result<TaskSnapshot, AoError> {
        Ok(TaskSnapshot {
            status: ao_protocol::workflow::TaskStatus::Pending,
            workflow: "mock-wf".into(),
            workflow_version: None,
            created: Utc::now(),
            project_name: "p".into(),
            working_directory: None,
            context: HashMap::new(),
            phases: HashMap::new(),
        })
    }
    async fn get_next_phase(&self, _: &str) -> Result<Option<PhaseDefinition>, AoError> { Ok(None) }
    async fn notify_phase_completed(&self, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn get_workflow_summaries(&self, _: Option<&[String]>) -> Vec<ao_protocol::workflow::WorkflowSummary> { vec![] }
    async fn stop_task(&self, _: &str) -> Result<std::path::PathBuf, AoError> { Ok(std::path::PathBuf::from("/tmp/output")) }
    async fn phase_required_outputs(&self, _: &str, _: &str) -> Result<Vec<String>, AoError> {
        Ok(vec!["a.md".into(), "b.md".into()])
    }
    async fn phase_write_progress_summary(&self, _: &str, _: &str) -> Option<String> {
        Some("Phase 'p1' now has 1/2 required outputs. Still missing: [b.md].".into())
    }
    async fn reopen_task(&self, _: &str, _: &str) -> Result<usize, AoError> { Ok(0) }
}

/// Runner that returns None for progress (free-form or error case).
struct MockRunnerNoProgress;

#[async_trait]
impl WorkflowRunnerHandle for MockRunnerNoProgress {
    async fn create_task(&self, _: &str, _: &str, _: Option<String>, _: Option<String>) -> Result<String, AoError> { Ok("t".into()) }
    async fn build_create_summary(&self, _: &str, _: &str) -> Result<String, AoError> { Ok(String::new()) }
    async fn write_phase_output(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn complete_phase(&self, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn skip_phase(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn start_task(&self, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn delete_task(&self, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn get_task_state(&self, _: &str) -> Result<TaskSnapshot, AoError> {
        Ok(TaskSnapshot {
            status: ao_protocol::workflow::TaskStatus::Pending,
            workflow: "mock-wf".into(),
            workflow_version: None,
            created: Utc::now(),
            project_name: "p".into(),
            working_directory: None,
            context: HashMap::new(),
            phases: HashMap::new(),
        })
    }
    async fn get_next_phase(&self, _: &str) -> Result<Option<PhaseDefinition>, AoError> { Ok(None) }
    async fn notify_phase_completed(&self, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn get_workflow_summaries(&self, _: Option<&[String]>) -> Vec<ao_protocol::workflow::WorkflowSummary> { vec![] }
    async fn stop_task(&self, _: &str) -> Result<std::path::PathBuf, AoError> { Ok(std::path::PathBuf::from("/tmp/output")) }
    async fn phase_required_outputs(&self, _: &str, _: &str) -> Result<Vec<String>, AoError> {
        Ok(vec![])
    }
    async fn phase_write_progress_summary(&self, _: &str, _: &str) -> Option<String> {
        None
    }
    async fn reopen_task(&self, _: &str, _: &str) -> Result<usize, AoError> { Ok(0) }
}

fn ctx_with(runner: impl WorkflowRunnerHandle + 'static) -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
        .with_workflow_runner(Arc::new(runner))
}

#[tokio::test]
async fn progress_all_present_appended_to_message() {
    let ctx = ctx_with(MockRunnerAllPresent);
    let out = WorkflowActionWriteOutput
        .invoke(
            json!({"task_id": "t1", "filename": "out.md", "content": "hello"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("Output written to 'out.md'."), "got: {}", s);
            assert!(s.contains("all 1 required output"), "got: {}", s);
            assert!(s.contains("WorkflowActionCompletePhase"), "got: {}", s);
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn progress_partial_shows_missing() {
    let ctx = ctx_with(MockRunnerPartial);
    let out = WorkflowActionWriteOutput
        .invoke(
            json!({"task_id": "t1", "filename": "a.md", "content": "hello"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert!(s.contains("Output written to 'a.md'."), "got: {}", s);
            assert!(s.contains("1/2"), "got: {}", s);
            assert!(s.contains("b.md"), "got: {}", s);
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn progress_none_keeps_original_message() {
    let ctx = ctx_with(MockRunnerNoProgress);
    let out = WorkflowActionWriteOutput
        .invoke(
            json!({"task_id": "t1", "filename": "free.txt", "content": "hello"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => {
            assert_eq!(s, "Output written to 'free.txt'.");
        }
        other => panic!("expected Text, got {:?}", other),
    }
}

// --- PRD passes validation tests ---

/// Runner whose get_task_state returns an empty phases map (prd phase not yet completed).
struct MockRunnerPrdPhaseActive;

#[async_trait]
impl WorkflowRunnerHandle for MockRunnerPrdPhaseActive {
    async fn create_task(&self, _: &str, _: &str, _: Option<String>, _: Option<String>) -> Result<String, AoError> { Ok("t".into()) }
    async fn build_create_summary(&self, _: &str, _: &str) -> Result<String, AoError> { Ok(String::new()) }
    async fn write_phase_output(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn complete_phase(&self, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn skip_phase(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn start_task(&self, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn delete_task(&self, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn get_task_state(&self, _: &str) -> Result<TaskSnapshot, AoError> {
        Ok(TaskSnapshot {
            status: ao_protocol::workflow::TaskStatus::Running,
            workflow: "ralph".into(),
            workflow_version: None,
            created: Utc::now(),
            project_name: "p".into(),
            working_directory: None,
            context: HashMap::new(),
            phases: HashMap::new(), // prd phase not completed
        })
    }
    async fn get_next_phase(&self, _: &str) -> Result<Option<PhaseDefinition>, AoError> { Ok(None) }
    async fn notify_phase_completed(&self, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn get_workflow_summaries(&self, _: Option<&[String]>) -> Vec<ao_protocol::workflow::WorkflowSummary> { vec![] }
    async fn stop_task(&self, _: &str) -> Result<PathBuf, AoError> { Ok(PathBuf::from("/tmp")) }
    async fn phase_required_outputs(&self, _: &str, _: &str) -> Result<Vec<String>, AoError> { Ok(vec![]) }
    async fn phase_write_progress_summary(&self, _: &str, _: &str) -> Option<String> { None }
    async fn reopen_task(&self, _: &str, _: &str) -> Result<usize, AoError> { Ok(0) }
}

/// Runner whose get_task_state returns prd phase as Completed (implementation phase).
struct MockRunnerPrdPhaseCompleted;

#[async_trait]
impl WorkflowRunnerHandle for MockRunnerPrdPhaseCompleted {
    async fn create_task(&self, _: &str, _: &str, _: Option<String>, _: Option<String>) -> Result<String, AoError> { Ok("t".into()) }
    async fn build_create_summary(&self, _: &str, _: &str) -> Result<String, AoError> { Ok(String::new()) }
    async fn write_phase_output(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn complete_phase(&self, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn skip_phase(&self, _: &str, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn start_task(&self, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn delete_task(&self, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn get_task_state(&self, _: &str) -> Result<TaskSnapshot, AoError> {
        let mut phases = HashMap::new();
        phases.insert("prd".into(), PhaseState {
            status: PhaseStatus::Completed,
            completed_at: Some(Utc::now()),
            skipped_at: None,
            started_at: Some(Utc::now()),
            reason: None,
            error: None,
            failed_at: None,
            paused_reason: None,
            input_tokens: None,
            output_tokens: None,
        });
        Ok(TaskSnapshot {
            status: ao_protocol::workflow::TaskStatus::Running,
            workflow: "ralph".into(),
            workflow_version: None,
            created: Utc::now(),
            project_name: "p".into(),
            working_directory: None,
            context: HashMap::new(),
            phases,
        })
    }
    async fn get_next_phase(&self, _: &str) -> Result<Option<PhaseDefinition>, AoError> { Ok(None) }
    async fn notify_phase_completed(&self, _: &str, _: &str) -> Result<(), AoError> { Ok(()) }
    async fn get_workflow_summaries(&self, _: Option<&[String]>) -> Vec<ao_protocol::workflow::WorkflowSummary> { vec![] }
    async fn stop_task(&self, _: &str) -> Result<PathBuf, AoError> { Ok(PathBuf::from("/tmp")) }
    async fn phase_required_outputs(&self, _: &str, _: &str) -> Result<Vec<String>, AoError> { Ok(vec![]) }
    async fn phase_write_progress_summary(&self, _: &str, _: &str) -> Option<String> { None }
    async fn reopen_task(&self, _: &str, _: &str) -> Result<usize, AoError> { Ok(0) }
}

fn valid_prd_all_false() -> String {
    serde_json::to_string(&json!({
        "project": "Test",
        "branchName": "test/branch",
        "description": "desc",
        "userStories": [
            {"id": "US-001", "title": "T1", "description": "d", "acceptanceCriteria": ["a"], "priority": 1, "passes": false, "notes": "n"},
            {"id": "US-002", "title": "T2", "description": "d", "acceptanceCriteria": ["a"], "priority": 2, "passes": false, "notes": "n"}
        ]
    })).unwrap()
}

fn prd_with_passes_true() -> String {
    serde_json::to_string(&json!({
        "project": "Test",
        "branchName": "test/branch",
        "description": "desc",
        "userStories": [
            {"id": "US-001", "title": "T1", "description": "d", "acceptanceCriteria": ["a"], "priority": 1, "passes": false, "notes": "n"},
            {"id": "US-002", "title": "T2", "description": "d", "acceptanceCriteria": ["a"], "priority": 2, "passes": true, "notes": "n"},
            {"id": "US-003", "title": "T3", "description": "d", "acceptanceCriteria": ["a"], "priority": 3, "passes": true, "notes": "n"}
        ]
    })).unwrap()
}

#[tokio::test]
async fn prd_json_with_passes_true_rejected_during_prd_phase() {
    let ctx = ctx_with(MockRunnerPrdPhaseActive);
    let out = WorkflowActionWriteOutput
        .invoke(
            json!({"task_id": "t1", "filename": "prd.json", "content": prd_with_passes_true()}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable, "should be recoverable");
            assert!(message.contains("PRD validation failed"), "got: {}", message);
            assert!(message.contains("US-002"), "should list US-002, got: {}", message);
            assert!(message.contains("US-003"), "should list US-003, got: {}", message);
            assert!(!message.contains("US-001"), "US-001 is false, should NOT be listed, got: {}", message);
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn prd_json_with_all_passes_false_succeeds() {
    let ctx = ctx_with(MockRunnerPrdPhaseActive);
    let out = WorkflowActionWriteOutput
        .invoke(
            json!({"task_id": "t1", "filename": "prd.json", "content": valid_prd_all_false()}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("prd.json"), "got: {}", s),
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn prd_json_with_passes_true_allowed_after_prd_phase_completes() {
    let ctx = ctx_with(MockRunnerPrdPhaseCompleted);
    let out = WorkflowActionWriteOutput
        .invoke(
            json!({"task_id": "t1", "filename": "prd.json", "content": prd_with_passes_true()}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("prd.json"), "got: {}", s),
        other => panic!("expected Text, got {:?}", other),
    }
}

#[tokio::test]
async fn non_prd_filename_skips_passes_validation() {
    let ctx = ctx_with(MockRunnerPrdPhaseActive);
    // content has passes:true but filename is NOT prd.json → no validation
    let out = WorkflowActionWriteOutput
        .invoke(
            json!({"task_id": "t1", "filename": "other.json", "content": prd_with_passes_true()}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Text(s) => assert!(s.contains("other.json"), "got: {}", s),
        other => panic!("expected Text, got {:?}", other),
    }
}
