//! Human-facing actions over the review/undo staging queue.
//!
//! `ao_engine_tools_core::trust_gate::stage_candidate` decides *whether* a
//! candidate memory write goes live immediately, waits for a human, or is
//! hard-blocked; `memory::write` and `ao_engine::reflection_subscriber`
//! persist every `StageForReview` verdict into
//! `ao_persistence::reflection_staging::ReflectionStagingStore` (
//! headless-first — MCP tool calls and the reflection pass are today's only
//! writers). This module is the other half: the five actions a human takes
//! on what has piled up there — `keep`, `edit`, `forget`, `pin` — plus
//! `undo`, which can reverse *any* live agent-authored write, not just ones
//! that passed through this queue (see [`undo`]'s doc for why an
//! `AutoConfirm` write needs the exact same reversal path).
//!
//! Kept deliberately narrow to `ArtifactKind::Memory`: a staged `Skill`
//! candidate already has a durable "not live yet" representation of its own
//! (the skill file's `disable-model-invocation: true` frontmatter, set by
//! the trust gate in `skill::mod`) and its own listing surface
//! (`SkillRegistry`) — duplicating that through this queue would give two
//! sources of truth for the same fact. [`keep`]/[`edit`]/[`forget`]/[`pin`]
//! reject a non-`Memory` candidate rather than silently no-op.

#[cfg(test)]
mod tests;

use ao_persistence::memory::MemoryStore;
use ao_persistence::reflection_staging::ReflectionStagingStore;
use ao_protocol::error::AoError;
use ao_protocol::memory::{MemoryScope, MemorySource};
use ao_protocol::outcome::ArtifactKind;
use ao_protocol::reflection_candidate::{ReflectionCandidate, ReflectionCandidateStatus};
use serde::Serialize;

/// Result of applying (`keep`/`edit`/`pin`) a staged candidate: the live
/// memory entry it became, and what else changed as a side effect.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AppliedCandidate {
    pub candidate_id: String,
    pub memory_id: String,
    /// Id of the existing entry this candidate's `contradicts` field named,
    /// if any — set to `Superseded` (pointing at `memory_id`) as part of
    /// applying this candidate. See [`ReflectionCandidate::contradicts`]'s
    /// doc for why this is always safe once a candidate reaches the queue.
    pub superseded: Option<String>,
    pub pinned: bool,
}

/// Result of [`undo`]: the write that was reversed, and the entry (if any)
/// that write had superseded and which `undo` restored to `Active`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UndoOutcome {
    pub memory_id: String,
    pub restored: Option<String>,
}

/// Look up a still-pending `Memory` candidate by id, or explain why it
/// cannot be acted on. Shared precondition for `keep`/`edit`/`forget`/`pin`.
async fn find_pending_memory_candidate(
    staging: &ReflectionStagingStore,
    agent_id: &str,
    candidate_id: &str,
) -> Result<ReflectionCandidate, AoError> {
    let candidate = staging
        .read_all(agent_id)
        .await?
        .into_iter()
        .find(|c| c.id == candidate_id)
        .ok_or_else(|| {
            AoError::MemoryNotFound(format!("staged candidate {candidate_id} not found"))
        })?;

    if candidate.status != ReflectionCandidateStatus::Pending {
        return Err(AoError::Conflict(format!(
            "staged candidate {candidate_id} is already resolved ({:?})",
            candidate.status
        )));
    }
    if candidate.kind != ArtifactKind::Memory {
        return Err(AoError::ValidationError(format!(
            "staged candidate {candidate_id} is a {:?} candidate; only Memory candidates go \
             through this review queue — a staged Skill is reviewed via its own \
             disable-model-invocation frontmatter flag",
            candidate.kind
        )));
    }
    Ok(candidate)
}

