use super::*;
use ao_engine_tools_core::{
    AskQuestionError, EventSink, FormAnswer, FormBridge, FormRequest, FormResponse, Registry,
    RunnerContext,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

// ── Test helpers ──────────────────────────────────────────────────────────

struct RecordingSink {
    events: Mutex<Vec<UserEvent>>,
}

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
        })
    }

    fn take(&self) -> Vec<UserEvent> {
        self.events.lock().unwrap().drain(..).collect()
    }
}

#[async_trait]
impl EventSink for RecordingSink {
    async fn emit(&self, event: UserEvent) -> Result<(), AoError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

fn make_ctx(sink: Arc<RecordingSink>, cwd: PathBuf) -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", cwd)
        .with_event_sink(sink as Arc<dyn EventSink + Send + Sync>)
}

fn make_ctx_with_bridge(
    sink: Arc<RecordingSink>,
    cwd: PathBuf,
    bridge: Arc<dyn FormBridge + Send + Sync>,
) -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", cwd)
        .with_event_sink(sink as Arc<dyn EventSink + Send + Sync>)
        .with_form_bridge(bridge)
}

/// Build a form response selecting a single option id for the dirty-removal
/// decision field — mirrors what the frontend delivers for a radio answer.
fn decision_response(option_id: &str) -> FormResponse {
    let mut answers = HashMap::new();
    answers.insert(
        REMOVE_FIELD_DECISION.to_string(),
        FormAnswer::Selections(vec![option_id.to_string()]),
    );
    FormResponse {
        form_id: String::new(),
        answers,
        ..Default::default()
    }
}

// ── Stub form bridges for approval-gate tests ─────────────────────────────

/// Selects "remove" — simulates the operator approving removal.
struct AllowRemovalBridge;

#[async_trait]
impl FormBridge for AllowRemovalBridge {
    async fn ask_form(&self, _req: FormRequest) -> Result<FormResponse, AskQuestionError> {
        Ok(decision_response(REMOVE_OPT_REMOVE))
    }
}

/// Selects "keep" — simulates the operator declining removal.
struct DenyRemovalBridge;

#[async_trait]
impl FormBridge for DenyRemovalBridge {
    async fn ask_form(&self, _req: FormRequest) -> Result<FormResponse, AskQuestionError> {
        Ok(decision_response(REMOVE_OPT_KEEP))
    }
}

/// Panics if called — asserts that the approval bridge is never consulted
/// for a clean-tree removal.
struct NeverCalledBridge;

#[async_trait]
impl FormBridge for NeverCalledBridge {
    async fn ask_form(&self, _req: FormRequest) -> Result<FormResponse, AskQuestionError> {
        panic!("form bridge should not be called for a clean-tree removal");
    }
}

/// Initialise a bare git repo in `dir` so git commands succeed without a
/// remote or any initial commit. Returns after `git init && git commit --allow-empty`.
fn git_init(dir: &std::path::Path) {
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .output()
            .expect("git must be available");
        assert!(
            status.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    };
    run(&["init", "-b", "main"]);
    run(&["commit", "--allow-empty", "-m", "init"]);
}

// ── EnterWorktree ─────────────────────────────────────────────────────────

#[tokio::test]
async fn enter_worktree_double_enter_guard() {
    let sink = RecordingSink::new();
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    git_init(&repo);

    let ctx = make_ctx(sink.clone(), repo.clone());

    // First enter — must succeed.
    let out = EnterWorktree
        .invoke(json!({}), &ctx)
        .await
        .unwrap();
    assert!(
        matches!(out, ToolOutput::Text(_)),
        "first enter must succeed, got {out:?}"
    );

    // Second enter — must return a recoverable error.
    let out2 = EnterWorktree
        .invoke(json!({}), &ctx)
        .await
        .unwrap();
    assert!(
        matches!(out2, ToolOutput::Error { recoverable: true, .. }),
        "double enter must be an error"
    );
}

