use std::path::Path;

use ao_engine_tools_core::skill_registry::usage::{SkillUsageEntry, UsageMap};
use ao_engine_tools_core::skill_registry::{
    parse_frontmatter, reindex_skills, ContextMode, SkillEntry, SkillProvenance, SkillRecord,
    SkillRegistry, SkillSource,
};
use ao_search_index::SearchIndex;
use chrono::Utc;

use super::*;

fn distilled_record(name: &str, description: &str, body: &str) -> SkillRecord {
    SkillRecord {
        name: name.to_string(),
        description: description.to_string(),
        context: ContextMode::Inline,
        agent: None,
        allowed_tools: vec![],
        arguments: vec![],
        body: body.to_string(),
        source: SkillSource::User,
        when_to_use: None,
        model: None,
        disable_model_invocation: false,
        provenance: SkillProvenance::Distilled,
        retired: false,
        retired_reason: None,
        superseded_by: None,
        distilled_from: vec![],
        version: 1,
    }
}

fn user_authored_record(name: &str, description: &str, body: &str) -> SkillRecord {
    SkillRecord { provenance: SkillProvenance::UserAuthored, ..distilled_record(name, description, body) }
}

fn registry_with(records: Vec<(&str, SkillRecord)>) -> SkillRegistry {
    let mut registry = SkillRegistry::empty();
    for (name, record) in records {
        registry.insert(name.to_string(), SkillEntry::Ok(record));
    }
    registry
}

fn skill_md(name: &str, description: &str, body: &str, distilled: bool) -> String {
    let origin_line = if distilled { "origin: distilled\n" } else { "" };
    format!("---\nname: {name}\ndescription: {description}\n{origin_line}---\n{body}\n")
}

fn write_skill_md(dir: &Path, name: &str, content: &str) {
    let skill_dir = dir.join("skills").join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
}

// ─── find_near_duplicates ───────────────────────────────────────────────────

#[tokio::test]
async fn find_near_duplicates_detects_only_true_near_dups() {
    let deploy_helper = distilled_record(
        "deploy-helper",
        "Deploys the service to production servers",
        "Run the deploy script and verify health checks pass reliably every time",
    );
    let deploy_assistant = distilled_record(
        "deploy-assistant",
        "Deploys the service to production servers quickly",
        "Run the deploy script and verify health checks pass reliably every single time",
    );
    let invoice_formatter = distilled_record(
        "invoice-formatter",
        "Formats customer invoices as PDF documents",
        "Convert invoice line items into a nicely formatted PDF report for customers",
    );

    let registry = registry_with(vec![
        ("deploy-helper", deploy_helper),
        ("deploy-assistant", deploy_assistant),
        ("invoice-formatter", invoice_formatter),
    ]);

    let index = SearchIndex::open_in_memory().unwrap();
    reindex_skills(&index, &registry).await.unwrap();

    let pairs = find_near_duplicates(&registry, &index).await;
    assert_eq!(pairs.len(), 1, "only the deploy-* pair should be flagged: {:?}", pairs);
    let pair = &pairs[0];
    let names = [pair.a.as_str(), pair.b.as_str()];
    assert!(names.contains(&"deploy-helper"));
    assert!(names.contains(&"deploy-assistant"));
    assert!(pair.similarity >= DUPLICATE_THRESHOLD);
}

#[tokio::test]
async fn find_near_duplicates_excludes_non_distilled_skills() {
    let deploy_helper = distilled_record(
        "deploy-helper",
        "Deploys the service to production servers",
        "Run the deploy script and verify health checks pass reliably every time",
    );
    // Near-identical text, but never marked `origin: distilled` — must never
    // be proposed for auto-merge no matter how similar it looks.
    let deploy_assistant = user_authored_record(
        "deploy-assistant",
        "Deploys the service to production servers quickly",
        "Run the deploy script and verify health checks pass reliably every single time",
    );

    let registry = registry_with(vec![("deploy-helper", deploy_helper), ("deploy-assistant", deploy_assistant)]);
    let index = SearchIndex::open_in_memory().unwrap();
    reindex_skills(&index, &registry).await.unwrap();

    let pairs = find_near_duplicates(&registry, &index).await;
    assert!(
        pairs.is_empty(),
        "a user-authored skill must never be proposed for auto-consolidation, even when near-identical: {:?}",
        pairs
    );
}

