//! Human-facing actions over the parked skill queue — the skill-domain
//! counterpart to [`crate::memory::review`].
//!
//! TWO WRITERS PARK SKILLS HERE, AND BOTH MUST BE LISTED.
//!
//! - The distillation pipeline's `SkillDistiller` (a different crate this
//!   one cannot depend on without a cycle — its caller lives one layer up, in
//!   the same crate that wires `AppState`) writes a generalized template as a
//!   **parked** `SKILL.md` — `disable-model-invocation: true` plus
//!   `origin: distilled` — whether it arrived by automatic repetition
//!   detection or by a human promoting a single observation.
//! - `SkillRegister` writes a skill the agent authored on request, parked by
//!   the same `disable-model-invocation: true` flag but carrying no `origin`
//!   key, so it parses as [`SkillProvenance::UserAuthored`].
//!
//! WHY [`is_parked_for_review`] DOES NOT CHECK PROVENANCE. This queue is the
//! only surface in the product that clears `disable-model-invocation`. A
//! parked skill it does not list can therefore never be enabled by any route:
//! `RunSkill` refuses it, and `patch_skill` only edits profile membership. So
//! the filter must match what parks a skill — the flag — rather than which
//! writer set it, or a whole writer's output is orphaned on disk.
//!
//! Provenance still gates the lifecycle sweeps, which rewrite a skill's
//! frontmatter without asking and must stay distilled-only. Those carry their
//! own independent checks (`consolidation::is_consolidatable`,
//! `retirement::is_retirable`) and are unaffected by this filter.
//!
//! [`crate::memory::review`]'s own module doc explains why a staged `Skill`
//! candidate never goes through the memory review queue: the "not live yet"
//! state already lives entirely in the skill's own frontmatter, and
//! [`SkillRegistry`] is its own listing surface. This module is that
//! surface's human-facing action set: [`list_queue`] (what is waiting),
//! [`accept`]/[`edit`]/[`reject`] (dispose of one parked skill), and
//! [`find_pending_skill_observation`] (look up a still-raw observation a
//! human wants to promote straight to a skill, ahead of the automatic
//! repetition threshold — the actual promotion call is made by the route
//! handler, which is the layer that can see both this module and the
//! distiller).

#[cfg(test)]
mod tests;

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

use ao_engine_tools_core::skill_registry::dispatch::{rewrite_user_skill, SkillRewriteError};
use ao_engine_tools_core::skill_registry::{
    set_body, set_description, set_disable_model_invocation, SkillEntry, SkillProvenance,
    SkillRecord, SkillRegistry, SkillSource,
};
use ao_persistence::reflection_staging::ReflectionStagingStore;
use ao_protocol::error::AoError;
use ao_protocol::outcome::ArtifactKind;
use ao_protocol::reflection_candidate::ReflectionCandidate;

/// One parked skill awaiting human review — from either writer, see the
/// module doc.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParkedSkillCandidate {
    pub name: String,
    pub description: String,
    pub body: String,
    /// `"distilled"` for a distillation-pipeline skill, `"user_authored"` for
    /// one an agent wrote via `SkillRegister`. The UI uses this to explain
    /// where a parked skill came from, so it must reflect the record rather
    /// than assume — the two arrive by different routes and a reviewer
    /// approving them needs to know which they are looking at.
    pub origin: String,
    pub distilled_from: Vec<String>,
    pub created_at: DateTime<Utc>,
}

/// One still-raw `Skill`-kind reflection candidate: an observation that
/// hasn't repeated enough to auto-distill yet, but is eligible for a human
/// to manually promote via [`ao_engine::SkillDistiller::generalize_single`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkillObservation {
    pub id: String,
    pub content: String,
    pub source_thread_id: String,
    pub created_at: DateTime<Utc>,
}

/// Full contents of the skill review queue for one agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SkillReviewQueue {
    pub candidates: Vec<ParkedSkillCandidate>,
    pub observations: Vec<SkillObservation>,
}

