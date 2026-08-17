//! Consolidation sweep: detect near-duplicate
//! *distilled* skills and merge them down to the higher-usage survivor.
//!
//! Two skills only ever become consolidation candidates when both sides are
//! [`SkillSource::User`] (the only pool [`rewrite_user_skill`] can write
//! back to) **and** both are [`SkillProvenance::Distilled`] (the intended
//! scope: "near-duplicate distilled skills"). A skill without that marker —
//! including one a human typed directly, and one a model wrote mid-turn via
//! a live `SkillRegister` call the distillation pipeline never touched — is
//! never a candidate for automatic merging, full stop. This is the HARD
//! INVARIANT this module and [`super::retirement`] both enforce: nothing
//! short of an explicit `origin: distilled` marker is trustworthy grounds to
//! auto-act on a skill. A near-duplicate pair that includes a non-distilled
//! skill is simply never proposed for merge — there is no silent
//! staging-around-the-invariant path.
//!
//! Detection is two-stage, mirroring `memory::contradiction`'s FTS5 + scorer
//! pattern: the FTS5 index over skill name+description
//! (`ao_engine_tools_core::skill_registry::search_index`) widens candidate
//! recall cheaply, then [`crate::memory::contradiction::SimilarityScorer`]
//! confirms each FTS5-surfaced pair against the stricter token-overlap bar
//! ([`DUPLICATE_THRESHOLD`]) over name+description+body — the same algorithm
//! the distillation pipeline already uses to group repeated procedures
//! (`ao_engine::skill_distillation::SKILL_SIMILARITY_THRESHOLD`), reused
//! here rather than re-implemented.
//!
//! Winner selection reads the usage sidecar
//! (`ao_engine_tools_core::skill_registry::usage`): the skill with the
//! higher invocation count survives; the other is retired via the same
//! disable+tombstone mechanism [`super::retirement`] uses
//! (`set_disable_model_invocation` + `set_retired`), after an explicit
//! `stage_candidate` call confirms the gate agrees this can never
//! auto-enable — so a "merge" never silently deletes or rewrites the
//! loser's content, it only ever quarantines it, exactly like every other
//! path that reaches the trust gate. Reversible the same way retirement is:
//! see [`super::retirement::reactivate`].

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::Path;

use ao_engine_tools_core::skill_registry::dispatch::rewrite_user_skill;
use ao_engine_tools_core::skill_registry::{
    set_disable_model_invocation, set_retired, set_version, usage::UsageMap, SkillEntry,
    SkillProvenance, SkillRecord, SkillRegistry, SkillSource,
};
use ao_engine_tools_core::trust_gate::{
    stage_candidate, ArtifactType, CandidateOrigin, CandidateScope, StagingRequest,
};
use ao_search_index::{ArtifactKind, SearchFilter, SearchIndex};

use crate::memory::contradiction::default_scorer;

/// How many FTS5 candidates to consider per skill when widening recall.
/// Skill pools are small (tens, not thousands — the same assumption
/// `skill_distillation`'s grouping makes), so this only needs to exceed the
/// largest plausible duplicate cluster.
const FTS_CANDIDATE_LIMIT: usize = 10;

/// A pair scores at or above this bar (Jaccard token overlap over
/// `name description body`) to be treated as a near-duplicate worth
/// proposing for consolidation. Matches
/// `ao_engine::skill_distillation::SKILL_SIMILARITY_THRESHOLD` — both
/// measure "is this the same procedure" over similarly-shaped skill text,
/// so they are tuned identically on purpose.
pub const DUPLICATE_THRESHOLD: f32 = 0.5;

/// One near-duplicate pair the FTS5 index + scorer confirmed, before a
/// winner has been picked.
#[derive(Debug, Clone, PartialEq)]
pub struct DuplicatePair {
    pub a: String,
    pub b: String,
    pub similarity: f32,
}

/// A consolidation decision: `keep` survives, `supersede` is the name that
/// [`apply`] will retire if the hard invariant re-check and the trust gate
/// both agree.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationDecision {
    pub keep: String,
    pub supersede: String,
    pub similarity: f32,
}

/// Result of [`apply`]: which decisions were actually written, and which
/// were skipped along with why (the hard invariant firing, or a write
/// error).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConsolidationOutcome {
    pub applied: Vec<ConsolidationDecision>,
    pub skipped: Vec<(ConsolidationDecision, String)>,
}

/// Whether `record` is eligible for automatic consolidation at all — see
/// the module doc's HARD INVARIANT. Shared by [`find_near_duplicates`]
/// (which never even builds a pair around an ineligible skill) and [`apply`]
/// (which re-checks against the live registry as defense in depth).
fn is_eligible(record: &SkillRecord) -> bool {
    record.source == SkillSource::User && record.provenance == SkillProvenance::Distilled
}

