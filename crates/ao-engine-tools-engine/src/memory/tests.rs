use std::sync::Arc;

use ao_persistence::{memory::MemoryStore, paths::DataRoot};
use ao_protocol::memory::MemoryScope;

use super::{
    agent_project_key, check_entry_caps, check_scope_caps, resolve_scope_context,
    resolve_working_dir, ScopeContext, AGENT_HARD_CAP, AGENT_SOFT_CAP, ENTRY_CHAR_HARD,
    ENTRY_CHAR_SOFT,
};

fn make_store(tmp: &tempfile::TempDir) -> Arc<MemoryStore> {
    Arc::new(MemoryStore::new(DataRoot::new(tmp.path())))
}

// --- check_entry_caps ---

#[test]
fn test_check_entry_caps_short_content_no_warning() {
    assert!(check_entry_caps("short content").unwrap().is_none());
}

#[test]
fn test_check_entry_caps_between_soft_and_hard_emits_warning() {
    let content = "x".repeat(ENTRY_CHAR_SOFT + 1);
    let warning = check_entry_caps(&content).unwrap();
    assert!(warning.is_some());
    assert!(warning.unwrap().contains("long"));
}

// Acceptance criterion: MemoryWrite over ENTRY_CHAR_HARD (8000 chars) returns error
#[test]
fn test_check_entry_caps_over_hard_cap_returns_error() {
    let content = "x".repeat(ENTRY_CHAR_HARD + 1);
    assert!(
        check_entry_caps(&content).is_err(),
        "content over ENTRY_CHAR_HARD must return Err"
    );
}

#[test]
fn test_check_entry_caps_at_exact_hard_cap_is_ok() {
    // Exactly at hard cap: not strictly over, so Ok (returns a warning since > soft)
    let content = "x".repeat(ENTRY_CHAR_HARD);
    assert!(check_entry_caps(&content).is_ok());
}

// --- check_scope_caps ---

#[tokio::test]
async fn test_check_scope_caps_empty_store_no_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let store = make_store(&tmp);
    let ctx = ScopeContext::Agent { agent_id: "agent-caps".to_string() };

    let result =
        check_scope_caps(&store, &ctx, AGENT_SOFT_CAP, AGENT_HARD_CAP).await.unwrap();
    assert!(result.is_none(), "empty scope must produce no warning");
}

#[tokio::test]
async fn test_check_scope_caps_at_hard_cap_returns_error() {
    use ao_protocol::memory::MemorySource;

    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let inner = MemoryStore::new(data_root);

    inner.add("cap-agent", "entry a", MemorySource::Agent).await.unwrap();
    inner.add("cap-agent", "entry b", MemorySource::Agent).await.unwrap();

    let store = Arc::new(inner);
    let ctx = ScopeContext::Agent { agent_id: "cap-agent".to_string() };
    // soft=1, hard=2 → 2 entries is at hard cap → Err
    let result = check_scope_caps(&store, &ctx, 1, 2).await;
    assert!(result.is_err(), "scope at hard cap must return Err");
}

#[tokio::test]
async fn test_check_scope_caps_at_soft_cap_returns_warning() {
    use ao_protocol::memory::MemorySource;

    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let inner = MemoryStore::new(data_root);

    inner.add("soft-agent", "entry a", MemorySource::Agent).await.unwrap();

    let store = Arc::new(inner);
    let ctx = ScopeContext::Agent { agent_id: "soft-agent".to_string() };
    // 1 entry at soft=1 → warning
    let result = check_scope_caps(&store, &ctx, 1, 5).await.unwrap();
    assert!(result.is_some(), "scope at soft cap must return a warning");
}

// --- resolve_scope_context ---

#[tokio::test]
async fn test_resolve_scope_context_agent() {
    let fallback = std::path::Path::new("/tmp");
    let ctx =
        resolve_scope_context(&MemoryScope::Agent, "my-agent", None, None, fallback, None)
            .await
            .unwrap();
    match ctx {
        ScopeContext::Agent { agent_id } => assert_eq!(agent_id, "my-agent"),
        other => panic!("expected Agent context, got {:?}", other),
    }
}

#[tokio::test]
async fn test_resolve_scope_context_global() {
    let fallback = std::path::Path::new("/tmp");
    let ctx =
        resolve_scope_context(&MemoryScope::Global, "any-agent", None, None, fallback, None)
            .await
            .unwrap();
    assert!(matches!(ctx, ScopeContext::Global));
}

#[tokio::test]
async fn test_resolve_scope_context_project_returns_32_char_hash() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx =
        resolve_scope_context(&MemoryScope::Project, "any-agent", Some(tmp.path()), None, tmp.path(), None)
            .await
            .unwrap();
    match ctx {
        ScopeContext::Project { hash, canonical_key } => {
            assert_eq!(hash.len(), 32, "project hash must be 32 hex chars");
            assert!(!canonical_key.is_empty());
        }
        other => panic!("expected Project context, got {:?}", other),
    }
}

#[tokio::test]
async fn test_resolve_scope_context_project_same_dir_same_hash_regardless_of_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx1 =
        resolve_scope_context(&MemoryScope::Project, "agent-a", Some(tmp.path()), None, tmp.path(), None)
            .await
            .unwrap();
    let ctx2 =
        resolve_scope_context(&MemoryScope::Project, "agent-b", Some(tmp.path()), None, tmp.path(), None)
            .await
            .unwrap();

    let (h1, h2) = match (ctx1, ctx2) {
        (ScopeContext::Project { hash: h1, .. }, ScopeContext::Project { hash: h2, .. }) => {
            (h1, h2)
        }
        _ => panic!("expected both to be Project"),
    };
    assert_eq!(h1, h2, "same directory must produce same project hash regardless of agent");
}

/// Delegated child with no model-supplied working_dir defaults to parent's cwd.
#[tokio::test]
async fn test_resolve_scope_context_project_uses_parent_cwd_when_no_explicit_dir() {
    let parent_dir = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();

    // No explicit working_dir from model, parent_cwd set → resolves to parent
    let ctx_parent = resolve_scope_context(
        &MemoryScope::Project,
        "child-agent",
        None,
        Some(parent_dir.path()),
        child_dir.path(),
    None,
)
    .await
    .unwrap();

    // Direct resolve against parent dir for comparison
    let ctx_direct = resolve_scope_context(
        &MemoryScope::Project,
        "child-agent",
        Some(parent_dir.path()),
        None,
        parent_dir.path(),
    None,
)
    .await
    .unwrap();

    let (h1, h2) = match (ctx_parent, ctx_direct) {
        (ScopeContext::Project { hash: h1, .. }, ScopeContext::Project { hash: h2, .. }) => {
            (h1, h2)
        }
        _ => panic!("expected both to be Project"),
    };
    assert_eq!(h1, h2, "delegated child must resolve to parent's project key when no working_dir");
}

