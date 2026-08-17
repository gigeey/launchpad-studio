//! Retirement sweep: usage-based skill retirement.
//!
//! Reads the same [`SkillUsageReport::dead`](ao_engine_tools_core::skill_registry::SkillUsageReport::dead)
//! list the usage report already computes
//! (`ao_engine_tools_core::skill_registry::rank`) and retires each dead
//! skill that clears the same HARD INVARIANT [`super::consolidation`]
//! enforces — [`SkillSource::User`] and [`SkillProvenance::Distilled`]
//! only. A dead skill that fails that check (user-authored, or plugin/MCP-
//! sourced) is never touched; it is only *reported* as a candidate a human
//! should review, via [`RetirementOutcome::staged_for_review`] — "stage it
//! instead," per the plan's own wording, means never mutating it
//! automatically, not silently dropping it from view.
//!
//! Retirement uses the identical disable+tombstone mechanism
//! [`super::consolidation::apply`] uses (`set_disable_model_invocation` +
//! `set_retired`), routed through the same trust gate call, so
//! the two sweeps share one notion of "retired" that a human
//! reviewing the skill pool only has to learn once. [`reactivate`] is the
//! reversal — the skill-domain equivalent of the memory undo surface
//! (`ao_engine_tools_engine::memory::review::undo`); memory's review module
//! explicitly declines to handle `Skill` candidates because a staged
//! skill's "not live yet" state already lives entirely in its own
//! frontmatter (see that module's doc), so undoing a skill retirement is
//! symmetric with applying it: clear the same two frontmatter markers.

#[cfg(test)]
mod tests;

use std::path::Path;

use chrono::{DateTime, Duration, Utc};

use ao_engine_tools_core::skill_registry::dispatch::{rewrite_user_skill, SkillRewriteError};
use ao_engine_tools_core::skill_registry::{
    clear_retired, rank, set_disable_model_invocation, set_retired, usage::UsageMap, SkillEntry,
    SkillProvenance, SkillRecord, SkillRegistry, SkillSource,
};
use ao_engine_tools_core::trust_gate::{
    stage_candidate, ArtifactType, CandidateOrigin, CandidateScope, StagingRequest,
};

/// A skill counts as unused past this many days with no invocations (or
/// none ever) when no caller-supplied threshold is available — the same
/// default the eventual scheduled sweep would pass to `rank`'s `dead_after`.
/// Not tuned against production data; a starting point matching this
/// codebase's other not-yet-scheduled sweeps (`memory::decay::decay_sweep`,
/// `memory::eviction`).
pub const DEFAULT_DEAD_AFTER_DAYS: i64 = 30;

/// Same eligibility rule [`super::consolidation`]'s private `is_eligible`
/// enforces — duplicated across the two modules on purpose: each sweep
/// re-affirms the hard invariant against its own inputs rather than
/// trusting the other sweep already checked (defense in depth for the one
/// rule that must never regress).
fn is_eligible(record: &SkillRecord) -> bool {
    record.source == SkillSource::User && record.provenance == SkillProvenance::Distilled
}

/// Result of one [`sweep`] call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RetirementOutcome {
    /// Skills actually disabled + tombstoned this sweep.
    pub retired: Vec<String>,
    /// Dead-by-usage skills the hard invariant excluded from automatic
    /// action — untouched on disk, surfaced here for a human to review and
    /// retire manually if they choose.
    pub staged_for_review: Vec<String>,
    /// Eligible, dead skills whose write itself failed, paired with the
    /// error — distinct from `staged_for_review` (which never attempted a
    /// write at all).
    pub failed: Vec<(String, String)>,
}

/// Run one retirement sweep: everything [`rank`] calls dead, that isn't
/// already retired, gets partitioned into `retired` / `staged_for_review` /
/// `failed`.
pub async fn sweep(
    data_dir: &Path,
    registry: &SkillRegistry,
    usage: &UsageMap,
    now: DateTime<Utc>,
    dead_after: Duration,
) -> RetirementOutcome {
    let report = rank(registry, usage, now, 0, dead_after);
    let mut outcome = RetirementOutcome::default();

    for stats in &report.dead {
        let Some(SkillEntry::Ok(record)) = registry.get(&stats.skill_id) else {
            continue;
        };
        if record.retired {
            continue; // already retired by a prior sweep — nothing to do.
        }
        if !is_eligible(record) {
            outcome.staged_for_review.push(stats.skill_id.clone());
            continue;
        }

        let gate_decision = stage_candidate(StagingRequest {
            artifact_type: ArtifactType::Skill,
            origin: CandidateOrigin::Reflected,
            scope: CandidateScope::Agent,
            contradicts_existing: false,
            overwrites_manual: false,
        });
        debug_assert!(
            !gate_decision.auto_enable(),
            "a usage-based retirement must never auto-enable"
        );

        let result = rewrite_user_skill(data_dir, &stats.skill_id, |content| {
            let staged = set_disable_model_invocation(content, true)?;
            set_retired(&staged, "unused", None)
        })
        .await;

        match result {
            Ok(()) => outcome.retired.push(stats.skill_id.clone()),
            Err(e) => outcome.failed.push((stats.skill_id.clone(), e.to_string())),
        }
    }

    outcome
}

/// Errors [`reactivate`] can return.
#[derive(Debug)]
pub enum ReactivateError {
    /// `name` does not resolve to a live [`SkillSource::User`] entry in the
    /// registry snapshot passed in — plugin/MCP skills have no write path
    /// here, matching every other mutation this module performs.
    NotUserSkill,
    /// `name` resolves to a live user-pool skill, but it isn't currently
    /// retired — nothing to reverse.
    NotRetired,
    Rewrite(SkillRewriteError),
}

impl std::fmt::Display for ReactivateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReactivateError::NotUserSkill => write!(f, "not a live user-pool skill"),
            ReactivateError::NotRetired => write!(f, "skill is not currently retired"),
            ReactivateError::Rewrite(e) => write!(f, "rewrite failed: {e}"),
        }
    }
}

/// Reverse a retirement: clear the tombstone and re-enable the skill
/// for model invocation. See the module doc for why this is the skill
/// domain's undo surface.
pub async fn reactivate(
    data_dir: &Path,
    registry: &SkillRegistry,
    name: &str,
) -> Result<(), ReactivateError> {
    let Some(SkillEntry::Ok(record)) = registry.get(name) else {
        return Err(ReactivateError::NotUserSkill);
    };
    if record.source != SkillSource::User {
        return Err(ReactivateError::NotUserSkill);
    }
    if !record.retired {
        return Err(ReactivateError::NotRetired);
    }

    rewrite_user_skill(data_dir, name, |content| {
        let cleared = clear_retired(content)?;
        set_disable_model_invocation(&cleared, false)
    })
    .await
    .map_err(ReactivateError::Rewrite)
}
