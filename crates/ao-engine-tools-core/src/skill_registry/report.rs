//! Consumes the per-skill invocation counters written by [`super::usage`] and
//! turns them into a ranked report: which skills are actually pulling their
//! weight, and which have gone quiet.
//!
//! `usage::increment` runs on every `RunSkill` call, but nothing reads the
//! resulting `.usage.json` back out. This module is the first reader —
//! everything downstream (consolidation ranking, retirement sweeps) builds on
//! the [`SkillUsageReport`] shape defined here rather than re-parsing the
//! sidecar file itself.

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::usage::{self, UsageMap};
use super::{SkillEntry, SkillRegistry};

/// One registered skill's usage standing, resolved against the registry so
/// skills that have never been invoked still appear (with `count: 0`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillUsageStats {
    pub skill_id: String,
    pub count: u64,
    /// `None` when the skill has no usage entry at all (never invoked).
    pub last_used: Option<DateTime<Utc>>,
}

/// Ranked usage report over a [`SkillRegistry`] snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SkillUsageReport {
    /// Every registry-visible skill's stats, sorted by `count` descending
    /// (ties broken by `skill_id` for a deterministic order). This is the
    /// full ranking that `top` and `dead` are both derived from.
    // TODO(skill-consolidation): consolidation ranking reads `ranked` to
    // pick which skill in a near-duplicate merge group keeps its slot —
    // higher usage wins.
    pub ranked: Vec<SkillUsageStats>,
    /// The head of `ranked`, bounded by the `top_n` passed to [`rank`].
    pub top: Vec<SkillUsageStats>,
    /// Skills considered dead: zero invocations ever, or last invoked before
    /// the `dead_after` cutoff passed to [`rank`].
    // TODO(skill-retirement): retirement sets `disable_model_invocation =
    // true` for every skill named here once a grace-period / confirmation
    // policy lands.
    pub dead: Vec<SkillUsageStats>,
}

/// Build a [`SkillUsageReport`] from an already-loaded usage map.
///
/// Pure and synchronous so callers (including tests) can supply a fixed
/// `now` and a hand-built [`UsageMap`] without touching the filesystem.
/// Skills with a [`SkillEntry::Err`] load failure are excluded — they have
/// no stable identity to attach usage to.
pub fn rank(
    registry: &SkillRegistry,
    usage: &UsageMap,
    now: DateTime<Utc>,
    top_n: usize,
    dead_after: Duration,
) -> SkillUsageReport {
    let mut ranked: Vec<SkillUsageStats> = registry
        .all_visible()
        .filter(|(_, entry)| matches!(entry, SkillEntry::Ok(_)))
        .map(|(name, _)| {
            let entry = usage.get(name);
            SkillUsageStats {
                skill_id: name.to_string(),
                count: entry.map(|e| e.count).unwrap_or(0),
                last_used: entry.map(|e| e.last_used),
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.skill_id.cmp(&b.skill_id))
    });

    let top = ranked.iter().take(top_n).cloned().collect();

    let dead = ranked
        .iter()
        .filter(|s| {
            s.count == 0
                || s.last_used
                    .is_some_and(|last| now.signed_duration_since(last) >= dead_after)
        })
        .cloned()
        .collect();

    SkillUsageReport { ranked, top, dead }
}

/// Load the usage sidecar at `usage_dir` and build a report against `registry`.
///
/// This is the entry point callers reach for in practice — it wraps
/// [`usage::load`] plus [`rank`] with the current time. Use [`rank`] directly
/// in tests or when a usage snapshot is already in hand.
pub async fn build_report(
    registry: &SkillRegistry,
    usage_dir: &Path,
    top_n: usize,
    dead_after: Duration,
) -> SkillUsageReport {
    let usage = usage::load(usage_dir).await;
    rank(registry, &usage, Utc::now(), top_n, dead_after)
}

