use super::*;
use ao_protocol::memory::{MemoryScope, MemorySource};
use chrono::Utc;

fn entry(id: &str, content: &str, status: MemoryStatus) -> MemoryEntry {
    let now = Utc::now();
    MemoryEntry {
        id: id.to_string(),
        content: content.to_string(),
        created_at: now,
        source: Some(MemorySource::Agent),
        scope: MemoryScope::Agent,
        scope_key: Some("agent-1".to_string()),
        updated_at: now,
        deleted_at: None,
        confidence: 1.0,
        status,
        superseded_by: None,
        pinned: false,
        decay_score: 1.0,
    }
}

// --- NormalizedTokenOverlapScorer ---

#[test]
fn scorer_identical_reworded_content_scores_high() {
    let scorer = NormalizedTokenOverlapScorer;
    let score = scorer.score(
        "user prefers tabs over spaces",
        "user prefers spaces over tabs",
    );
    assert!(score >= CONTRADICTION_THRESHOLD, "score was {score}");
}

#[test]
fn scorer_partial_overlap_same_topic_scores_high() {
    let scorer = NormalizedTokenOverlapScorer;
    let score = scorer.score(
        "the user's favorite color is blue",
        "the user's favorite color is red",
    );
    assert!(score >= CONTRADICTION_THRESHOLD, "score was {score}");
}

#[test]
fn scorer_unrelated_content_scores_low() {
    let scorer = NormalizedTokenOverlapScorer;
    let score = scorer.score(
        "user prefers dark mode in the editor",
        "the project database is postgresql",
    );
    assert!(score < CONTRADICTION_THRESHOLD, "score was {score}");
}

#[test]
fn scorer_empty_content_never_scores_as_a_match() {
    let scorer = NormalizedTokenOverlapScorer;
    assert_eq!(scorer.score("", ""), 0.0);
    assert_eq!(scorer.score("", "some real content here"), 0.0);
}

#[test]
fn scorer_ignores_case_and_punctuation() {
    let scorer = NormalizedTokenOverlapScorer;
    let score = scorer.score("User Prefers TABS!", "user, prefers tabs.");
    assert_eq!(score, 1.0);
}

#[test]
fn scorer_drops_short_filler_words() {
    let scorer = NormalizedTokenOverlapScorer;
    // Shares only "to", "is", "a" (all under MIN_TOKEN_LEN) -> no real overlap.
    let score = scorer.score("to go is a plan", "to be is a way");
    assert_eq!(score, 0.0);
}

// --- find_contradiction ---

#[test]
fn find_contradiction_returns_none_when_nothing_similar() {
    let entries = vec![entry("1", "the sky is blue", MemoryStatus::Active)];
    let scorer = default_scorer();
    let result = find_contradiction(&entries, "the database uses postgresql", scorer.as_ref());
    assert!(result.is_none());
}

#[test]
fn find_contradiction_finds_reworded_match() {
    let entries = vec![entry("1", "user prefers tabs over spaces", MemoryStatus::Active)];
    let scorer = default_scorer();
    let result =
        find_contradiction(&entries, "user prefers spaces over tabs", scorer.as_ref()).unwrap();
    assert_eq!(result.entry.id, "1");
    assert!(result.score >= CONTRADICTION_THRESHOLD);
}

#[test]
fn find_contradiction_ignores_already_superseded_entries() {
    let entries = vec![entry("1", "user prefers tabs over spaces", MemoryStatus::Superseded)];
    let scorer = default_scorer();
    let result = find_contradiction(&entries, "user prefers spaces over tabs", scorer.as_ref());
    assert!(result.is_none(), "a Superseded entry must not be re-matched");
}

#[test]
fn find_contradiction_ignores_archived_entries() {
    let entries = vec![entry("1", "user prefers tabs over spaces", MemoryStatus::Archived)];
    let scorer = default_scorer();
    let result = find_contradiction(&entries, "user prefers spaces over tabs", scorer.as_ref());
    assert!(result.is_none());
}

#[test]
fn find_contradiction_picks_the_highest_scoring_match_among_several() {
    let entries = vec![
        entry("low", "user likes coffee in the morning", MemoryStatus::Active),
        entry("high", "user prefers tabs over spaces for indentation", MemoryStatus::Active),
    ];
    let scorer = default_scorer();
    let result =
        find_contradiction(&entries, "user prefers spaces over tabs for indentation", scorer.as_ref())
            .unwrap();
    assert_eq!(result.entry.id, "high");
}

// --- find_contradiction_with_fts_candidates ---

