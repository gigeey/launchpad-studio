//! Promotion-judge staging half.
//!
//! A thread-scope memory entry that survives long enough to be judge-
//! eligible (a behavioral pre-filter — not this module's concern) is
//! handed to a skeptical model call
//! (`ao_engine_tools_runner::promotion_judge`, one layer up, where the
//! `ProviderClient` seam this crate does not depend on actually lives) that
//! decides whether it generalizes beyond the thread it was captured in.
//! This module only ever sees the RESULT of that call — [`PromotionVerdict`]
//! — and turns a `Promote` verdict into a [`ReflectionCandidate`] in the
//! SAME `ReflectionStagingStore` every other out-of-band candidate lands in.
//!
//! Mirrors `memory::review`'s split of responsibilities: that module is the
//! human-facing half of this same queue (`keep`/`edit`/`forget`/`pin`); this
//! is a second, model-judged PRODUCER into it — never a parallel store, and
//! never a second review surface.
//!
//! Every promoted candidate is tagged `ArtifactType::Memory`/
//! `ArtifactKind::Memory` — the same tag the reflection pass's own `Memory`
//! candidates already carry. Nothing here (or anywhere upstream of it)
//! hardcodes `Skill` as the only artifact type a staged candidate can be.
//!
//! Supersede-on-promote: a `Promote` verdict is checked against the
//! destination scope's own live entries with the same similarity guard
//! `ao_engine::reflection_subscriber` already applies to ordinary reflected
//! candidates (`contradiction::find_contradiction`). A match marks the
//! resulting candidate's `contradicts` field, so confirming it in
//! `memory::review` (`keep`/`edit`/`pin`) supersedes the old entry instead of
//! appending a duplicate — durable memory stays curated rather than
//! accumulating restatements every time a thread-scope note generalizes.
//! A match against a `Manual`/user-authored or `pinned` entry is never
//! linked this way — those entries are excluded from the contradiction scan
//! entirely, so nothing this module produces can ever supersede one, even
//! after a human confirms the candidate (user artifacts always
//! outrank agent artifacts).

#[cfg(test)]
mod tests;

use chrono::Utc;
use uuid::Uuid;

use ao_engine_tools_core::trust_gate::{
    stage_candidate, ArtifactType, CandidateOrigin, CandidateScope, StagingRequest,
};
use ao_persistence::reflection_staging::ReflectionStagingStore;
use ao_protocol::error::AoError;
use ao_protocol::memory::{MemoryEntry, MemoryScope};
use ao_protocol::outcome::ArtifactKind;
use ao_protocol::reflection_candidate::{ReflectionCandidate, ReflectionCandidateStatus};

use crate::memory::contradiction::{default_scorer, find_contradiction};
use crate::memory::promotion_budget::PromotionBudgetGate;

/// The skeptical promotion judge's verdict on one thread-scope entry.
///
/// Produced by `ao_engine_tools_runner::promotion_judge::PromotionJudgeEngine`
/// (the model-invoking half, kept out of this crate on purpose — see the
/// module doc); consumed here to decide whether anything reaches the
/// durable staging queue. This type, not a raw JSON blob, is the seam
/// between the two crates: the runner crate depends on this one already, so
/// defining it here (rather than duplicating it upstream) keeps the verdict
/// shape a single source of truth.
#[derive(Debug, Clone, PartialEq)]
pub enum PromotionVerdict {
    /// The judge decided this entry would help across many FUTURE threads,
    /// not just the one it was captured in. `generalized_content` is the
    /// judge's OWN reusable rewrite, stripped of thread-specific instance
    /// details — never the concrete original text verbatim. This is plan
    /// the skill distillation move, applied here to memory instead.
    Promote {
        generalized_content: String,
        rationale: String,
    },
    /// Thread-specific, or the judge could not confidently tell — must not
    /// reach the review queue.
    Reject { rationale: String },
}