#[tokio::test]
async fn find_near_duplicates_excludes_plugin_sourced_skills() {
    let mut plugin_twin = distilled_record(
        "deploy-helper-plugin",
        "Deploys the service to production servers",
        "Run the deploy script and verify health checks pass reliably every time",
    );
    plugin_twin.source = SkillSource::Plugin { plugin_name: "ops-pack".to_string() };
    plugin_twin.provenance = SkillProvenance::UserAuthored; // plugins never carry the distilled marker

    let deploy_helper = distilled_record(
        "deploy-helper",
        "Deploys the service to production servers",
        "Run the deploy script and verify health checks pass reliably every time",
    );

    let registry = registry_with(vec![("deploy-helper", deploy_helper), ("deploy-helper-plugin", plugin_twin)]);
    let index = SearchIndex::open_in_memory().unwrap();
    reindex_skills(&index, &registry).await.unwrap();

    let pairs = find_near_duplicates(&registry, &index).await;
    assert!(pairs.is_empty(), "a plugin-sourced skill has no write path here and must never be a candidate");
}

// ─── plan_consolidation ─────────────────────────────────────────────────────

#[test]
fn plan_consolidation_keeps_higher_usage_skill() {
    let pair =
        DuplicatePair { a: "deploy-helper".to_string(), b: "deploy-assistant".to_string(), similarity: 0.8 };
    let mut usage = UsageMap::new();
    usage.insert("deploy-helper".to_string(), SkillUsageEntry { count: 5, last_used: Utc::now() });
    usage.insert("deploy-assistant".to_string(), SkillUsageEntry { count: 2, last_used: Utc::now() });

    let decisions = plan_consolidation(&[pair], &usage);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].keep, "deploy-helper");
    assert_eq!(decisions[0].supersede, "deploy-assistant");
}

#[test]
fn plan_consolidation_lower_usage_side_is_superseded_regardless_of_pair_order() {
    let pair =
        DuplicatePair { a: "deploy-assistant".to_string(), b: "deploy-helper".to_string(), similarity: 0.8 };
    let mut usage = UsageMap::new();
    usage.insert("deploy-helper".to_string(), SkillUsageEntry { count: 5, last_used: Utc::now() });
    usage.insert("deploy-assistant".to_string(), SkillUsageEntry { count: 2, last_used: Utc::now() });

    let decisions = plan_consolidation(&[pair], &usage);
    assert_eq!(decisions[0].keep, "deploy-helper");
    assert_eq!(decisions[0].supersede, "deploy-assistant");
}

#[test]
fn plan_consolidation_ties_break_lexicographically() {
    let pair = DuplicatePair { a: "zeta".to_string(), b: "alpha".to_string(), similarity: 0.8 };
    let decisions = plan_consolidation(&[pair], &UsageMap::new());
    assert_eq!(decisions[0].keep, "alpha");
    assert_eq!(decisions[0].supersede, "zeta");
}

