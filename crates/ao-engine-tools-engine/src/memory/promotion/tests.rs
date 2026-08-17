use super::*;
use ao_persistence::paths::DataRoot;
use ao_protocol::memory::{MemorySource, MemoryStatus};
use chrono::Utc as ChronoUtc;
use crate::memory::promotion_budget::{PromotionBudgetController, PromotionBudgetGate};

fn staging_store(tmp: &tempfile::TempDir) -> ReflectionStagingStore {
    ReflectionStagingStore::new(DataRoot::new(tmp.path()))
}

fn promote_verdict(content: &str) -> PromotionVerdict {
    PromotionVerdict::Promote {
        generalized_content: content.to_string(),
        rationale: "generalizable".to_string(),
    }
}

fn durable_entry(id: &str, content: &str, source: Option<MemorySource>, pinned: bool) -> MemoryEntry {
    MemoryEntry {
        id: id.to_string(),
        content: content.to_string(),
        created_at: ChronoUtc::now(),
        source,
        scope: MemoryScope::Agent,
        scope_key: Some("agent-1".to_string()),
        updated_at: ChronoUtc::now(),
        deleted_at: None,
        confidence: 1.0,
        status: MemoryStatus::Active,
        superseded_by: None,
        pinned,
        decay_score: 1.0,
    }
}

#[tokio::test]
async fn a_promote_verdict_stages_the_generalized_content_as_a_pending_memory_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);

    let verdict = PromotionVerdict::Promote {
        generalized_content: "Prefer concise commit messages.".to_string(),
        rationale: "stated as a durable, recurring preference".to_string(),
    };

    let staged = apply_promotion_verdict(&staging, "agent-1", "thread-1", verdict, &[])
        .await
        .unwrap();
    let candidate = staged.expect("a Promote verdict must stage a candidate");

    assert_eq!(candidate.content, "Prefer concise commit messages.");
    assert_eq!(candidate.kind, ArtifactKind::Memory);
    assert_eq!(candidate.agent_id, "agent-1");
    assert_eq!(candidate.source_thread_id, "thread-1");
    assert_eq!(candidate.status, ReflectionCandidateStatus::Pending);
    assert_eq!(candidate.target_scope, MemoryScope::Agent);
    assert_eq!(candidate.target_scope_key, Some("agent-1".to_string()));
    assert_eq!(candidate.contradicts, None);
    assert!(candidate.reason.contains("stated as a durable, recurring preference"));

    let pending = staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, candidate.id);
}

#[tokio::test]
async fn a_reject_verdict_never_reaches_the_staging_store() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);

    let verdict = PromotionVerdict::Reject {
        rationale: "only makes sense given this one conversation's specific file".to_string(),
    };

    let staged = apply_promotion_verdict(&staging, "agent-1", "thread-1", verdict, &[])
        .await
        .unwrap();
    assert!(staged.is_none(), "a Reject verdict must not produce a candidate");

    let pending = staging.list_pending("agent-1").await.unwrap();
    assert!(pending.is_empty(), "a Reject verdict must never reach the review queue");
}

#[tokio::test]
async fn a_promote_verdict_with_empty_generalized_content_is_rejected_outright() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);

    let verdict = PromotionVerdict::Promote {
        generalized_content: "   ".to_string(),
        rationale: "malformed judge reply".to_string(),
    };

    let err = apply_promotion_verdict(&staging, "agent-1", "thread-1", verdict, &[])
        .await
        .unwrap_err();
    assert!(matches!(err, AoError::ValidationError(_)));

    assert!(staging.list_pending("agent-1").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_promoted_candidate_never_auto_confirms_and_stays_out_of_live_memory() {
    // A model-judged promotion goes through the exact same
    // `CandidateOrigin::Reflected` gate as every other reflected candidate
    // — it always stages for review and never auto-confirms, regardless of
    // how confident the judge's own verdict was.
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);

    let verdict = PromotionVerdict::Promote {
        generalized_content: "Always run the linter before committing.".to_string(),
        rationale: "recurring convention across threads".to_string(),
    };

    apply_promotion_verdict(&staging, "agent-1", "thread-1", verdict, &[])
        .await
        .unwrap();

    let pending = staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].status,
        ReflectionCandidateStatus::Pending,
        "a promoted candidate must wait for human review, never auto-confirm"
    );
}

#[tokio::test]
async fn staged_candidates_from_different_agents_do_not_mix() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);

    apply_promotion_verdict(
        &staging,
        "agent-1",
        "thread-1",
        PromotionVerdict::Promote {
            generalized_content: "Agent-1's durable note.".to_string(),
            rationale: "generalizable".to_string(),
        },
        &[],
    )
    .await
    .unwrap();
    apply_promotion_verdict(
        &staging,
        "agent-2",
        "thread-2",
        PromotionVerdict::Promote {
            generalized_content: "Agent-2's durable note.".to_string(),
            rationale: "generalizable".to_string(),
        },
        &[],
    )
    .await
    .unwrap();

    assert_eq!(staging.list_pending("agent-1").await.unwrap().len(), 1);
    assert_eq!(staging.list_pending("agent-2").await.unwrap().len(), 1);
}