/// Render a report as a plain-text listing — a headless debug/report entry
/// point that a future CLI command or admin route can print or log directly
/// without re-deriving the ranking.
pub fn format_report(report: &SkillUsageReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("top {} skills by usage:\n", report.top.len()));
    for stats in &report.top {
        out.push_str(&format!(
            "  {:<32} count={:<6} last_used={}\n",
            stats.skill_id,
            stats.count,
            stats
                .last_used
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "never".to_string())
        ));
    }
    out.push_str(&format!("dead skills ({}):\n", report.dead.len()));
    for stats in &report.dead {
        out.push_str(&format!(
            "  {:<32} count={:<6} last_used={}\n",
            stats.skill_id,
            stats.count,
            stats
                .last_used
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "never".to_string())
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_registry::usage::SkillUsageEntry;
    use crate::skill_registry::{ContextMode, SkillProvenance, SkillRecord, SkillSource};

    fn stub_record(name: &str) -> SkillRecord {
        SkillRecord {
            name: name.to_string(),
            description: "stub".to_string(),
            context: ContextMode::Inline,
            agent: None,
            allowed_tools: vec![],
            arguments: vec![],
            body: String::new(),
            source: SkillSource::User,
            when_to_use: None,
            model: None,
            disable_model_invocation: false,
            provenance: SkillProvenance::UserAuthored,
            retired: false,
            retired_reason: None,
            superseded_by: None,
            distilled_from: vec![],
            version: 1,
        }
    }

    fn registry_with(names: &[&str]) -> SkillRegistry {
        let mut registry = SkillRegistry::empty();
        for name in names {
            registry.insert(name.to_string(), SkillEntry::Ok(stub_record(name)));
        }
        registry
    }

    #[test]
    fn used_skill_ranks_above_dead_skill() {
        let registry = registry_with(&["active-skill", "dead-skill"]);
        let now = Utc::now();

        let mut usage = UsageMap::new();
        usage.insert(
            "active-skill".to_string(),
            SkillUsageEntry {
                count: 5,
                last_used: now,
            },
        );

        let report = rank(&registry, &usage, now, 10, Duration::days(30));

        assert_eq!(report.ranked.len(), 2);
        assert_eq!(report.ranked[0].skill_id, "active-skill");
        assert_eq!(report.ranked[0].count, 5);
        assert_eq!(report.ranked[1].skill_id, "dead-skill");
        assert_eq!(report.ranked[1].count, 0);

        assert_eq!(report.top.len(), 2);
        assert_eq!(report.top[0].skill_id, "active-skill");

        assert_eq!(report.dead.len(), 1);
        assert_eq!(report.dead[0].skill_id, "dead-skill");
    }

    #[test]
    fn top_n_bounds_the_top_slice_without_truncating_ranked() {
        let registry = registry_with(&["a", "b", "c"]);
        let now = Utc::now();

        let mut usage = UsageMap::new();
        usage.insert(
            "a".to_string(),
            SkillUsageEntry {
                count: 3,
                last_used: now,
            },
        );
        usage.insert(
            "b".to_string(),
            SkillUsageEntry {
                count: 2,
                last_used: now,
            },
        );
        usage.insert(
            "c".to_string(),
            SkillUsageEntry {
                count: 1,
                last_used: now,
            },
        );

        let report = rank(&registry, &usage, now, 2, Duration::days(30));

        assert_eq!(
            report.ranked.len(),
            3,
            "ranked keeps every registered skill"
        );
        assert_eq!(report.top.len(), 2, "top is bounded by top_n");
        assert_eq!(report.top[0].skill_id, "a");
        assert_eq!(report.top[1].skill_id, "b");
    }

    #[test]
    fn stale_last_used_counts_as_dead_even_with_nonzero_count() {
        let registry = registry_with(&["stale-skill"]);
        let now = Utc::now();
        let ninety_days_ago = now - Duration::days(90);

        let mut usage = UsageMap::new();
        usage.insert(
            "stale-skill".to_string(),
            SkillUsageEntry {
                count: 12,
                last_used: ninety_days_ago,
            },
        );

        let report = rank(&registry, &usage, now, 10, Duration::days(30));

        assert_eq!(report.dead.len(), 1);
        assert_eq!(report.dead[0].skill_id, "stale-skill");
    }

    #[test]
    fn recently_used_skill_is_not_dead() {
        let registry = registry_with(&["fresh-skill"]);
        let now = Utc::now();
        let one_day_ago = now - Duration::days(1);

        let mut usage = UsageMap::new();
        usage.insert(
            "fresh-skill".to_string(),
            SkillUsageEntry {
                count: 4,
                last_used: one_day_ago,
            },
        );

        let report = rank(&registry, &usage, now, 10, Duration::days(30));

        assert!(report.dead.is_empty());
    }

    #[test]
    fn load_error_entries_are_excluded_from_the_report() {
        let mut registry = registry_with(&["good-skill"]);
        registry.insert(
            "broken-skill".to_string(),
            SkillEntry::Err("boom".to_string()),
        );

        let report = rank(
            &registry,
            &UsageMap::new(),
            Utc::now(),
            10,
            Duration::days(30),
        );

        assert_eq!(report.ranked.len(), 1);
        assert_eq!(report.ranked[0].skill_id, "good-skill");
    }

    #[test]
    fn ties_break_by_skill_id_for_determinism() {
        let registry = registry_with(&["zebra", "alpha"]);
        let now = Utc::now();

        let mut usage = UsageMap::new();
        usage.insert(
            "zebra".to_string(),
            SkillUsageEntry {
                count: 1,
                last_used: now,
            },
        );
        usage.insert(
            "alpha".to_string(),
            SkillUsageEntry {
                count: 1,
                last_used: now,
            },
        );

        let report = rank(&registry, &usage, now, 10, Duration::days(30));

        assert_eq!(report.ranked[0].skill_id, "alpha");
        assert_eq!(report.ranked[1].skill_id, "zebra");
    }

    #[tokio::test]
    async fn build_report_reads_the_usage_sidecar_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = registry_with(&["disk-skill"]);

        usage::increment(tmp.path(), "disk-skill").await.unwrap();

        let report = build_report(&registry, tmp.path(), 10, Duration::days(30)).await;

        assert_eq!(report.ranked.len(), 1);
        assert_eq!(report.ranked[0].skill_id, "disk-skill");
        assert_eq!(report.ranked[0].count, 1);
        assert!(
            report.dead.is_empty(),
            "just-incremented skill should not be dead"
        );
    }

    #[test]
    fn format_report_lists_top_and_dead_sections() {
        let registry = registry_with(&["active-skill", "dead-skill"]);
        let now = Utc::now();

        let mut usage = UsageMap::new();
        usage.insert(
            "active-skill".to_string(),
            SkillUsageEntry {
                count: 5,
                last_used: now,
            },
        );

        let report = rank(&registry, &usage, now, 10, Duration::days(30));
        let text = format_report(&report);

        assert!(text.contains("top 2 skills by usage"));
        assert!(text.contains("active-skill"));
        assert!(text.contains("dead skills (1)"));
        assert!(text.contains("dead-skill"));
    }
}
