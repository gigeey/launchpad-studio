use std::path::PathBuf;

use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput};
use ao_protocol::project::ProjectStatus;
use serde_json::json;

use super::ProjectGet;
use crate::project::tests::{fake_project, temp_project_store};

#[tokio::test]
async fn missing_project_scope_returns_error() {
    // No project_id or project_store set.
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"));
    let out = ProjectGet.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable, "expected non-recoverable");
            assert!(
                message.contains("project-scoped"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_project_store_returns_error() {
    // project_id set but no project_store.
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-1".to_string());
    let out = ProjectGet.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable, "expected non-recoverable");
            assert!(
                message.contains("not available"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn happy_path_returns_project_fields() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-42", ProjectStatus::Interviewing);
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-42".to_string())
        .with_project_store(store);

    let out = ProjectGet.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["id"], "proj-42");
            assert_eq!(v["goal"], "Finish the thing");
            assert_eq!(v["status"], "interviewing");
        }
        other => panic!("expected Structured, got {other:?}"),
    }
}

#[tokio::test]
async fn project_not_found_returns_error() {
    let (_dir, store) = temp_project_store().await;
    // Store is empty — no project was created.
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("does-not-exist".to_string())
        .with_project_store(store);

    let out = ProjectGet.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { message, .. } => {
            assert!(message.contains("not found"), "unexpected message: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}