/// Write `content` into `candidate.target_scope`, resolve any contradiction
/// it named, and (if `pinned`) flag the result eviction-exempt. Shared body
/// for [`keep`], [`edit`], and [`pin`] — they differ only in which content
/// string is written, which [`MemorySource`] it is tagged with, and whether
/// `pinned` is set.
async fn apply_candidate(
    store: &MemoryStore,
    candidate: &ReflectionCandidate,
    content: &str,
    source: MemorySource,
    pinned: bool,
) -> Result<AppliedCandidate, AoError> {
    let memory_id = match &candidate.target_scope {
        MemoryScope::Agent => store.add(&candidate.agent_id, content, source).await?.id,
        MemoryScope::Global => store.add_global(content, source).await?.id,
        MemoryScope::Project => {
            let hash = candidate.target_scope_key.as_deref().ok_or_else(|| {
                AoError::Internal(format!(
                    "staged candidate {} targets project scope with no target_scope_key",
                    candidate.id
                ))
            })?;
            store.add_project(hash, content, source).await?.id
        }
        MemoryScope::AgentProject => {
            return Err(AoError::Internal(
                "AgentProject is a reserved scope; no staged candidate should ever target it"
                    .to_string(),
            ));
        }
        MemoryScope::Thread => {
            return Err(AoError::Internal(
                "Thread scope is ephemeral and applies immediately (see MemoryWrite); no staged \
                 candidate ever targets it"
                    .to_string(),
            ));
        }
    };

    let mut superseded = None;
    if let Some(old_id) = &candidate.contradicts {
        match &candidate.target_scope {
            MemoryScope::Agent => store.supersede(&candidate.agent_id, old_id, &memory_id).await?,
            MemoryScope::Global => store.supersede_global(old_id, &memory_id).await?,
            MemoryScope::Project => {
                let hash = candidate.target_scope_key.as_deref().unwrap_or_default();
                store.supersede_project(hash, old_id, &memory_id).await?;
            }
            MemoryScope::AgentProject | MemoryScope::Thread => unreachable!("checked above"),
        }
        superseded = Some(old_id.clone());
    }

    if pinned {
        match &candidate.target_scope {
            MemoryScope::Agent => store.set_pinned(&candidate.agent_id, &memory_id, true).await?,
            MemoryScope::Global => store.set_pinned_global(&memory_id, true).await?,
            MemoryScope::Project => {
                let hash = candidate.target_scope_key.as_deref().unwrap_or_default();
                store.set_pinned_project(hash, &memory_id, true).await?;
            }
            MemoryScope::AgentProject | MemoryScope::Thread => unreachable!("checked above"),
        }
    }

    Ok(AppliedCandidate { candidate_id: candidate.id.clone(), memory_id, superseded, pinned })
}

/// Approve a staged candidate as-is: write its proposed content unchanged
/// and mark it `Confirmed`. Tagged `MemorySource::Agent` — a human approved
/// release from quarantine, but the words themselves are still agent-
/// authored, so it stays as evictable/decayable as any other agent entry.
/// Use [`pin`] instead when the entry should also be eviction-exempt.
pub async fn keep(
    store: &MemoryStore,
    staging: &ReflectionStagingStore,
    agent_id: &str,
    candidate_id: &str,
) -> Result<AppliedCandidate, AoError> {
    let candidate = find_pending_memory_candidate(staging, agent_id, candidate_id).await?;
    let content = candidate.content.clone();
    let outcome = apply_candidate(store, &candidate, &content, MemorySource::Agent, false).await?;
    staging
        .update_status(agent_id, &[candidate.id], ReflectionCandidateStatus::Confirmed)
        .await?;
    Ok(outcome)
}

/// Approve a staged candidate with human-edited content. Tagged
/// `MemorySource::Manual` — the words going live are now human-authored,
/// so the resulting entry gets the same "never silently superseded" trust
/// a directly user-authored memory has.
pub async fn edit(
    store: &MemoryStore,
    staging: &ReflectionStagingStore,
    agent_id: &str,
    candidate_id: &str,
    edited_content: &str,
) -> Result<AppliedCandidate, AoError> {
    if edited_content.trim().is_empty() {
        return Err(AoError::ValidationError("edited content must not be empty".to_string()));
    }
    let candidate = find_pending_memory_candidate(staging, agent_id, candidate_id).await?;
    let outcome =
        apply_candidate(store, &candidate, edited_content, MemorySource::Manual, false).await?;
    staging
        .update_status(agent_id, &[candidate.id], ReflectionCandidateStatus::Confirmed)
        .await?;
    Ok(outcome)
}

/// Reject a staged candidate: nothing is written to the live memory store,
/// and the candidate is marked `Rejected` so it stops showing up as
/// pending. Idempotent-unsafe by design — rejecting an already-resolved
/// candidate is an error (see [`find_pending_memory_candidate`]), so a
/// double-click can't silently no-op past a real state mismatch.
pub async fn forget(
    staging: &ReflectionStagingStore,
    agent_id: &str,
    candidate_id: &str,
) -> Result<(), AoError> {
    let candidate = find_pending_memory_candidate(staging, agent_id, candidate_id).await?;
    staging
        .update_status(agent_id, &[candidate.id], ReflectionCandidateStatus::Rejected)
        .await?;
    Ok(())
}

