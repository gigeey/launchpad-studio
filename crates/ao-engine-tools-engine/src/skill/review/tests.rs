use std::path::Path;

use ao_engine_tools_core::skill_registry::{
    parse_frontmatter, ContextMode, SkillEntry, SkillProvenance, SkillRecord, SkillRegistry, SkillSource,
};
use ao_persistence::paths::DataRoot;
use ao_persistence::reflection_staging::ReflectionStagingStore;
use ao_protocol::error::AoError;
use ao_protocol::memory::MemoryScope;
use ao_protocol::outcome::ArtifactKind;
use ao_protocol::reflection_candidate::{ReflectionCandidate, ReflectionCandidateStatus};
use chrono::Utc;

use super::*;

#[allow(clippy::too_many_arguments)]
fn record(
    name: &str,
    description: &str,
    body: &str,
    source: SkillSource,
    provenance: SkillProvenance,
    disable_model_invocation: bool,
    retired: bool,
    distilled_from: Vec<String>,
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
        disable_model_invocation,
        provenance,
        retired,
        retired_reason: if retired { Some("unused".to_string()) } else { None },
        superseded_by: None,
        distilled_from,
        version: 1,
    }
}

fn parked_distilled_record(name: &str, description: &str, body: &str) -> SkillRecord {
    record(
        name,
        description,
        body,
        SkillSource::User,
        SkillProvenance::Distilled,
        true,
        false,
        vec!["cand-1".to_string()],
    )
}

fn registry_with(records: Vec<(&str, SkillRecord)>) -> SkillRegistry {
    let mut registry = SkillRegistry::empty();
    for (name, record) in records {
        registry.insert(name.to_string(), SkillEntry::Ok(record));
    }
    registry
}

fn parked_skill_md(name: &str, description: &str, body: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {description}\ndisable-model-invocation: true\norigin: distilled\ndistilled-from:\n  - cand-1\n---\n{body}\n"
    )
}

fn write_skill_md(dir: &Path, name: &str, content: &str) {
    let skill_dir = dir.join("skills").join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
}

fn make_candidate(id: &str, agent_id: &str, kind: ArtifactKind, status: ReflectionCandidateStatus) -> ReflectionCandidate {
    ReflectionCandidate {
        id: id.to_string(),
        kind,
        agent_id: agent_id.to_string(),
        source_thread_id: "thread-1".to_string(),
        content: "observed procedure".to_string(),
        status,
        target_scope: MemoryScope::Agent,
        target_scope_key: Some(agent_id.to_string()),
        contradicts: None,
        reason: "test".to_string(),
        created_at: Utc::now(),
    }
}

// ─── list_queue ─────────────────────────────────────────────────────────────

