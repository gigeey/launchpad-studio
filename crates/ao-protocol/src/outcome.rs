//! Per-turn outcome signal for the self-improvement loop.
//!
//! [`OutcomeRecord`] is the feedback substrate memory decay/promotion and
//! skill retirement consume: for every turn, which memory entries and
//! skills were actually drawn on, and how the turn seems to have gone.
//! Without it, "this artifact was surfaced" and "the turn went well" are
//! two facts sitting in different places with no join key between them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which store an [`ArtifactRef::id`] should be looked up in.
///
/// Kept as an explicit enum rather than inferring the kind from the id's
/// shape, so a consumer joining outcome records back to the memory or skill
/// store never has to guess which table to query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Memory,
    Skill,
}

/// Points at one memory entry or skill that played a role in a turn.
///
/// `id` is a `MemoryEntry::id` when `kind` is [`ArtifactKind::Memory`], or
/// the skill name passed to `RunSkill` when `kind` is [`ArtifactKind::Skill`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub kind: ArtifactKind,
    pub id: String,
}

impl ArtifactRef {
    pub fn memory(id: impl Into<String>) -> Self {
        Self {
            kind: ArtifactKind::Memory,
            id: id.into(),
        }
    }

    pub fn skill(name: impl Into<String>) -> Self {
        Self {
            kind: ArtifactKind::Skill,
            id: name.into(),
        }
    }
}

/// How confident a turn's [`OutcomeRecord`] is about reflecting on the
/// artifacts it used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OutcomeSignal {
    /// A direct, unambiguous reaction: thumbs up/down, an explicit
    /// accept/reject of a review item, or a stated "that's wrong" /
    /// "that's right". `positive` carries the direction; `detail` is the
    /// triggering text when one is available.
    Explicit {
        positive: bool,
        detail: Option<String>,
    },
    /// No direct reaction was observed, but the turn's shape implies things
    /// went fine — it completed with no immediate follow-up correction, an
    /// artifact was surfaced and the turn succeeded, or a skill was reused
    /// without complaint. The weakest and most abundant signal.
    Implicit,
    /// A correction followed immediately after a specific memory/skill was
    /// used — the strongest signal that the artifact steered the turn
    /// wrong. `detail` is the correcting text when one is available.
    Negative { detail: Option<String> },
}

/// A lightweight per-turn record of which artifacts were in play and how
/// the turn seems to have gone.
///
/// One record is persisted per turn, alongside that turn's transcript
/// entry, so a later pass can compute "surfaced ∧ outcome" without
/// re-reading the whole transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// Unique id for this turn.
    pub turn_id: String,
    /// The session this turn belongs to.
    pub session_id: String,
    /// Every memory entry and skill this turn drew on, in first-touch
    /// order. Memory entries come from the system-prompt surfacing pass at
    /// turn start; skills come from `RunSkill` invocations during the turn.
    pub artifacts_used: Vec<ArtifactRef>,
    // TODO(outcome-signal): `signal` here is a first-cut heuristic (Implicit
    // by default whenever the turn completes). Correlating a *specific*
    // artifact with a following correction (rather than the whole turn),
    // and parsing explicit user reactions, is richer derivation that
    // belongs to memory decay/boost and skill retirement — the actual
    // consumers of this record.
    pub signal: OutcomeSignal,
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_record_round_trips_through_json() {
        let record = OutcomeRecord {
            turn_id: "turn-1".to_string(),
            session_id: "session-1".to_string(),
            artifacts_used: vec![
                ArtifactRef::memory("mem-abc"),
                ArtifactRef::skill("review-pr"),
            ],
            signal: OutcomeSignal::Implicit,
            timestamp: Utc::now(),
        };

        let json = serde_json::to_string(&record).unwrap();
        let deserialized: OutcomeRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, deserialized);
    }

    #[test]
    fn artifact_kind_serializes_snake_case() {
        let memory_ref = ArtifactRef::memory("mem-1");
        let json = serde_json::to_string(&memory_ref).unwrap();
        assert_eq!(json, r#"{"kind":"memory","id":"mem-1"}"#);

        let skill_ref = ArtifactRef::skill("review-pr");
        let json = serde_json::to_string(&skill_ref).unwrap();
        assert_eq!(json, r#"{"kind":"skill","id":"review-pr"}"#);
    }

    #[test]
    fn explicit_and_negative_signals_round_trip_with_detail() {
        for signal in [
            OutcomeSignal::Explicit {
                positive: false,
                detail: Some("that's wrong".to_string()),
            },
            OutcomeSignal::Negative {
                detail: Some("actually it's X not Y".to_string()),
            },
            OutcomeSignal::Implicit,
        ] {
            let json = serde_json::to_string(&signal).unwrap();
            let deserialized: OutcomeSignal = serde_json::from_str(&json).unwrap();
            assert_eq!(signal, deserialized);
        }
    }
}
