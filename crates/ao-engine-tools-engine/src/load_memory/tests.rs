use super::*;

use ao_engine_tools_core::RunnerContext;
use ao_persistence::project_key::{hash_project_key, resolve_project_key};
use ao_persistence::{memory::MemoryStore, paths::DataRoot};
use ao_protocol::memory::MemorySource;
use serde_json::json;

fn make_ctx(store: Arc<MemoryStore>, cwd: &std::path::Path) -> RunnerContext {
    RunnerContext::new_with_cwd("session-1", "agent-1", cwd.to_path_buf())
        .with_memory_store(store)
}

async fn seed_project(store: &MemoryStore, repo: &std::path::Path, content: &str) -> String {
    let canonical_key = resolve_project_key(repo).await.unwrap();
    let hash = hash_project_key(&canonical_key);
    store.add_project(&hash, content, MemorySource::Agent).await.unwrap();
    hash
}

#[tokio::test]
async fn test_load_memory_reads_sibling_repo_project_scope() {
    let data_dir = tempfile::tempdir().unwrap();
    let session_repo = tempfile::tempdir().unwrap();
    let target_repo = tempfile::tempdir().unwrap();

    let store = Arc::new(MemoryStore::new(DataRoot::new(data_dir.path())));
    seed_project(&store, target_repo.path(), "target repo build quirk").await;

    let ctx = make_ctx(store, session_repo.path());
    let tool = LoadMemory;

    let out = tool
        .invoke(
            json!({ "repo": target_repo.path().to_string_lossy() }),
            &ctx,
        )
        .await
        .unwrap();

    let value = match out {
        ao_engine_tools_core::ToolOutput::Structured(v) => v,
        other => panic!("expected structured output, got {:?}", other),
    };

    assert_eq!(value["entry_count"], json!(1));
    assert_eq!(value["truncated"], json!(false));
    let entries = value["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["content"], json!("target repo build quirk"));
}

#[tokio::test]
async fn test_load_memory_session_repo_never_leaks_into_target_scope() {
    let data_dir = tempfile::tempdir().unwrap();
    let session_repo = tempfile::tempdir().unwrap();
    let target_repo = tempfile::tempdir().unwrap();

    let store = Arc::new(MemoryStore::new(DataRoot::new(data_dir.path())));
    // Seed the SESSION's own repo with a memory that must not leak into the
    // target repo's result — the whole point of `repo` is decoupling "which
    // repo's learnings" from "which repo the session launched in".
    seed_project(&store, session_repo.path(), "session repo secret").await;
    seed_project(&store, target_repo.path(), "target repo fact").await;

    let ctx = make_ctx(store, session_repo.path());
    let tool = LoadMemory;

    let out = tool
        .invoke(
            json!({ "repo": target_repo.path().to_string_lossy() }),
            &ctx,
        )
        .await
        .unwrap();

    let value = match out {
        ao_engine_tools_core::ToolOutput::Structured(v) => v,
        other => panic!("expected structured output, got {:?}", other),
    };

    let entries = value["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["content"], json!("target repo fact"));
}

#[tokio::test]
async fn test_load_memory_missing_repo_field_errors() {
    let data_dir = tempfile::tempdir().unwrap();
    let session_repo = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new(DataRoot::new(data_dir.path())));
    let ctx = make_ctx(store, session_repo.path());

    let out = LoadMemory.invoke(json!({}), &ctx).await.unwrap();
    match out {
        ao_engine_tools_core::ToolOutput::Error { .. } => {}
        other => panic!("expected error output, got {:?}", other),
    }
}

#[tokio::test]
async fn test_load_memory_nonexistent_repo_path_errors() {
    let data_dir = tempfile::tempdir().unwrap();
    let session_repo = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new(DataRoot::new(data_dir.path())));
    let ctx = make_ctx(store, session_repo.path());

    let bogus = session_repo.path().join("does-not-exist-at-all");
    let out = LoadMemory
        .invoke(json!({ "repo": bogus.to_string_lossy() }), &ctx)
        .await
        .unwrap();
    match out {
        ao_engine_tools_core::ToolOutput::Error { message, .. } => {
            assert!(message.contains("does not exist"), "message was: {message}");
        }
        other => panic!("expected error output, got {:?}", other),
    }
}

