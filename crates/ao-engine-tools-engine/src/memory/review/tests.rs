use super::*;
use ao_persistence::paths::DataRoot;
use ao_protocol::memory::MemoryStatus;
use chrono::Utc;
use uuid::Uuid;

fn make_stores(tmp: &tempfile::TempDir) -> (MemoryStore, ReflectionStagingStore) {
    let root = DataRoot::new(tmp.path());
    (MemoryStore::new(root.clone()), ReflectionStagingStore::new(root))
}

fn memory_candidate(agent_id: &str, content: &str, target_scope: MemoryScope, target_scope_key: Option<String>) -> ReflectionCandidate {
    ReflectionCandidate {
        id: Uuid::new_v4().to_string(),
        kind: ArtifactKind::Memory,
        agent_id: agent_id.to_string(),
        source_thread_id: "session-1".to_string(),
        content: content.to_string(),
        status: ReflectionCandidateStatus::Pending,
        target_scope,
        target_scope_key,
        contradicts: None,
        reason: "test candidate".to_string(),
        created_at: Utc::now(),
    }
}

// --- keep ---

#[tokio::test]
async fn keep_applies_content_unchanged_and_marks_confirmed() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let candidate = memory_candidate("agent-1", "prefer async/await", MemoryScope::Agent, Some("agent-1".to_string()));
    staging.stage("agent-1", &candidate).await.unwrap();

    let outcome = keep(&store, &staging, "agent-1", &candidate.id).await.unwrap();
    assert_eq!(outcome.candidate_id, candidate.id);
    assert!(outcome.superseded.is_none());
    assert!(!outcome.pinned);

    let entries = store.list("agent-1").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, outcome.memory_id);
    assert_eq!(entries[0].content, "prefer async/await");
    assert_eq!(entries[0].source, Some(MemorySource::Agent));
    assert!(!entries[0].pinned);

    let pending = staging.list_pending("agent-1").await.unwrap();
    assert!(pending.is_empty(), "kept candidate must no longer be pending");
    let all = staging.read_all("agent-1").await.unwrap();
    assert_eq!(all[0].status, ReflectionCandidateStatus::Confirmed);
}

#[tokio::test]
async fn keep_missing_candidate_errors_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let err = keep(&store, &staging, "agent-1", "nonexistent").await.unwrap_err();
    assert!(matches!(err, AoError::MemoryNotFound(_)));
}

#[tokio::test]
async fn keep_already_resolved_candidate_errors_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let candidate = memory_candidate("agent-1", "content", MemoryScope::Agent, Some("agent-1".to_string()));
    staging.stage("agent-1", &candidate).await.unwrap();
    keep(&store, &staging, "agent-1", &candidate.id).await.unwrap();

    let err = keep(&store, &staging, "agent-1", &candidate.id).await.unwrap_err();
    assert!(matches!(err, AoError::Conflict(_)), "acting on an already-resolved candidate must not silently no-op");
}

#[tokio::test]
async fn keep_on_skill_candidate_errors_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let mut candidate = memory_candidate("agent-1", "content", MemoryScope::Agent, Some("agent-1".to_string()));
    candidate.kind = ArtifactKind::Skill;
    staging.stage("agent-1", &candidate).await.unwrap();

    let err = keep(&store, &staging, "agent-1", &candidate.id).await.unwrap_err();
    assert!(matches!(err, AoError::ValidationError(_)));
}

#[tokio::test]
async fn keep_resolves_a_named_contradiction_by_superseding_the_old_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let old = store.add("agent-1", "old fact", MemorySource::Agent).await.unwrap();

    let mut candidate = memory_candidate("agent-1", "new fact", MemoryScope::Agent, Some("agent-1".to_string()));
    candidate.contradicts = Some(old.id.clone());
    staging.stage("agent-1", &candidate).await.unwrap();

    let outcome = keep(&store, &staging, "agent-1", &candidate.id).await.unwrap();
    assert_eq!(outcome.superseded, Some(old.id.clone()));

    let entries = store.list("agent-1").await.unwrap();
    let old_entry = entries.iter().find(|e| e.id == old.id).unwrap();
    assert_eq!(old_entry.status, MemoryStatus::Superseded);
    assert_eq!(old_entry.superseded_by, Some(outcome.memory_id));
}