/// Model-supplied working_dir wins over parent_cwd.
#[tokio::test]
async fn test_resolve_scope_context_project_explicit_working_dir_wins_over_parent_cwd() {
    let parent_dir = tempfile::tempdir().unwrap();
    let explicit_dir = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();

    // Explicit working_dir provided by model → wins over parent_cwd
    let ctx_explicit = resolve_scope_context(
        &MemoryScope::Project,
        "child-agent",
        Some(explicit_dir.path()),
        Some(parent_dir.path()),
        child_dir.path(),
    None,
)
    .await
    .unwrap();

    let ctx_direct = resolve_scope_context(
        &MemoryScope::Project,
        "child-agent",
        Some(explicit_dir.path()),
        None,
        explicit_dir.path(),
    None,
)
    .await
    .unwrap();

    let (h1, h2) = match (ctx_explicit, ctx_direct) {
        (ScopeContext::Project { hash: h1, .. }, ScopeContext::Project { hash: h2, .. }) => {
            (h1, h2)
        }
        _ => panic!("expected both to be Project"),
    };
    assert_eq!(h1, h2, "explicit working_dir must override parent_cwd");
}

// --- resolve_scope_context: agent×project (reserved cell) ---

/// The `agent×project` cell must resolve to a distinct key per (agent, repo)
/// pair — it is unrepresentable if it collapses onto either the plain agent
/// scope or the plain project scope.
#[tokio::test]
async fn test_resolve_scope_context_agent_project_resolves_distinct_key() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = resolve_scope_context(
        &MemoryScope::AgentProject,
        "agent-a",
        Some(tmp.path()),
        None,
        tmp.path(),
    None,
)
    .await
    .unwrap();

    match ctx {
        ScopeContext::AgentProject {
            agent_id,
            project_hash,
            key,
        } => {
            assert_eq!(agent_id, "agent-a");
            assert_eq!(project_hash.len(), 32, "project hash must be 32 hex chars");
            assert_eq!(key.len(), 32, "cell key must be 32 hex chars");
            // The cell key must differ from the bare project hash — otherwise
            // it has collapsed onto the plain `Project` scope.
            assert_ne!(key, project_hash);
        }
        other => panic!("expected AgentProject context, got {:?}", other),
    }
}

/// Two agents in the same repo must resolve to two different `agent×project`
/// keys — otherwise the cell has collapsed onto `Project` (visible to every
/// agent) rather than staying agent-specific.
#[tokio::test]
async fn test_resolve_scope_context_agent_project_differs_by_agent_same_repo() {
    let tmp = tempfile::tempdir().unwrap();

    let ctx_a = resolve_scope_context(
        &MemoryScope::AgentProject,
        "agent-a",
        Some(tmp.path()),
        None,
        tmp.path(),
    None,
)
    .await
    .unwrap();
    let ctx_b = resolve_scope_context(
        &MemoryScope::AgentProject,
        "agent-b",
        Some(tmp.path()),
        None,
        tmp.path(),
    None,
)
    .await
    .unwrap();

    let (key_a, hash_a) = match ctx_a {
        ScopeContext::AgentProject { key, project_hash, .. } => (key, project_hash),
        other => panic!("expected AgentProject context, got {:?}", other),
    };
    let (key_b, hash_b) = match ctx_b {
        ScopeContext::AgentProject { key, project_hash, .. } => (key, project_hash),
        other => panic!("expected AgentProject context, got {:?}", other),
    };

    assert_eq!(hash_a, hash_b, "same repo must resolve to the same project hash");
    assert_ne!(key_a, key_b, "different agents in the same repo must get distinct cell keys");
}

/// The same agent in two different repos must also resolve to two different
/// `agent×project` keys — otherwise the cell has collapsed onto the plain
/// `Agent` scope (leaking across every repo the agent touches).
#[tokio::test]
async fn test_resolve_scope_context_agent_project_differs_by_repo_same_agent() {
    let tmp_a = tempfile::tempdir().unwrap();
    let tmp_b = tempfile::tempdir().unwrap();

    let ctx_a = resolve_scope_context(
        &MemoryScope::AgentProject,
        "agent-shared",
        Some(tmp_a.path()),
        None,
        tmp_a.path(),
    None,
)
    .await
    .unwrap();
    let ctx_b = resolve_scope_context(
        &MemoryScope::AgentProject,
        "agent-shared",
        Some(tmp_b.path()),
        None,
        tmp_b.path(),
    None,
)
    .await
    .unwrap();

    let key_a = match ctx_a {
        ScopeContext::AgentProject { key, .. } => key,
        other => panic!("expected AgentProject context, got {:?}", other),
    };
    let key_b = match ctx_b {
        ScopeContext::AgentProject { key, .. } => key,
        other => panic!("expected AgentProject context, got {:?}", other),
    };

    assert_ne!(key_a, key_b, "same agent across two repos must get distinct cell keys");
}

#[test]
fn test_agent_project_key_is_stable_and_order_sensitive() {
    let k1 = agent_project_key("agent-x", "0123456789abcdef0123456789abcdef");
    let k2 = agent_project_key("agent-x", "0123456789abcdef0123456789abcdef");
    assert_eq!(k1, k2, "same inputs must produce the same key");

    // Guard against naive string-concat collisions: swapping which half of a
    // shared byte sequence each argument owns must not collide.
    let k3 = agent_project_key("ab", "cd");
    let k4 = agent_project_key("a", "bcd");
    assert_ne!(k3, k4, "different (agent_id, project_hash) splits must not collide");
}

// --- resolve_working_dir ---

#[test]
fn test_resolve_working_dir_none_returns_cwd() {
    let cwd = std::path::PathBuf::from("/tmp/runner-cwd");
    assert_eq!(resolve_working_dir(None, &cwd), cwd);
}

#[test]
fn test_resolve_working_dir_empty_string_returns_cwd() {
    let cwd = std::path::PathBuf::from("/tmp/runner-cwd");
    assert_eq!(resolve_working_dir(Some(""), &cwd), cwd);
    assert_eq!(resolve_working_dir(Some("   "), &cwd), cwd);
}

#[test]
fn test_resolve_working_dir_absolute_passes_through() {
    let cwd = std::path::PathBuf::from("/tmp/runner-cwd");
    let abs = "/Users/me/dev/repo";
    assert_eq!(resolve_working_dir(Some(abs), &cwd), std::path::PathBuf::from(abs));
}

#[test]
fn test_resolve_working_dir_relative_joins_onto_cwd() {
    let cwd = std::path::PathBuf::from("/tmp/runner-cwd");
    assert_eq!(
        resolve_working_dir(Some("sub/dir"), &cwd),
        std::path::PathBuf::from("/tmp/runner-cwd/sub/dir")
    );
}

#[test]
fn test_resolve_working_dir_tilde_expands_when_home_set() {
    let cwd = std::path::PathBuf::from("/tmp/runner-cwd");
    let prev = std::env::var("HOME").ok();
    std::env::set_var("HOME", "/Users/testuser");
    assert_eq!(
        resolve_working_dir(Some("~"), &cwd),
        std::path::PathBuf::from("/Users/testuser")
    );
    assert_eq!(
        resolve_working_dir(Some("~/dev/repo"), &cwd),
        std::path::PathBuf::from("/Users/testuser/dev/repo")
    );
    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
}

#[test]
fn test_resolve_working_dir_trims_whitespace_around_input() {
    let cwd = std::path::PathBuf::from("/tmp/runner-cwd");
    assert_eq!(
        resolve_working_dir(Some("  /Users/me/dev  "), &cwd),
        std::path::PathBuf::from("/Users/me/dev")
    );
}

// --- MemoryWrite invoke-level: contradiction guard ---
//
// Acceptance criteria: (a) an agent write contradicting a
// Manual entry is routed to the gate, not applied; (b) an agent-vs-agent
// contradiction marks the old entry Superseded + superseded_by; (c) the
// pre-existing byte-equal dedup behavior is unchanged.

