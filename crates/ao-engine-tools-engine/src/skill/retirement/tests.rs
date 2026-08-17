use std::path::Path;

use ao_engine_tools_core::skill_registry::usage::{SkillUsageEntry, UsageMap};
use ao_engine_tools_core::skill_registry::{
    parse_frontmatter, ContextMode, SkillEntry, SkillProvenance, SkillRecord, SkillRegistry, SkillSource,
};
use chrono::{Duration, Utc};

use super::*;

#[allow(clippy::too_many_arguments)]
fn record(
    name: &str,
    description: &str,
    body: &str,
    source: SkillSource,
    provenance: SkillProvenance,
    retired: bool,
) -> SkillRecord {
    SkillRecord {
        name: name.to_string(),
        description: description.to_string(),
        context: ContextMode::Inline,
        agent: None,
        allowed_tools: vec![],
        arguments: vec![],
        body: body.to_string(),
        source,
        when_to_use: None,
        model: None,
        disable_model_invocation: retired,
        provenance,
        retired,
        retired_reason: if retired { Some("unused".to_string()) } else { None },
        superseded_by: None,
        distilled_from: vec![],
        version: 1,
    }
}

fn distilled_user_record(name: &str, description: &str, body: &str) -> SkillRecord {
    record(name, description, body, SkillSource::User, SkillProvenance::Distilled, false)
}

fn user_authored_record(name: &str, description: &str, body: &str) -> SkillRecord {
    record(name, description, body, SkillSource::User, SkillProvenance::UserAuthored, false)
}

fn plugin_record(name: &str, description: &str, body: &str) -> SkillRecord {
    record(
        name,
        description,
        body,
        SkillSource::Plugin { plugin_name: "ops-pack".to_string() },
        SkillProvenance::UserAuthored,
        false,
    )
}

fn registry_with(records: Vec<(&str, SkillRecord)>) -> SkillRegistry {
    let mut registry = SkillRegistry::empty();
    for (name, record) in records {
        registry.insert(name.to_string(), SkillEntry::Ok(record));
    }
    registry
}

fn skill_md(name: &str, description: &str, body: &str, distilled: bool, retired: bool) -> String {
    let mut extra = String::new();
    if distilled {
        extra.push_str("origin: distilled\n");
    }
    if retired {
        extra.push_str("disable-model-invocation: true\n");
        extra.push_str("retired: true\n");
        extra.push_str("retired-reason: unused\n");
    }
    format!("---\nname: {name}\ndescription: {description}\n{extra}---\n{body}\n")
}

fn write_skill_md(dir: &Path, name: &str, content: &str) {
    let skill_dir = dir.join("skills").join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
}

// ─── sweep ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn sweep_retires_a_dead_distilled_user_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let content = skill_md("stale-helper", "Helps with a stale task", "do stale things", true, false);
    write_skill_md(tmp.path(), "stale-helper", &content);

    let registry = registry_with(vec![(
        "stale-helper",
        distilled_user_record("stale-helper", "Helps with a stale task", "do stale things"),
    )]);
    let usage = UsageMap::new(); // never invoked -> count 0 -> dead regardless of dead_after

    let now = Utc::now();
    let outcome = sweep(tmp.path(), &registry, &usage, now, Duration::days(DEFAULT_DEAD_AFTER_DAYS)).await;

    assert_eq!(outcome.retired, vec!["stale-helper".to_string()]);
    assert!(outcome.staged_for_review.is_empty());
    assert!(outcome.failed.is_empty(), "unexpected failures: {:?}", outcome.failed);

    let after = std::fs::read_to_string(tmp.path().join("skills/stale-helper/SKILL.md")).unwrap();
    let parsed = parse_frontmatter(&after).unwrap();
    assert!(parsed.disable_model_invocation, "retired skill must be quarantined");
    assert!(parsed.retired, "retired skill must be tombstoned");
    assert_eq!(parsed.retired_reason.as_deref(), Some("unused"));
    assert_eq!(parsed.superseded_by, None, "usage-based retirement supersedes nothing");
}

#[tokio::test]
async fn sweep_never_retires_a_user_authored_dead_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let content = skill_md("hand-written", "A human wrote this", "do the thing", false, false);
    write_skill_md(tmp.path(), "hand-written", &content);

    let registry = registry_with(vec![(
        "hand-written",
        user_authored_record("hand-written", "A human wrote this", "do the thing"),
    )]);
    let usage = UsageMap::new();

    let now = Utc::now();
    let outcome = sweep(tmp.path(), &registry, &usage, now, Duration::days(DEFAULT_DEAD_AFTER_DAYS)).await;

    assert!(outcome.retired.is_empty(), "must never auto-retire a user-authored skill");
    assert_eq!(outcome.staged_for_review, vec!["hand-written".to_string()]);
    assert!(outcome.failed.is_empty());

    let after = std::fs::read_to_string(tmp.path().join("skills/hand-written/SKILL.md")).unwrap();
    assert_eq!(after, content, "hard invariant: file must be byte-for-byte unchanged");
}

