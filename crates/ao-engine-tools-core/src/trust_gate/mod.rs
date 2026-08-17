//! Shared trust boundary for artifacts the system proposes to itself.
//!
//! This module exists because of a specific regression. `SkillRegister`
//! used to write a skill into a `.pending/` staging area and required a separate
//! confirmation step before it became invocable; that staging gate was
//! dropped, so self-authored skills went live the instant they were
//! written. This module re-establishes the review boundary — generically,
//! so the memory and (eventually) workflow crystallization tiers can share
//! it instead of every artifact type growing its own bespoke gate.
//!
//! The gate is deliberately small and pure: [`stage_candidate`] takes a
//! description of a candidate artifact and returns a verdict. It does not
//! touch a filesystem, a registry, or a specific artifact's storage format —
//! each artifact type (skills, memory today; workflows later) owns
//! translating the verdict into its own "not live yet" representation (for
//! skills: forcing `disable_model_invocation` — see
//! `skill_registry::set_disable_model_invocation`; for memory: declining to
//! write and returning a "staged"/"blocked" tool result — see
//! `ao_engine_tools_engine::memory::write`).
//!
//! ## The three tiers
//!
//! - [`StagingTier::AutoConfirm`] — apply live, no review. Reserved for a
//!   NEW *agent-scope* memory that contradicts nothing: lowest blast
//!   radius, reversible via the eventual undo surface.
//! - [`StagingTier::StageForReview`] — recorded as a pending candidate; a
//!   human must confirm before it goes live/model-invocable. Covers: any
//!   model-invocable skill (skills are never low-stakes enough to skip
//!   review — see the doc on [`ArtifactType::Skill`]'s arm in
//!   [`stage_candidate`]), any supersede/contradiction this call site could
//!   not independently verify as safe, any cross-scope promotion, and any
//!   write to *project* or *global* scope.
//! - [`StagingTier::NeverAuto`] — hard block, always requires an explicit
//!   human action. The one case: overwriting a `Manual`/user-authored (or
//!   unverifiable-provenance) entry. Matches the "never silently supersede
//!   a user memory" guard already on file — this tier is that
//!   guard's generic, artifact-agnostic name.
//!
//! Kept out of this module on purpose: any notion of *which* contradictions
//! a specific artifact type has already independently verified as safe to
//! silently resolve. Memory's write path (`ao_engine_tools_engine::memory::
//! write`) has its own narrower, already-shipped carve-out — an agent
//! correcting its own prior agent-authored note is treated as low enough
//! risk to resolve without reaching this gate at all —
//! and that stays a caller-side judgment call, not a rule this generic gate
//! bakes in. Baking artifact-specific classification into the generic gate
//! is exactly the "hardcode skill as the only registerable output" trap
//! this design warns against.

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

/// The class of artifact a staging candidate will become once confirmed.
///
/// Mirrors the crystallization ladder: increasing determinism as
/// confidence rises — memory (facts) → skill (model-driven procedure) →
/// workflow (code-driven procedure). `Workflow` is **reserved**: no producer
/// emits it yet (workflow distillation is deferred), but the gate already
/// accepts and dispatches on it, so adding a real workflow-distillation
/// producer later is a new call site, not a breaking enum change here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactType {
    Memory,
    Skill,
    /// Reserved for workflow distillation. Kept as
    /// a live enum variant today — at ~zero cost — specifically so this gate
    /// never has to widen from a `{ memory, skill }`-only shape later.
    Workflow,
}

