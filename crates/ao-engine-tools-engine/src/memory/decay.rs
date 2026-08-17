//! Periodic decay + usage boost scoring for persisted memory entries.
//!
//! `MemoryEntry::decay_score` is the deliberate inline exception to "usage
//! lives in the `.usage.json` sidecar, never inline on the entry": it
//! only ever changes as the output of [`decay_sweep`], which a caller is
//! meant to invoke periodically (daily, say), not once per surface-and-use.
//! Because a sweep only ever runs occasionally, persisting its result inline
//! never turns into the per-read JSONL rewrite the sidecar was built to
//! avoid.
//!
//! Each sweep does two things to every eligible entry: multiply its current
//! `decay_score` by a fixed per-sweep decay rate (time passed, nothing
//! happened), then add back a boost sized by how the sidecar's
//! `use_count`/`last_used` for that entry look *right now* — so an entry
//! that keeps getting surfaced-and-used between sweeps holds its score even
//! under repeated decay, while one nobody has touched keeps sliding down.
//! The sweep treats every call as one discrete time step regardless of the
//! wall-clock gap since the previous call — the caller's cadence, not a
//! stored "last swept at", is what makes this "periodic".
//!
//! [`MemorySource::Manual`] entries and anything pinned through the
//! review queue are excluded from decay entirely, mirroring
//! `eviction::select_eviction_candidate`'s exemptions: a human's own entry,
//! or one they've vouched for, never fades no matter how long it goes
//! unused. Only `MemoryStatus::Active` entries participate — an already
//! `Archived`/`Superseded` entry is no longer live guidance and has nothing
//! left to score.
//!
//! The resulting score is consumed downstream, not by this module: the
//! eviction scorer (`eviction::eviction_score`) reads `decay_score` as one
//! of its inputs, and a future relevance-ranked retrieval pass is expected
//! to do the same.

#[cfg(test)]
mod tests;

use ao_engine_tools_core::memory_usage::MemoryUsageMap;
use ao_protocol::memory::{MemoryEntry, MemorySource, MemoryStatus};
use chrono::{DateTime, Utc};

/// Ceiling every decay score is clamped to — a freshly created or fully
/// boosted entry never scores above "brand new."
pub const MAX_DECAY_SCORE: f32 = 1.0;
/// Floor every decay score is clamped to.
pub const MIN_DECAY_SCORE: f32 = 0.0;

/// Multiplicative decay applied to an eligible entry's `decay_score` on
/// every sweep, before the usage boost is added back. At `0.9`, five
/// consecutive sweeps with no offsetting use roughly halve an entry's score
/// (`0.9^5 ≈ 0.59`) — not tuned against production data, a starting point
/// the actual sweep cadence (not yet wired to a scheduler) can adjust later.
const DECAY_RATE_PER_SWEEP: f32 = 0.9;

/// Half-life, in days, for how quickly a use's contribution to the boost
/// fades with time. An entry used moments before a sweep gets close to the
/// full boost; one whose last use was a half-life ago gets roughly half.
const BOOST_RECENCY_HALF_LIFE_DAYS: f64 = 7.0;

/// Ceiling on the boost a single sweep can add back, so one burst of uses
/// can't instantly overpower several sweeps' worth of decay on its own —
/// sustained use across multiple sweeps is what keeps a score up, not a
/// single spike.
const MAX_BOOST_PER_SWEEP: f32 = 0.3;

/// Whether an entry participates in decay at all.
///
/// Mirrors `eviction::select_eviction_candidate`'s exemptions: a `Manual`
/// entry never auto-decays, a pinned entry is exempt the same way it is
/// exempt from eviction, and only `Active` entries are live guidance worth
/// scoring.
pub fn is_decay_eligible(entry: &MemoryEntry) -> bool {
    entry.status == MemoryStatus::Active
        && !matches!(entry.source, Some(MemorySource::Manual))
        && !entry.pinned
}

/// The per-sweep usage boost for one entry: `0.0` if it has never been
/// surfaced (no sidecar row), otherwise scaled by how recently it was last
/// used and how many times, saturating at [`MAX_BOOST_PER_SWEEP`].
fn usage_boost(entry_id: &str, usage: &MemoryUsageMap, now: DateTime<Utc>) -> f32 {
    let Some(usage_entry) = usage.get(entry_id) else {
        return 0.0;
    };
    let age_days = (now - usage_entry.last_used).num_seconds().max(0) as f64 / 86_400.0;
    let recency_factor = 2f64.powf(-age_days / BOOST_RECENCY_HALF_LIFE_DAYS) as f32;
    let use_factor = 1.0 - 1.0 / (1.0 + usage_entry.use_count as f32);
    MAX_BOOST_PER_SWEEP * recency_factor * use_factor
}

/// One entry's `decay_score` after a sweep, paired with its id so a caller
/// can persist the change (append an updated row, same shape as
/// `MemoryStore::apply_archive`/`apply_supersede`) without re-deriving which
/// entries were even eligible.
#[derive(Debug, Clone, PartialEq)]
pub struct DecayUpdate {
    pub id: String,
    pub new_score: f32,
}

/// Run one decay sweep over a scope's live entries.
///
/// For every [`is_decay_eligible`] entry: multiply its current
/// `decay_score` by [`DECAY_RATE_PER_SWEEP`], add back [`usage_boost`]
/// sourced from the entry's row (if any) in the `.usage.json` sidecar,
/// then clamp to `[MIN_DECAY_SCORE, MAX_DECAY_SCORE]`. Entries the sweep
/// skips (Manual, pinned, non-Active) are absent from the result — their
/// score never moves, and a caller must not write anything back for them.
///
/// Calling this repeatedly — feeding each call's `new_score` back into the
/// next call's input `decay_score` — is how an entry's score fades over
/// multiple sweeps when nothing touches it; calling it after the sidecar
/// records new uses is how the boost offsets that fade.
pub fn decay_sweep(entries: &[MemoryEntry], usage: &MemoryUsageMap, now: DateTime<Utc>) -> Vec<DecayUpdate> {
    entries
        .iter()
        .filter(|e| is_decay_eligible(e))
        .map(|e| {
            let decayed = e.decay_score * DECAY_RATE_PER_SWEEP;
            let boosted = decayed + usage_boost(&e.id, usage, now);
            DecayUpdate { id: e.id.clone(), new_score: boosted.clamp(MIN_DECAY_SCORE, MAX_DECAY_SCORE) }
        })
        .collect()
}
