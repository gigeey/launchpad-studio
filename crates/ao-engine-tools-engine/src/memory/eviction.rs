//! Score-based eviction for a scope sitting at its hard entry cap.
//!
//! Hitting a scope's hard cap used to reject the write outright and push the
//! model toward manual `MemoryList` + `MemoryDelete` surgery — a wedge, not a
//! boundary. This module picks which existing entry should make room for a
//! new one by scoring every eligible entry and handing back the lowest
//! scorer, so the caller (`write.rs`) can archive it (soft-tombstone, never
//! hard-delete) instead of refusing the write. A full scope then behaves
//! like a sliding window rather than a wall.
//!
//! Value is estimated from inputs already on disk or in the usage
//! sidecar: how recently the entry was touched, how often it has been
//! surfaced and used, how much the store trusts it (`confidence`), the
//! decay sweep's slow-moving `decay_score`, and who authored it. A
//! `MemorySource::Manual` entry is never a candidate — see
//! [`select_eviction_candidate`] — a human wrote it on purpose and the store
//! must never silently forget it to make room for something else.
//!
//! `decay_score` and the recency/usage terms below measure related but
//! distinct things: recency/usage are recomputed fresh from the current
//! `updated_at`/sidecar state on every call, while `decay_score` is whatever
//! the last `decay::decay_sweep` run left behind — a longer-running trend
//! that survives even if the instantaneous signals briefly look better than
//! an entry's actual track record.
//!
//! [`thread_eviction_sweep`] below is a second, deliberately different
//! consumer of this module for `Thread` scope: a thread lives for a single
//! session, so its entries never accrue the usage-sidecar history or the
//! `decay_score` trend the scorer above weighs. Plain `created_at` order
//! is the right-sized replacement for that tier, not a reuse of
//! [`select_eviction_candidate`].

#[cfg(test)]
mod tests;

use ao_engine_tools_core::memory_usage::MemoryUsageMap;
use ao_protocol::memory::{MemoryEntry, MemorySource, MemoryStatus};
use chrono::{DateTime, Utc};

/// Recency half-life, in days: an entry whose reference time is this far in
/// the past has its recency contribution to [`eviction_score`] cut in half.
/// Not tuned against production data — a starting point for the decay work
/// Decay builds on top of this scorer.
const RECENCY_HALF_LIFE_DAYS: f64 = 30.0;

/// Weight of the recency term in [`eviction_score`].
const RECENCY_WEIGHT: f32 = 0.5;
/// Weight of the usage term in [`eviction_score`].
const USAGE_WEIGHT: f32 = 0.3;
/// Weight of the `decay_score` term in [`eviction_score`].
const DECAY_WEIGHT: f32 = 0.4;

/// Per-[`MemorySource`] adjustment layered on top of confidence/recency/usage.
///
/// A promoted entry has already cleared an evidence bar to get there, so it
/// gets a small boost; unknown provenance (`None`, always a legacy row)
/// gets a small penalty since it is the least verifiable candidate among
/// the entries actually eligible for eviction. Plain `Agent` writes are the
/// neutral baseline. `Manual` is listed for completeness only —
/// [`select_eviction_candidate`] filters those out before any score is
/// computed, so this arm is never actually read.
fn source_bonus(source: &Option<MemorySource>) -> f32 {
    match source {
        Some(MemorySource::GlobalPromotion) => 0.25,
        Some(MemorySource::Agent) => 0.0,
        None => -0.1,
        Some(MemorySource::Manual) => 0.0,
    }
}

/// The timestamp eviction scoring treats as "last touched" for an entry:
/// the usage sidecar's `last_used` if the entry has ever been surfaced and
/// used, otherwise its own `updated_at`.
fn reference_time(entry: &MemoryEntry, usage: &MemoryUsageMap) -> DateTime<Utc> {
    usage.get(&entry.id).map(|u| u.last_used).unwrap_or(entry.updated_at)
}

