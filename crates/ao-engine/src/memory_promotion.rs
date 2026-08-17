//! Promotion-judge orchestration: resolves
//! which `AgentProfile` decides a thread-scope memory entry's fate, drives
//! it through the app's existing provider seam, and turns a `Promote`
//! verdict into a candidate in the existing `ReflectionStagingStore`.
//!
//! Mirrors `crate::reflection_subscriber::ReflectionSubscriber` and
//! `crate::skill_distillation::SkillDistiller`'s shape exactly — the same
//! two collaborators (`PersistenceLayer`, `ProviderResolver`), the same
//! profile-resolution fallback (the optional `reflection_agent_id`
//! preference, else the entry's own agent) — because this is the SAME
//! execution-engine seam, not a second one. The model call itself
//! (`ao_engine_tools_runner::promotion_judge::ProviderPromotionJudge`) and
//! the staging half (`ao_engine_tools_engine::memory::promotion::
//! apply_promotion_verdict`) already exist in their respective crates; this
//! module is the glue only `ao-engine` can provide (it is the one crate
//! that depends on both `ao-engine-tools-runner`, for the `ProviderClient`
//! seam, and `ao-persistence`, for `ReflectionStagingStore` and
//! `AgentProfile` resolution).
//!
//! Wired into two triggers, both owned by
//! `crate::reflection_subscriber::ReflectionSubscriber`: an unconditional
//! sweep on `ReflectionTriggerReason::Archived` (a thread's learnings are
//! final once it closes), and a debounced periodic sweep on every other
//! reflection trigger (`AnchorRotated`/`IdleTimeout`) — the path that lets a
//! `Default` thread's notes reach durable memory at all, since a `Default`
//! thread can never archive. See
//! [`ReflectionSubscriber::run_periodic_promotion_sweep`](crate::reflection_subscriber::ReflectionSubscriber::run_periodic_promotion_sweep)
//! for the debounce (`PROMOTION_SWEEP_INTERVAL`) and
//! [`MIN_PROMOTION_SURVIVAL`]/[`is_promotion_eligible`] below for the
//! minimum in-thread-survival behavioral pre-filter — deciding *when*
//! an entry becomes judge-eligible in the first place, applied only to the
//! periodic path (the archival sweep needs no survival window: once a
//! thread is archived its thread-scope notes can no longer be revised, so
//! there is nothing left for the window to protect against).
//!
//! The acceptance-rate promotion budget IS wired in here:
//! [`MemoryPromotionJudge::promote`] re-derives the current acceptance rate
//! from `persistence.outcome`'s persisted human keep/forget history on
//! every call (`ao_engine_tools_engine::memory::promotion_budget`), surfaces
//! it into the judge's own prompt for alignment, and gates the resulting
//! verdict through [`apply_promotion_verdict_with_budget`] before it can
//! ever reach staging — the hard-ceiling half of the enforcement rule. Only
//! the in-cycle attempt counter lives purely in memory (resets on process
//! restart); the rate itself is always read fresh from disk, so it never
//! drifts from what `memory::review`'s `keep`/`edit`/`forget`/`pin` actually
//! recorded.

#[cfg(test)]
mod tests;

use std::sync::Arc;

use tokio::sync::Mutex;

use ao_engine_tools_engine::memory::promotion::{apply_promotion_verdict_with_budget, PromotionVerdict};
use ao_engine_tools_engine::memory::promotion_budget::{
    decisions_from_outcome_history, PromotionBudgetController, PromotionBudgetGate,
};
use ao_engine_tools_runner::promotion_judge::{ProviderPromotionJudge, PromotionJudgeEngine};
use ao_persistence::PersistenceLayer;
use ao_protocol::memory::{MemoryEntry, MemoryStatus};
use ao_protocol::reflection_candidate::ReflectionCandidate;
use chrono::{DateTime, Duration, Utc};

use crate::reflection_subscriber::ProviderResolver;

/// Minimum wall-clock duration a thread-scope memory entry must sit
/// uncontradicted since its creation or last edit before the periodic
/// promotion sweep will hand it to the judge at all. The in-thread-survival
/// pre-filter this module's doc used to defer to "a later pass" — this is
/// that pass.
///
/// Keeps a still-being-revised note out of the judge's hands until it has
/// had a chance to settle: a later thread-scope write that corrects or
/// contradicts an existing note goes through `MemoryStore::edit_thread`,
/// which bumps `MemoryEntry::updated_at` to the edit time — resetting this
/// window — rather than marking the note superseded (thread scope has no
/// supersede/contradiction tracking of its own; see
/// `ao_persistence::memory::MemoryStore`'s "Thread scope" section).
///
/// Not applied to the archival-triggered sweep: once a thread is archived
/// its thread-scope notes can no longer be revised, so there is no
/// still-settling state left for this window to protect against.
pub const MIN_PROMOTION_SURVIVAL: Duration = Duration::minutes(10);

/// Whether `entry` has survived long enough, uncontradicted, to be worth a
/// judge call — see [`MIN_PROMOTION_SURVIVAL`]. `now` is a parameter rather
/// than read internally so this stays a pure, deterministic predicate a
/// caller can unit test without a clock.
pub fn is_promotion_eligible(entry: &MemoryEntry, now: DateTime<Utc>) -> bool {
    entry.status == MemoryStatus::Active && now - entry.updated_at >= MIN_PROMOTION_SURVIVAL
}