/// Where a staging candidate came from — determines whether the gate even
/// applies, and (for out-of-band candidates) whether it may ever reach the
/// top [`StagingTier::AutoConfirm`] tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOrigin {
    /// Entered directly by the human user. The gate never quarantines these
    /// — user artifacts always outrank agent artifacts.
    Manual,
    /// Written by the model mid-turn, unprompted by any out-of-band pass —
    /// e.g. a direct `SkillRegister` or `MemoryWrite` tool call. This is the
    /// origin that regression concerned, and the only origin eligible for
    /// [`StagingTier::AutoConfirm`]: the user is watching this
    /// turn happen, so a bounded, reversible action can go live without a
    /// separate review step.
    SelfAuthored,
    /// Produced by the out-of-band reflection pass
    /// reading an untrimmed transcript delta, or by the skill distiller
    /// built on top of it. Always stages for review at minimum (plan
    /// the user was not watching this turn happen, so nothing
    /// proposed out-of-band skips review, no matter how low the blast
    /// radius looks.
    Reflected,
}

/// Which scope tier a staging candidate targets. Determines the
/// tier (c) cross-scope-promotion and tier (d) project/global-write rules.
///
/// Named/kept separate from `ao_protocol::memory::MemoryScope` on purpose:
/// this gate is artifact-agnostic (memory today, skills and
/// workflows later), and a future skill/workflow producer may want to
/// express the same blast-radius axis without this module depending on
/// memory's own scope type (which also carries the reserved `AgentProject`
/// cell, irrelevant to gating).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateScope {
    /// Visible only to the authoring agent. The one scope eligible for
    /// [`StagingTier::AutoConfirm`].
    Agent,
    /// Visible to every agent working in the same repository.
    Project,
    /// Visible to every agent across every repository.
    Global,
}

/// The gate's three possible verdicts for a [`StagingRequest`] (
/// accepted as-is 2026-07-09).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StagingTier {
    /// Apply live, no review.
    AutoConfirm,
    /// Recorded as a pending candidate; a human must confirm before it goes
    /// live / becomes model-invocable.
    StageForReview,
    /// Hard block: never applied automatically, no matter what. A human
    /// must take an explicit, separate action (e.g. edit the existing entry
    /// directly) — this never even reaches a one-click "confirm" queue.
    NeverAuto,
}

/// Everything the gate needs to decide a [`StagingRequest`]'s tier.
///
/// This is the shape the memory-contradiction, reflection-candidate, and
/// distilled-skill paths — along with the `MemoryWrite`/`SkillRegister`
/// wiring — construct and pass to [`stage_candidate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagingRequest {
    pub artifact_type: ArtifactType,
    pub origin: CandidateOrigin,
    /// Which scope this candidate targets. Irrelevant to the verdict for
    /// artifact types without a scope concept of their own — set
    /// [`CandidateScope::Agent`] in that case, since it is the permissive
    /// default and every non-memory rule fires on the artifact type or the
    /// other flags instead.
    pub scope: CandidateScope,
    /// True when this candidate supersedes/contradicts a live entry in a
    /// way the *caller* has not independently verified is safe to resolve
    /// on its own. Set `false` for a contradiction the
    /// caller has already verified safe by its own narrower rule (e.g.
    /// memory's agent-vs-agent supersede) — that stays a
    /// caller-side judgment call the generic gate never makes (see the
    /// module doc).
    pub contradicts_existing: bool,
    /// True when the entry this candidate would overwrite/supersede is
    /// itself `Manual`/user-authored, or its provenance can't be verified
    /// safe ("treat unknown provenance as cautiously as Manual").
    /// Always wins the [`StagingTier::NeverAuto`] hard block, regardless of
    /// every other field. Only meaningful when `contradicts_existing` is
    /// also `true`.
    pub overwrites_manual: bool,
}

/// The gate's verdict for a [`StagingRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagingDecision {
    pub artifact_type: ArtifactType,
    pub tier: StagingTier,
    /// Human-readable justification, suitable for a tool result message or
    /// a review-surface log line.
    pub reason: String,
}

impl StagingDecision {
    /// `true` only for [`StagingTier::AutoConfirm`] — the candidate may go
    /// live (model-invocable / active) immediately. `false` for either
    /// staged tier: the caller must record this candidate in whatever "not
    /// live yet" representation its artifact type uses and wait for a human
    /// to confirm it (or, for [`StagingTier::NeverAuto`], never surface a
    /// one-click confirm at all).
    pub fn auto_enable(&self) -> bool {
        matches!(self.tier, StagingTier::AutoConfirm)
    }