mod write_contradiction_guard {
    use super::make_store;
    use crate::memory::write::MemoryWrite;
    use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
    use ao_persistence::project_key::{hash_project_key, resolve_project_key};
    use ao_protocol::memory::{MemorySource, MemoryStatus};
    use serde_json::json;

    fn make_ctx(store: std::sync::Arc<ao_persistence::memory::MemoryStore>) -> RunnerContext {
        let cwd = std::env::temp_dir();
        RunnerContext::new_with_cwd("session-1", "agent-1", cwd).with_memory_store(store)
    }

    fn as_structured(out: ToolOutput) -> serde_json::Value {
        match out {
            ToolOutput::Structured(v) => v,
            other => panic!("expected structured output, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn agent_write_contradicting_manual_entry_is_gated_not_applied() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let manual = store
            .add("agent-1", "user prefers tabs over spaces", MemorySource::Manual)
            .await
            .unwrap();

        let ctx = make_ctx(store.clone());
        let out = MemoryWrite
            .invoke(
                json!({ "scope": "agent", "content": "user prefers spaces over tabs" }),
                &ctx,
            )
            .await
            .unwrap();

        let value = as_structured(out);
        assert_eq!(value["staged"], json!(true));
        assert_eq!(value["applied"], json!(false));
        assert_eq!(value["contradicts"], json!(manual.id));

        // The Manual entry must be untouched: still the only entry, still
        // Active, never silently superseded.
        let entries = store.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), 1, "the contradicting write must not have been applied");
        assert_eq!(entries[0].id, manual.id);
        assert_eq!(entries[0].content, "user prefers tabs over spaces");
        assert_eq!(entries[0].status, MemoryStatus::Active);
        assert_eq!(entries[0].superseded_by, None);
    }

    #[tokio::test]
    async fn agent_vs_agent_contradiction_marks_superseded_and_writes_new_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let old = store
            .add("agent-1", "user prefers tabs over spaces", MemorySource::Agent)
            .await
            .unwrap();

        let ctx = make_ctx(store.clone());
        let out = MemoryWrite
            .invoke(
                json!({ "scope": "agent", "content": "user prefers spaces over tabs" }),
                &ctx,
            )
            .await
            .unwrap();

        let value = as_structured(out);
        assert_eq!(value["superseded"], json!(old.id));
        let new_id = value["id"].as_str().unwrap().to_string();
        assert_ne!(new_id, old.id);

        let entries = store.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), 2, "both the old and new entries stay on disk");

        let old_entry = entries.iter().find(|e| e.id == old.id).unwrap();
        assert_eq!(old_entry.status, MemoryStatus::Superseded);
        assert_eq!(old_entry.superseded_by, Some(new_id.clone()));

        let new_entry = entries.iter().find(|e| e.id == new_id).unwrap();
        assert_eq!(new_entry.status, MemoryStatus::Active);
        assert_eq!(new_entry.content, "user prefers spaces over tabs");
    }

    #[tokio::test]
    async fn byte_equal_dedup_still_works_via_invoke() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let ctx = make_ctx(store.clone());

        let first = MemoryWrite
            .invoke(json!({ "scope": "agent", "content": "remember this exactly" }), &ctx)
            .await
            .unwrap();
        let first_value = as_structured(first);
        assert_eq!(first_value["deduplicated"], json!(false));

        let second = MemoryWrite
            .invoke(json!({ "scope": "agent", "content": "remember this exactly" }), &ctx)
            .await
            .unwrap();
        let second_value = as_structured(second);
        assert_eq!(second_value["deduplicated"], json!(true));
        assert_eq!(second_value["id"], first_value["id"]);

        let entries = store.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), 1, "byte-equal resubmission must not create a duplicate");
    }

    #[tokio::test]
    async fn unrelated_write_applies_normally_with_no_superseded_field() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        store.add("agent-1", "the sky is blue", MemorySource::Manual).await.unwrap();

        let ctx = make_ctx(store.clone());
        let out = MemoryWrite
            .invoke(json!({ "scope": "agent", "content": "the database uses postgresql" }), &ctx)
            .await
            .unwrap();

        let value = as_structured(out);
        assert!(value.get("staged").is_none());
        assert!(
            value.get("superseded").is_none(),
            "an unrelated write must not report a supersession"
        );

        let entries = store.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), 2, "both the pre-existing and new entries must be live");
    }

    #[tokio::test]
    async fn global_scope_contradicting_manual_entry_is_gated_not_applied() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let manual = store
            .add_global("user prefers tabs over spaces", MemorySource::Manual)
            .await
            .unwrap();

        let ctx = make_ctx(store.clone());
        let out = MemoryWrite
            .invoke(
                json!({ "scope": "global", "content": "user prefers spaces over tabs" }),
                &ctx,
            )
            .await
            .unwrap();

        let value = as_structured(out);
        assert_eq!(value["staged"], json!(true));
        assert_eq!(value["applied"], json!(false));

        let entries = store.list_global().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, manual.id);
    }

    /// Project scope is the one place a legacy entry can have `source: None`
    /// (rows written before `add_project` carried provenance). Unknown
    /// provenance must be treated as cautiously as `Manual` — there is no
    /// way to prove it is safe to silently override.
    #[tokio::test]
    async fn project_scope_contradiction_with_unknown_source_entry_is_gated() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let repo = tempfile::tempdir().unwrap();
        let canonical_key = resolve_project_key(repo.path()).await.unwrap();
        let hash = hash_project_key(&canonical_key);

        // Hand-write a legacy-shaped row: no `source` key at all, matching
        // on-disk data from before project-scope entries carried provenance.
        let legacy_line = format!(
            r#"{{"id":"legacy-001","content":"user prefers tabs over spaces","created_at":"2024-06-15T12:34:56Z","scope":"Project","scope_key":"{hash}"}}"#
        );
        let data_dir = ao_persistence::paths::DataRoot::new(tmp.path());
        let path = data_dir.memory_project_path(&hash);
        tokio::fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&path, format!("{}\n", legacy_line)).await.unwrap();

        let ctx = RunnerContext::new_with_cwd("session-1", "agent-1", repo.path().to_path_buf())
            .with_memory_store(store.clone());
        let out = MemoryWrite
            .invoke(
                json!({ "scope": "project", "content": "user prefers spaces over tabs" }),
                &ctx,
            )
            .await
            .unwrap();

        let value = as_structured(out);
        assert_eq!(value["staged"], json!(true), "unknown provenance must gate, not apply");
        assert_eq!(value["applied"], json!(false));

        let entries = store.list_project(&hash).await.unwrap();
        assert_eq!(entries.len(), 1, "the unattributed legacy entry must not be silently overridden");
    }
}

// --- MemoryWrite invoke-level: tiered auto-confirm boundary ---
//
// Acceptance criteria: (1) a NEW
// agent-scope memory that contradicts nothing auto-confirms (applies live,
// no staging) -- unchanged from before this task; (2d) a write to project or
// global scope always stages for review now, even with no contradiction at
// all -- this is the gap this task closes (previously such a write applied
// live with no gating whatsoever).

mod write_scope_gate {
    use super::make_store;
    use crate::memory::write::MemoryWrite;
    use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
    use ao_persistence::project_key::{hash_project_key, resolve_project_key};
    use serde_json::json;