#[tokio::test]
async fn test_load_memory_small_scope_returns_full_without_truncation() {
    let data_dir = tempfile::tempdir().unwrap();
    let session_repo = tempfile::tempdir().unwrap();
    let target_repo = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new(DataRoot::new(data_dir.path())));

    seed_project(&store, target_repo.path(), "short fact one").await;
    seed_project(&store, target_repo.path(), "short fact two").await;

    let ctx = make_ctx(store, session_repo.path());
    let out = LoadMemory
        .invoke(
            json!({ "repo": target_repo.path().to_string_lossy(), "budget_chars": 500 }),
            &ctx,
        )
        .await
        .unwrap();

    let value = match out {
        ao_engine_tools_core::ToolOutput::Structured(v) => v,
        other => panic!("expected structured output, got {:?}", other),
    };
    assert_eq!(value["entry_count"], json!(2));
    assert_eq!(value["returned_count"], json!(2));
    assert_eq!(value["truncated"], json!(false));
    assert_eq!(value["filtered_by_task"], json!(false));
}

#[tokio::test]
async fn test_load_memory_over_budget_truncates_and_ranks_by_task() {
    let data_dir = tempfile::tempdir().unwrap();
    let session_repo = tempfile::tempdir().unwrap();
    let target_repo = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new(DataRoot::new(data_dir.path())));

    // Three entries, each individually under the budget, but together over
    // it — forces the ranking/truncation path.
    let padding = "x".repeat(200);
    seed_project(&store, target_repo.path(), &format!("unrelated note alpha {padding}")).await;
    seed_project(
        &store,
        target_repo.path(),
        &format!("deployment rollback runbook {padding}"),
    )
    .await;
    seed_project(&store, target_repo.path(), &format!("unrelated note beta {padding}")).await;

    let ctx = make_ctx(store, session_repo.path());
    let out = LoadMemory
        .invoke(
            json!({
                "repo": target_repo.path().to_string_lossy(),
                "task": "how do I roll back a deployment",
                "budget_chars": 300,
            }),
            &ctx,
        )
        .await
        .unwrap();

    let value = match out {
        ao_engine_tools_core::ToolOutput::Structured(v) => v,
        other => panic!("expected structured output, got {:?}", other),
    };

    assert_eq!(value["entry_count"], json!(3));
    assert_eq!(value["truncated"], json!(true));
    assert_eq!(value["filtered_by_task"], json!(true));
    let entries = value["entries"].as_array().unwrap();
    assert!(!entries.is_empty(), "at least the top-ranked entry must survive a tight budget");
    assert!(
        entries[0]["content"]
            .as_str()
            .unwrap()
            .contains("deployment rollback runbook"),
        "the task-relevant entry must be ranked first: {:?}",
        entries[0]
    );
}

#[tokio::test]
async fn test_load_memory_reports_project_root_and_input() {
    let data_dir = tempfile::tempdir().unwrap();
    let session_repo = tempfile::tempdir().unwrap();
    let target_repo = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new(DataRoot::new(data_dir.path())));
    seed_project(&store, target_repo.path(), "fact").await;

    let ctx = make_ctx(store, session_repo.path());
    let repo_str = target_repo.path().to_string_lossy().into_owned();
    let out = LoadMemory
        .invoke(json!({ "repo": repo_str.clone() }), &ctx)
        .await
        .unwrap();

    let value = match out {
        ao_engine_tools_core::ToolOutput::Structured(v) => v,
        other => panic!("expected structured output, got {:?}", other),
    };
    assert_eq!(value["repo_input"], json!(repo_str));
    assert!(value["project_root"].as_str().unwrap().len() > 0);
}

#[test]
fn test_tokenize_drops_short_words_and_dedupes() {
    let tokens = tokenize("to Roll roll back a Deployment deployment");
    assert!(tokens.contains(&"roll".to_string()));
    assert!(tokens.contains(&"back".to_string()));
    assert!(tokens.contains(&"deployment".to_string()));
    assert!(!tokens.contains(&"to".to_string()), "2-char words must be dropped");
    assert!(!tokens.contains(&"a".to_string()), "1-char words must be dropped");
    assert_eq!(
        tokens.iter().filter(|t| *t == "roll").count(),
        1,
        "tokens must be deduplicated"
    );
}

#[test]
fn test_keyword_score_counts_distinct_matches() {
    let tokens = vec!["roll".to_string(), "back".to_string(), "missing".to_string()];
    let score = keyword_score("deployment rollback runbook", &tokens);
    // "roll" and "back" both match as substrings of "rollback"; "missing" does not.
    assert_eq!(score, 2);
}