/// Apply `verdict` for one entry originating in `thread_id`: stage a
/// `Promote` verdict into `staging` as a `Memory`-kind [`ReflectionCandidate`]
/// and return it, or do nothing at all for a `Reject` and return `Ok(None)`.
///
/// A `Reject` verdict never touches `staging` — it does not even reach the
/// review shortlist, let alone a live scope. This is one step more
/// conservative than the baseline guarantee that a judged candidate,
/// worst case, only makes the review queue noisier and never writes
/// durable memory on its own: here, a rejected verdict does not even make
/// the queue noisier.
///
/// Every `Promote` verdict runs through the SAME trust gate every other
/// reflected candidate goes through: `CandidateOrigin::Reflected` always
/// stages for review and never auto-confirms, regardless of scope — a
/// model-judged promotion is no more trusted than the reflection pass's own
/// first-pass proposals.
///
/// `existing_durable_entries` is the destination scope's current live memory
/// (e.g. `MemoryStore::list(agent_id)` for the `Agent`-scope target this
/// function always proposes) — supplied by the caller rather than fetched
/// here so this stays a pure staging step with no store dependency beyond
/// `staging` itself. Used only for the supersede-on-promote check: a match
/// against a `Manual`/user-authored or `pinned` entry is excluded before the
/// scan even runs, so the resulting candidate's `contradicts` field can
/// never point at one — [`memory::review`](crate::memory::review)'s
/// `keep`/`edit`/`pin` may resolve `contradicts` without a second safety
/// check, exactly as it already does for ordinary reflected candidates.
pub async fn apply_promotion_verdict(
    staging: &ReflectionStagingStore,
    agent_id: &str,
    thread_id: &str,
    verdict: PromotionVerdict,
    existing_durable_entries: &[MemoryEntry],
) -> Result<Option<ReflectionCandidate>, AoError> {
    let (content, judge_rationale) = match verdict {
        PromotionVerdict::Reject { .. } => return Ok(None),
        PromotionVerdict::Promote {
            generalized_content,
            rationale,
        } => (generalized_content, rationale),
    };

    if content.trim().is_empty() {
        return Err(AoError::ValidationError(
            "promotion judge returned an empty generalized_content for a Promote verdict"
                .to_string(),
        ));
    }

    // Supersede-on-promote: only a non-Manual, non-pinned match is eligible
    // to be linked via `contradicts` — a Manual/pinned entry is filtered out
    // before scoring even runs, so it is never a candidate for supersession,
    // no matter how similar the generalized content looks.
    let eligible_for_contradiction: Vec<MemoryEntry> = existing_durable_entries
        .iter()
        .filter(|e| !matches!(e.source, Some(ao_protocol::memory::MemorySource::Manual)))
        .filter(|e| !e.pinned)
        .cloned()
        .collect();
    let contradicts = find_contradiction(&eligible_for_contradiction, &content, default_scorer().as_ref())
        .map(|m| m.entry.id.clone());

    let decision = stage_candidate(StagingRequest {
        artifact_type: ArtifactType::Memory,
        origin: CandidateOrigin::Reflected,
        scope: CandidateScope::Agent,
        contradicts_existing: contradicts.is_some(),
        overwrites_manual: false,
    });
    debug_assert!(
        !decision.auto_enable(),
        "a model-judged promotion must never auto-confirm"
    );

    let candidate = ReflectionCandidate {
        id: Uuid::new_v4().to_string(),
        kind: ArtifactKind::Memory,
        agent_id: agent_id.to_string(),
        source_thread_id: thread_id.to_string(),
        content,
        status: ReflectionCandidateStatus::Pending,
        // A promotion always widens Thread scope into durable Agent scope —
        // the one direction the pipeline defines (thread memory -> agent
        // memory review queue); nothing here ever targets Project/Global.
        target_scope: MemoryScope::Agent,
        target_scope_key: Some(agent_id.to_string()),
        contradicts,
        reason: format!("{}; promotion judge: {judge_rationale}", decision.reason),
        created_at: Utc::now(),
    };

    staging.stage(agent_id, &candidate).await?;
    Ok(Some(candidate))
}

/// [`apply_promotion_verdict`], additionally gated by an acceptance-rate
/// promotion budget.
///
/// A `Reject` verdict passes straight through — it was never going to stage
/// anything, so it never consumes a budget slot. A `Promote` verdict only
/// reaches `apply_promotion_verdict` (and therefore the staging queue) when
/// `gate` still has room in its current cycle; once the cycle's budget is
/// exhausted, a `Promote` verdict is discarded exactly like a `Reject` one
/// — `Ok(None)`, nothing staged, no matter how confident the judge was.
/// This is the hard-ceiling half of the hybrid enforcement rule; the
/// prompt-alignment half (surfacing the current rate/budget into the
/// judge's own prompt) is the caller's concern, one layer up, where the
/// judge is actually invoked.
pub async fn apply_promotion_verdict_with_budget(
    staging: &ReflectionStagingStore,
    agent_id: &str,
    thread_id: &str,
    verdict: PromotionVerdict,
    existing_durable_entries: &[MemoryEntry],
    gate: &mut PromotionBudgetGate,
) -> Result<Option<ReflectionCandidate>, AoError> {
    if matches!(verdict, PromotionVerdict::Promote { .. }) && !gate.try_reserve() {
        return Ok(None);
    }
    apply_promotion_verdict(staging, agent_id, thread_id, verdict, existing_durable_entries).await
}