/// Both writers' parked skills must be listed. `hand-written` stands in for
/// `SkillRegister` output — parked, but `UserAuthored` rather than
/// `Distilled`. Listing it is what makes it enableable at all: no other
/// surface clears `disable-model-invocation`.
#[tokio::test]
async fn list_queue_returns_every_parked_user_pool_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let content = parked_skill_md("parked-one", "A parked skill", "do the thing");
    write_skill_md(tmp.path(), "parked-one", &content);

    let live_content = "---\nname: live-one\ndescription: Already live\norigin: distilled\n---\nbody\n";
    write_skill_md(tmp.path(), "live-one", live_content);

    let retired_content =
        "---\nname: retired-one\ndescription: Retired\ndisable-model-invocation: true\norigin: distilled\nretired: true\n---\nbody\n";
    write_skill_md(tmp.path(), "retired-one", retired_content);

    let user_authored_content =
        "---\nname: hand-written\ndescription: A human wrote this\ndisable-model-invocation: true\n---\nbody\n";
    write_skill_md(tmp.path(), "hand-written", user_authored_content);

    let registry = registry_with(vec![
        ("parked-one", parked_distilled_record("parked-one", "A parked skill", "do the thing")),
        (
            "live-one",
            record("live-one", "Already live", "body", SkillSource::User, SkillProvenance::Distilled, false, false, vec![]),
        ),
        (
            "retired-one",
            record("retired-one", "Retired", "body", SkillSource::User, SkillProvenance::Distilled, true, true, vec![]),
        ),
        (
            "hand-written",
            record(
                "hand-written",
                "A human wrote this",
                "body",
                SkillSource::User,
                SkillProvenance::UserAuthored,
                true,
                false,
                vec![],
            ),
        ),
    ]);

    let data_root = DataRoot::new(tmp.path());
    let staging = ReflectionStagingStore::new(data_root);
    staging
        .stage("agent-1", &make_candidate("cand-skill", "agent-1", ArtifactKind::Skill, ReflectionCandidateStatus::Pending))
        .await
        .unwrap();
    staging
        .stage("agent-1", &make_candidate("cand-memory", "agent-1", ArtifactKind::Memory, ReflectionCandidateStatus::Pending))
        .await
        .unwrap();
    staging
        .stage(
            "agent-1",
            &make_candidate("cand-distilled", "agent-1", ArtifactKind::Skill, ReflectionCandidateStatus::Distilled),
        )
        .await
        .unwrap();

    let queue = list_queue(tmp.path(), &registry, &staging, "agent-1").await.unwrap();

    assert_eq!(
        queue.candidates.len(),
        2,
        "both parked non-retired user-pool skills qualify, whatever their provenance"
    );

    let distilled = queue.candidates.iter().find(|c| c.name == "parked-one").expect("parked-one");
    assert_eq!(distilled.origin, "distilled");
    assert_eq!(distilled.distilled_from, vec!["cand-1".to_string()]);

    let authored = queue.candidates.iter().find(|c| c.name == "hand-written").expect("hand-written");
    assert_eq!(authored.origin, "user_authored", "origin must reflect the record, not a constant");
    assert!(authored.distilled_from.is_empty());

    // The live and retired skills are still excluded — relaxing the
    // provenance check must not relax either of those.
    assert!(!queue.candidates.iter().any(|c| c.name == "live-one"));
    assert!(!queue.candidates.iter().any(|c| c.name == "retired-one"));

    assert_eq!(queue.observations.len(), 1, "only the pending Skill-kind candidate qualifies");
    assert_eq!(queue.observations[0].id, "cand-skill");
}

// ─── accept ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn accept_clears_disable_model_invocation() {
    let tmp = tempfile::tempdir().unwrap();
    let content = parked_skill_md("parked-one", "desc", "body");
    write_skill_md(tmp.path(), "parked-one", &content);
    let registry = registry_with(vec![("parked-one", parked_distilled_record("parked-one", "desc", "body"))]);

    accept(tmp.path(), &registry, "parked-one").await.unwrap();

    let after = std::fs::read_to_string(tmp.path().join("skills/parked-one/SKILL.md")).unwrap();
    let parsed = parse_frontmatter(&after).unwrap();
    assert!(!parsed.disable_model_invocation, "accept must make the skill live");
    assert_eq!(parsed.name, "parked-one");
}

/// A skill an agent wrote via `SkillRegister` parks as `UserAuthored`, and
/// `accept` must clear its gate exactly like a distilled one — this is the
/// only path that makes such a skill invocable.
#[tokio::test]
async fn accept_clears_the_gate_on_a_parked_user_authored_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let content =
        "---\nname: hand-written\ndescription: desc\ndisable-model-invocation: true\n---\nbody\n";
    write_skill_md(tmp.path(), "hand-written", content);
    let registry = registry_with(vec![(
        "hand-written",
        record(
            "hand-written",
            "desc",
            "body",
            SkillSource::User,
            SkillProvenance::UserAuthored,
            true,
            false,
            vec![],
        ),
    )]);

    accept(tmp.path(), &registry, "hand-written").await.unwrap();

    let after = std::fs::read_to_string(tmp.path().join("skills/hand-written/SKILL.md")).unwrap();
    assert!(!parse_frontmatter(&after).unwrap().disable_model_invocation);
}

/// "Not parked" means the gate is already clear — an already-live skill is not
/// a review candidate and `accept` must refuse it.
#[tokio::test]
async fn accept_errors_for_a_skill_that_is_not_parked() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = registry_with(vec![(
        "already-live",
        record(
            "already-live",
            "desc",
            "body",
            SkillSource::User,
            SkillProvenance::UserAuthored,
            false,
            false,
            vec![],
        ),
    )]);

    let err = accept(tmp.path(), &registry, "already-live").await.unwrap_err();
    assert!(matches!(err, AoError::ValidationError(_)));
}

#[tokio::test]
async fn accept_errors_when_skill_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::empty();

    let err = accept(tmp.path(), &registry, "ghost").await.unwrap_err();
    assert!(matches!(err, AoError::SkillNotFound(_)));
}