/// Approve a staged candidate as-is, like [`keep`], but also flag the
/// resulting entry eviction-exempt (see `memory::eviction`'s `pinned`
/// filter) regardless of its `MemorySource`. The review-queue equivalent of
/// a human saying "yes, and never let the eviction sweep quietly remove
/// this."
pub async fn pin(
    store: &MemoryStore,
    staging: &ReflectionStagingStore,
    agent_id: &str,
    candidate_id: &str,
) -> Result<AppliedCandidate, AoError> {
    let candidate = find_pending_memory_candidate(staging, agent_id, candidate_id).await?;
    let content = candidate.content.clone();
    let outcome = apply_candidate(store, &candidate, &content, MemorySource::Agent, true).await?;
    staging
        .update_status(agent_id, &[candidate.id], ReflectionCandidateStatus::Confirmed)
        .await?;
    Ok(outcome)
}

/// Reverse a live agent-scope memory write by the id of the entry it
/// produced — whether that entry went live via the trust gate's
/// `AutoConfirm` tier (an in-turn `MemoryWrite` call that never touched
/// this queue at all) or via [`keep`]/[`edit`]/[`pin`] applying a staged
/// candidate. Both cases end in exactly one thing: a `MemoryEntry` with an
/// id, and (optionally) another entry it superseded. Reversing "the entry
/// this id names, plus whatever it superseded" therefore undoes either
/// path with the same code — `undo` never needs to know which path
/// produced the entry it is reversing.
///
/// Scoped to what a single write actually changed: the new entry is
/// soft-deleted, and any entry with `superseded_by == Some(memory_id)` is
/// restored to `Active`. Deliberately does **not** attempt to reverse a
/// hard-cap eviction that happened to run as a side effect of the same
/// write — that eviction picked the *scope's* least valuable entry, not
/// something this specific write did, so restoring it is a different
/// operation than undoing this write.
pub async fn undo(
    store: &MemoryStore,
    scope: &MemoryScope,
    scope_key: Option<&str>,
    memory_id: &str,
) -> Result<UndoOutcome, AoError> {
    let existing = match scope {
        MemoryScope::Agent => {
            let agent_id = scope_key.ok_or_else(|| {
                AoError::ValidationError("agent scope requires scope_key (the agent id)".to_string())
            })?;
            store.list(agent_id).await?
        }
        MemoryScope::Global => store.list_global().await?,
        MemoryScope::Project => {
            let hash = scope_key.ok_or_else(|| {
                AoError::ValidationError("project scope requires scope_key (the project hash)".to_string())
            })?;
            store.list_project(hash).await?
        }
        MemoryScope::AgentProject => {
            return Err(AoError::Internal(
                "AgentProject is a reserved scope; no live write ever targets it".to_string(),
            ));
        }
        MemoryScope::Thread => {
            return Err(AoError::Internal(
                "Thread scope is ephemeral; entries are dropped on cap or thread end, not \
                 reversed through undo"
                    .to_string(),
            ));
        }
    };

    if !existing.iter().any(|e| e.id == memory_id) {
        return Err(AoError::MemoryNotFound(format!(
            "memory entry {memory_id} not found in the given scope (already undone, or never existed)"
        )));
    }

    let restored = existing.iter().find(|e| e.superseded_by.as_deref() == Some(memory_id)).map(|e| e.id.clone());

    if let Some(restored_id) = &restored {
        match scope {
            MemoryScope::Agent => store.restore(scope_key.unwrap(), restored_id).await?,
            MemoryScope::Global => store.restore_global(restored_id).await?,
            MemoryScope::Project => store.restore_project(scope_key.unwrap(), restored_id).await?,
            MemoryScope::AgentProject | MemoryScope::Thread => unreachable!("checked above"),
        }
    }

    match scope {
        MemoryScope::Agent => {
            store.delete(scope_key.unwrap(), memory_id).await?;
        }
        MemoryScope::Global => {
            store.delete_global(memory_id).await?;
        }
        MemoryScope::Project => {
            store.delete_project(scope_key.unwrap(), memory_id).await?;
        }
        MemoryScope::AgentProject | MemoryScope::Thread => unreachable!("checked above"),
    }

    Ok(UndoOutcome { memory_id: memory_id.to_string(), restored })
}