/// A skill counts as "parked, awaiting its first review" when it is a
/// user-pool skill that is quarantined and has not yet been either approved
/// (which clears `disable_model_invocation`) or tombstoned by a later
/// lifecycle sweep.
///
/// Deliberately provenance-blind: both writers that park a skill —
/// `SkillDistiller` (`origin: distilled`) and `SkillRegister` (no `origin`
/// key, so `UserAuthored`) — produce something a human must approve before
/// `RunSkill` will dispatch it, and this queue is the only surface that
/// performs that approval.
///
/// `retired` is checked separately from `disable_model_invocation` on purpose
/// — a retired skill also carries `disable_model_invocation: true`, but it
/// already went through review once (it was live before the sweep
/// tombstoned it); surfacing it here would wrongly imply nobody has looked at
/// it yet.
///
/// `source == User` still matters: plugin- and MCP-sourced skills are not ours
/// to rewrite, and [`rewrite_user_skill`] only resolves paths under the user
/// pool.
fn is_parked_for_review(record: &SkillRecord) -> bool {
    record.source == SkillSource::User && record.disable_model_invocation && !record.retired
}

/// Wire label for a parked skill's provenance. Snake-case rather than the
/// `Debug` spelling so the value is stable against a rename of the enum
/// variant, which is serialized straight into the review-queue JSON.
fn provenance_label(provenance: SkillProvenance) -> String {
    match provenance {
        SkillProvenance::Distilled => "distilled".to_string(),
        SkillProvenance::UserAuthored => "user_authored".to_string(),
    }
}

/// Best-effort creation timestamp for a user-pool skill's `SKILL.md`,
/// falling back to the file's modified time (some filesystems don't report
/// birth time) and finally to now if neither is available. `SkillRecord`
/// itself carries no timestamp — it's a parsed frontmatter view of a file
/// this codebase never stamps with `created-at` — so the filesystem is the
/// only source of truth for "when was this parked".
pub fn parked_skill_created_at(data_dir: &Path, name: &str) -> DateTime<Utc> {
    let path = data_dir.join("skills").join(name).join("SKILL.md");
    std::fs::metadata(&path)
        .and_then(|meta| meta.created().or_else(|_| meta.modified()))
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now())
}

fn require_parked<'a>(
    registry: &'a SkillRegistry,
    name: &str,
) -> Result<&'a SkillRecord, AoError> {
    match registry.get(name) {
        Some(SkillEntry::Ok(record)) if is_parked_for_review(record) => Ok(record),
        Some(SkillEntry::Ok(_)) => Err(AoError::ValidationError(format!(
            "skill '{name}' is not parked awaiting review"
        ))),
        _ => Err(AoError::SkillNotFound(format!(
            "no parked skill named '{name}'"
        ))),
    }
}

fn rewrite_err_to_ao(name: &str, e: SkillRewriteError) -> AoError {
    match e {
        SkillRewriteError::NotFound => {
            AoError::SkillNotFound(format!("no parked skill named '{name}'"))
        }
        other => AoError::Internal(format!("failed to rewrite skill '{name}': {other}")),
    }
}

/// List every parked skill plus every still-pending `Skill`
/// observation for `agent_id` — the read side of the review queue.
///
/// `registry` must already be scoped to this agent (i.e. loaded via
/// [`SkillRegistry::load`] against the agent's own profile) — this function
/// never loads a registry of its own, matching the pattern
/// [`crate::skill::consolidation`]/[`crate::skill::retirement`] already use
/// for every sweep that reads a registry snapshot.
pub async fn list_queue(
    data_dir: &Path,
    registry: &SkillRegistry,
    staging: &ReflectionStagingStore,
    agent_id: &str,
) -> Result<SkillReviewQueue, AoError> {
    let candidates = registry
        .all_visible()
        .filter_map(|(name, entry)| match entry {
            SkillEntry::Ok(record) if is_parked_for_review(record) => Some(ParkedSkillCandidate {
                name: name.to_string(),
                description: record.description.clone(),
                body: record.body.clone(),
                origin: provenance_label(record.provenance),
                distilled_from: record.distilled_from.clone(),
                created_at: parked_skill_created_at(data_dir, name),
            }),
            _ => None,
        })
        .collect();

    let observations = staging
        .list_pending(agent_id)
        .await?
        .into_iter()
        .filter(|c| c.kind == ArtifactKind::Skill)
        .map(|c| SkillObservation {
            id: c.id,
            content: c.content,
            source_thread_id: c.source_thread_id,
            created_at: c.created_at,
        })
        .collect();

    Ok(SkillReviewQueue { candidates, observations })
}

