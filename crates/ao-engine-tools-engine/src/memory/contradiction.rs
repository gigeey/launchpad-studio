//! Semantic contradiction guard.
//!
//! Byte-equal dedup (see `write.rs`) only catches an agent re-submitting the
//! exact same string. It does nothing about a reworded restatement — "user
//! prefers tabs over spaces" landing next to an existing "user prefers spaces
//! over tabs" — which is the more common and more dangerous case: two
//! near-duplicate entries silently coexist, and whichever one the context
//! loader happens to surface last wins, with no record that anything was
//! overridden.
//!
//! This module answers one question: *is `content` probably talking about
//! the same thing as an existing entry?* It deliberately does not try to
//! answer the harder question of whether the new content *agrees or
//! disagrees* with that entry — telling "the user re-stated the same
//! preference" apart from "the user changed their mind" needs semantic
//! understanding that a string-similarity pass cannot provide. See
//! [`SimilarityScorer`] for how that harder half is deferred, not ignored.
//!
//! Treating every high-overlap match as a same-topic candidate is the
//! conservative choice precisely because the two failure modes it can't tell
//! apart are handled safely either way by the caller (`write.rs`): a match
//! against a user-authored entry always routes to human review instead of
//! writing (user artifacts always outrank agent artifacts), and a match
//! against an agent-authored entry marks the old one `Superseded` rather than
//! deleting it — so a false positive (an elaboration mistaken for a
//! contradiction) never loses information, it just costs a review step or an
//! extra provenance hop.

#[cfg(test)]
mod tests;

use std::collections::HashSet;

use ao_protocol::memory::{MemoryEntry, MemoryStatus};

/// A candidate match against the entries already in a scope.
pub struct ContradictionMatch<'a> {
    pub entry: &'a MemoryEntry,
    pub score: f32,
}

/// How similar two memory contents are, on a `0.0..=1.0` scale, where higher
/// means "more likely to be about the same fact."
///
/// This is the seam left open ("embeddings dependency: local
/// model vs. hosted?"). [`NormalizedTokenOverlapScorer`] is the only
/// implementation today — pure string normalization, no model, no network
/// call, no new dependency. A future embeddings-backed scorer (local or
/// hosted) plugs in by implementing this same trait and swapping the
/// instance [`default_scorer`] returns; nothing in `write.rs` or the trust
/// gate call needs to change, since both only ever see a `f32` score.
pub trait SimilarityScorer: Send + Sync {
    /// Score how likely `a` and `b` describe the same fact. Must be
    /// symmetric-in-spirit (implementations are not required to guarantee
    /// exact symmetry, but callers must not depend on argument order).
    fn score(&self, a: &str, b: &str) -> f32;
}

/// Normalizes both strings to a lowercase alphanumeric token set and scores
/// via Jaccard similarity (`|intersection| / |union|`).
///
/// Tokens shorter than [`MIN_TOKEN_LEN`] are dropped as noise (articles,
/// prepositions) so two contents that only share filler words don't score as
/// related. An empty token set on either side scores `0.0` rather than the
/// degenerate `1.0` two empty sets would otherwise produce.
pub struct NormalizedTokenOverlapScorer;

const MIN_TOKEN_LEN: usize = 3;

fn normalized_tokens(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.chars().count() >= MIN_TOKEN_LEN)
        .collect()
}

impl SimilarityScorer for NormalizedTokenOverlapScorer {
    fn score(&self, a: &str, b: &str) -> f32 {
        let ta = normalized_tokens(a);
        let tb = normalized_tokens(b);
        if ta.is_empty() || tb.is_empty() {
            return 0.0;
        }
        let intersection = ta.intersection(&tb).count();
        let union = ta.union(&tb).count();
        intersection as f32 / union as f32
    }
}

/// The default scorer used by the write path today. Swap this to change what
/// backs [`find_contradiction`] without touching any call site.
pub fn default_scorer() -> Box<dyn SimilarityScorer> {
    Box::new(NormalizedTokenOverlapScorer)
}