    fn make_ctx(store: std::sync::Arc<ao_persistence::memory::MemoryStore>) -> RunnerContext {
        let cwd = std::env::temp_dir();
        RunnerContext::new_with_cwd("session-1", "agent-1", cwd).with_memory_store(store)
    }

    fn as_structured(out: ToolOutput) -> serde_json::Value {
        match out {
            ToolOutput::Structured(v) => v,
            other => panic!("expected structured output, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn agent_scope_new_write_with_no_contradiction_auto_confirms() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let ctx = make_ctx(store.clone());

        let out = MemoryWrite
            .invoke(json!({ "scope": "agent", "content": "the api base url is configurable" }), &ctx)
            .await
            .unwrap();

        let value = as_structured(out);
        assert!(value.get("staged").is_none(), "agent-scope auto-confirm must not stage");
        assert!(value.get("id").is_some(), "an applied write must report the new entry's id");

        let entries = store.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), 1, "the write must have been applied live");
    }

    #[tokio::test]
    async fn project_scope_new_write_with_no_contradiction_stages_for_review() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let repo = tempfile::tempdir().unwrap();
        let canonical_key = resolve_project_key(repo.path()).await.unwrap();
        let hash = hash_project_key(&canonical_key);

        let ctx = RunnerContext::new_with_cwd("session-1", "agent-1", repo.path().to_path_buf())
            .with_memory_store(store.clone());
        let out = MemoryWrite
            .invoke(json!({ "scope": "project", "content": "the team uses trunk-based development" }), &ctx)
            .await
            .unwrap();

        let value = as_structured(out);
        assert_eq!(
            value["staged"], json!(true),
            "a project-scope write must stage even with no contradiction: {value:?}"
        );
        assert_eq!(value["applied"], json!(false));
        assert_eq!(value["tier"], json!("stage_for_review"));
        assert!(value.get("contradicts").is_none(), "there is nothing to contradict here");

        let entries = store.list_project(&hash).await.unwrap();
        assert!(entries.is_empty(), "a staged write must not land in the live project store");
    }

    #[tokio::test]
    async fn global_scope_new_write_with_no_contradiction_stages_for_review() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let ctx = make_ctx(store.clone());

        let out = MemoryWrite
            .invoke(json!({ "scope": "global", "content": "prefer async/await over callbacks" }), &ctx)
            .await
            .unwrap();

        let value = as_structured(out);
        assert_eq!(
            value["staged"], json!(true),
            "a global-scope write must stage even with no contradiction: {value:?}"
        );
        assert_eq!(value["applied"], json!(false));
        assert_eq!(value["tier"], json!("stage_for_review"));

        let entries = store.list_global().await.unwrap();
        assert!(entries.is_empty(), "a staged write must not land in the live global store");
    }

    /// The `NeverAuto` hard block (overwriting a Manual entry) must report
    /// `tier: "never_auto"`, distinct from an ordinary `stage_for_review`
    /// write -- both share the `staged`/`applied` shape for backward
    /// compatibility, but the tier tag disambiguates them for a future
    /// review-queue consumer.
    #[tokio::test]
    async fn manual_hard_block_reports_never_auto_tier() {
        use ao_protocol::memory::MemorySource;

        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        store.add("agent-1", "user prefers tabs over spaces", MemorySource::Manual).await.unwrap();

        let ctx = make_ctx(store.clone());
        let out = MemoryWrite
            .invoke(json!({ "scope": "agent", "content": "user prefers spaces over tabs" }), &ctx)
            .await
            .unwrap();

        let value = as_structured(out);
        assert_eq!(value["tier"], json!("never_auto"));
    }

    // --- `StageForReview` candidates must persist into the
    // review queue (`ao_persistence::reflection_staging::ReflectionStagingStore`)
    // wired through `ctx.reflection_staging`, so a human has something
    // durable to `keep`/`edit`/`forget`/`pin` later -- the transient tool
    // result alone is not enough.

    #[tokio::test]
    async fn stage_for_review_write_persists_into_the_review_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let staging = std::sync::Arc::new(ao_persistence::reflection_staging::ReflectionStagingStore::new(
            ao_persistence::paths::DataRoot::new(tmp.path()),
        ));

        let ctx = make_ctx(store.clone()).with_reflection_staging(staging.clone());
        let out = MemoryWrite
            .invoke(json!({ "scope": "global", "content": "prefer async/await over callbacks" }), &ctx)
            .await
            .unwrap();
        let value = as_structured(out);
        let candidate_id = value["candidate_id"].as_str().unwrap().to_string();

        let pending = staging.list_pending("agent-1").await.unwrap();
        assert_eq!(pending.len(), 1, "the StageForReview write must land in the queue");
        assert_eq!(pending[0].id, candidate_id, "the tool result must report the queued candidate's id");
        assert_eq!(pending[0].content, "prefer async/await over callbacks");
        assert_eq!(pending[0].target_scope, ao_protocol::memory::MemoryScope::Global);
        assert_eq!(pending[0].target_scope_key, None);
        assert_eq!(
            pending[0].status,
            ao_protocol::reflection_candidate::ReflectionCandidateStatus::Pending
        );
    }

    #[tokio::test]
    async fn never_auto_hard_block_is_never_persisted_into_the_review_queue() {
        use ao_protocol::memory::MemorySource;

        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        store.add("agent-1", "user prefers tabs over spaces", MemorySource::Manual).await.unwrap();
        let staging = std::sync::Arc::new(ao_persistence::reflection_staging::ReflectionStagingStore::new(
            ao_persistence::paths::DataRoot::new(tmp.path()),
        ));

        let ctx = make_ctx(store.clone()).with_reflection_staging(staging.clone());
        let out = MemoryWrite
            .invoke(json!({ "scope": "agent", "content": "user prefers spaces over tabs" }), &ctx)
            .await
            .unwrap();
        let value = as_structured(out);
        assert_eq!(value["tier"], json!("never_auto"));
        assert!(
            value.get("candidate_id").is_none(),
            "a never_auto write must not report a candidate_id -- it never reaches the queue"
        );

        let pending = staging.list_pending("agent-1").await.unwrap();
        assert!(
            pending.is_empty(),
            "the review queue's contents must be exactly the StageForReview set, never NeverAuto"
        );
    }

    #[tokio::test]
    async fn project_scope_write_persists_with_project_target_scope_key() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let repo = tempfile::tempdir().unwrap();
        let canonical_key = resolve_project_key(repo.path()).await.unwrap();
        let hash = hash_project_key(&canonical_key);
        let staging = std::sync::Arc::new(ao_persistence::reflection_staging::ReflectionStagingStore::new(
            ao_persistence::paths::DataRoot::new(tmp.path()),
        ));

        let ctx = RunnerContext::new_with_cwd("session-1", "agent-1", repo.path().to_path_buf())
            .with_memory_store(store.clone())
            .with_reflection_staging(staging.clone());
        MemoryWrite
            .invoke(json!({ "scope": "project", "content": "the team uses trunk-based development" }), &ctx)
            .await
            .unwrap();

        let pending = staging.list_pending("agent-1").await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target_scope, ao_protocol::memory::MemoryScope::Project);
        assert_eq!(pending[0].target_scope_key, Some(hash));
    }
}

