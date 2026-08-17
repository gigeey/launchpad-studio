use std::path::PathBuf;
use std::sync::Arc;

use ao_engine_tools_core::{
    EngineTool, RunnerContext, ToolOutput, VerificationEngine, VerificationInput,
    VerificationVerdict,
};
use ao_protocol::project::{ProjectStatus, VerificationRecord, MAX_VERIFICATION_ROUNDS};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::ProjectVerify;
use crate::project::tests::{fake_project, temp_project_store};

// ---------------------------------------------------------------------------
// Mock verifier helpers
// ---------------------------------------------------------------------------

struct PassVerifier;

#[async_trait]
impl VerificationEngine for PassVerifier {
    async fn verify(&self, _input: VerificationInput) -> Result<VerificationVerdict, String> {
        Ok(VerificationVerdict {
            verdict: "pass".to_string(),
            confidence: "high".to_string(),
            gaps: vec![],
            rationale: "All goals met.".to_string(),
        })
    }
}

struct FailVerifier {
    gaps: Vec<String>,
}

#[async_trait]
impl VerificationEngine for FailVerifier {
    async fn verify(&self, _input: VerificationInput) -> Result<VerificationVerdict, String> {
        Ok(VerificationVerdict {
            verdict: "fail".to_string(),
            confidence: "medium".to_string(),
            gaps: self.gaps.clone(),
            rationale: "Gaps remain.".to_string(),
        })
    }
}

fn ctx_with_engine(
    project_id: &str,
    store: Arc<ao_persistence::projects::ProjectStore>,
    engine: Arc<dyn VerificationEngine>,
) -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project(project_id.to_string())
        .with_project_store(store)
        .with_verification_engine(engine)
}

fn ctx_with_full_engine(
    project_id: &str,
    store: Arc<ao_persistence::projects::ProjectStore>,
    quick: Arc<dyn VerificationEngine>,
    full: Arc<dyn VerificationEngine>,
) -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project(project_id.to_string())
        .with_project_store(store)
        .with_verification_engine(quick)
        .with_full_verification_engine(full)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_project_scope_returns_error() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"));
    let out = ProjectVerify.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable);
            assert!(message.contains("project-scoped"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_engine_returns_error() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-1", ProjectStatus::Active);
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-1".to_string())
        .with_project_store(store);

    let out = ProjectVerify.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, .. } => assert!(!recoverable),
        other => panic!("expected Error, got {other:?}"),
    }
}

/// mode='full' with no full engine configured returns a recoverable error
/// directing the caller to configure the full engine.
#[tokio::test]
async fn mode_full_no_engine_returns_recoverable_error() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-full-none", ProjectStatus::Active);
    store.create(&project).await.unwrap();

    // Context has a quick engine but NOT a full engine.
    let ctx = ctx_with_engine("proj-full-none", store, Arc::new(PassVerifier));
    let out = ProjectVerify
        .invoke(json!({"mode": "full"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable, "full mode error with no engine should be recoverable");
            assert!(
                message.contains("not available") || message.contains("working_dir"),
                "got: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// mode='full' with a full engine configured produces a structured verdict.
#[tokio::test]
async fn mode_full_with_engine_produces_verdict() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-full", ProjectStatus::Active);
    store.create(&project).await.unwrap();

    let ctx = ctx_with_full_engine(
        "proj-full",
        store.clone(),
        Arc::new(PassVerifier),
        Arc::new(PassVerifier),
    );
    let out = ProjectVerify
        .invoke(json!({"mode": "full"}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["verdict"], "pass");
            assert_eq!(v["engine"], "full", "output must report engine kind");
        }
        other => panic!("expected Structured, got {other:?}"),
    }

    let saved = store.get("proj-full").await.unwrap().unwrap();
    assert_eq!(saved.verifications.len(), 1);
    assert_eq!(saved.verifications[0].engine, "full", "record must store engine='full'");
}

/// engine kind is stored on the VerificationRecord.
#[tokio::test]
async fn engine_kind_persisted_on_record() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-ek", ProjectStatus::Active);
    store.create(&project).await.unwrap();

    // Run quick first, then full.
    let ctx_quick = ctx_with_engine("proj-ek", store.clone(), Arc::new(PassVerifier));
    ProjectVerify.invoke(json!({}), &ctx_quick).await.unwrap();

    let ctx_full = ctx_with_full_engine(
        "proj-ek",
        store.clone(),
        Arc::new(PassVerifier),
        Arc::new(PassVerifier),
    );
    ProjectVerify
        .invoke(json!({"mode": "full"}), &ctx_full)
        .await
        .unwrap();

    let saved = store.get("proj-ek").await.unwrap().unwrap();
    assert_eq!(saved.verifications.len(), 2);
    assert_eq!(saved.verifications[0].engine, "quick");
    assert_eq!(saved.verifications[1].engine, "full");
}