#[tokio::test]
async fn enter_worktree_not_a_git_repo_is_error() {
    let sink = RecordingSink::new();
    let tmp = TempDir::new().unwrap();
    // No git init — should fail.
    let ctx = make_ctx(sink.clone(), tmp.path().to_path_buf());
    let out = EnterWorktree.invoke(json!({}), &ctx).await.unwrap();
    assert!(matches!(out, ToolOutput::Error { recoverable: true, .. }));
}

#[tokio::test]
async fn enter_worktree_with_name_uses_slug() {
    let sink = RecordingSink::new();
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    git_init(&repo);

    let ctx = make_ctx(sink.clone(), repo.clone());
    let out = EnterWorktree
        .invoke(json!({ "name": "my feature" }), &ctx)
        .await
        .unwrap();

    assert!(matches!(out, ToolOutput::Text(_)));

    // The new cwd must end with the slugified name.
    let cwd = ctx.cwd.read().unwrap().clone();
    assert!(
        cwd.to_string_lossy().contains("my-feature"),
        "cwd {cwd:?} should contain 'my-feature'"
    );
    // Stack must have one entry.
    assert_eq!(ctx.worktree_stack.lock().unwrap().len(), 1);
}

// ── ExitWorktree — action required ───────────────────────────────────────

#[tokio::test]
async fn exit_worktree_empty_stack_is_error() {
    let sink = RecordingSink::new();
    let tmp = TempDir::new().unwrap();
    let ctx = make_ctx(sink.clone(), tmp.path().to_path_buf());

    let out = ExitWorktree
        .invoke(json!({ "action": "keep" }), &ctx)
        .await
        .unwrap();
    assert!(
        matches!(out, ToolOutput::Error { recoverable: true, .. }),
        "exit on empty stack must be an error"
    );
    // No events emitted.
    assert!(sink.take().is_empty());
}

#[tokio::test]
async fn exit_worktree_missing_action_is_error() {
    let sink = RecordingSink::new();
    let tmp = TempDir::new().unwrap();
    let ctx = make_ctx(sink.clone(), tmp.path().to_path_buf());
    let out = ExitWorktree.invoke(json!({}), &ctx).await.unwrap();
    assert!(matches!(out, ToolOutput::Error { recoverable: true, .. }));
}

// ── Full enter → exit(keep) roundtrip ───────────────────────────────────

#[tokio::test]
async fn enter_then_exit_keep_restores_cwd() {
    let sink = RecordingSink::new();
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    git_init(&repo);

    let ctx = make_ctx(sink.clone(), repo.clone());

    // Enter
    let enter_out = EnterWorktree
        .invoke(json!({ "name": "test-keep" }), &ctx)
        .await
        .unwrap();
    assert!(matches!(enter_out, ToolOutput::Text(_)));

    let worktree_cwd = ctx.cwd.read().unwrap().clone();
    assert_ne!(worktree_cwd, repo, "cwd must have moved into worktree");

    sink.take(); // discard enter event

    // Exit with keep
    let exit_out = ExitWorktree
        .invoke(json!({ "action": "keep" }), &ctx)
        .await
        .unwrap();
    assert!(matches!(exit_out, ToolOutput::Text(_)));

    // cwd restored
    assert_eq!(ctx.cwd.read().unwrap().clone(), repo);
    // stack empty
    assert!(ctx.worktree_stack.lock().unwrap().is_empty());

    // Branch and worktree dir must still exist on disk.
    let stack_entry_path = worktree_cwd.clone();
    assert!(
        stack_entry_path.exists(),
        "worktree directory must still exist after keep"
    );

    // One CwdChanged event emitted.
    let events = sink.take();
    assert_eq!(events.len(), 1);
    match &events[0] {
        UserEvent::CwdChanged { from, to } => {
            assert_eq!(*from, worktree_cwd);
            assert_eq!(*to, repo);
        }
        _ => panic!("expected CwdChanged"),
    }
}

// ── Full enter → exit(remove) on clean tree ─────────────────────────────

