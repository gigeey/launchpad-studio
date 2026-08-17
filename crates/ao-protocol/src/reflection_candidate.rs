//! Staged output of the OBSERVE reflection pass.
//!
//! The reflection pass runs out-of-band, off the user's turn, so nobody is
//! watching a live tool response when it proposes something — unlike an
//! in-turn `MemoryWrite`/`SkillRegister` call, whose transient "staged, not
//! applied" result at least reaches the calling turn. A [`ReflectionCandidate`]
//! is the durable record that survives past the pass that produced it: it
//! lands in `ao_persistence::reflection_staging::ReflectionStagingStore` so a
//! human (or a later task-11 generalization pass, for skills) has something
//! concrete to read and act on.
//!
//! Every candidate this type represents has already been through the
//! trust gate (`ao_engine_tools_core::trust_gate::stage_candidate`) with
//! `CandidateOrigin::Reflected` — which always quarantines — so `status`
//! starts at [`ReflectionCandidateStatus::Pending`] and nothing is ever
//! written to a live memory/skill store as a side effect of staging.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::memory::MemoryScope;
use crate::outcome::ArtifactKind;

/// Review state of a staged candidate.
///
/// [`Self::Pending`] is what every candidate starts as. [`Self::Confirmed`]
/// and [`Self::Rejected`] are written by the human-facing review surface,
/// not yet built — the variants exist now so that surface
/// has a state machine to write into rather than inventing one later.
/// [`Self::Distilled`] is the one exception already produced today: the
/// distillation pass sets it on the group of concrete `Skill`
/// candidates it folded into one generalized template, so the same
/// observations are never re-clustered into a second template on a later
/// pass. It does not mean the resulting skill is live — that skill still sits
/// behind the trust gate exactly like any other reflected candidate.
///
/// [`Self::Expired`] is the staged-candidate TTL sweep's terminal state
/// (`ao_engine_tools_engine::memory::staged_ttl`): a candidate nobody
/// reviewed within the sweep's TTL window. Deliberately a distinct variant
/// from [`Self::Rejected`] — `Rejected` means a human looked at this and said
/// no (and, per `memory::promotion_budget`, feeds the acceptance-rate
/// signal); `Expired` means nobody looked at all, so it must never be
/// mistaken for that human ground truth. Both leave the candidate itself on
/// disk for audit — this is a soft-tombstone, not a deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReflectionCandidateStatus {
    #[default]
    Pending,
    Confirmed,
    Rejected,
    Distilled,
    Expired,
}

/// A candidate memory or skill proposed by the reflection pass from a
/// transcript delta, awaiting confirmation before it can ever go live.
///
/// `kind` reuses [`ArtifactKind`] (the outcome-tracking tag)
/// rather than introducing a third parallel memory/skill enum — this crate
/// sits below `ao-engine-tools-core`, where the trust gate's own
/// `ArtifactType` lives, so the one call site that invokes the gate converts
/// between the two instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflectionCandidate {
    pub id: String,
    pub kind: ArtifactKind,
    /// Agent whose thread this candidate was distilled from.
    pub agent_id: String,
    /// Source `Thread::id` the candidate's content was read from.
    pub source_thread_id: String,
    /// Proposed content: for `Memory`, the memory text itself; for `Skill`,
    /// a description of the concrete procedure as observed. The
    /// generalization pass turns a `Skill` candidate's content into a
    /// reusable template — this field is the seam it reads from.
    pub content: String,
    pub status: ReflectionCandidateStatus,
    /// Which scope this candidate is written into once the review queue
    /// applies it (`keep`/`edit`/`pin`). `#[serde(default)]` reads every
    /// earlier row (reflection candidates only ever proposed agent scope) as
    /// [`MemoryScope::Agent`], matching their actual behavior before this
    /// field existed.
    #[serde(default)]
    pub target_scope: MemoryScope,
    /// Storage key for [`Self::target_scope`]: the project hash when
    /// `target_scope == Project`, unused (and always `None`) for
    /// `Agent`/`Global`/`AgentProject`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_scope_key: Option<String>,
    /// Id of the existing memory entry this candidate's content probably
    /// restates or contradicts (similarity check), if any. Only ever set
    /// for `kind == Memory`. Whatever eventually implements confirmation
    /// must consult this before applying the candidate, so a contested
    /// candidate is never silently written over — least of all over a
    /// user-authored (`Manual`) entry.
    ///
    /// By construction (`ao_engine_tools_core::trust_gate::stage_candidate`'s
    /// precedence order — the never-auto hard block for an unverified/
    /// `Manual`-provenance contradiction is always checked *before* a
    /// candidate is allowed to reach the stage-for-review tier and land
    /// here), any candidate that reaches this queue with `contradicts` set
    /// is already known to be safe to resolve: the review queue's `keep`/
    /// `edit`/`pin` actions may supersede it without a second safety check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contradicts: Option<String>,
    /// Human-readable justification from the trust gate's verdict.
    pub reason: String,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> ReflectionCandidate {
        ReflectionCandidate {
            id: "cand-1".to_string(),
            kind: ArtifactKind::Memory,
            agent_id: "agent-1".to_string(),
            source_thread_id: "thread-1".to_string(),
            content: "User prefers tabs over spaces.".to_string(),
            status: ReflectionCandidateStatus::Pending,
            target_scope: MemoryScope::Agent,
            target_scope_key: Some("agent-1".to_string()),
            contradicts: None,
            reason: "self-improvement candidate defaults to quarantine pending confirmation"
                .to_string(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn status_default_is_pending_and_serializes_snake_case() {
        assert_eq!(ReflectionCandidateStatus::default(), ReflectionCandidateStatus::Pending);
        let json = serde_json::to_string(&ReflectionCandidateStatus::Pending).unwrap();
        assert_eq!(json, "\"pending\"");
    }

    #[test]
    fn contradicts_omitted_when_none_round_trips() {
        let c = candidate();
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("contradicts"));
        let round: ReflectionCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(round.contradicts, None);
    }

    #[test]
    fn contradicts_set_round_trips() {
        let mut c = candidate();
        c.contradicts = Some("mem-existing".to_string());
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("mem-existing"));
        let round: ReflectionCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(round.contradicts, Some("mem-existing".to_string()));
    }

    #[test]
    fn kind_serializes_snake_case() {
        let c = candidate();
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"kind\":\"memory\""));
    }
}