#[test]
fn fts_upgrade_surfaces_a_near_duplicate_plain_similarity_alone_misses() {
    // Padded with enough unrelated vocabulary that the shared tokens with
    // `content` are diluted below CONTRADICTION_THRESHOLD by Jaccard's union
    // denominator, while still clearing FTS_CORROBORATED_THRESHOLD.
    let padded = entry(
        "padded",
        "user prefers tabs over spaces for indentation in every python backend file, \
         javascript config, and shell script across the whole monorepo",
        MemoryStatus::Active,
    );
    let content = "user prefers tabs over spaces for indentation";
    let scorer = default_scorer();

    let plain_score = scorer.score(&padded.content, content);
    assert!(
        plain_score < CONTRADICTION_THRESHOLD,
        "fixture must be below the strict threshold on its own, was {plain_score}"
    );
    assert!(
        plain_score >= FTS_CORROBORATED_THRESHOLD,
        "fixture must still clear the corroborated threshold, was {plain_score}"
    );

    let entries = vec![padded];

    // Without FTS5 candidates, the plain pass misses it entirely.
    assert!(find_contradiction(&entries, content, scorer.as_ref()).is_none());

    // The FTS5 index independently ranked "padded" as related to `content`
    // (as `search_similar_ids` would for this query) -> now surfaced.
    let fts_ids = vec!["padded".to_string()];
    let result =
        find_contradiction_with_fts_candidates(&entries, content, scorer.as_ref(), &fts_ids)
            .expect("FTS5-corroborated candidate must be surfaced");
    assert_eq!(result.entry.id, "padded");
}

#[test]
fn fts_candidate_below_corroborated_threshold_is_not_surfaced() {
    // Shares no real vocabulary with `content` at all, so even though the
    // (hypothetical) FTS5 hit list names it, the corroboration score gate
    // still rejects it -- FTS5 presence alone is not sufficient.
    let unrelated = entry("unrelated", "the project database is postgresql", MemoryStatus::Active);
    let entries = vec![unrelated];
    let scorer = default_scorer();
    let fts_ids = vec!["unrelated".to_string()];

    let result = find_contradiction_with_fts_candidates(
        &entries,
        "user prefers dark mode in the editor",
        scorer.as_ref(),
        &fts_ids,
    );
    assert!(result.is_none());
}

#[test]
fn fts_candidates_ignore_non_active_entries() {
    let superseded = entry("old", "user prefers tabs over spaces for indentation", MemoryStatus::Superseded);
    let entries = vec![superseded];
    let scorer = default_scorer();
    let fts_ids = vec!["old".to_string()];

    let result = find_contradiction_with_fts_candidates(
        &entries,
        "user prefers tabs over spaces for indentation",
        scorer.as_ref(),
        &fts_ids,
    );
    assert!(result.is_none(), "a Superseded entry must not be re-matched via FTS5 either");
}

#[test]
fn fts_candidates_ignore_ids_not_present_in_existing() {
    let entries = vec![entry("1", "the sky is blue", MemoryStatus::Active)];
    let scorer = default_scorer();
    let fts_ids = vec!["does-not-exist".to_string()];

    let result = find_contradiction_with_fts_candidates(
        &entries,
        "the database uses postgresql",
        scorer.as_ref(),
        &fts_ids,
    );
    assert!(result.is_none());
}

#[test]
fn empty_fts_candidates_falls_back_to_plain_behavior() {
    let entries = vec![entry("1", "user prefers tabs over spaces", MemoryStatus::Active)];
    let scorer = default_scorer();

    let result =
        find_contradiction_with_fts_candidates(&entries, "user prefers spaces over tabs", scorer.as_ref(), &[])
            .unwrap();
    assert_eq!(result.entry.id, "1");
}

#[test]
fn plain_match_wins_when_it_scores_higher_than_the_fts_candidate() {
    let strong = entry("strong", "user prefers tabs over spaces", MemoryStatus::Active);
    let weak = entry(
        "weak",
        "user prefers tabs over spaces for indentation in every python backend file, \
         javascript config, and shell script across the whole monorepo",
        MemoryStatus::Active,
    );
    let entries = vec![strong, weak];
    let scorer = default_scorer();
    // FTS5 ranked the weaker (padded) entry as the top candidate, but the
    // stronger exact-topic match still wins overall.
    let fts_ids = vec!["weak".to_string()];

    let result = find_contradiction_with_fts_candidates(
        &entries,
        "user prefers spaces over tabs",
        scorer.as_ref(),
        &fts_ids,
    )
    .unwrap();
    assert_eq!(result.entry.id, "strong");
}
