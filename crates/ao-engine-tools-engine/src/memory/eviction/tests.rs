use super::*;
use ao_engine_tools_core::memory_usage::{MemoryUsageEntry, MemoryUsageMap};
use ao_protocol::memory::{MemoryScope, MemoryStatus};
use chrono::Duration;

fn entry(id: &str, source: Option<MemorySource>, confidence: f32, updated_at: DateTime<Utc>) -> MemoryEntry {
    MemoryEntry {
        id: id.to_string(),
        content: format!("content for {id}"),
        created_at: updated_at,
        source,
        scope: MemoryScope::Agent,
        scope_key: Some("agent-1".to_string()),
        updated_at,
        deleted_at: None,
        confidence,
        status: MemoryStatus::Active,
        superseded_by: None,
        pinned: false,
        decay_score: 1.0,
    }
}

/// Same as [`entry`] but pinned — for exercising the pin exemption
/// alongside the existing `MemorySource::Manual` exemption.
fn pinned_entry(id: &str, source: Option<MemorySource>, confidence: f32, updated_at: DateTime<Utc>) -> MemoryEntry {
    MemoryEntry { pinned: true, ..entry(id, source, confidence, updated_at) }
}

/// Same as [`entry`] but with an explicit `decay_score` — for exercising
/// how the decay sweep's output feeds into eviction scoring.
fn entry_with_decay(
    id: &str,
    source: Option<MemorySource>,
    confidence: f32,
    updated_at: DateTime<Utc>,
    decay_score: f32,
) -> MemoryEntry {
    MemoryEntry { decay_score, ..entry(id, source, confidence, updated_at) }
}

fn usage_with(entries: &[(&str, u64, DateTime<Utc>)]) -> MemoryUsageMap {
    entries
        .iter()
        .map(|(id, use_count, last_used)| {
            (
                id.to_string(),
                MemoryUsageEntry { use_count: *use_count, last_used: *last_used },
            )
        })
        .collect()
}

// --- eviction_score ---

#[test]
fn fresh_high_confidence_entry_scores_higher_than_stale_low_confidence_entry() {
    let now = Utc::now();
    let usage = MemoryUsageMap::new();

    let fresh = entry("fresh", Some(MemorySource::Agent), 1.0, now);
    let stale = entry("stale", Some(MemorySource::Agent), 0.2, now - Duration::days(120));

    let fresh_score = eviction_score(&fresh, &usage, now);
    let stale_score = eviction_score(&stale, &usage, now);
    assert!(
        fresh_score > stale_score,
        "fresh score {fresh_score} should exceed stale score {stale_score}"
    );
}

#[test]
fn higher_use_count_increases_score() {
    let now = Utc::now();
    let unused = entry("unused", Some(MemorySource::Agent), 0.5, now - Duration::days(10));
    let heavily_used = entry("heavily-used", Some(MemorySource::Agent), 0.5, now - Duration::days(10));

    let no_usage = MemoryUsageMap::new();
    let with_usage = usage_with(&[("heavily-used", 50, now - Duration::days(10))]);

    let unused_score = eviction_score(&unused, &no_usage, now);
    let used_score = eviction_score(&heavily_used, &with_usage, now);
    assert!(
        used_score > unused_score,
        "used score {used_score} should exceed unused score {unused_score}"
    );
}

#[test]
fn global_promotion_source_scores_higher_than_plain_agent_source_all_else_equal() {
    let now = Utc::now();
    let usage = MemoryUsageMap::new();

    let promoted = entry("promoted", Some(MemorySource::GlobalPromotion), 0.5, now);
    let agent = entry("agent", Some(MemorySource::Agent), 0.5, now);

    assert!(eviction_score(&promoted, &usage, now) > eviction_score(&agent, &usage, now));
}

#[test]
fn unknown_source_scores_lower_than_plain_agent_source_all_else_equal() {
    let now = Utc::now();
    let usage = MemoryUsageMap::new();

    let unknown = entry("unknown", None, 0.5, now);
    let agent = entry("agent", Some(MemorySource::Agent), 0.5, now);

    assert!(eviction_score(&unknown, &usage, now) < eviction_score(&agent, &usage, now));
}

#[test]
fn usage_sidecar_last_used_overrides_updated_at_for_recency() {
    let now = Utc::now();
    // updated_at looks ancient, but the usage sidecar says it was just used —
    // recency scoring must follow the sidecar, not the stale updated_at.
    let old_updated_at = now - Duration::days(120);
    let e = entry("e", Some(MemorySource::Agent), 0.5, old_updated_at);

    let stale_view = eviction_score(&e, &MemoryUsageMap::new(), now);
    let fresh_usage = usage_with(&[("e", 1, now)]);
    let fresh_view = eviction_score(&e, &fresh_usage, now);

    assert!(
        fresh_view > stale_view,
        "recent last_used ({fresh_view}) should score higher than stale updated_at fallback ({stale_view})"
    );
}