// --- MemoryWrite invoke-level: upgraded past string-similarity via FTS5 ---
//
// Acceptance criteria: (a) a near-duplicate that
// plain normalized-string similarity alone would miss is surfaced once the
// FTS5 index is queried for candidates; (b) the never-supersede-Manual
// guard still holds for a match surfaced this way -- it stages for review
// exactly like a plain-similarity match, never auto-applies.

mod write_contradiction_guard_fts {
    use crate::memory::write::MemoryWrite;
    use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
    use ao_persistence::{memory::MemoryStore, paths::DataRoot};
    use ao_protocol::memory::{MemorySource, MemoryStatus};
    use ao_search_index::SearchIndex;
    use serde_json::json;
    use std::sync::Arc;

    /// A restatement of `NEAR_DUPLICATE_CONTENT` padded with enough unrelated
    /// vocabulary that Jaccard's union denominator dilutes the shared tokens
    /// below `CONTRADICTION_THRESHOLD` (see the equivalent fixture in
    /// `contradiction/tests.rs`, which asserts the exact score) -- a plain
    /// scan of existing entries would not flag it, but FTS5's `bm25` ranking
    /// (no such length penalty) surfaces it as a top candidate for a query of
    /// `NEAR_DUPLICATE_CONTENT`.
    const PADDED_ENTRY_CONTENT: &str = "user prefers tabs over spaces for indentation in every \
        python backend file, javascript config, and shell script across the whole monorepo";
    const NEAR_DUPLICATE_CONTENT: &str = "user prefers tabs over spaces for indentation";

    fn make_indexed_store(tmp: &tempfile::TempDir) -> Arc<MemoryStore> {
        let index = SearchIndex::open_in_memory().unwrap();
        Arc::new(MemoryStore::new(DataRoot::new(tmp.path())).with_index(index))
    }

    fn make_ctx(store: Arc<MemoryStore>) -> RunnerContext {
        let cwd = std::env::temp_dir();
        RunnerContext::new_with_cwd("session-1", "agent-1", cwd).with_memory_store(store)
    }

    fn as_structured(out: ToolOutput) -> serde_json::Value {
        match out {
            ToolOutput::Structured(v) => v,
            other => panic!("expected structured output, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn fts5_surfaces_agent_authored_near_duplicate_plain_similarity_missed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_indexed_store(&tmp);
        let old = store
            .add("agent-1", PADDED_ENTRY_CONTENT, MemorySource::Agent)
            .await
            .unwrap();

        let ctx = make_ctx(store.clone());
        let out = MemoryWrite
            .invoke(json!({ "scope": "agent", "content": NEAR_DUPLICATE_CONTENT }), &ctx)
            .await
            .unwrap();

        let value = as_structured(out);
        assert_eq!(
            value["superseded"],
            json!(old.id),
            "FTS5 must surface the padded entry as a contradiction even though \
             plain string similarity alone would score it below threshold"
        );
        let new_id = value["id"].as_str().unwrap().to_string();
        assert_ne!(new_id, old.id);

        let entries = store.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), 2, "both the old and new entries stay on disk");
        let old_entry = entries.iter().find(|e| e.id == old.id).unwrap();
        assert_eq!(old_entry.status, MemoryStatus::Superseded);
        assert_eq!(old_entry.superseded_by, Some(new_id));
    }

    #[tokio::test]
    async fn fts5_surfaced_match_against_manual_entry_still_stages_never_supersedes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_indexed_store(&tmp);
        let manual = store
            .add("agent-1", PADDED_ENTRY_CONTENT, MemorySource::Manual)
            .await
            .unwrap();

        let ctx = make_ctx(store.clone());
        let out = MemoryWrite
            .invoke(json!({ "scope": "agent", "content": NEAR_DUPLICATE_CONTENT }), &ctx)
            .await
            .unwrap();

        let value = as_structured(out);
        assert_eq!(value["staged"], json!(true));
        assert_eq!(value["applied"], json!(false));
        assert_eq!(value["contradicts"], json!(manual.id));

        // The Manual entry must be untouched, exactly like a plain-similarity
        // match against Manual -- surfacing the candidate via FTS5 instead of
        // the string-similarity scan must not weaken this guard.
        let entries = store.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), 1, "the contradicting write must not have been applied");
        assert_eq!(entries[0].id, manual.id);
        assert_eq!(entries[0].content, PADDED_ENTRY_CONTENT);
        assert_eq!(entries[0].status, MemoryStatus::Active);
        assert_eq!(entries[0].superseded_by, None);
    }

    #[tokio::test]
    async fn unrelated_write_with_index_attached_applies_normally() {
        // Guards against the FTS5 query itself introducing false positives:
        // with the index attached and populated, a genuinely unrelated write
        // must still go through untouched.
        let tmp = tempfile::tempdir().unwrap();
        let store = make_indexed_store(&tmp);
        store.add("agent-1", PADDED_ENTRY_CONTENT, MemorySource::Manual).await.unwrap();

        let ctx = make_ctx(store.clone());
        let out = MemoryWrite
            .invoke(json!({ "scope": "agent", "content": "the project database is postgresql" }), &ctx)
            .await
            .unwrap();

        let value = as_structured(out);
        assert!(value.get("staged").is_none());
        assert!(value.get("superseded").is_none());

        let entries = store.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), 2, "both the pre-existing and new entries must be live");
    }
}

// --- MemoryWrite invoke-level: graceful eviction ---
//
// Acceptance criteria: (a) a write at the hard cap evicts
// the lowest-scoring non-Manual entry to `Archived` and the new write
// succeeds — no more rejection/wedge; (b) a `Manual` entry is never chosen
// for eviction even when it would score lowest.

mod write_eviction_guard {
    use super::make_store;
    use crate::memory::write::MemoryWrite;
    use crate::memory::AGENT_HARD_CAP;
    use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
    use ao_persistence::memory::MemoryStore;
    use ao_protocol::memory::{MemoryEntry, MemoryScope, MemorySource, MemoryStatus};
    use serde_json::json;
    use std::sync::Arc;

    fn make_ctx(store: Arc<MemoryStore>) -> RunnerContext {
        let cwd = std::env::temp_dir();
        RunnerContext::new_with_cwd("session-1", "agent-1", cwd).with_memory_store(store)
    }

    fn as_structured(out: ToolOutput) -> serde_json::Value {
        match out {
            ToolOutput::Structured(v) => v,
            other => panic!("expected structured output, got {:?}", other),
        }
    }

    /// Hand-append a fully-formed raw entry so its `confidence` can be set
    /// below what `MemoryStore::add` (always `1.0`) allows — the only way to
    /// pin the eviction scorer's outcome deterministically in a test.
    async fn append_raw_entry(store: &MemoryStore, agent_id: &str, entry: &MemoryEntry) {
        let path = store.agent_scope_path(agent_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        use tokio::io::AsyncWriteExt;
        let line = serde_json::to_string(entry).unwrap();
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .unwrap();
        file.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    }

    fn low_confidence_entry(id: &str, source: MemorySource) -> MemoryEntry {
        let ancient = "2020-01-01T00:00:00Z".parse().unwrap();
        MemoryEntry {
            id: id.to_string(),
            content: format!("filler content {id}"),
            created_at: ancient,
            source: Some(source),
            scope: MemoryScope::Agent,
            scope_key: Some("agent-1".to_string()),
            updated_at: ancient,
            deleted_at: None,
            confidence: 0.0,
            status: MemoryStatus::Active,
            superseded_by: None,
            pinned: false,
            decay_score: 1.0,
        }
    }

    #[tokio::test]
    async fn write_at_hard_cap_evicts_lowest_scoring_entry_and_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);