// ─── apply ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn apply_retires_the_loser_and_bumps_the_winners_version() {
    let tmp = tempfile::tempdir().unwrap();
    let keep_content = skill_md("deploy-helper", "Deploys the service", "do it", true);
    let supersede_content = skill_md("deploy-assistant", "Deploys the service too", "do it too", true);
    write_skill_md(tmp.path(), "deploy-helper", &keep_content);
    write_skill_md(tmp.path(), "deploy-assistant", &supersede_content);

    let registry = registry_with(vec![
        ("deploy-helper", distilled_record("deploy-helper", "Deploys the service", "do it")),
        ("deploy-assistant", distilled_record("deploy-assistant", "Deploys the service too", "do it too")),
    ]);

    let decision = ConsolidationDecision {
        keep: "deploy-helper".to_string(),
        supersede: "deploy-assistant".to_string(),
        similarity: 0.9,
    };
    let outcome = apply(tmp.path(), &registry, &[decision.clone()]).await;

    assert_eq!(outcome.applied, vec![decision]);
    assert!(outcome.skipped.is_empty(), "unexpected skips: {:?}", outcome.skipped);

    let loser_content = std::fs::read_to_string(tmp.path().join("skills/deploy-assistant/SKILL.md")).unwrap();
    let parsed = parse_frontmatter(&loser_content).unwrap();
    assert!(parsed.disable_model_invocation, "loser must be quarantined");
    assert!(parsed.retired, "loser must be tombstoned");
    assert_eq!(parsed.retired_reason.as_deref(), Some("consolidated"));
    assert_eq!(parsed.superseded_by.as_deref(), Some("deploy-helper"));

    // The winner absorbed a duplicate's procedure, so its version
    // advances by 1 even though its name/description/body are untouched.
    let winner_content = std::fs::read_to_string(tmp.path().join("skills/deploy-helper/SKILL.md")).unwrap();
    let winner_parsed = parse_frontmatter(&winner_content).unwrap();
    assert_eq!(winner_parsed.version, 2, "winner's version must advance after absorbing a merge");
    assert_eq!(winner_parsed.name, "deploy-helper");
    assert_eq!(winner_parsed.description, "Deploys the service");
    assert_eq!(winner_parsed.body, "do it\n");
}

#[tokio::test]
async fn apply_never_touches_a_user_authored_skill_even_if_a_decision_names_one() {
    let tmp = tempfile::tempdir().unwrap();
    let user_content = skill_md("hand-written", "A skill a human wrote", "do the thing", false);
    write_skill_md(tmp.path(), "hand-written", &user_content);
    write_skill_md(
        tmp.path(),
        "distilled-winner",
        &skill_md("distilled-winner", "distilled winner", "body", true),
    );

    let registry = registry_with(vec![
        ("hand-written", user_authored_record("hand-written", "A skill a human wrote", "do the thing")),
        ("distilled-winner", distilled_record("distilled-winner", "distilled winner", "body")),
    ]);

    // A decision that (incorrectly) names a user-authored skill as the
    // supersede target — e.g. built from a stale registry snapshot. `apply`
    // must independently re-check the hard invariant, not just trust the
    // decision it was handed.
    let decision = ConsolidationDecision {
        keep: "distilled-winner".to_string(),
        supersede: "hand-written".to_string(),
        similarity: 0.99,
    };
    let outcome = apply(tmp.path(), &registry, &[decision.clone()]).await;

    assert!(outcome.applied.is_empty(), "must never auto-consolidate a user-authored skill");
    assert_eq!(outcome.skipped.len(), 1);
    assert_eq!(outcome.skipped[0].0, decision);

    let after = std::fs::read_to_string(tmp.path().join("skills/hand-written/SKILL.md")).unwrap();
    assert_eq!(
        after, user_content,
        "hard invariant: a user-authored skill's file must be byte-for-byte unchanged"
    );
}

#[tokio::test]
async fn apply_skips_a_decision_whose_supersede_target_is_missing_from_the_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = registry_with(vec![("only-skill", distilled_record("only-skill", "desc", "body"))]);

    let decision = ConsolidationDecision {
        keep: "only-skill".to_string(),
        supersede: "ghost-skill".to_string(),
        similarity: 0.9,
    };
    let outcome = apply(tmp.path(), &registry, &[decision.clone()]).await;

    assert!(outcome.applied.is_empty());
    assert_eq!(outcome.skipped.len(), 1);
    assert_eq!(outcome.skipped[0].0, decision);
}