#[tokio::test]
async fn sweep_never_retires_a_plugin_sourced_dead_skill() {
    let tmp = tempfile::tempdir().unwrap();
    // Deliberately no file written under skills/ — a plugin-sourced skill
    // has no user-pool file at all, so sweep must never even attempt one.
    let registry = registry_with(vec![("plugin-skill", plugin_record("plugin-skill", "desc", "body"))]);
    let usage = UsageMap::new();

    let now = Utc::now();
    let outcome = sweep(tmp.path(), &registry, &usage, now, Duration::days(DEFAULT_DEAD_AFTER_DAYS)).await;

    assert!(outcome.retired.is_empty());
    assert_eq!(outcome.staged_for_review, vec!["plugin-skill".to_string()]);
    assert!(outcome.failed.is_empty(), "must never even attempt (and fail) a write for a plugin skill");
}

#[tokio::test]
async fn sweep_skips_a_skill_already_retired_by_a_prior_sweep() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = record("already-retired", "desc", "body", SkillSource::User, SkillProvenance::Distilled, true);
    let registry = registry_with(vec![("already-retired", stub)]);
    let usage = UsageMap::new();

    let now = Utc::now();
    let outcome = sweep(tmp.path(), &registry, &usage, now, Duration::days(DEFAULT_DEAD_AFTER_DAYS)).await;

    assert!(outcome.retired.is_empty(), "an already-retired skill is a no-op, not re-retired");
    assert!(outcome.staged_for_review.is_empty());
    assert!(outcome.failed.is_empty());
}

#[tokio::test]
async fn sweep_does_not_retire_a_recently_used_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let registry =
        registry_with(vec![("fresh-skill", distilled_user_record("fresh-skill", "desc", "body"))]);
    let now = Utc::now();
    let mut usage = UsageMap::new();
    usage.insert("fresh-skill".to_string(), SkillUsageEntry { count: 5, last_used: now - Duration::days(1) });

    let outcome = sweep(tmp.path(), &registry, &usage, now, Duration::days(DEFAULT_DEAD_AFTER_DAYS)).await;

    assert!(outcome.retired.is_empty());
    assert!(outcome.staged_for_review.is_empty());
}

// ─── reactivate ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn reactivate_clears_the_tombstone_and_reenables_the_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let content = skill_md("stale-helper", "desc", "body", true, true);
    write_skill_md(tmp.path(), "stale-helper", &content);

    let stub = record("stale-helper", "desc", "body", SkillSource::User, SkillProvenance::Distilled, true);
    let registry = registry_with(vec![("stale-helper", stub)]);

    reactivate(tmp.path(), &registry, "stale-helper").await.unwrap();

    let after = std::fs::read_to_string(tmp.path().join("skills/stale-helper/SKILL.md")).unwrap();
    let parsed = parse_frontmatter(&after).unwrap();
    assert!(!parsed.disable_model_invocation, "reactivation must re-enable model invocation");
    assert!(!parsed.retired, "reactivation must clear the tombstone");
    assert_eq!(parsed.retired_reason, None);
    assert_eq!(parsed.superseded_by, None);
    assert_eq!(parsed.name, "stale-helper", "unrelated frontmatter must survive reactivation");
}

#[tokio::test]
async fn reactivate_errors_when_skill_is_not_retired() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = registry_with(vec![("live-skill", distilled_user_record("live-skill", "desc", "body"))]);

    let err = reactivate(tmp.path(), &registry, "live-skill").await.unwrap_err();
    assert!(matches!(err, ReactivateError::NotRetired));
}

#[tokio::test]
async fn reactivate_errors_for_a_plugin_sourced_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = record(
        "plugin-skill",
        "desc",
        "body",
        SkillSource::Plugin { plugin_name: "ops-pack".to_string() },
        SkillProvenance::UserAuthored,
        true,
    );
    let registry = registry_with(vec![("plugin-skill", stub)]);

    let err = reactivate(tmp.path(), &registry, "plugin-skill").await.unwrap_err();
    assert!(matches!(err, ReactivateError::NotUserSkill));
}

#[tokio::test]
async fn reactivate_errors_when_skill_missing_from_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::empty();
    let err = reactivate(tmp.path(), &registry, "ghost").await.unwrap_err();
    assert!(matches!(err, ReactivateError::NotUserSkill));
}