#[tokio::test]
async fn enter_then_exit_remove_clean_tree_succeeds() {
    let sink = RecordingSink::new();
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    git_init(&repo);

    let ctx = make_ctx(sink.clone(), repo.clone());

    // Enter
    EnterWorktree
        .invoke(json!({ "name": "test-remove" }), &ctx)
        .await
        .unwrap();

    let worktree_cwd = ctx.cwd.read().unwrap().clone();
    sink.take();

    // Exit with remove — tree is clean (no new commits, no staged changes).
    let exit_out = ExitWorktree
        .invoke(json!({ "action": "remove" }), &ctx)
        .await
        .unwrap();

    match &exit_out {
        ToolOutput::Text(msg) => {
            assert!(msg.contains("removed"), "output should mention removal: {msg}");
        }
        ToolOutput::Error { message, .. } => {
            panic!("expected success but got error: {message}");
        }
        _ => panic!("expected Text"),
    }

    // cwd restored.
    assert_eq!(ctx.cwd.read().unwrap().clone(), repo);
    // Worktree directory must be gone.
    assert!(
        !worktree_cwd.exists(),
        "worktree directory must be removed after remove action"
    );
}

// ── Metadata ─────────────────────────────────────────────────────────────

#[test]
fn enter_worktree_name() {
    assert_eq!(EnterWorktree.name(), "EnterWorktree");
}

#[test]
fn exit_worktree_name() {
    assert_eq!(ExitWorktree.name(), "ExitWorktree");
}

#[test]
fn enter_worktree_not_concurrency_safe() {
    assert!(!EnterWorktree.is_concurrency_safe());
}

#[test]
fn exit_worktree_not_concurrency_safe() {
    assert!(!ExitWorktree.is_concurrency_safe());
}

#[test]
fn enter_worktree_mutates_filesystem() {
    assert!(EnterWorktree.mutates_filesystem());
}

#[test]
fn exit_worktree_mutates_filesystem() {
    assert!(ExitWorktree.mutates_filesystem());
}

#[test]
fn lookup_enter_worktree_through_registry() {
    let mut r = Registry::new();
    r.register_engine(Arc::new(EnterWorktree));
    assert!(r.lookup_engine("EnterWorktree").is_some());
}

#[test]
fn lookup_exit_worktree_through_registry() {
    let mut r = Registry::new();
    r.register_engine(Arc::new(ExitWorktree));
    assert!(r.lookup_engine("ExitWorktree").is_some());
}

// ── Plan-mode denial ─────────────────────────────────────────────────────

#[test]
fn enter_worktree_mutates_for_input_is_true() {
    assert!(EnterWorktree.mutates_for_input(&json!({})));
}

#[test]
fn exit_worktree_mutates_for_input_is_true() {
    assert!(ExitWorktree.mutates_for_input(&json!({ "action": "keep" })));
}

// ── Slug logic ───────────────────────────────────────────────────────────

#[test]
fn slugify_basic() {
    assert_eq!(slugify("My Feature"), "my-feature");
}

#[test]
fn slugify_collapses_consecutive_separators() {
    assert_eq!(slugify("hello   world"), "hello-world");
}

#[test]
fn slugify_strips_leading_trailing_dashes() {
    assert_eq!(slugify("  foo  "), "foo");
}

#[test]
fn slugify_preserves_dots_and_dashes() {
    assert_eq!(slugify("feat-1.2"), "feat-1.2");
}

#[test]
fn derive_slug_no_name_returns_wt_prefix() {
    let s = derive_slug(None);
    assert!(s.starts_with("wt-"), "expected wt- prefix, got {s}");
    assert_eq!(s.len(), 11); // "wt-" + 8 hex chars
}

#[test]
fn derive_slug_with_name_uses_slugified() {
    let s = derive_slug(Some("My Task"));
    assert_eq!(s, "my-task");
}

// ── Approval-gate tests ──────────────────────────────────────────────────

/// Write an untracked file into `dir` so the worktree appears dirty to git.
fn make_dirty(dir: &std::path::Path) {
    std::fs::write(dir.join("dirty.txt"), "uncommitted change")
        .expect("write dirty file");
}