/// Approve a parked skill as-is: clear `disable-model-invocation`
/// so it becomes model-invocable. Only ever touches the skill's own
/// frontmatter — a distilled skill is already listed in its authoring
/// agent's `AgentProfile.skills` (written there by
/// [`ao_engine_tools_core::skill_registry::dispatch::write_skill_to_user_pool`]
/// when it was first parked), so there is nothing to add there, and this
/// never shares the skill to any other agent's pool.
pub async fn accept(data_dir: &Path, registry: &SkillRegistry, name: &str) -> Result<(), AoError> {
    require_parked(registry, name)?;
    rewrite_user_skill(data_dir, name, |content| set_disable_model_invocation(content, false))
        .await
        .map_err(|e| rewrite_err_to_ao(name, e))
}

/// Approve a parked skill with human-edited content: rewrite its
/// body (required) and, if given, its description. Clears
/// `disable-model-invocation` too unless `keep_parked` is set, in which case
/// the edit is saved but the skill stays parked for a later `accept`.
pub async fn edit(
    data_dir: &Path,
    registry: &SkillRegistry,
    name: &str,
    body: &str,
    description: Option<&str>,
    keep_parked: bool,
) -> Result<(), AoError> {
    require_parked(registry, name)?;
    rewrite_user_skill(data_dir, name, |content| {
        let mut updated = content.to_string();
        if let Some(desc) = description {
            updated = set_description(&updated, desc)?;
        }
        updated = set_body(&updated, body)?;
        if !keep_parked {
            updated = set_disable_model_invocation(&updated, false)?;
        }
        Ok(updated)
    })
    .await
    .map_err(|e| rewrite_err_to_ao(name, e))
}

/// Reject a parked skill: delete its `SKILL.md` (and directory)
/// from the user pool entirely. The caller is responsible for also removing
/// `name` from the authoring agent's `AgentProfile.skills` — that list lives
/// on the profile the route handler already has loaded, not anything this
/// module owns (mirrors how [`crate::memory::review`] never touches
/// `AgentProfile` either).
pub async fn reject(data_dir: &Path, registry: &SkillRegistry, name: &str) -> Result<(), AoError> {
    require_parked(registry, name)?;
    let skill_dir = data_dir.join("skills").join(name);
    tokio::fs::remove_dir_all(&skill_dir)
        .await
        .map_err(|e| AoError::Internal(format!("failed to delete parked skill '{name}': {e}")))
}

/// Look up a still-staged reflection candidate by id, regardless of status —
/// the human-promote path (`POST .../skills/review/promote`) hands this
/// straight to `SkillDistiller::generalize_single`, which is itself the
/// authority on whether the candidate's kind/status/content make it
/// eligible; duplicating that validation here would just be a second place
/// for the two checks to drift apart.
pub async fn find_pending_skill_observation(
    staging: &ReflectionStagingStore,
    agent_id: &str,
    candidate_id: &str,
) -> Result<ReflectionCandidate, AoError> {
    staging
        .read_all(agent_id)
        .await?
        .into_iter()
        .find(|c| c.id == candidate_id)
        .ok_or_else(|| AoError::MemoryNotFound(format!("staged candidate {candidate_id} not found")))
}