        // AGENT_HARD_CAP - 1 normal, high-confidence, fresh entries...
        for i in 0..AGENT_HARD_CAP - 1 {
            store
                .add("agent-1", &format!("filler entry {i}"), MemorySource::Agent)
                .await
                .unwrap();
        }
        // ...plus one deliberately worst-scoring entry (zero confidence,
        // ancient timestamp) to reach the hard cap exactly.
        let weak = low_confidence_entry("weak-entry", MemorySource::Agent);
        append_raw_entry(&store, "agent-1", &weak).await;

        let before = store.list("agent-1").await.unwrap();
        assert_eq!(before.len(), AGENT_HARD_CAP, "setup must land exactly at the hard cap");

        let ctx = make_ctx(store.clone());
        let out = MemoryWrite
            .invoke(json!({ "scope": "agent", "content": "the write that used to wedge" }), &ctx)
            .await
            .unwrap();

        let value = as_structured(out);
        assert!(value.get("error").is_none(), "hitting the hard cap must no longer reject the write");
        assert_eq!(value["evicted"], json!("weak-entry"), "the lowest-scoring entry must be evicted");
        let new_id = value["id"].as_str().unwrap().to_string();

        let after = store.list("agent-1").await.unwrap();
        assert_eq!(after.len(), AGENT_HARD_CAP + 1, "the evicted entry stays on disk, plus the new entry");

        let evicted = after.iter().find(|e| e.id == "weak-entry").unwrap();
        assert_eq!(evicted.status, MemoryStatus::Archived, "eviction must archive, not delete");

        let active_count = after.iter().filter(|e| e.status == MemoryStatus::Active).count();
        assert_eq!(active_count, AGENT_HARD_CAP, "active count must stay at the cap (sliding window)");

        let new_entry = after.iter().find(|e| e.id == new_id).unwrap();
        assert_eq!(new_entry.status, MemoryStatus::Active);
        assert_eq!(new_entry.content, "the write that used to wedge");
    }

    #[tokio::test]
    async fn manual_entry_is_never_evicted_even_when_lowest_scoring() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);

        // AGENT_HARD_CAP - 1 normal, high-confidence, fresh entries...
        for i in 0..AGENT_HARD_CAP - 1 {
            store
                .add("agent-1", &format!("filler entry {i}"), MemorySource::Agent)
                .await
                .unwrap();
        }
        // ...plus one Manual entry engineered to score worse than every
        // other live entry (zero confidence, ancient timestamp) — if
        // eviction ever ignores the Manual exemption, this is the one it
        // would pick.
        let manual_worst = low_confidence_entry("manual-worst", MemorySource::Manual);
        append_raw_entry(&store, "agent-1", &manual_worst).await;

        let before = store.list("agent-1").await.unwrap();
        assert_eq!(before.len(), AGENT_HARD_CAP, "setup must land exactly at the hard cap");

        let ctx = make_ctx(store.clone());
        let out = MemoryWrite
            .invoke(json!({ "scope": "agent", "content": "another write at the cap" }), &ctx)
            .await
            .unwrap();

        let value = as_structured(out);
        assert!(value.get("error").is_none());
        let evicted_id = value["evicted"].as_str().unwrap().to_string();
        assert_ne!(evicted_id, "manual-worst", "the Manual entry must never be picked for eviction");

        let after = store.list("agent-1").await.unwrap();
        let manual_entry = after.iter().find(|e| e.id == "manual-worst").unwrap();
        assert_eq!(
            manual_entry.status,
            MemoryStatus::Active,
            "the Manual entry must remain Active/untouched"
        );

        let evicted_entry = after.iter().find(|e| e.id == evicted_id).unwrap();
        assert_eq!(evicted_entry.status, MemoryStatus::Archived);
        assert_ne!(
            evicted_entry.source,
            Some(MemorySource::Manual),
            "whatever got evicted must not be the Manual entry"
        );
    }

    #[tokio::test]
    async fn every_active_entry_manual_still_rejects_the_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);

        for i in 0..AGENT_HARD_CAP {
            let entry = low_confidence_entry(&format!("manual-{i}"), MemorySource::Manual);
            append_raw_entry(&store, "agent-1", &entry).await;
        }

        let ctx = make_ctx(store.clone());
        let out = MemoryWrite
            .invoke(json!({ "scope": "agent", "content": "cannot fit, nothing to evict" }), &ctx)
            .await
            .unwrap();

        let value = as_structured(out);
        assert!(
            value.get("error").is_some(),
            "with no eligible eviction candidate the write must still be rejected, not silently dropped"
        );

        let entries = store.list("agent-1").await.unwrap();
        assert_eq!(entries.len(), AGENT_HARD_CAP, "no new entry must have been written");
        assert!(entries.iter().all(|e| e.status == MemoryStatus::Active));
    }
}

// --- MemoryDelete invoke-level: hard invariant ---
//
// Hard invariant: delete/tombstone must clean BOTH the durable
// JSONL log (already soft-tombstoned by `MemoryStore::delete*`) AND the
// `.usage.json` sidecar — never leave a usage row pointing at an id the
// JSONL no longer carries as live, in any of the three scopes.

mod delete_cleans_usage_sidecar {
    use super::make_store;
    use crate::memory::delete::MemoryDelete;
    use ao_engine_tools_core::{memory_usage, IoTool, RunnerContext, ToolOutput};
    use ao_persistence::memory::MemoryStore;
    use ao_protocol::memory::MemorySource;
    use serde_json::json;
    use std::sync::Arc;

    fn make_ctx(store: Arc<MemoryStore>) -> RunnerContext {
        let cwd = std::env::temp_dir();
        RunnerContext::new_with_cwd("session-1", "agent-1", cwd).with_memory_store(store)
    }