/// Stage 1: widen recall via the FTS5 index, confirm via the token-overlap
/// scorer. Returns each qualifying pair once (canonicalized so `(a, b)` and
/// `(b, a)` never both appear).
pub async fn find_near_duplicates(registry: &SkillRegistry, index: &SearchIndex) -> Vec<DuplicatePair> {
    let scorer = default_scorer();
    let eligible: Vec<(&str, &SkillRecord)> = registry
        .all_visible()
        .filter_map(|(name, entry)| match entry {
            SkillEntry::Ok(record) if is_eligible(record) => Some((name, record)),
            _ => None,
        })
        .collect();

    let mut pairs = Vec::new();
    let mut seen_pairs: HashSet<(String, String)> = HashSet::new();

    for (name, record) in &eligible {
        let query_text = format!("{} {}", record.name, record.description);
        let filter = SearchFilter::new().with_artifact(ArtifactKind::Skill);
        let hits = match index.query(query_text, filter, FTS_CANDIDATE_LIMIT).await {
            Ok(hits) => hits,
            Err(e) => {
                tracing::warn!(
                    "skill consolidation: search index query failed for '{}': {}",
                    name,
                    e
                );
                Vec::new()
            }
        };

        for hit in hits {
            if hit.id == **name {
                continue;
            }
            let Some((other_name, other_record)) = eligible.iter().find(|(n, _)| *n == hit.id) else {
                continue; // FTS5 surfaced a non-eligible skill; never a consolidation candidate.
            };

            let pair_key = if *name <= *other_name {
                (name.to_string(), other_name.to_string())
            } else {
                (other_name.to_string(), name.to_string())
            };
            if !seen_pairs.insert(pair_key.clone()) {
                continue;
            }

            let text_a = format!("{} {} {}", record.name, record.description, record.body);
            let text_b =
                format!("{} {} {}", other_record.name, other_record.description, other_record.body);
            let similarity = scorer.score(&text_a, &text_b);
            if similarity >= DUPLICATE_THRESHOLD {
                pairs.push(DuplicatePair { a: pair_key.0, b: pair_key.1, similarity });
            }
        }
    }

    pairs
}

/// Stage 2: pick a winner per pair from the usage sidecar — higher
/// invocation count survives; ties break toward the lexicographically
/// smaller name for determinism (mirrors `report::rank`'s own tie-break).
pub fn plan_consolidation(pairs: &[DuplicatePair], usage: &UsageMap) -> Vec<ConsolidationDecision> {
    pairs
        .iter()
        .map(|pair| {
            let count_a = usage.get(&pair.a).map(|e| e.count).unwrap_or(0);
            let count_b = usage.get(&pair.b).map(|e| e.count).unwrap_or(0);
            let (keep, supersede) = match count_a.cmp(&count_b) {
                std::cmp::Ordering::Greater => (pair.a.clone(), pair.b.clone()),
                std::cmp::Ordering::Less => (pair.b.clone(), pair.a.clone()),
                std::cmp::Ordering::Equal if pair.a <= pair.b => (pair.a.clone(), pair.b.clone()),
                std::cmp::Ordering::Equal => (pair.b.clone(), pair.a.clone()),
            };
            ConsolidationDecision { keep, supersede, similarity: pair.similarity }
        })
        .collect()
}

/// Stage 3: apply each decision by retiring `supersede` — routed through the
/// trust gate exactly like every other lifecycle mutation
/// (`stage_candidate` with [`CandidateOrigin::Reflected`], since a
/// background sweep is definitionally out-of-band). Re-checks the hard
/// invariant against the live registry before writing anything (defense in
/// depth against a caller passing decisions built from a stale registry
/// snapshot) — a decision whose `supersede` name is no longer eligible in
/// `registry` is skipped, never applied.
///
/// On a successful retirement, also bumps `keep`'s version by 1: the
/// winner absorbed a duplicate's procedure into its own track
/// record, even though its body is untouched. Best-effort — a failure to
/// stamp the winner's version is logged but does not undo the retirement
/// already written to the loser.
pub async fn apply(
    data_dir: &Path,
    registry: &SkillRegistry,
    decisions: &[ConsolidationDecision],
) -> ConsolidationOutcome {
    let mut outcome = ConsolidationOutcome::default();

    for decision in decisions {
        let Some(SkillEntry::Ok(loser)) = registry.get(&decision.supersede) else {
            outcome
                .skipped
                .push((decision.clone(), "supersede target not found in the live registry".to_string()));
            continue;
        };
        if !is_eligible(loser) {
            outcome.skipped.push((
                decision.clone(),
                "not eligible for auto-consolidation (not a distilled user-pool skill); the hard \
                 invariant requires staging this for manual review instead"
                    .to_string(),
            ));
            continue;
        }

        let gate_decision = stage_candidate(StagingRequest {
            artifact_type: ArtifactType::Skill,
            origin: CandidateOrigin::Reflected,
            scope: CandidateScope::Agent,
            contradicts_existing: true,
            overwrites_manual: false,
        });
        debug_assert!(
            !gate_decision.auto_enable(),
            "a consolidation merge must never auto-enable"
        );

        let keep = decision.keep.clone();
        let result = rewrite_user_skill(data_dir, &decision.supersede, move |content| {
            let staged = set_disable_model_invocation(content, true)?;
            set_retired(&staged, "consolidated", Some(&keep))
        })
        .await;

        match result {
            Ok(()) => {
                let next_version = match registry.get(&decision.keep) {
                    Some(SkillEntry::Ok(winner)) => winner.version.saturating_add(1),
                    _ => 1,
                };
                if let Err(e) = rewrite_user_skill(data_dir, &decision.keep, move |content| {
                    set_version(content, next_version)
                })
                .await
                {
                    tracing::warn!(
                        "skill consolidation: retired '{}' into '{}' but failed to bump the \
                         winner's version: {}",
                        decision.supersede,
                        decision.keep,
                        e
                    );
                }
                outcome.applied.push(decision.clone());
            }
            Err(e) => outcome.skipped.push((decision.clone(), format!("write failed: {e}"))),
        }
    }

    outcome
}