// --- supersede-on-promote (curation half) --------------------------------

#[tokio::test]
async fn promoting_over_an_agent_authored_duplicate_links_contradicts_for_supersede() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);

    let existing = vec![durable_entry(
        "mem-auto-1",
        "User prefers concise commit messages",
        Some(MemorySource::Agent),
        false,
    )];

    let staged = apply_promotion_verdict(
        &staging,
        "agent-1",
        "thread-1",
        promote_verdict("User prefers concise commit messages"),
        &existing,
    )
    .await
    .unwrap()
    .expect("a Promote verdict must stage a candidate");

    assert_eq!(
        staged.contradicts,
        Some("mem-auto-1".to_string()),
        "a match against an AUTO-sourced entry must be linked so review::keep supersedes it"
    );
}

#[tokio::test]
async fn promoting_over_a_manual_entry_never_links_contradicts() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);

    let existing = vec![durable_entry(
        "mem-manual-1",
        "User prefers concise commit messages",
        Some(MemorySource::Manual),
        false,
    )];

    let staged = apply_promotion_verdict(
        &staging,
        "agent-1",
        "thread-1",
        promote_verdict("User prefers concise commit messages"),
        &existing,
    )
    .await
    .unwrap()
    .expect("a Promote verdict must still stage — just without a supersede link");

    assert_eq!(
        staged.contradicts, None,
        "a Manual/user-authored entry must never be auto-superseded, even after human review"
    );
}

#[tokio::test]
async fn promoting_over_a_pinned_entry_never_links_contradicts() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);

    let existing = vec![durable_entry(
        "mem-pinned-1",
        "User prefers concise commit messages",
        Some(MemorySource::Agent),
        true,
    )];

    let staged = apply_promotion_verdict(
        &staging,
        "agent-1",
        "thread-1",
        promote_verdict("User prefers concise commit messages"),
        &existing,
    )
    .await
    .unwrap()
    .expect("a Promote verdict must still stage — just without a supersede link");

    assert_eq!(
        staged.contradicts, None,
        "a pinned entry must never be auto-superseded regardless of source"
    );
}

#[tokio::test]
async fn promoting_with_no_similar_existing_entry_leaves_contradicts_unset() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);

    let existing = vec![durable_entry(
        "mem-unrelated-1",
        "Completely unrelated fact about deploy cadence",
        Some(MemorySource::Agent),
        false,
    )];

    let staged = apply_promotion_verdict(
        &staging,
        "agent-1",
        "thread-1",
        promote_verdict("User prefers concise commit messages"),
        &existing,
    )
    .await
    .unwrap()
    .expect("a Promote verdict must stage a candidate");

    assert_eq!(staged.contradicts, None);
}

// --- apply_promotion_verdict_with_budget (hybrid enforcement) -----------

#[tokio::test]
async fn a_reject_verdict_never_consumes_a_budget_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);
    // Cold start: budget is exactly `MIN_BUDGET`.
    let mut gate = PromotionBudgetGate::new(PromotionBudgetController::new());

    for _ in 0..5 {
        let staged = apply_promotion_verdict_with_budget(
            &staging,
            "agent-1",
            "thread-1",
            PromotionVerdict::Reject { rationale: "n/a".to_string() },
            &[],
            &mut gate,
        )
        .await
        .unwrap();
        assert!(staged.is_none());
    }

    // The budget slot is still untouched — a Promote verdict right after
    // must still be allowed through.
    let staged = apply_promotion_verdict_with_budget(
        &staging,
        "agent-1",
        "thread-1",
        promote_verdict("durable fact"),
        &[],
        &mut gate,
    )
    .await
    .unwrap();
    assert!(staged.is_some(), "reject verdicts must never eat into the promote budget");
}

#[tokio::test]
async fn a_promote_verdict_beyond_the_budget_is_discarded_like_a_reject() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = staging_store(&tmp);
    // Cold start: budget is exactly `MIN_BUDGET` (1) per cycle.
    let mut gate = PromotionBudgetGate::new(PromotionBudgetController::new());

    let first = apply_promotion_verdict_with_budget(
        &staging,
        "agent-1",
        "thread-1",
        promote_verdict("first durable fact"),
        &[],
        &mut gate,
    )
    .await
    .unwrap();
    assert!(first.is_some(), "the first promote must fit inside the cold-start budget");

    let second = apply_promotion_verdict_with_budget(
        &staging,
        "agent-1",
        "thread-1",
        promote_verdict("second durable fact"),
        &[],
        &mut gate,
    )
    .await
    .unwrap();
    assert!(
        second.is_none(),
        "a second promote in the same cycle must be discarded once the budget is exhausted, \
         no matter how confident the judge was"
    );

    let pending = staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1, "only the within-budget candidate may reach staging");
}