// ─── edit ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn edit_rewrites_body_and_goes_live_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let content = parked_skill_md("parked-one", "old description", "old body");
    write_skill_md(tmp.path(), "parked-one", &content);
    let registry = registry_with(vec![("parked-one", parked_distilled_record("parked-one", "old description", "old body"))]);

    edit(tmp.path(), &registry, "parked-one", "new body", None, false).await.unwrap();

    let after = std::fs::read_to_string(tmp.path().join("skills/parked-one/SKILL.md")).unwrap();
    let parsed = parse_frontmatter(&after).unwrap();
    assert_eq!(parsed.body, "new body");
    assert_eq!(parsed.description, "old description", "description untouched when not given");
    assert!(!parsed.disable_model_invocation, "edit without keep_parked must go live");
}

#[tokio::test]
async fn edit_with_description_updates_both_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let content = parked_skill_md("parked-one", "old description", "old body");
    write_skill_md(tmp.path(), "parked-one", &content);
    let registry = registry_with(vec![("parked-one", parked_distilled_record("parked-one", "old description", "old body"))]);

    edit(tmp.path(), &registry, "parked-one", "new body", Some("new description"), false).await.unwrap();

    let after = std::fs::read_to_string(tmp.path().join("skills/parked-one/SKILL.md")).unwrap();
    let parsed = parse_frontmatter(&after).unwrap();
    assert_eq!(parsed.body, "new body");
    assert_eq!(parsed.description, "new description");
}

#[tokio::test]
async fn edit_with_keep_parked_stays_parked() {
    let tmp = tempfile::tempdir().unwrap();
    let content = parked_skill_md("parked-one", "old description", "old body");
    write_skill_md(tmp.path(), "parked-one", &content);
    let registry = registry_with(vec![("parked-one", parked_distilled_record("parked-one", "old description", "old body"))]);

    edit(tmp.path(), &registry, "parked-one", "new body", None, true).await.unwrap();

    let after = std::fs::read_to_string(tmp.path().join("skills/parked-one/SKILL.md")).unwrap();
    let parsed = parse_frontmatter(&after).unwrap();
    assert_eq!(parsed.body, "new body");
    assert!(parsed.disable_model_invocation, "keep_parked must leave the skill parked");
}

#[tokio::test]
async fn edit_errors_for_a_skill_that_is_not_parked() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::empty();

    let err = edit(tmp.path(), &registry, "ghost", "new body", None, false).await.unwrap_err();
    assert!(matches!(err, AoError::SkillNotFound(_)));
}

// ─── reject ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn reject_deletes_the_parked_skill_file() {
    let tmp = tempfile::tempdir().unwrap();
    let content = parked_skill_md("parked-one", "desc", "body");
    write_skill_md(tmp.path(), "parked-one", &content);
    let registry = registry_with(vec![("parked-one", parked_distilled_record("parked-one", "desc", "body"))]);

    reject(tmp.path(), &registry, "parked-one").await.unwrap();

    assert!(!tmp.path().join("skills/parked-one").exists(), "parked skill directory must be removed");
}

#[tokio::test]
async fn reject_errors_for_a_skill_that_is_not_parked() {
    let tmp = tempfile::tempdir().unwrap();
    let content = "---\nname: live-one\ndescription: desc\norigin: distilled\n---\nbody\n";
    write_skill_md(tmp.path(), "live-one", content);
    let registry = registry_with(vec![(
        "live-one",
        record("live-one", "desc", "body", SkillSource::User, SkillProvenance::Distilled, false, false, vec![]),
    )]);

    let err = reject(tmp.path(), &registry, "live-one").await.unwrap_err();
    assert!(matches!(err, AoError::ValidationError(_)));
    assert!(tmp.path().join("skills/live-one").exists(), "a rejected error must not delete the file");
}

// ─── find_pending_skill_observation ────────────────────────────────────────

#[tokio::test]
async fn find_pending_skill_observation_returns_the_matching_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let staging = ReflectionStagingStore::new(data_root);
    staging
        .stage("agent-1", &make_candidate("cand-1", "agent-1", ArtifactKind::Skill, ReflectionCandidateStatus::Pending))
        .await
        .unwrap();

    let found = find_pending_skill_observation(&staging, "agent-1", "cand-1").await.unwrap();
    assert_eq!(found.id, "cand-1");
}

#[tokio::test]
async fn find_pending_skill_observation_errors_when_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let staging = ReflectionStagingStore::new(data_root);

    let err = find_pending_skill_observation(&staging, "agent-1", "ghost").await.unwrap_err();
    assert!(matches!(err, AoError::MemoryNotFound(_)));
}
