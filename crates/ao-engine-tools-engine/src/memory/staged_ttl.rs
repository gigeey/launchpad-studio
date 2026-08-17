//! Staged-candidate TTL sweep — bounds the "Pending review" backlog
//! (`ReflectionStagingStore`) so an unreviewed candidate does not sit
//! forever. Distinct from, and deliberately not a reuse of,
//! [`super::eviction::select_eviction_candidate`]/[`super::eviction::thread_eviction_sweep`]:
//! those two evict *live memory entries* out of a scope at its cap; this
//! sweep expires *staged candidates* awaiting human review out of the queue
//! purely by age, regardless of how many are pending. Both land on the same
//! non-destructive posture — soft-tombstone, never a hard delete — via
//! [`ao_protocol::reflection_candidate::ReflectionCandidateStatus::Expired`].
//!
//! Ordering with the promotion judge (`ao_engine::memory_promotion`):
//! this sweep only ever touches a candidate already in
//! [`ReflectionCandidateStatus::Pending`], read fresh at sweep time. A
//! candidate the promotion judge just staged carries a `created_at` of "now"
//! and so cannot already be older than the TTL — it is never a false
//! positive here, independent of which of the two runs first in a given
//! tick. Nothing in this module writes to `Confirmed`/`Rejected`/`Distilled`
//! candidates, so a human's own review decision (and the acceptance-rate
//! signal `memory::promotion_budget` derives from it) is never touched by
//! an expiry sweep.

#[cfg(test)]
mod tests;

use chrono::{DateTime, Duration, Utc};

use ao_persistence::reflection_staging::ReflectionStagingStore;
use ao_protocol::error::AoError;
use ao_protocol::reflection_candidate::{ReflectionCandidate, ReflectionCandidateStatus};

/// Default TTL, in days, for an unreviewed staged candidate. Tunable by
/// changing this constant — not tuned against production data, same
/// untuned-starting-point posture `eviction.rs`'s `RECENCY_HALF_LIFE_DAYS`
/// documents for itself. Evaluated against each candidate's own
/// `created_at`, so the very first sweep after this ships also drains
/// whatever backlog already accumulated under the old "retained forever"
/// behavior, not just candidates staged from here on.
pub const STAGED_CANDIDATE_TTL_DAYS: i64 = 7;

/// Pure selection: ids of every `Pending` candidate in `candidates` whose
/// `created_at` is at least `ttl` older than `now`. Anything already
/// `Confirmed`/`Rejected`/`Distilled`/`Expired` is left alone regardless of
/// age — this sweep only ever drains the *unreviewed* tail of the queue.
pub fn expired_staged_candidate_ids(
    candidates: &[ReflectionCandidate],
    now: DateTime<Utc>,
    ttl: Duration,
) -> Vec<String> {
    candidates
        .iter()
        .filter(|c| c.status == ReflectionCandidateStatus::Pending)
        .filter(|c| now - c.created_at >= ttl)
        .map(|c| c.id.clone())
        .collect()
}

/// Sweep one agent's staged candidates: read fresh from `staging`, select
/// every `Pending` candidate older than `ttl` (per [`expired_staged_candidate_ids`]),
/// and flip them to [`ReflectionCandidateStatus::Expired`] — a soft-tombstone
/// that drops out of [`ReflectionStagingStore::list_pending`] but stays on
/// disk for audit, exactly like every other status transition this store
/// supports. Returns the number of candidates expired.
pub async fn sweep_expired_staged_candidates(
    staging: &ReflectionStagingStore,
    agent_id: &str,
    now: DateTime<Utc>,
    ttl: Duration,
) -> Result<usize, AoError> {
    let all = staging.read_all(agent_id).await?;
    let expired_ids = expired_staged_candidate_ids(&all, now, ttl);
    if expired_ids.is_empty() {
        return Ok(0);
    }
    let count = expired_ids.len();
    staging
        .update_status(agent_id, &expired_ids, ReflectionCandidateStatus::Expired)
        .await?;
    Ok(count)
}
