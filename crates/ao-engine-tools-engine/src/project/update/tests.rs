use std::path::PathBuf;

use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput};
use ao_protocol::project::ProjectStatus;
use serde_json::json;

use super::ProjectUpdate;
use crate::project::tests::{fake_project, temp_project_store};

#[tokio::test]
async fn missing_project_scope_returns_error() {
    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"));
    let out = ProjectUpdate.invoke(json!({}), &ctx).await.unwrap();
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
    let out = ProjectUpdate.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable);
            assert!(message.contains("not available"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn happy_path_updates_name_and_spec() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-1", ProjectStatus::Interviewing);
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-1".to_string())
        .with_project_store(store.clone());

    let out = ProjectUpdate
        .invoke(
            json!({"name": "My Renamed Project", "spec": "# Goals\nDo the thing."}),
            &ctx,
        )
        .await
        .unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["name"], "My Renamed Project");
            assert_eq!(v["spec"], "# Goals\nDo the thing.");
            assert_eq!(v["status"], "interviewing");
        }
        other => panic!("expected Structured, got {other:?}"),
    }

    // Verify it was persisted.
    let saved = store.get("proj-1").await.unwrap().unwrap();
    assert_eq!(saved.name, "My Renamed Project");
    assert_eq!(saved.spec.as_deref(), Some("# Goals\nDo the thing."));
}

#[tokio::test]
async fn activate_transitions_interviewing_to_active() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-2", ProjectStatus::Interviewing);
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-2".to_string())
        .with_project_store(store.clone());

    let out = ProjectUpdate
        .invoke(
            json!({"spec": "Ready spec.", "activate": true}),
            &ctx,
        )
        .await
        .unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"], "active");
            assert_eq!(v["activated"], true);
        }
        other => panic!("expected Structured, got {other:?}"),
    }

    // Confirm persistence.
    let saved = store.get("proj-2").await.unwrap().unwrap();
    assert!(matches!(saved.status, ao_protocol::project::ProjectStatus::Active));
}

#[tokio::test]
async fn activate_from_active_is_recoverable_error() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-3", ProjectStatus::Active);
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-3".to_string())
        .with_project_store(store);

    let out = ProjectUpdate
        .invoke(json!({"activate": true}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable, "expected recoverable error");
            assert!(message.contains("already Active"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn activate_from_completed_is_recoverable_error() {
    let (_dir, store) = temp_project_store().await;
    let project = fake_project("proj-4", ProjectStatus::Completed);
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-4".to_string())
        .with_project_store(store);

    let out = ProjectUpdate
        .invoke(json!({"activate": true}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("Completed"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn null_fields_clear_optionals() {
    let (_dir, store) = temp_project_store().await;
    let mut project = fake_project("proj-5", ProjectStatus::Interviewing);
    project.emoji = Some("🚀".to_string());
    project.spec = Some("old spec".to_string());
    store.create(&project).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent-1", PathBuf::from("/tmp"))
        .with_project("proj-5".to_string())
        .with_project_store(store.clone());

    let out = ProjectUpdate
        .invoke(json!({"emoji": null, "spec": null}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert!(v["emoji"].is_null());
            assert!(v["spec"].is_null());
        }
        other => panic!("expected Structured, got {other:?}"),
    }

    let saved = store.get("proj-5").await.unwrap().unwrap();
    assert!(saved.emoji.is_none());
    assert!(saved.spec.is_none());
}