#[tokio::test]
async fn happy_pass_verdict_persisted_and_returned() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-2", ProjectStatus::Active);
    store.create(&project).await.unwrap();

    let ctx = ctx_with_engine("proj-2", store.clone(), Arc::new(PassVerifier));
    let out = ProjectVerify.invoke(json!({}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["verdict"], "pass");
            assert_eq!(v["round"], 1);
            assert!(v["gaps"].as_array().unwrap().is_empty());
        }
        other => panic!("expected Structured, got {other:?}"),
    }

    let saved = store.get("proj-2").await.unwrap().unwrap();
    assert_eq!(saved.verifications.len(), 1);
    assert_eq!(saved.verifications[0].verdict, "pass");
    assert_eq!(saved.verifications[0].engine, "quick");
}

#[tokio::test]
async fn fail_with_gaps_verdict_persisted() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-3", ProjectStatus::Active);
    store.create(&project).await.unwrap();

    let engine = Arc::new(FailVerifier {
        gaps: vec!["tests missing".to_string(), "docs not written".to_string()],
    });
    let ctx = ctx_with_engine("proj-3", store.clone(), engine);

    let out = ProjectVerify.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["verdict"], "fail");
            let gaps = v["gaps"].as_array().unwrap();
            assert_eq!(gaps.len(), 2);
            assert_eq!(gaps[0], "tests missing");
        }
        other => panic!("expected Structured, got {other:?}"),
    }

    let saved = store.get("proj-3").await.unwrap().unwrap();
    assert_eq!(saved.verifications[0].gaps, vec!["tests missing", "docs not written"]);
}

/// The round cap only counts FULL verifications — quick verifications do not
/// consume formal rounds. A project can have many quick checks without being
/// escalated to NeedsReview.
#[tokio::test]
async fn round_cap_counts_only_full_verifications() {
    let (_dir, store) = temp_project_store().await;
    let mut project = fake_project("proj-cap-q", ProjectStatus::Active);

    // Pre-fill the maximum number of quick verifications.
    for i in 0..MAX_VERIFICATION_ROUNDS {
        project.verifications.push(VerificationRecord {
            round: (i + 1) as u32,
            timestamp: Utc::now(),
            verdict: "fail".to_string(),
            gaps: vec!["gap".to_string()],
            confidence: "high".to_string(),
            rationale: "not done".to_string(),
            engine: "quick".to_string(),
        });
    }
    store.create(&project).await.unwrap();

    // A further quick verification must still succeed — quick runs don't count.
    let ctx = ctx_with_engine("proj-cap-q", store.clone(), Arc::new(PassVerifier));
    let out = ProjectVerify.invoke(json!({}), &ctx).await.unwrap();
    assert!(
        matches!(out, ToolOutput::Structured(_)),
        "quick run must succeed even after MAX_VERIFICATION_ROUNDS quick records; got {out:?}"
    );
}

/// When FULL verifications reach the cap the project is escalated.
#[tokio::test]
async fn round_cap_escalates_to_needs_review_on_full_rounds() {
    let (_dir, store) = temp_project_store().await;
    let mut project = fake_project("proj-cap-f", ProjectStatus::Active);

    // Pre-fill the maximum number of FULL verifications.
    for i in 0..MAX_VERIFICATION_ROUNDS {
        project.verifications.push(VerificationRecord {
            round: (i + 1) as u32,
            timestamp: Utc::now(),
            verdict: "fail".to_string(),
            gaps: vec!["gap".to_string()],
            confidence: "high".to_string(),
            rationale: "not done".to_string(),
            engine: "full".to_string(),
        });
    }
    store.create(&project).await.unwrap();

    // A further FULL verification must now trigger the cap.
    let ctx = ctx_with_full_engine(
        "proj-cap-f",
        store.clone(),
        Arc::new(PassVerifier),
        Arc::new(PassVerifier),
    );
    let out = ProjectVerify
        .invoke(json!({"mode": "full"}), &ctx)
        .await
        .unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable);
            assert!(message.contains("round cap"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }

    let saved = store.get("proj-cap-f").await.unwrap().unwrap();
    assert!(matches!(saved.status, ProjectStatus::NeedsReview));
}