/// A score at or above this threshold is treated as "same topic" for the
/// purposes of the contradiction guard. Tuned against short, single-fact
/// memory entries (the store's own soft cap encourages these); long
/// multi-topic entries are more likely to false-positive against unrelated
/// content that happens to share a few words, but the caller's
/// (never-delete, always-provenance) handling keeps that failure mode cheap.
pub const CONTRADICTION_THRESHOLD: f32 = 0.6;

/// Find the highest-scoring *active* existing entry that is probably the
/// same fact as `content`, if its score clears [`CONTRADICTION_THRESHOLD`].
///
/// Only entries with `status == Active` are considered — an entry that is
/// already `Superseded` or `Archived` has already been through this guard
/// once and is not live guidance to protect further.
pub fn find_contradiction<'a>(
    existing: &'a [MemoryEntry],
    content: &str,
    scorer: &dyn SimilarityScorer,
) -> Option<ContradictionMatch<'a>> {
    existing
        .iter()
        .filter(|e| e.status == MemoryStatus::Active)
        .map(|e| ContradictionMatch { entry: e, score: scorer.score(&e.content, content) })
        .filter(|m| m.score >= CONTRADICTION_THRESHOLD)
        .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
}

/// A score at or above this bar counts as "same topic" only for a candidate
/// the FTS5 index has *independently* ranked as related to `content` (see
/// [`find_contradiction_with_fts_candidates`]) — lower than
/// [`CONTRADICTION_THRESHOLD`] because Jaccard punishes a restatement that
/// happens to sit inside a longer entry padded with unrelated detail: the
/// shared tokens are diluted by the union with every extra word, even though
/// every one of `content`'s distinctive terms is present. FTS5's `bm25`
/// ranking has no such length penalty, so a candidate it surfaces has
/// already cleared a real relevance bar; this constant only needs to rule
/// out coincidental single-word overlap on top of that.
pub const FTS_CORROBORATED_THRESHOLD: f32 = 0.3;

/// [`find_contradiction`], widened with candidates the FTS5 full-text index
/// surfaced for `content` — the upgrade past pure string-similarity (plan
/// `fts_candidate_ids` is the id list from
/// `ao_persistence::memory::MemoryStore::search_similar_ids`, already scoped
/// to the same memory scope and the `Memory` artifact kind, ranked best
/// match first; an empty slice (no index attached, or the query had no
/// indexable tokens) falls back to exactly [`find_contradiction`]'s
/// behavior.
///
/// The index only ever *widens recall*, never replaces the strict pass: a
/// candidate must still clear [`CONTRADICTION_THRESHOLD`] on its own, or be
/// an FTS5 hit that clears the lower [`FTS_CORROBORATED_THRESHOLD`] — the
/// index's own ranking is what makes lowering the bar safe for that specific
/// candidate, instead of lowering it for every entry in the scope and
/// inviting false positives.
pub fn find_contradiction_with_fts_candidates<'a>(
    existing: &'a [MemoryEntry],
    content: &str,
    scorer: &dyn SimilarityScorer,
    fts_candidate_ids: &[String],
) -> Option<ContradictionMatch<'a>> {
    let plain_match = find_contradiction(existing, content, scorer);

    let fts_match = fts_candidate_ids
        .iter()
        .filter_map(|id| {
            let candidate = existing
                .iter()
                .find(|e| e.id == *id && e.status == MemoryStatus::Active)?;
            let score = scorer.score(&candidate.content, content);
            (score >= FTS_CORROBORATED_THRESHOLD)
                .then_some(ContradictionMatch { entry: candidate, score })
        })
        .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));

    match (plain_match, fts_match) {
        (Some(p), Some(f)) if f.score > p.score => Some(f),
        (Some(p), _) => Some(p),
        (None, f) => f,
    }
}