#[tokio::test]
async fn keep_applies_into_project_scope_using_target_scope_key() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let candidate = memory_candidate("agent-1", "team uses trunk-based dev", MemoryScope::Project, Some("hash-abc".to_string()));
    staging.stage("agent-1", &candidate).await.unwrap();

    let outcome = keep(&store, &staging, "agent-1", &candidate.id).await.unwrap();
    let entries = store.list_project("hash-abc").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, outcome.memory_id);
}

#[tokio::test]
async fn keep_applies_into_global_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let candidate = memory_candidate("agent-1", "prefer trunk-based dev", MemoryScope::Global, None);
    staging.stage("agent-1", &candidate).await.unwrap();

    let outcome = keep(&store, &staging, "agent-1", &candidate.id).await.unwrap();
    let entries = store.list_global().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, outcome.memory_id);
}

// --- edit ---

#[tokio::test]
async fn edit_applies_edited_content_tagged_manual() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let candidate = memory_candidate("agent-1", "original proposal", MemoryScope::Agent, Some("agent-1".to_string()));
    staging.stage("agent-1", &candidate).await.unwrap();

    let outcome = edit(&store, &staging, "agent-1", &candidate.id, "human-corrected content").await.unwrap();

    let entries = store.list("agent-1").await.unwrap();
    let entry = entries.iter().find(|e| e.id == outcome.memory_id).unwrap();
    assert_eq!(entry.content, "human-corrected content");
    assert_eq!(entry.source, Some(MemorySource::Manual));

    let all = staging.read_all("agent-1").await.unwrap();
    assert_eq!(all[0].status, ReflectionCandidateStatus::Confirmed);
}

#[tokio::test]
async fn edit_empty_content_errors_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let candidate = memory_candidate("agent-1", "content", MemoryScope::Agent, Some("agent-1".to_string()));
    staging.stage("agent-1", &candidate).await.unwrap();

    let err = edit(&store, &staging, "agent-1", &candidate.id, "   ").await.unwrap_err();
    assert!(matches!(err, AoError::ValidationError(_)));

    // Rejecting empty content must not consume the candidate.
    let pending = staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1);
}

// --- forget ---

#[tokio::test]
async fn forget_writes_nothing_and_marks_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let candidate = memory_candidate("agent-1", "content", MemoryScope::Agent, Some("agent-1".to_string()));
    staging.stage("agent-1", &candidate).await.unwrap();

    forget(&staging, "agent-1", &candidate.id).await.unwrap();

    assert!(store.list("agent-1").await.unwrap().is_empty(), "forget must never write a live entry");
    let all = staging.read_all("agent-1").await.unwrap();
    assert_eq!(all[0].status, ReflectionCandidateStatus::Rejected);
    assert!(staging.list_pending("agent-1").await.unwrap().is_empty());
}

#[tokio::test]
async fn forget_already_resolved_candidate_errors_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let (_store, staging) = make_stores(&tmp);
    let candidate = memory_candidate("agent-1", "content", MemoryScope::Agent, Some("agent-1".to_string()));
    staging.stage("agent-1", &candidate).await.unwrap();
    forget(&staging, "agent-1", &candidate.id).await.unwrap();

    let err = forget(&staging, "agent-1", &candidate.id).await.unwrap_err();
    assert!(matches!(err, AoError::Conflict(_)));
}

// --- pin ---

#[tokio::test]
async fn pin_applies_content_and_sets_pinned_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let candidate = memory_candidate("agent-1", "important fact", MemoryScope::Agent, Some("agent-1".to_string()));
    staging.stage("agent-1", &candidate).await.unwrap();

    let outcome = pin(&store, &staging, "agent-1", &candidate.id).await.unwrap();
    assert!(outcome.pinned);

    let entries = store.list("agent-1").await.unwrap();
    let entry = entries.iter().find(|e| e.id == outcome.memory_id).unwrap();
    assert!(entry.pinned);
    assert_eq!(entry.source, Some(MemorySource::Agent));
}