    /// `true` only for [`StagingTier::NeverAuto`] — the hard block. Distinct
    /// from "just" not auto-enabling: a [`StagingTier::StageForReview`]
    /// candidate is still meant to reach a review queue a human can
    /// one-click confirm, whereas [`StagingTier::NeverAuto`] never should.
    pub fn is_hard_blocked(&self) -> bool {
        matches!(self.tier, StagingTier::NeverAuto)
    }
}

/// Decide whether `request` may go live immediately, must be staged for
/// review, or is hard-blocked entirely. Pure and synchronous — no I/O, no registry lookups.
///
/// Rules, in precedence order:
/// 1. [`CandidateOrigin::Manual`] is always trusted; the gate does not apply.
/// 2. `overwrites_manual` is the [`StagingTier::NeverAuto`] hard block,
///    regardless of every other field.
/// 3. [`CandidateOrigin::Reflected`] (out-of-band) always stages at minimum
///    — never reaches [`StagingTier::AutoConfirm`]: nothing
///    proposed out-of-band skips review.
/// 4. `contradicts_existing` stages for review.
/// 5. Any scope other than [`CandidateScope::Agent`] stages for review.
/// 6. The one case left — [`CandidateOrigin::SelfAuthored`], agent scope, no
///    unverified contradiction — resolves per artifact type:
///    [`ArtifactType::Memory`] auto-confirms;
///    [`ArtifactType::Skill`] still stages (every model-invocable skill
///    always requires review — there is no low-stakes
///    carve-out); [`ArtifactType::Workflow`] stages (reserved, no auto-
///    confirm rule defined yet).
pub fn stage_candidate(request: StagingRequest) -> StagingDecision {
    let artifact_type = request.artifact_type;

    if request.origin == CandidateOrigin::Manual {
        return StagingDecision {
            artifact_type,
            tier: StagingTier::AutoConfirm,
            reason: "manually authored by the user; the trust gate does not apply".to_string(),
        };
    }

    if request.overwrites_manual {
        return StagingDecision {
            artifact_type,
            tier: StagingTier::NeverAuto,
            reason: "would overwrite a Manual/user-authored (or unverifiable-provenance) entry; \
                      hard-blocked — always requires an explicit human action"
                .to_string(),
        };
    }

    if request.origin == CandidateOrigin::Reflected {
        return StagingDecision {
            artifact_type,
            tier: StagingTier::StageForReview,
            reason: "produced by the out-of-band reflection pass; always staged for review \
                      regardless of scope or contradiction"
                .to_string(),
        };
    }

    if request.contradicts_existing {
        return StagingDecision {
            artifact_type,
            tier: StagingTier::StageForReview,
            reason: "supersedes or contradicts an existing entry not independently verified \
                      safe to override; staged for review"
                .to_string(),
        };
    }

    if request.scope != CandidateScope::Agent {
        return StagingDecision {
            artifact_type,
            tier: StagingTier::StageForReview,
            reason: "writes to project/global scope (or promotes across scope) always stage \
                      for review"
                .to_string(),
        };
    }

    match artifact_type {
        ArtifactType::Memory => StagingDecision {
            artifact_type,
            tier: StagingTier::AutoConfirm,
            reason: "new agent-scope memory that contradicts nothing; auto-confirmed"
                .to_string(),
        },
        ArtifactType::Skill => StagingDecision {
            artifact_type,
            tier: StagingTier::StageForReview,
            reason: "every model-invocable skill stages for review, regardless of scope or \
                      contradiction — there is no low-stakes auto-enable path"
                .to_string(),
        },
        ArtifactType::Workflow => StagingDecision {
            artifact_type,
            tier: StagingTier::StageForReview,
            reason: "reserved artifact type; no auto-confirm rule defined yet, so it defaults \
                      to the safe tier"
                .to_string(),
        },
    }
}