// --- select_eviction_candidate ---

#[test]
fn selects_the_lowest_scoring_entry() {
    let now = Utc::now();
    let entries = vec![
        entry("high-conf-fresh", Some(MemorySource::Agent), 1.0, now),
        entry("low-conf-stale", Some(MemorySource::Agent), 0.1, now - Duration::days(200)),
        entry("mid", Some(MemorySource::Agent), 0.5, now - Duration::days(30)),
    ];
    let usage = MemoryUsageMap::new();

    let picked = select_eviction_candidate(&entries, &usage, now).unwrap();
    assert_eq!(picked.id, "low-conf-stale");
}

#[test]
fn manual_entries_are_never_selected_even_when_lowest_scoring() {
    let now = Utc::now();
    let entries = vec![
        // Deliberately the worst possible score, but Manual — must be exempt.
        entry("manual-worst", Some(MemorySource::Manual), 0.0, now - Duration::days(400)),
        entry("agent-ok", Some(MemorySource::Agent), 0.9, now),
    ];
    let usage = MemoryUsageMap::new();

    let picked = select_eviction_candidate(&entries, &usage, now).unwrap();
    assert_eq!(picked.id, "agent-ok", "the Manual entry must never be picked, regardless of score");
}

#[test]
fn returns_none_when_every_entry_is_manual() {
    let now = Utc::now();
    let entries = vec![
        entry("manual-1", Some(MemorySource::Manual), 0.5, now),
        entry("manual-2", Some(MemorySource::Manual), 0.5, now),
    ];
    let usage = MemoryUsageMap::new();

    assert!(select_eviction_candidate(&entries, &usage, now).is_none());
}

#[test]
fn pinned_entries_are_never_selected_even_when_lowest_scoring() {
    let now = Utc::now();
    let entries = vec![
        // Deliberately the worst possible score, but pinned — must be exempt,
        // exactly like a Manual entry, even though the source is Agent.
        pinned_entry("pinned-worst", Some(MemorySource::Agent), 0.0, now - Duration::days(400)),
        entry("agent-ok", Some(MemorySource::Agent), 0.9, now),
    ];
    let usage = MemoryUsageMap::new();

    let picked = select_eviction_candidate(&entries, &usage, now).unwrap();
    assert_eq!(picked.id, "agent-ok", "a pinned entry must never be picked, regardless of score");
}

#[test]
fn returns_none_when_every_entry_is_pinned_or_manual() {
    let now = Utc::now();
    let entries = vec![
        pinned_entry("pinned-1", Some(MemorySource::Agent), 0.5, now),
        entry("manual-1", Some(MemorySource::Manual), 0.5, now),
    ];
    let usage = MemoryUsageMap::new();

    assert!(select_eviction_candidate(&entries, &usage, now).is_none());
}

#[test]
fn empty_scope_has_no_candidate() {
    let now = Utc::now();
    assert!(select_eviction_candidate(&[], &MemoryUsageMap::new(), now).is_none());
}

#[test]
fn already_archived_or_superseded_entries_are_never_reselected() {
    let now = Utc::now();
    let mut archived = entry("already-archived", Some(MemorySource::Agent), 0.0, now - Duration::days(400));
    archived.status = MemoryStatus::Archived;
    let mut superseded = entry("already-superseded", Some(MemorySource::Agent), 0.0, now - Duration::days(400));
    superseded.status = MemoryStatus::Superseded;
    let live = entry("live", Some(MemorySource::Agent), 0.9, now);

    let entries = vec![archived, superseded, live];
    let usage = MemoryUsageMap::new();

    let picked = select_eviction_candidate(&entries, &usage, now).unwrap();
    assert_eq!(picked.id, "live", "non-Active entries must never be re-selected for eviction");
}

#[test]
fn ties_break_toward_the_older_reference_time() {
    let now = Utc::now();
    // Same confidence/source/use_count, but different reference times —
    // scores tie except for the recency term, so the older one must win.
    let older = entry("older", Some(MemorySource::Agent), 0.5, now - Duration::days(60));
    let newer = entry("newer", Some(MemorySource::Agent), 0.5, now - Duration::days(1));
    let entries = vec![newer, older];
    let usage = MemoryUsageMap::new();

    let picked = select_eviction_candidate(&entries, &usage, now).unwrap();
    assert_eq!(picked.id, "older");
}

// --- confidence integration ---

#[test]
fn lower_confidence_scores_lower_all_else_equal() {
    let now = Utc::now();
    let usage = MemoryUsageMap::new();

    let high_confidence = entry("high-confidence", Some(MemorySource::Agent), 0.9, now - Duration::days(10));
    let low_confidence = entry("low-confidence", Some(MemorySource::Agent), 0.1, now - Duration::days(10));

    assert!(
        eviction_score(&high_confidence, &usage, now) > eviction_score(&low_confidence, &usage, now),
        "lower confidence should score lower, all else equal"
    );
}

