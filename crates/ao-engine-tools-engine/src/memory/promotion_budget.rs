//! Acceptance-rate promotion budget controller.
//!
//! [`promotion::apply_promotion_verdict`] answers "did the judge think this
//! generalizes?" This module answers a different question: "how much of
//! what the judge approves should actually be allowed to reach staging right
//! now?" — tuned by whether a human actually keeps what already reached the
//! review queue, never by the judge's own confidence in itself.
//!
//! Ground truth, and nothing else: [`ReviewDecision::Accepted`] is emitted
//! only for a candidate a human resolved via `keep`/`edit`/`pin`
//! (`memory::review`, `ReflectionCandidateStatus::Confirmed`);
//! [`ReviewDecision::Rejected`] only for `forget`
//! (`ReflectionCandidateStatus::Rejected`, same module). Neither variant, nor
//! any function in this module, ever takes a confidence score, a rationale,
//! or any other judge-authored input as a parameter — there is no code path
//! here for the judge's own opinion of itself to leak into the rate it is
//! measured against. That is what makes this a control loop rather than an
//! echo chamber.
//!
//! Instrumentation reuses the existing per-turn [`ao_protocol::outcome::OutcomeRecord`]
//! shape as a second producer rather than inventing a new
//! telemetry store: [`record_review_decision`] appends one, tagged so
//! [`decisions_from_outcome_history`] can pick it back out of a store that
//! may also carry ordinary per-turn records.
//!
//! v2 (deliberately not built here — a deferred list):
//! a candidate-frequency/opportunity input distinct from acceptance rate;
//! bandit-style budget tuning in place of the linear rule below; per-agent
//! and per-category acceptance rates in place of the one rate this module
//! computes over whatever history it is handed.

#[cfg(test)]
mod tests;

use std::collections::VecDeque;

use ao_persistence::outcome::OutcomeStore;
use ao_protocol::error::AoError;
use ao_protocol::outcome::{ArtifactRef, OutcomeRecord, OutcomeSignal};
use chrono::Utc;

/// One resolved human staging-gate decision — the only signal
/// [`PromotionBudgetController`] ever consumes. See the module doc for why
/// this type deliberately carries nothing else (no confidence, no
/// rationale): a narrower type is a stronger guarantee than a wider one a
/// caller has to remember not to read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    /// `keep` / `edit` / `pin` — the human approved what was staged.
    Accepted,
    /// `forget` — the human dismissed what was staged.
    Rejected,
}

/// Marker prefix written into an [`OutcomeRecord`]'s `detail` so
/// [`decisions_from_outcome_history`] can pick this producer's records back
/// out of a store that may also carry ordinary per-turn outcome records.
/// Not a new store or a new field — just a tag on the existing shape.
const REVIEW_DECISION_DETAIL_PREFIX: &str = "g6_promotion_review:";

/// Append one human staging-gate resolution as an [`OutcomeRecord`] —
/// reusing the existing per-turn outcome-record shape as a second producer
/// rather than standing up a new instrumentation store. Call this once,
/// right after `memory::review::keep`/`edit`/`pin`/`forget` resolves a
/// candidate, passing [`ReviewDecision::Accepted`] for the first three and
/// [`ReviewDecision::Rejected`] for `forget`.
pub async fn record_review_decision(
    outcome_store: &OutcomeStore,
    agent_id: &str,
    candidate_id: &str,
    decision: ReviewDecision,
) -> Result<(), AoError> {
    let positive = matches!(decision, ReviewDecision::Accepted);
    let record = OutcomeRecord {
        turn_id: candidate_id.to_string(),
        session_id: agent_id.to_string(),
        artifacts_used: vec![ArtifactRef::memory(candidate_id)],
        signal: OutcomeSignal::Explicit {
            positive,
            detail: Some(format!(
                "{REVIEW_DECISION_DETAIL_PREFIX}{}",
                if positive { "accepted" } else { "rejected" }
            )),
        },
        timestamp: Utc::now(),
    };
    outcome_store.append(agent_id, &record).await
}