/// Dirty worktree + operator approves removal → worktree is deleted.
#[tokio::test]
async fn dirty_worktree_remove_with_allow_bridge_removes() {
    let sink = RecordingSink::new();
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    git_init(&repo);

    let ctx = make_ctx_with_bridge(
        sink.clone(),
        repo.clone(),
        Arc::new(AllowRemovalBridge),
    );

    EnterWorktree
        .invoke(json!({ "name": "dirty-allow" }), &ctx)
        .await
        .unwrap();

    let worktree_cwd = ctx.cwd.read().unwrap().clone();
    make_dirty(&worktree_cwd);
    sink.take();

    let out = ExitWorktree
        .invoke(json!({ "action": "remove" }), &ctx)
        .await
        .unwrap();

    match &out {
        ToolOutput::Text(msg) => {
            assert!(msg.contains("removed"), "expected removed, got: {msg}");
        }
        ToolOutput::Error { message, .. } => panic!("expected success, got error: {message}"),
        _ => panic!("unexpected output: {out:?}"),
    }

    assert_eq!(ctx.cwd.read().unwrap().clone(), repo, "cwd must be restored");
    assert!(ctx.worktree_stack.lock().unwrap().is_empty(), "stack must be empty");
    assert!(!worktree_cwd.exists(), "worktree directory must be deleted");
}

/// Dirty worktree + operator declines removal → worktree is preserved, cwd restored.
#[tokio::test]
async fn dirty_worktree_remove_with_deny_bridge_preserves() {
    let sink = RecordingSink::new();
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    git_init(&repo);

    let ctx = make_ctx_with_bridge(
        sink.clone(),
        repo.clone(),
        Arc::new(DenyRemovalBridge),
    );

    EnterWorktree
        .invoke(json!({ "name": "dirty-deny" }), &ctx)
        .await
        .unwrap();

    let worktree_cwd = ctx.cwd.read().unwrap().clone();
    make_dirty(&worktree_cwd);
    sink.take();

    let out = ExitWorktree
        .invoke(json!({ "action": "remove" }), &ctx)
        .await
        .unwrap();

    match &out {
        ToolOutput::Text(msg) => {
            assert!(
                msg.contains("declined") || msg.contains("preserved"),
                "expected declined/preserved message, got: {msg}"
            );
        }
        ToolOutput::Error { message, .. } => {
            panic!("expected Text (declined), got error: {message}");
        }
        _ => panic!("unexpected output: {out:?}"),
    }

    assert_eq!(ctx.cwd.read().unwrap().clone(), repo, "cwd must be restored");
    assert!(worktree_cwd.exists(), "worktree directory must still exist");
}

/// Clean worktree remove proceeds without consulting the approval bridge at all.
#[tokio::test]
async fn clean_worktree_remove_does_not_ask_bridge() {
    let sink = RecordingSink::new();
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    git_init(&repo);

    // NeverCalledBridge panics if the approval gate fires — proving the clean
    // path skips the prompt entirely.
    let ctx = make_ctx_with_bridge(
        sink.clone(),
        repo.clone(),
        Arc::new(NeverCalledBridge),
    );

    EnterWorktree
        .invoke(json!({ "name": "clean-no-ask" }), &ctx)
        .await
        .unwrap();

    let worktree_cwd = ctx.cwd.read().unwrap().clone();
    sink.take();

    // Tree is clean — no dirty file written.
    let out = ExitWorktree
        .invoke(json!({ "action": "remove" }), &ctx)
        .await
        .unwrap();

    match &out {
        ToolOutput::Text(msg) => {
            assert!(msg.contains("removed"), "expected removed, got: {msg}");
        }
        ToolOutput::Error { message, .. } => panic!("expected success, got error: {message}"),
        _ => panic!("unexpected output: {out:?}"),
    }

    assert_eq!(ctx.cwd.read().unwrap().clone(), repo, "cwd must be restored");
    assert!(!worktree_cwd.exists(), "worktree directory must be deleted");
}