/// Outcome of one [`MemoryPromotionJudge::promote`] call.
///
/// Not `PartialEq` — it wraps [`ReflectionCandidate`], which does not
/// implement it either (see that type's own doc for why).
#[derive(Debug, Clone)]
pub enum PromotionOutcome {
    /// The judge found `entry` generalizable; `0` is the resulting staged
    /// candidate (already durably persisted in `ReflectionStagingStore`).
    Promoted(ReflectionCandidate),
    /// The judge found `entry` thread-specific (or could not confidently
    /// tell); nothing was staged.
    Rejected { rationale: String },
}

/// Orchestrates the promotion judge for one agent's thread-scope memory
/// entries. Holds the exact same two collaborators
/// `reflection_subscriber::ReflectionSubscriber` and
/// `skill_distillation::SkillDistiller` do — `PersistenceLayer` and a
/// `ProviderResolver` — so production wiring constructs all three from the
/// same `build_reflection_provider` function (see that function's doc for
/// the shared rationale).
#[derive(Clone)]
pub struct MemoryPromotionJudge {
    persistence: Arc<PersistenceLayer>,
    resolve_provider: ProviderResolver,
    /// The promotion budget's enforcement half. Only the in-cycle attempt
    /// count lives here across calls — the acceptance-rate window itself is
    /// re-read from `persistence.outcome` at the top of every [`Self::promote`]
    /// call, so it can never go stale relative to what a human actually
    /// decided in `memory::review`.
    budget_gate: Arc<Mutex<PromotionBudgetGate>>,
}

impl MemoryPromotionJudge {
    pub fn new(persistence: Arc<PersistenceLayer>, resolve_provider: ProviderResolver) -> Self {
        Self {
            persistence,
            resolve_provider,
            budget_gate: Arc::new(Mutex::new(PromotionBudgetGate::new(PromotionBudgetController::new()))),
        }
    }

    /// Judge one thread-scope entry and, on a `Promote` verdict, stage the
    /// judge's generalized rewrite into `ReflectionStagingStore`.
    ///
    /// Resolves the SAME way the reflection pass and distillation
    /// do — the optional `reflection_agent_id` preference, falling back to
    /// `agent_id` (the entry's own agent) — and drives the model only
    /// through whatever `resolve_provider` (production:
    /// `crate::build_reflection_provider`) hands back. Never constructs a
    /// provider client itself.
    pub async fn promote(
        &self,
        agent_id: &str,
        thread_id: &str,
        entry: &MemoryEntry,
    ) -> Result<PromotionOutcome, String> {
        let prefs = self
            .persistence
            .preferences
            .get()
            .await
            .map_err(|e| format!("failed to load preferences for promotion judge: {e}"))?
            .unwrap_or_default();
        let profile_id = prefs
            .reflection_agent_id
            .unwrap_or_else(|| agent_id.to_string());
        let profile = self
            .persistence
            .agents
            .get(&profile_id)
            .await
            .map_err(|e| format!("failed to load agent profile '{profile_id}': {e}"))?
            .ok_or_else(|| format!("promotion judge agent profile '{profile_id}' not found"))?;

        // Hard rule: drive the model through the app's existing
        // provider/runner seam — never a bespoke client. This orchestrator
        // never constructs a `ProviderClient` itself; it only ever consumes
        // whatever `resolve_provider` hands back.
        let provider = (self.resolve_provider)(&profile).ok_or_else(|| {
            format!("no provider configured for promotion judge agent profile '{profile_id}'")
        })?;

        // Re-derive the current acceptance rate from the SAME persisted
        // human keep/forget history `memory::review` writes to — never from
        // the judge's own confidence — and surface it into the judge's
        // prompt for alignment (the prompt-alignment half of the promotion
        // budget's hybrid enforcement rule; the hard ceiling is enforced
        // below regardless of what the judge does with this context).
        let history = self
            .persistence
            .outcome
            .read_all(agent_id)
            .await
            .map_err(|e| format!("failed to load promotion review history: {e}"))?;
        let controller = PromotionBudgetController::from_history(decisions_from_outcome_history(&history));
        let budget_context = controller.prompt_context();

        let judge = ProviderPromotionJudge::new(provider);
        let judge_input = format!("{budget_context}\n\n{}", entry.content);
        let verdict = judge.judge(&judge_input).await?;
        let rejected_rationale = match &verdict {
            PromotionVerdict::Reject { rationale } => Some(rationale.clone()),
            PromotionVerdict::Promote { .. } => None,
        };

        // Supersede-on-promote: the
        // destination scope is always `Agent` (see `apply_promotion_verdict`'s
        // doc), so its current live entries are what a `Promote` verdict is
        // checked against for a duplicate/contradiction to supersede instead
        // of appending alongside.
        let destination_entries = self
            .persistence
            .memory
            .list(agent_id)
            .await
            .map_err(|e| format!("failed to load destination scope memory for promotion judge: {e}"))?;

        let staged = {
            let mut gate = self.budget_gate.lock().await;
            gate.set_controller(controller);
            apply_promotion_verdict_with_budget(
                &self.persistence.reflection_staging,
                agent_id,
                thread_id,
                verdict,
                &destination_entries,
                &mut gate,
            )
            .await
            .map_err(|e| format!("failed to stage promoted memory candidate: {e}"))?
        };

        Ok(match staged {
            Some(candidate) => PromotionOutcome::Promoted(candidate),
            None => PromotionOutcome::Rejected {
                rationale: rejected_rationale.unwrap_or_default(),
            },
        })
    }
}