#[test]
fn a_low_confidence_entry_is_selected_over_a_higher_confidence_one_with_equal_recency_and_usage() {
    let now = Utc::now();
    let usage = MemoryUsageMap::new();

    let entries = vec![
        entry("high-confidence", Some(MemorySource::Agent), 0.9, now - Duration::days(10)),
        entry("low-confidence", Some(MemorySource::Agent), 0.1, now - Duration::days(10)),
    ];

    let picked = select_eviction_candidate(&entries, &usage, now).unwrap();
    assert_eq!(picked.id, "low-confidence", "the lower-confidence entry should be evicted first, all else equal");
}

// --- decay_score integration ---

#[test]
fn lower_decay_score_lowers_eviction_score_all_else_equal() {
    let now = Utc::now();
    let usage = MemoryUsageMap::new();

    let fresh = entry_with_decay("fresh-decay", Some(MemorySource::Agent), 0.5, now, 1.0);
    let decayed = entry_with_decay("decayed", Some(MemorySource::Agent), 0.5, now, 0.1);

    assert!(
        eviction_score(&fresh, &usage, now) > eviction_score(&decayed, &usage, now),
        "an entry the sweep has decayed should score lower than one it hasn't touched"
    );
}

#[test]
fn a_heavily_decayed_entry_is_selected_over_a_fresher_one_with_equal_confidence() {
    let now = Utc::now();
    let usage = MemoryUsageMap::new();

    let entries = vec![
        entry_with_decay("undecayed", Some(MemorySource::Agent), 0.5, now, 1.0),
        entry_with_decay("worn-down", Some(MemorySource::Agent), 0.5, now, 0.0),
    ];

    let picked = select_eviction_candidate(&entries, &usage, now).unwrap();
    assert_eq!(picked.id, "worn-down", "the decayed entry should be the one evicted");
}

// --- thread_eviction_sweep ---

#[test]
fn thread_sweep_evicts_nothing_when_under_cap() {
    let now = Utc::now();
    let entries = vec![
        entry("a", Some(MemorySource::Agent), 1.0, now - Duration::minutes(2)),
        entry("b", Some(MemorySource::Agent), 1.0, now - Duration::minutes(1)),
    ];

    assert!(thread_eviction_sweep(&entries, 5).is_empty(), "nothing should be evicted below the cap");
}

#[test]
fn thread_sweep_picks_oldest_first_up_to_overflow() {
    let now = Utc::now();
    // Five entries, oldest to newest, cap of 3 -> the two oldest must go.
    let entries = vec![
        entry("oldest", Some(MemorySource::Agent), 1.0, now - Duration::minutes(5)),
        entry("second-oldest", Some(MemorySource::Agent), 1.0, now - Duration::minutes(4)),
        entry("middle", Some(MemorySource::Agent), 1.0, now - Duration::minutes(3)),
        entry("second-newest", Some(MemorySource::Agent), 1.0, now - Duration::minutes(2)),
        entry("newest", Some(MemorySource::Agent), 1.0, now - Duration::minutes(1)),
    ];

    let evicted = thread_eviction_sweep(&entries, 3);
    assert_eq!(evicted, vec!["oldest".to_string(), "second-oldest".to_string()]);
}

#[test]
fn thread_sweep_reduces_live_entry_count_to_the_cap_when_applied() {
    let now = Utc::now();
    let mut entries: Vec<MemoryEntry> = (0..8)
        .map(|i| entry(&format!("entry-{i}"), Some(MemorySource::Agent), 1.0, now - Duration::minutes(8 - i)))
        .collect();
    assert_eq!(entries.len(), 8);

    let evicted = thread_eviction_sweep(&entries, 5);
    assert_eq!(evicted.len(), 3, "3 entries must be evicted to bring 8 down to a cap of 5");

    // Apply the pass the way the write path does: drop every evicted id.
    entries.retain(|e| !evicted.contains(&e.id));
    assert_eq!(entries.len(), 5, "live entry count must be reduced to the cap once the pass is applied");
}

#[test]
fn thread_sweep_ignores_non_active_entries_on_both_sides_of_the_count() {
    let now = Utc::now();
    let mut archived = entry("archived-old", Some(MemorySource::Agent), 1.0, now - Duration::minutes(10));
    archived.status = MemoryStatus::Archived;
    let entries = vec![
        archived,
        entry("live-a", Some(MemorySource::Agent), 1.0, now - Duration::minutes(2)),
        entry("live-b", Some(MemorySource::Agent), 1.0, now - Duration::minutes(1)),
    ];

    // Cap of 2 with 2 live entries: nothing should be evicted, and the
    // already-archived entry must never be picked even though it's the
    // oldest by created_at.
    let evicted = thread_eviction_sweep(&entries, 2);
    assert!(evicted.is_empty());
}

#[test]
fn thread_sweep_on_empty_scope_evicts_nothing() {
    assert!(thread_eviction_sweep(&[], 5).is_empty());
}
