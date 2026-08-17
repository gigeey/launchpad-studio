use super::*;
use ao_persistence::paths::DataRoot;
use ao_protocol::memory::MemoryScope;
use ao_protocol::outcome::ArtifactKind;

fn candidate_at(id: &str, created_at: DateTime<Utc>, status: ReflectionCandidateStatus) -> ReflectionCandidate {
    ReflectionCandidate {
        id: id.to_string(),
        kind: ArtifactKind::Memory,
        agent_id: "agent-1".to_string(),
        source_thread_id: "thread-1".to_string(),
        content: "some staged content".to_string(),
        status,
        target_scope: MemoryScope::Agent,
        target_scope_key: Some("agent-1".to_string()),
        contradicts: None,
        reason: "staged for review".to_string(),
        created_at,
    }
}

// --- expired_staged_candidate_ids (pure selection) -----------------------

#[test]
fn a_pending_candidate_older_than_ttl_is_selected() {
    let now = Utc::now();
    let ttl = Duration::days(STAGED_CANDIDATE_TTL_DAYS);
    let old = candidate_at("old", now - Duration::days(8), ReflectionCandidateStatus::Pending);

    let expired = expired_staged_candidate_ids(&[old], now, ttl);
    assert_eq!(expired, vec!["old".to_string()]);
}

#[test]
fn a_fresh_pending_candidate_is_retained() {
    let now = Utc::now();
    let ttl = Duration::days(STAGED_CANDIDATE_TTL_DAYS);
    let fresh = candidate_at("fresh", now - Duration::hours(1), ReflectionCandidateStatus::Pending);

    let expired = expired_staged_candidate_ids(&[fresh], now, ttl);
    assert!(expired.is_empty());
}

#[test]
fn exactly_at_the_ttl_boundary_is_selected() {
    let now = Utc::now();
    let ttl = Duration::days(STAGED_CANDIDATE_TTL_DAYS);
    let boundary = candidate_at("boundary", now - ttl, ReflectionCandidateStatus::Pending);

    let expired = expired_staged_candidate_ids(&[boundary], now, ttl);
    assert_eq!(expired, vec!["boundary".to_string()]);
}

#[test]
fn non_pending_candidates_are_never_selected_regardless_of_age() {
    let now = Utc::now();
    let ttl = Duration::days(STAGED_CANDIDATE_TTL_DAYS);
    let ancient = now - Duration::days(365);

    let candidates = vec![
        candidate_at("confirmed", ancient, ReflectionCandidateStatus::Confirmed),
        candidate_at("rejected", ancient, ReflectionCandidateStatus::Rejected),
        candidate_at("distilled", ancient, ReflectionCandidateStatus::Distilled),
        candidate_at("already-expired", ancient, ReflectionCandidateStatus::Expired),
    ];

    assert!(expired_staged_candidate_ids(&candidates, now, ttl).is_empty());
}

#[test]
fn a_just_promoted_item_is_not_swept_in_the_same_pass() {
    // Simulates the ordering concern directly: a candidate the promotion
    // judge staged moments before the sweep runs has `created_at` close to
    // `now`, so it can never look "older than TTL" in the same tick that
    // produced it — regardless of which of the two actually ran first.
    let now = Utc::now();
    let ttl = Duration::days(STAGED_CANDIDATE_TTL_DAYS);
    let just_promoted = candidate_at("just-promoted", now, ReflectionCandidateStatus::Pending);

    assert!(expired_staged_candidate_ids(&[just_promoted], now, ttl).is_empty());
}

// --- sweep_expired_staged_candidates (async driver) ----------------------

#[tokio::test]
async fn sweep_flips_expired_candidates_to_expired_status_and_leaves_them_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = ReflectionStagingStore::new(DataRoot::new(tmp.path()));
    let now = Utc::now();

    staging
        .stage("agent-1", &candidate_at("old", now - Duration::days(30), ReflectionCandidateStatus::Pending))
        .await
        .unwrap();
    staging
        .stage("agent-1", &candidate_at("fresh", now, ReflectionCandidateStatus::Pending))
        .await
        .unwrap();

    let count =
        sweep_expired_staged_candidates(&staging, "agent-1", now, Duration::days(STAGED_CANDIDATE_TTL_DAYS))
            .await
            .unwrap();
    assert_eq!(count, 1);

    // Cleared from the pending/"Held for review" queue...
    let pending = staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "fresh");

    // ...but still readable on disk for audit, soft-tombstoned rather than
    // deleted.
    let all = staging.read_all("agent-1").await.unwrap();
    assert_eq!(all.len(), 2, "expiry must never hard-delete a candidate");
    let old = all.iter().find(|c| c.id == "old").unwrap();
    assert_eq!(old.status, ReflectionCandidateStatus::Expired);
}

#[tokio::test]
async fn sweep_retroactively_drains_a_backlog_staged_before_this_shipped() {
    // The TTL is evaluated against each candidate's own `created_at`, so a
    // long-standing backlog (170+ items accumulated before this sweep
    // existed) is drained on the very first sweep, not just candidates
    // staged from here on.
    let tmp = tempfile::tempdir().unwrap();
    let staging = ReflectionStagingStore::new(DataRoot::new(tmp.path()));
    let now = Utc::now();
    let long_ago = now - Duration::days(400);

    for i in 0..5 {
        staging
            .stage(
                "agent-1",
                &candidate_at(&format!("backlog-{i}"), long_ago, ReflectionCandidateStatus::Pending),
            )
            .await
            .unwrap();
    }

    let count =
        sweep_expired_staged_candidates(&staging, "agent-1", now, Duration::days(STAGED_CANDIDATE_TTL_DAYS))
            .await
            .unwrap();
    assert_eq!(count, 5);
    assert!(staging.list_pending("agent-1").await.unwrap().is_empty());
}

#[tokio::test]
async fn sweep_on_an_agent_with_no_staged_candidates_is_a_harmless_no_op() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = ReflectionStagingStore::new(DataRoot::new(tmp.path()));

    let count = sweep_expired_staged_candidates(
        &staging,
        "nonexistent-agent",
        Utc::now(),
        Duration::days(STAGED_CANDIDATE_TTL_DAYS),
    )
    .await
    .unwrap();
    assert_eq!(count, 0);
}
