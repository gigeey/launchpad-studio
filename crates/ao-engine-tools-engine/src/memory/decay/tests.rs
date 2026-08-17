use super::*;
use ao_engine_tools_core::memory_usage::{MemoryUsageEntry, MemoryUsageMap};
use ao_protocol::memory::MemoryScope;
use chrono::Duration;

fn entry(id: &str, source: Option<MemorySource>, decay_score: f32) -> MemoryEntry {
    let now = Utc::now();
    MemoryEntry {
        id: id.to_string(),
        content: format!("content for {id}"),
        created_at: now,
        source,
        scope: MemoryScope::Agent,
        scope_key: Some("agent-1".to_string()),
        updated_at: now,
        deleted_at: None,
        confidence: 1.0,
        status: MemoryStatus::Active,
        superseded_by: None,
        pinned: false,
        decay_score,
    }
}

fn pinned_entry(id: &str, source: Option<MemorySource>, decay_score: f32) -> MemoryEntry {
    MemoryEntry { pinned: true, ..entry(id, source, decay_score) }
}

fn usage_with(entries: &[(&str, u64, DateTime<Utc>)]) -> MemoryUsageMap {
    entries
        .iter()
        .map(|(id, use_count, last_used)| {
            (id.to_string(), MemoryUsageEntry { use_count: *use_count, last_used: *last_used })
        })
        .collect()
}

fn score_of(updates: &[DecayUpdate], id: &str) -> f32 {
    updates.iter().find(|u| u.id == id).unwrap_or_else(|| panic!("no update for {id}")).new_score
}

// --- is_decay_eligible ---

#[test]
fn active_agent_entry_is_eligible() {
    assert!(is_decay_eligible(&entry("e", Some(MemorySource::Agent), 1.0)));
}

#[test]
fn manual_entry_is_never_eligible() {
    assert!(!is_decay_eligible(&entry("e", Some(MemorySource::Manual), 1.0)));
}

#[test]
fn pinned_entry_is_never_eligible() {
    assert!(!is_decay_eligible(&pinned_entry("e", Some(MemorySource::Agent), 1.0)));
}

#[test]
fn non_active_entry_is_never_eligible() {
    let mut archived = entry("e", Some(MemorySource::Agent), 1.0);
    archived.status = MemoryStatus::Archived;
    assert!(!is_decay_eligible(&archived));

    let mut superseded = entry("s", Some(MemorySource::Agent), 1.0);
    superseded.status = MemoryStatus::Superseded;
    assert!(!is_decay_eligible(&superseded));
}

// --- decay_sweep: eligibility filtering ---

#[test]
fn sweep_skips_manual_pinned_and_non_active_entries() {
    let now = Utc::now();
    let mut archived = entry("archived", Some(MemorySource::Agent), 1.0);
    archived.status = MemoryStatus::Archived;

    let entries = vec![
        entry("eligible", Some(MemorySource::Agent), 1.0),
        entry("manual", Some(MemorySource::Manual), 1.0),
        pinned_entry("pinned", Some(MemorySource::Agent), 1.0),
        archived,
    ];
    let updates = decay_sweep(&entries, &MemoryUsageMap::new(), now);

    assert_eq!(updates.len(), 1, "only the eligible entry should get an update");
    assert_eq!(updates[0].id, "eligible");
}

#[test]
fn empty_scope_produces_no_updates() {
    assert!(decay_sweep(&[], &MemoryUsageMap::new(), Utc::now()).is_empty());
}

// --- decay-over-multiple-sweeps ---

#[test]
fn decay_score_decreases_over_multiple_sweeps_without_use() {
    let now = Utc::now();
    let usage = MemoryUsageMap::new();
    let mut e = entry("e", Some(MemorySource::Agent), MAX_DECAY_SCORE);

    let mut previous = e.decay_score;
    for sweep_number in 1..=5 {
        let updates = decay_sweep(&[e.clone()], &usage, now);
        let new_score = score_of(&updates, "e");
        assert!(
            new_score < previous,
            "sweep {sweep_number}: score {new_score} should be strictly lower than the prior sweep's {previous} when unused"
        );
        e.decay_score = new_score;
        previous = new_score;
    }

    assert!(previous < MAX_DECAY_SCORE * 0.6, "five unoffset sweeps should have decayed the score substantially");
}

#[test]
fn decay_score_never_drops_below_the_floor() {
    let now = Utc::now();
    let usage = MemoryUsageMap::new();
    let mut e = entry("e", Some(MemorySource::Agent), MAX_DECAY_SCORE);

    for _ in 0..200 {
        let updates = decay_sweep(&[e.clone()], &usage, now);
        e.decay_score = score_of(&updates, "e");
        assert!(e.decay_score >= MIN_DECAY_SCORE);
    }
}

// --- boost-on-use ---

#[test]
fn boost_from_recent_use_more_than_offsets_decay() {
    let now = Utc::now();
    let used = entry("used", Some(MemorySource::Agent), 0.5);
    let unused = entry("unused", Some(MemorySource::Agent), 0.5);
    let usage = usage_with(&[("used", 50, now)]);

    let updates = decay_sweep(&[used, unused], &usage, now);

    let used_score = score_of(&updates, "used");
    let unused_score = score_of(&updates, "unused");
    assert!(
        used_score > unused_score,
        "recently-used entry ({used_score}) should score higher than an untouched one ({unused_score}) after the same sweep"
    );
    assert!(
        used_score > 0.5 * DECAY_RATE_PER_SWEEP,
        "heavy recent use should push the score above what pure decay alone would leave it at"
    );
}

#[test]
fn usage_boost_is_zero_when_entry_never_surfaced() {
    let now = Utc::now();
    let e = entry("e", Some(MemorySource::Agent), 1.0);
    assert_eq!(usage_boost(&e.id, &MemoryUsageMap::new(), now), 0.0);
}

#[test]
fn usage_boost_fades_as_last_used_recedes_into_the_past() {
    let now = Utc::now();
    let recent = usage_with(&[("e", 10, now)]);
    let stale = usage_with(&[("e", 10, now - Duration::days(60))]);

    let recent_boost = usage_boost("e", &recent, now);
    let stale_boost = usage_boost("e", &stale, now);
    assert!(
        recent_boost > stale_boost,
        "a recently-used entry's boost ({recent_boost}) should exceed one used long ago ({stale_boost})"
    );
}

#[test]
fn usage_boost_saturates_with_diminishing_returns_per_additional_use() {
    let now = Utc::now();
    let few_uses = usage_with(&[("e", 1, now)]);
    let many_uses = usage_with(&[("e", 1000, now)]);

    let few_boost = usage_boost("e", &few_uses, now);
    let many_boost = usage_boost("e", &many_uses, now);
    assert!(many_boost > few_boost, "more uses should still boost more, even with diminishing returns");
    assert!(many_boost <= MAX_BOOST_PER_SWEEP, "boost must never exceed the per-sweep ceiling");
}

#[test]
fn decay_score_never_exceeds_the_ceiling_even_with_a_large_boost() {
    let now = Utc::now();
    let e = entry("e", Some(MemorySource::Agent), MAX_DECAY_SCORE);
    let usage = usage_with(&[("e", 10_000, now)]);

    let updates = decay_sweep(&[e], &usage, now);
    assert!(score_of(&updates, "e") <= MAX_DECAY_SCORE);
}