/// Score one entry's eviction worthiness — **lower means more evictable**.
///
/// ```text
/// score = confidence
///       + RECENCY_WEIGHT * recency_factor
///       + USAGE_WEIGHT   * usage_factor
///       + DECAY_WEIGHT   * entry.decay_score
///       + source_bonus
/// ```
///
/// - `confidence` is the entry's own field, already `0.0..=1.0`.
/// - `recency_factor = 2 ^ (-age_days / RECENCY_HALF_LIFE_DAYS)`, where
///   `age_days` is `now - reference_time(entry)` in days (floored at `0`).
///   Ranges over `(0.0, 1.0]`: a just-touched entry scores near `1.0`, one
///   untouched for a full half-life scores `0.5`.
/// - `usage_factor = 1 - 1 / (1 + use_count)`, `use_count` from the
///   sidecar (`0` if the entry has no sidecar row yet). Ranges over `[0.0,
///   1.0)`, saturating with diminishing returns per additional use.
/// - `entry.decay_score` is already `0.0..=1.0` (see `decay::decay_sweep`)
///   — defaults to `1.0` for any entry no sweep has touched yet, so this
///   term is a no-op until decay sweeps actually run.
/// - `source_bonus` — see [`source_bonus`].
pub fn eviction_score(entry: &MemoryEntry, usage: &MemoryUsageMap, now: DateTime<Utc>) -> f32 {
    let age_days = (now - reference_time(entry, usage)).num_seconds().max(0) as f64 / 86_400.0;
    let recency_factor = 2f64.powf(-age_days / RECENCY_HALF_LIFE_DAYS) as f32;

    let use_count = usage.get(&entry.id).map(|u| u.use_count).unwrap_or(0);
    let usage_factor = 1.0 - 1.0 / (1.0 + use_count as f32);

    entry.confidence
        + RECENCY_WEIGHT * recency_factor
        + USAGE_WEIGHT * usage_factor
        + DECAY_WEIGHT * entry.decay_score
        + source_bonus(&entry.source)
}

/// Pick the lowest-scoring entry eligible for eviction, if any exist.
///
/// Three filters apply before any score is compared: `MemorySource::Manual`
/// entries are always excluded — they are never auto-evicted, regardless of
/// score; entries a human has `pin`ned through the review queue are excluded the same way, regardless of `source` — a human
/// vouching for an agent-authored entry earns it the same protection a
/// user-authored one already has; and only `MemoryStatus::Active` entries
/// are considered, since an already `Archived`/`Superseded` entry has
/// already left live guidance and re-archiving it would free no room. Ties
/// break toward the older reference time ([`reference_time`]) so the choice
/// is deterministic rather than depending on input order.
pub fn select_eviction_candidate<'a>(
    existing: &'a [MemoryEntry],
    usage: &MemoryUsageMap,
    now: DateTime<Utc>,
) -> Option<&'a MemoryEntry> {
    existing
        .iter()
        .filter(|e| e.status == MemoryStatus::Active)
        .filter(|e| !matches!(e.source, Some(MemorySource::Manual)))
        .filter(|e| !e.pinned)
        .map(|e| (e, eviction_score(e, usage, now)))
        .min_by(|(a, a_score), (b, b_score)| {
            a_score
                .partial_cmp(b_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| reference_time(a, usage).cmp(&reference_time(b, usage)))
        })
        .map(|(e, _)| e)
}

/// Oldest-first eviction pass for `Thread` scope —
/// mandatory auto-evict for the tier that must "die with the thread" rather
/// than wedge at its hard cap the way a durable scope can. Every `Active`
/// entry is eligible: thread entries are never `Manual`-sourced (see
/// `write.rs`'s `write_thread_entry`), so unlike [`select_eviction_candidate`]
/// there is no source/pin exemption to carry over, and no usage/decay
/// signal worth scoring over a single session's lifetime — `created_at`
/// order alone is the right-sized replacement.
///
/// Returns the ids of however many of `existing`'s live entries must be
/// dropped, oldest first, to bring the count down to `cap` — empty once
/// already at or under it. Takes `cap` rather than assuming "one entry over"
/// so a caller can enforce the bound unconditionally: a thread's memory must
/// never grow unbounded, no matter how many entries ended up past the cap.
pub fn thread_eviction_sweep(existing: &[MemoryEntry], cap: usize) -> Vec<String> {
    let mut active: Vec<&MemoryEntry> = existing.iter().filter(|e| e.status == MemoryStatus::Active).collect();
    active.sort_by_key(|e| e.created_at);
    let overflow = active.len().saturating_sub(cap);
    active.into_iter().take(overflow).map(|e| e.id.clone()).collect()
}
