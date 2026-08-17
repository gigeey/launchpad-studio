use std::path::PathBuf;
use std::sync::Arc;

use ao_engine_tools_core::{
    EngineTool, RunnerContext, ToolOutput, VerificationEngine, VerificationInput,
    VerificationVerdict,
};
use ao_protocol::project::{ProjectStatus, VerificationRecord};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::ProjectComplete;
use crate::project::tests::{fake_project, temp_project_store};

// ---------------------------------------------------------------------------
// Mock verifiers
// ---------------------------------------------------------------------------

struct PassVerifier;

#[async_trait]
impl VerificationEngine for PassVerifier {
    async fn verify(&self, _input: VerificationInput) -> Result<VerificationVerdict, String> {
        Ok(VerificationVerdict {
            verdict: "pass".to_string(),
            confidence: "high".to_string(),
            gaps: vec![],
            rationale: "Goal met.".to_string(),
        })
    }
}

struct FailVerifier;

#[async_trait]
impl VerificationEngine for FailVerifier {
    async fn verify(&self, _input: VerificationInput) -> Result<VerificationVerdict, String> {
        Ok(VerificationVerdict {
            verdict: "fail".to_string(),
            confidence: "medium".to_string(),
            gaps: vec!["tests missing".to_string()],
            rationale: "No tests written.".to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn full_passing_record(round: u32) -> VerificationRecord {
    VerificationRecord {
        round,
        timestamp: Utc::now(),
        verdict: "pass".to_string(),
        gaps: vec![],
        confidence: "high".to_string(),
        rationale: "passed".to_string(),
        engine: "full".to_string(),
    }
}

fn quick_passing_record(round: u32) -> VerificationRecord {
    VerificationRecord {
        round,
        timestamp: Utc::now(),
        verdict: "pass".to_string(),
        gaps: vec![],
        confidence: "high".to_string(),
        rationale: "passed".to_string(),
        engine: "quick".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_project_scope_returns_error() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"));
    let out = ProjectComplete
        .invoke(json!({"summary": "all done"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable);
            assert!(message.contains("project-scoped"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_project_store_returns_error() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-1".to_string());
    let out = ProjectComplete
        .invoke(json!({"summary": "all done"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable);
            assert!(message.contains("not available"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_summary_returns_recoverable_error() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-1", ProjectStatus::Active);
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-1".to_string())
        .with_project_store(store);

    let out = ProjectComplete.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("summary"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// A passing FULL verification satisfies the gate.
#[tokio::test]
async fn happy_path_with_full_passing_verification() {
    let (_dir, store) = temp_project_store().await;
    let mut project = fake_project("proj-full-pass", ProjectStatus::Active);
    project.verifications.push(full_passing_record(1));
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-full-pass".to_string())
        .with_project_store(store.clone())
        .with_verification_engine(Arc::new(PassVerifier))
        .with_full_verification_engine(Arc::new(PassVerifier));

    let out = ProjectComplete
        .invoke(json!({"summary": "Shipped the feature successfully."}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"], "completed");
            assert_eq!(v["id"], "proj-full-pass");
        }
        other => panic!("expected Structured, got {other:?}"),
    }

    let saved = store.get("proj-full-pass").await.unwrap().unwrap();
    assert!(matches!(saved.status, ProjectStatus::Completed));
}

/// A quick-only passing verification is NOT sufficient for completion.
/// The gate now requires engine='full'. With only a quick pass, auto-verify
/// fires, falls back to the quick engine (no full engine wired), and the
/// auto-verify may pass but the record is still "quick" — the gate sees no
/// full pass.
///
/// In this test the full engine is not injected, so the auto-verify path
/// uses the quick engine. The record produced has engine='quick'. The gate
/// then still fails (no full pass), and because there is no full engine the
/// tool returns the "not available" error.
#[tokio::test]
async fn quick_only_pass_no_full_engine_is_rejected() {
    let (_dir, store) = temp_project_store().await;
    let mut project = fake_project("proj-quick-reject", ProjectStatus::Active);
    project.verifications.push(quick_passing_record(1));
    store.create(&project).await.unwrap();

    // Context has only a quick engine — no full engine.
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-quick-reject".to_string())
        .with_project_store(store.clone())
        .with_verification_engine(Arc::new(PassVerifier));

    let out = ProjectComplete
        .invoke(json!({"summary": "Done."}), &ctx)
        .await
        .unwrap();

    // Should be rejected because: gate requires "full", no "full" pass exists,
    // auto-verify runs but produces a "quick" record (no full engine), and the
    // error reflects the missing full engine.
    match out {
        ToolOutput::Error { recoverable, .. } => {
            assert!(recoverable, "gate rejection must be recoverable so agent can retry");
        }
        ToolOutput::Structured(v) => {
            // If somehow auto-verify path ran and the quick auto-record still
            // doesn't satisfy the gate, we'd loop. In current implementation the
            // error from missing full engine is returned before auto-verify.
            panic!("expected Error (full engine not available), got Structured: {v:?}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// Without any verification, auto-runs the full engine when available. Pass → completes.
#[tokio::test]
async fn auto_verification_with_full_engine_pass_completes_project() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-auto-full", ProjectStatus::Active);
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-auto-full".to_string())
        .with_project_store(store.clone())
        .with_verification_engine(Arc::new(PassVerifier))
        .with_full_verification_engine(Arc::new(PassVerifier));

    let out = ProjectComplete
        .invoke(json!({"summary": "Done."}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"], "completed");
        }
        other => panic!("expected Structured, got {other:?}"),
    }

    // Check that a "full" engine record was written.
    let saved = store.get("proj-auto-full").await.unwrap().unwrap();
    assert!(
        saved.verifications.iter().any(|v| v.engine == "full"),
        "auto-verify must produce a 'full' engine record; got: {:?}",
        saved.verifications
    );
}

/// Without a passing verification, auto-runs the verifier. Fail → recoverable error with gaps.
#[tokio::test]
async fn auto_verification_fail_rejects_completion_with_gaps() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-4", ProjectStatus::Active);
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-4".to_string())
        .with_project_store(store.clone())
        .with_verification_engine(Arc::new(FailVerifier))
        .with_full_verification_engine(Arc::new(FailVerifier));

    let out = ProjectComplete
        .invoke(json!({"summary": "Done."}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("tests missing"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// No verification engine + no passing verification → recoverable error.
#[tokio::test]
async fn no_engine_no_verification_returns_recoverable_error() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-5", ProjectStatus::Active);
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-5".to_string())
        .with_project_store(store);

    let out = ProjectComplete
        .invoke(json!({"summary": "Done."}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn complete_from_interviewing_is_recoverable_error() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-6", ProjectStatus::Interviewing);
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-6".to_string())
        .with_project_store(store);

    let out = ProjectComplete
        .invoke(json!({"summary": "done"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("Interviewing"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn complete_from_completed_is_recoverable_error() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-7", ProjectStatus::Completed);
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-7".to_string())
        .with_project_store(store);

    let out = ProjectComplete
        .invoke(json!({"summary": "done again"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("already Completed"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