    fn as_structured(out: ToolOutput) -> serde_json::Value {
        match out {
            ToolOutput::Structured(v) => v,
            other => panic!("expected structured output, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn agent_scope_delete_removes_both_the_jsonl_entry_and_the_usage_sidecar_row() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let entry = store.add("agent-1", "remember this", MemorySource::Agent).await.unwrap();

        // Simulate the entry having been surfaced-and-used before deletion.
        let scope_path = store.agent_scope_path("agent-1");
        memory_usage::increment(&scope_path, &entry.id).await.unwrap();
        assert!(memory_usage::load(&scope_path).await.contains_key(&entry.id));

        let ctx = make_ctx(store.clone());
        let out = MemoryDelete
            .invoke(json!({ "id": entry.id, "scope": "agent" }), &ctx)
            .await
            .unwrap();
        assert_eq!(as_structured(out)["deleted"], json!(true));

        assert!(store.list("agent-1").await.unwrap().is_empty(), "JSONL entry must be tombstoned");
        assert!(
            !memory_usage::load(&scope_path).await.contains_key(&entry.id),
            "usage sidecar row must be removed alongside the JSONL tombstone"
        );
    }

    #[tokio::test]
    async fn global_scope_delete_removes_both_the_jsonl_entry_and_the_usage_sidecar_row() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let entry = store.add_global("shared fact", MemorySource::Manual).await.unwrap();

        let scope_path = store.global_scope_path();
        memory_usage::increment(&scope_path, &entry.id).await.unwrap();

        let ctx = make_ctx(store.clone());
        let out = MemoryDelete
            .invoke(json!({ "id": entry.id, "scope": "global" }), &ctx)
            .await
            .unwrap();
        assert_eq!(as_structured(out)["deleted"], json!(true));

        assert!(store.list_global().await.unwrap().is_empty());
        assert!(
            !memory_usage::load(&scope_path).await.contains_key(&entry.id),
            "usage sidecar row must be removed alongside the JSONL tombstone"
        );
    }

    #[tokio::test]
    async fn project_scope_delete_removes_both_the_jsonl_entry_and_the_usage_sidecar_row() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let repo = tempfile::tempdir().unwrap();
        let canonical_key = ao_persistence::project_key::resolve_project_key(repo.path()).await.unwrap();
        let hash = ao_persistence::project_key::hash_project_key(&canonical_key);

        let op = store.add_project(&hash, "project fact", MemorySource::Manual).await.unwrap();
        let scope_path = store.project_scope_path(&hash);
        memory_usage::increment(&scope_path, &op.id).await.unwrap();

        let ctx = RunnerContext::new_with_cwd("session-1", "agent-1", repo.path().to_path_buf())
            .with_memory_store(store.clone());
        let out = MemoryDelete
            .invoke(json!({ "id": op.id, "scope": "project" }), &ctx)
            .await
            .unwrap();
        assert_eq!(as_structured(out)["deleted"], json!(true));

        assert!(store.list_project(&hash).await.unwrap().is_empty(), "JSONL entry must be tombstoned");
        assert!(
            !memory_usage::load(&scope_path).await.contains_key(&op.id),
            "usage sidecar row must be removed alongside the JSONL tombstone"
        );
    }

    #[tokio::test]
    async fn deleting_an_entry_never_surfaced_does_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let entry = store.add("agent-1", "never surfaced", MemorySource::Agent).await.unwrap();

        let ctx = make_ctx(store.clone());
        let out = MemoryDelete
            .invoke(json!({ "id": entry.id, "scope": "agent" }), &ctx)
            .await
            .unwrap();
        assert_eq!(as_structured(out)["deleted"], json!(true), "delete must succeed even with no sidecar row to clean");
    }

    #[tokio::test]
    async fn deleting_a_nonexistent_id_does_not_touch_the_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let entry = store.add("agent-1", "kept", MemorySource::Agent).await.unwrap();
        let scope_path = store.agent_scope_path("agent-1");
        memory_usage::increment(&scope_path, &entry.id).await.unwrap();

        let ctx = make_ctx(store.clone());
        let out = MemoryDelete
            .invoke(json!({ "id": "does-not-exist", "scope": "agent" }), &ctx)
            .await
            .unwrap();
        assert!(as_structured(out).get("error").is_some(), "a missing id must report not-found");

        assert!(
            memory_usage::load(&scope_path).await.contains_key(&entry.id),
            "an unrelated entry's sidecar row must survive a failed delete of a different id"
        );
    }
}

// --- Thread scope ---
//
// Acceptance criteria: (a) write→list→edit→delete round-trips through the
// four tools exactly like the durable scopes; (b) entries written under one
// thread id are invisible under a different thread id — no cross-thread
// bleed; (c) resolving Thread scope with no thread id available on the
// runner context is a recoverable `AoError`, not a panic.

mod thread_scope_resolution {
    use super::make_store;
    use crate::memory::store::resolve_scope_context;
    use ao_protocol::memory::MemoryScope;

    #[tokio::test]
    async fn resolve_scope_context_thread_with_id_returns_thread_context() {
        let fallback = std::path::Path::new("/tmp");
        let ctx = resolve_scope_context(
            &MemoryScope::Thread,
            "any-agent",
            None,
            None,
            fallback,
            Some("thread-abc"),
        )
        .await
        .unwrap();
        match ctx {
            super::ScopeContext::Thread { thread_id } => assert_eq!(thread_id, "thread-abc"),
            other => panic!("expected Thread context, got {:?}", other),
        }
    }