/// Pick this module's own tagged records back out of a full outcome
/// history, oldest first, mapped to the [`ReviewDecision`] they represent.
/// Any record not carrying [`REVIEW_DECISION_DETAIL_PREFIX`] (an ordinary
/// per-turn record from elsewhere) is ignored — never mistaken for a human
/// staging-gate decision.
pub fn decisions_from_outcome_history(history: &[OutcomeRecord]) -> Vec<ReviewDecision> {
    history
        .iter()
        .filter_map(|record| match &record.signal {
            OutcomeSignal::Explicit {
                positive,
                detail: Some(detail),
            } if detail.starts_with(REVIEW_DECISION_DETAIL_PREFIX) => Some(if *positive {
                ReviewDecision::Accepted
            } else {
                ReviewDecision::Rejected
            }),
            _ => None,
        })
        .collect()
}

/// How many recent decisions the sliding window remembers. A count window,
/// not a calendar one — a quiet stretch with few promotion attempts
/// shouldn't stale the acceptance-rate signal just because little time has
/// passed.
pub const WINDOW_SIZE: usize = 20;

/// The tight end of the budget range. Deliberately doubles as both of the
/// module's two mandatory safety properties at once:
/// - **Conservative cold start** — no history yet resolves to this value
///   (see [`PromotionBudgetController::budget`]), never to [`MAX_BUDGET`].
/// - **Exploration floor** — the linear rule's output is clamped so it can
///   never drop below this value either, even after a run of all-`Rejected`.
///
/// One constant serves both roles on purpose: cold start cannot reasonably
/// be more conservative than "the tightest the budget is ever allowed to
/// go," so a second, lower knob would just be an unreachable dead value.
pub const MIN_BUDGET: u32 = 1;

/// The loose end of the budget range: the most promotions the linear rule
/// ever allows per window, reached only at a 100% acceptance rate.
pub const MAX_BUDGET: u32 = 10;

/// Sliding-window acceptance-rate tracker and linear promotion-budget rule
/// Pure and synchronous by construction — every input is a
/// plain [`ReviewDecision`], so there is no way for a provider call, a
/// judge's rationale, or anything else to reach this type except through
/// [`Self::record`] / [`Self::from_history`].
#[derive(Debug, Clone, Default)]
pub struct PromotionBudgetController {
    window: VecDeque<ReviewDecision>,
}

impl PromotionBudgetController {
    /// A controller with no history yet — [`Self::budget`] resolves to
    /// [`MIN_BUDGET`] until [`Self::record`] is called.
    pub fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(WINDOW_SIZE),
        }
    }

    /// Rebuild a controller's window from an ordered history, oldest first
    /// — typically [`decisions_from_outcome_history`]'s output. Only the
    /// most recent [`WINDOW_SIZE`] entries end up in the window; older ones
    /// are exactly what a *count* window (rather than a calendar one) is
    /// meant to age out.
    pub fn from_history(decisions: impl IntoIterator<Item = ReviewDecision>) -> Self {
        let mut controller = Self::new();
        for decision in decisions {
            controller.record(decision);
        }
        controller
    }

    /// Record one human staging-gate resolution, evicting the oldest entry
    /// once the window is at [`WINDOW_SIZE`] capacity.
    pub fn record(&mut self, decision: ReviewDecision) {
        if self.window.len() == WINDOW_SIZE {
            self.window.pop_front();
        }
        self.window.push_back(decision);
    }

    /// How many decisions are currently in the window.
    pub fn len(&self) -> usize {
        self.window.len()
    }

    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }

    /// Acceptance rate over the current window, in `[0.0, 1.0]`. `None`
    /// when the window is empty — cold start has no rate to compute at all,
    /// which is a different thing from a rate of zero (an all-`Rejected`
    /// window still produces `Some(0.0)`, not `None`).
    pub fn acceptance_rate(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let accepted = self
            .window
            .iter()
            .filter(|d| **d == ReviewDecision::Accepted)
            .count();
        Some(accepted as f64 / self.window.len() as f64)
    }

    /// The deterministic promotion budget for the next window: the hard
    /// ceiling this crate's staging path enforces regardless of what the
    /// judge argues. This is the hard-enforcement half of the hybrid
    /// enforcement rule; the prompt-alignment half lives one layer up,
    /// where the judge is actually invoked.
    ///
    /// Linear rule: `MIN_BUDGET + rate * (MAX_BUDGET - MIN_BUDGET)`, rounded
    /// to the nearest whole promotion. The result is clamped to
    /// `[MIN_BUDGET, MAX_BUDGET]`; the clamp is redundant given `rate` is
    /// already restricted to `[0.0, 1.0]`, but it keeps both safety
    /// properties explicit in the code that enforces them rather than only
    /// implied by the input range.
    ///
    /// Cold start (`acceptance_rate` is `None`) and the exploration floor (a
    /// run of all-`Rejected`, rate `0.0`) both resolve to [`MIN_BUDGET`] —
    /// see that constant's doc for why one value serves both properties.
    pub fn budget(&self) -> u32 {
        let rate = match self.acceptance_rate() {
            None => return MIN_BUDGET,
            Some(r) => r,
        };
        let span = (MAX_BUDGET - MIN_BUDGET) as f64;
        let raw = MIN_BUDGET as f64 + rate * span;
        (raw.round() as u32).clamp(MIN_BUDGET, MAX_BUDGET)
    }

    /// A short, human-readable line summarizing the current rate/budget,
    /// meant to be surfaced into the promotion judge's own prompt: the
    /// current acceptance rate and budget are injected into the judge's
    /// prompt so its reasoning stays aligned with the enforced constraint.
    /// This is alignment only — the judge reading this line has no way to
    /// raise its own budget; [`PromotionBudgetGate::try_reserve`] enforces
    /// the same number in code regardless of what the judge argues.
    pub fn prompt_context(&self) -> String {
        match self.acceptance_rate() {
            None => format!(
                "Promotion history: none yet. Current promotion budget: {MIN_BUDGET} per window \
                 (conservative cold start — earn a higher budget by getting kept, not rejected)."
            ),
            Some(rate) => {
                let accepted = self
                    .window
                    .iter()
                    .filter(|d| **d == ReviewDecision::Accepted)
                    .count();
                format!(
                    "Promotion history: {accepted}/{} of the last human-reviewed promotions were \
                     kept ({:.0}% acceptance). Current promotion budget: {} per window.",
                    self.window.len(),
                    rate * 100.0,
                    self.budget()
                )
            }
        }
    }
}