// --- undo ---

#[tokio::test]
async fn undo_reverses_an_autoconfirmed_agent_scope_write() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _staging) = make_stores(&tmp);

    // Simulates `MemoryWrite`'s AutoConfirm path: a fresh agent-scope write
    // that never touches the staging queue at all.
    let entry = store.add("agent-1", "auto-confirmed fact", MemorySource::Agent).await.unwrap();
    assert_eq!(store.list("agent-1").await.unwrap().len(), 1);

    let outcome = undo(&store, &MemoryScope::Agent, Some("agent-1"), &entry.id).await.unwrap();
    assert_eq!(outcome.memory_id, entry.id);
    assert_eq!(outcome.restored, None);

    assert!(store.list("agent-1").await.unwrap().is_empty(), "undo must remove the auto-confirmed entry");
}

#[tokio::test]
async fn undo_reverses_a_kept_candidate_the_same_way() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let candidate = memory_candidate("agent-1", "kept fact", MemoryScope::Agent, Some("agent-1".to_string()));
    staging.stage("agent-1", &candidate).await.unwrap();
    let outcome = keep(&store, &staging, "agent-1", &candidate.id).await.unwrap();

    undo(&store, &MemoryScope::Agent, Some("agent-1"), &outcome.memory_id).await.unwrap();
    assert!(store.list("agent-1").await.unwrap().is_empty());
}

#[tokio::test]
async fn undo_restores_the_entry_a_write_superseded() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, staging) = make_stores(&tmp);
    let old = store.add("agent-1", "old fact", MemorySource::Agent).await.unwrap();

    let mut candidate = memory_candidate("agent-1", "new fact", MemoryScope::Agent, Some("agent-1".to_string()));
    candidate.contradicts = Some(old.id.clone());
    staging.stage("agent-1", &candidate).await.unwrap();
    let outcome = keep(&store, &staging, "agent-1", &candidate.id).await.unwrap();

    // Sanity: old entry really is superseded before undo runs.
    let entries = store.list("agent-1").await.unwrap();
    assert_eq!(entries.iter().find(|e| e.id == old.id).unwrap().status, MemoryStatus::Superseded);

    let undo_outcome = undo(&store, &MemoryScope::Agent, Some("agent-1"), &outcome.memory_id).await.unwrap();
    assert_eq!(undo_outcome.restored, Some(old.id.clone()));

    let entries = store.list("agent-1").await.unwrap();
    let restored = entries.iter().find(|e| e.id == old.id).unwrap();
    assert_eq!(restored.status, MemoryStatus::Active, "undo must restore the superseded entry to Active");
    assert_eq!(restored.superseded_by, None);
    assert!(entries.iter().all(|e| e.id != outcome.memory_id), "the undone entry itself must be gone");
}

#[tokio::test]
async fn undo_missing_entry_errors_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _staging) = make_stores(&tmp);
    let err = undo(&store, &MemoryScope::Agent, Some("agent-1"), "nonexistent").await.unwrap_err();
    assert!(matches!(err, AoError::MemoryNotFound(_)));
}

#[tokio::test]
async fn undo_agent_scope_without_scope_key_errors_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _staging) = make_stores(&tmp);
    let err = undo(&store, &MemoryScope::Agent, None, "some-id").await.unwrap_err();
    assert!(matches!(err, AoError::ValidationError(_)));
}

#[tokio::test]
async fn undo_reverses_a_global_scope_write() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _staging) = make_stores(&tmp);
    let entry = store.add_global("global fact", MemorySource::Agent).await.unwrap();

    undo(&store, &MemoryScope::Global, None, &entry.id).await.unwrap();
    assert!(store.list_global().await.unwrap().is_empty());
}

#[tokio::test]
async fn undo_reverses_a_project_scope_write() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _staging) = make_stores(&tmp);
    let op = store.add_project("hash-abc", "project fact", MemorySource::Agent).await.unwrap();

    undo(&store, &MemoryScope::Project, Some("hash-abc"), &op.id).await.unwrap();
    assert!(store.list_project("hash-abc").await.unwrap().is_empty());
}