    /// Acceptance: a context with no thread id resolves to the agent's
    /// default thread id, mirroring `ListThreads`'s treatment of the
    /// implicit main-conversation thread — never a panic, never a fallback
    /// to a different scope.
    #[tokio::test]
    async fn resolve_scope_context_thread_without_id_falls_back_to_default_thread() {
        use ao_protocol::thread::default_thread_id;

        let fallback = std::path::Path::new("/tmp");
        let ctx =
            resolve_scope_context(&MemoryScope::Thread, "any-agent", None, None, fallback, None)
                .await
                .unwrap();
        match ctx {
            super::ScopeContext::Thread { thread_id } => {
                assert_eq!(thread_id, default_thread_id("any-agent"))
            }
            other => panic!("expected Thread context, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn resolve_scope_context_thread_with_blank_id_falls_back_to_default_thread() {
        use ao_protocol::thread::default_thread_id;

        let fallback = std::path::Path::new("/tmp");
        let ctx = resolve_scope_context(
            &MemoryScope::Thread,
            "any-agent",
            None,
            None,
            fallback,
            Some("   "),
        )
        .await
        .unwrap();
        match ctx {
            super::ScopeContext::Thread { thread_id } => {
                assert_eq!(thread_id, default_thread_id("any-agent"), "a blank thread id must be treated the same as a missing one")
            }
            other => panic!("expected Thread context, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn check_scope_caps_thread_counts_live_entries() {
        use ao_protocol::memory::MemorySource;
        use super::{check_scope_caps, ScopeContext};

        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        store.add_thread("thread-a", "note one", MemorySource::Agent).await.unwrap();

        let ctx = ScopeContext::Thread { thread_id: "thread-a".to_string() };
        let result = check_scope_caps(&store, &ctx, 1, 5).await.unwrap();
        assert!(result.is_some(), "one entry at soft cap 1 must produce a warning");
    }
}

mod thread_round_trip {
    use super::make_store;
    use crate::memory::delete::MemoryDelete;
    use crate::memory::edit::MemoryEdit;
    use crate::memory::list::MemoryList;
    use crate::memory::write::MemoryWrite;
    use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
    use ao_persistence::memory::MemoryStore;
    use serde_json::json;
    use std::sync::Arc;

    fn make_ctx_for_thread(store: Arc<MemoryStore>, thread_id: &str) -> RunnerContext {
        let cwd = std::env::temp_dir();
        RunnerContext::new_with_cwd("session-1", "agent-1", cwd)
            .with_memory_store(store)
            .with_thread(thread_id.to_string())
    }

    fn as_structured(out: ToolOutput) -> serde_json::Value {
        match out {
            ToolOutput::Structured(v) => v,
            other => panic!("expected structured output, got {:?}", other),
        }
    }

    /// Acceptance (a): write → list → edit → delete round-trips through the
    /// tool layer exactly like the durable scopes do.
    #[tokio::test]
    async fn write_list_edit_delete_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let ctx = make_ctx_for_thread(store.clone(), "thread-1");

        let write_out = MemoryWrite
            .invoke(json!({ "scope": "thread", "content": "user is debugging the login flow" }), &ctx)
            .await
            .unwrap();
        let write_value = as_structured(write_out);
        assert!(write_value.get("error").is_none(), "thread write must succeed: {write_value:?}");
        let id = write_value["id"].as_str().unwrap().to_string();
        assert_eq!(write_value["scope"], json!("thread"));

        let list_out = MemoryList.invoke(json!({ "scope": "thread" }), &ctx).await.unwrap();
        let list_value = as_structured(list_out);
        assert_eq!(list_value["total"], json!(1));
        assert_eq!(list_value["entries"][0]["id"], json!(id));

        let edit_out = MemoryEdit
            .invoke(json!({ "id": id, "scope": "thread", "content": "user fixed the login flow" }), &ctx)
            .await
            .unwrap();
        assert_eq!(as_structured(edit_out)["id"], json!(id));

        let entries_after_edit = store.list_thread("thread-1").await.unwrap();
        assert_eq!(entries_after_edit.len(), 1);
        assert_eq!(entries_after_edit[0].content, "user fixed the login flow");

        let delete_out = MemoryDelete.invoke(json!({ "id": id, "scope": "thread" }), &ctx).await.unwrap();
        assert_eq!(as_structured(delete_out)["deleted"], json!(true));

        assert!(store.list_thread("thread-1").await.unwrap().is_empty(), "delete must tombstone the entry");
    }

    /// Acceptance (b): entries written under one thread must not be visible
    /// under a different thread — no cross-thread bleed in either direction.
    #[tokio::test]
    async fn entries_are_isolated_between_threads() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let ctx_a = make_ctx_for_thread(store.clone(), "thread-a");
        let ctx_b = make_ctx_for_thread(store.clone(), "thread-b");

        MemoryWrite
            .invoke(json!({ "scope": "thread", "content": "thread A's working note" }), &ctx_a)
            .await
            .unwrap();
        MemoryWrite
            .invoke(json!({ "scope": "thread", "content": "thread B's working note" }), &ctx_b)
            .await
            .unwrap();

        let list_a = as_structured(MemoryList.invoke(json!({ "scope": "thread" }), &ctx_a).await.unwrap());
        assert_eq!(list_a["total"], json!(1));
        assert_eq!(list_a["entries"][0]["content_preview"], json!("thread A's working note"));

        let list_b = as_structured(MemoryList.invoke(json!({ "scope": "thread" }), &ctx_b).await.unwrap());
        assert_eq!(list_b["total"], json!(1));
        assert_eq!(list_b["entries"][0]["content_preview"], json!("thread B's working note"));

        let entries_a = store.list_thread("thread-a").await.unwrap();
        assert_eq!(entries_a.len(), 1);
        assert!(entries_a.iter().all(|e| e.content == "thread A's working note"));

        let entries_b = store.list_thread("thread-b").await.unwrap();
        assert_eq!(entries_b.len(), 1);
        assert!(entries_b.iter().all(|e| e.content == "thread B's working note"));
    }

    /// Acceptance (c) at the tool layer: a context with no thread id (e.g. a
    /// top-level agent context that never called `.with_thread`, i.e. the
    /// main conversation) must land the write in the agent's default thread
    /// bucket (`default_thread_id`), mirroring `ListThreads`'s treatment of
    /// the implicit main-conversation thread as a real, addressable thread —
    /// never a silent fallback to agent/project/global scope.
    #[tokio::test]
    async fn write_without_thread_id_falls_back_to_default_thread() {
        use ao_protocol::thread::default_thread_id;

        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let cwd = std::env::temp_dir();
        let ctx = RunnerContext::new_with_cwd("session-1", "agent-1", cwd).with_memory_store(store.clone());

        let out = MemoryWrite
            .invoke(json!({ "scope": "thread", "content": "note from the main conversation" }), &ctx)
            .await
            .unwrap();
        let value = as_structured(out);
        assert!(value.get("error").is_none(), "write with no thread id must succeed: {value:?}");
        assert_eq!(value["scope"], json!("thread"));

        let default_id = default_thread_id("agent-1");
        let default_bucket = store.list_thread(&default_id).await.unwrap();
        assert_eq!(default_bucket.len(), 1, "entry must land in the agent's default thread bucket");
        assert_eq!(default_bucket[0].content, "note from the main conversation");

        assert!(store.list("agent-1").await.unwrap().is_empty(), "must not leak into agent scope");
        assert!(store.list_global().await.unwrap().is_empty(), "must not leak into global scope");
    }

    /// Same fallback as `write_without_thread_id_falls_back_to_default_thread`,
    /// exercised through `MemoryList` — the default-thread write above must
    /// be visible again through a second no-thread-id context.
    #[tokio::test]
    async fn list_without_thread_id_falls_back_to_default_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let cwd = std::env::temp_dir();
        let ctx = RunnerContext::new_with_cwd("session-1", "agent-1", cwd).with_memory_store(store);

        MemoryWrite
            .invoke(json!({ "scope": "thread", "content": "note from the main conversation" }), &ctx)
            .await
            .unwrap();

        let out = MemoryList.invoke(json!({ "scope": "thread" }), &ctx).await.unwrap();
        let value = as_structured(out);
        assert_eq!(value["total"], json!(1));
        assert_eq!(value["entries"][0]["content_preview"], json!("note from the main conversation"));
    }

    /// Byte-equal dedup parity with the durable scopes.
    #[tokio::test]
    async fn byte_equal_dedup_works_via_invoke() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let ctx = make_ctx_for_thread(store.clone(), "thread-dedup");

        let first = as_structured(
            MemoryWrite
                .invoke(json!({ "scope": "thread", "content": "remember this exactly" }), &ctx)
                .await
                .unwrap(),
        );
        assert_eq!(first["deduplicated"], json!(false));

        let second = as_structured(
            MemoryWrite
                .invoke(json!({ "scope": "thread", "content": "remember this exactly" }), &ctx)
                .await
                .unwrap(),
        );
        assert_eq!(second["deduplicated"], json!(true));
        assert_eq!(second["id"], first["id"]);

        assert_eq!(store.list_thread("thread-dedup").await.unwrap().len(), 1);
    }

    /// Hitting the thread hard cap never rejects the write — the oldest
    /// live entry is dropped to make room, silently (no `evicted` field).
    #[tokio::test]
    async fn write_at_hard_cap_drops_oldest_entry_instead_of_rejecting() {
        use crate::memory::THREAD_HARD_CAP;

        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(&tmp);
        let ctx = make_ctx_for_thread(store.clone(), "thread-cap");

        for i in 0..THREAD_HARD_CAP {
            MemoryWrite
                .invoke(json!({ "scope": "thread", "content": format!("filler note {i}") }), &ctx)
                .await
                .unwrap();
        }
        let before = store.list_thread("thread-cap").await.unwrap();
        assert_eq!(before.len(), THREAD_HARD_CAP, "setup must land exactly at the hard cap");
        let oldest_id = before.iter().min_by_key(|e| e.created_at).unwrap().id.clone();

        let out = MemoryWrite
            .invoke(json!({ "scope": "thread", "content": "the write that must not be rejected" }), &ctx)
            .await
            .unwrap();
        let value = as_structured(out);
        assert!(value.get("error").is_none(), "hitting the thread hard cap must never reject the write");
        assert!(value.get("evicted").is_none(), "thread eviction is silent — no evicted field is reported");

        let after = store.list_thread("thread-cap").await.unwrap();
        assert_eq!(after.len(), THREAD_HARD_CAP, "count must stay at the cap: one dropped, one added");
        assert!(
            after.iter().all(|e| e.id != oldest_id),
            "the oldest entry must have been dropped to make room"
        );
    }
}