/// Wraps a [`PromotionBudgetController`] with the hard-enforcement half of
/// the hybrid enforcement rule: a real, in-code cap on how many promotions
/// may reach staging within the current window of attempts, independent of
/// how confident the judge's verdict was.
///
/// The cap resets once every [`WINDOW_SIZE`] promotion *attempts* (not
/// decisions) — the natural cadence for "cap on promotions-per-window" when
/// the rate itself is refreshed from the latest human decisions on the same
/// cadence. Attempts and grants are tracked separately: `attempts_in_cycle`
/// counts every call so a cycle always rolls over after `WINDOW_SIZE` calls
/// even once the budget is tight enough that most of them are denied;
/// `granted_in_cycle` counts only the ones actually reserved, which is what
/// the budget check itself compares against.
#[derive(Debug, Clone, Default)]
pub struct PromotionBudgetGate {
    controller: PromotionBudgetController,
    attempts_in_cycle: u32,
    granted_in_cycle: u32,
}

impl PromotionBudgetGate {
    pub fn new(controller: PromotionBudgetController) -> Self {
        Self {
            controller,
            attempts_in_cycle: 0,
            granted_in_cycle: 0,
        }
    }

    /// The current cycle's cap.
    pub fn budget(&self) -> u32 {
        self.controller.budget()
    }

    pub fn controller(&self) -> &PromotionBudgetController {
        &self.controller
    }

    /// Replace the inner controller (e.g. after re-reading the latest human
    /// decisions from disk) without disturbing the in-flight attempt count
    /// — only [`Self::try_reserve`] advances or rolls over the cycle.
    pub fn set_controller(&mut self, controller: PromotionBudgetController) {
        self.controller = controller;
    }

    /// Attempt to reserve one promotion slot against the current cycle's
    /// budget. Returns `false` once the cycle is exhausted — the caller
    /// must treat this exactly like a `Reject` verdict: nothing reaches
    /// staging, no matter how confident the judge was.
    ///
    /// A full cycle ([`WINDOW_SIZE`] attempts) always rolls over into a
    /// fresh one, re-evaluated against whatever the controller's budget is
    /// at that moment — this is what lets the budget loosen or tighten
    /// between cycles as more human decisions arrive.
    pub fn try_reserve(&mut self) -> bool {
        if self.attempts_in_cycle >= WINDOW_SIZE as u32 {
            self.attempts_in_cycle = 0;
            self.granted_in_cycle = 0;
        }
        self.attempts_in_cycle += 1;

        if self.granted_in_cycle >= self.budget() {
            return false;
        }
        self.granted_in_cycle += 1;
        true
    }
}
