use ao_engine_tools_core::{
    memory_usage,
    trust_gate::{
        stage_candidate, ArtifactType, CandidateOrigin, CandidateScope, StagingRequest, StagingTier,
    },
    IoTool, RunnerContext, ToolOutput,
};
use ao_protocol::{
    error::AoError,
    memory::{MemoryScope, MemorySource, MemoryStatus},
    outcome::ArtifactKind,
    reflection_candidate::{ReflectionCandidate, ReflectionCandidateStatus},
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use std::sync::Arc;

use super::contradiction::{default_scorer, find_contradiction_with_fts_candidates};
use super::eviction::{select_eviction_candidate, thread_eviction_sweep};
use super::store::{
    ScopeContext, resolve_scope_context, resolve_working_dir,
    AGENT_HARD_CAP, AGENT_SOFT_CAP,
    GLOBAL_HARD_CAP, GLOBAL_SOFT_CAP,
    PROJECT_HARD_CAP, PROJECT_SOFT_CAP,
    ENTRY_CHAR_HARD, ENTRY_CHAR_SOFT,
    THREAD_HARD_CAP, THREAD_SOFT_CAP, THREAD_ENTRY_CHAR_HARD, THREAD_ENTRY_CHAR_SOFT,
};

/// How many FTS5 hits to pull as contradiction candidates. A scope's active
/// count is capped at [`AGENT_HARD_CAP`]/[`PROJECT_HARD_CAP`]/
/// [`GLOBAL_HARD_CAP`] (100/150/40), so this only ever narrows the field —
/// wide enough to catch a near-duplicate that isn't the single closest
/// bm25 match, small enough to keep the follow-up similarity pass cheap.
const FTS_CANDIDATE_LIMIT: usize = 10;

pub struct MemoryWrite;

#[async_trait]
impl IoTool for MemoryWrite {
    fn name(&self) -> &str {
        "MemoryWrite"
    }

    fn description(&self) -> &str {
        super::prompt::WRITE_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["agent", "project", "global", "thread"],
                    "description": "Which memory scope to write to. 'thread' is ephemeral working memory scoped to the current thread only — it does not persist once the thread ends."
                },
                "content": {
                    "type": "string",
                    "description": "The memory content to save."
                },
                "working_dir": {
                    "type": "string",
                    "description": "Optional. Override directory used to resolve the project key for scope='project'. Supports '~' expansion and accepts absolute or relative paths (relative is joined onto the runner cwd). Ignored for 'agent' and 'global' scopes. Pass this when the agent has navigated outside the runner's launch directory (e.g., into a sibling repo) so project memories key off the repo you're actually working in."
                }
            },
            "required": ["scope", "content"],
            "additionalProperties": false
        })
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let scope_str = match input.get("scope").and_then(Value::as_str) {
            Some(s) => s,
            None => {
                return Ok(ToolOutput::error("Missing required field: scope", false));
            }
        };
        let content = match input.get("content").and_then(Value::as_str) {
            Some(s) => s,
            None => {
                return Ok(ToolOutput::error("Missing required field: content", false));
            }
        };

        let scope = match scope_str {
            "agent" => MemoryScope::Agent,
            "project" => MemoryScope::Project,
            "global" => MemoryScope::Global,
            "thread" => MemoryScope::Thread,
            other => {
                return Ok(ToolOutput::error(
                    format!(
                        "Invalid scope '{other}'. Must be one of: agent, project, global, thread."
                    ),
                    false,
                ));
            }
        };

        // 1. Check content char hard cap. Thread scope uses its own, smaller
        // caps (it is a scratch tier, not durable curated notes).
        let (entry_char_soft, entry_char_hard) = if scope == MemoryScope::Thread {
            (THREAD_ENTRY_CHAR_SOFT, THREAD_ENTRY_CHAR_HARD)
        } else {
            (ENTRY_CHAR_SOFT, ENTRY_CHAR_HARD)
        };
        let char_len = content.chars().count();
        if char_len > entry_char_hard {
            return Ok(ToolOutput::structured(json!({
                "error": format!(
                    "Entry is too long ({char_len} chars). Maximum is {entry_char_hard} chars."
                )
            })));
        }
        let char_warning = if char_len > entry_char_soft {
            Some(format!(
                "⚠ Entry is long ({char_len} chars). Consider summarizing to keep memories concise."
            ))
        } else {
            None
        };

        // 2. Get store.
        let store = match &ctx.memory_store {
            Some(s) => s.clone(),
            None => {
                return Ok(ToolOutput::error(
                    "Memory store not available in this context.",
                    false,
                ));
            }
        };

        // 3. Resolve scope context (git subprocess only for project scope).
        let cwd = ctx.cwd.read().unwrap().clone();
        let explicit_working_dir = input
            .get("working_dir")
            .and_then(Value::as_str)
            .map(|s| resolve_working_dir(Some(s), &cwd));
        let scope_ctx = match resolve_scope_context(
            &scope,
            &ctx.agent_id,
            explicit_working_dir.as_deref(),
            ctx.parent_current_cwd.as_deref(),
            &cwd,
            ctx.thread_id.as_deref(),
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolOutput::error(
                    format!("Failed to resolve scope context: {e}"),
                    false,
                ));
            }
        };

        // Thread scope is an ephemeral, single-tenant tier with no
        // contradiction guard, trust-gate staging, or durable eviction
        // scoring (see `write_thread_entry`'s doc) — it never falls through
        // to the durable-scope pipeline below.
        if let ScopeContext::Thread { thread_id } = &scope_ctx {
            return write_thread_entry(&store, thread_id, content, char_warning).await;
        }

        let (soft_cap, hard_cap) = match &scope_ctx {
            ScopeContext::Agent { .. } => (AGENT_SOFT_CAP, AGENT_HARD_CAP),
            ScopeContext::Project { .. } => (PROJECT_SOFT_CAP, PROJECT_HARD_CAP),
            ScopeContext::Global => (GLOBAL_SOFT_CAP, GLOBAL_HARD_CAP),
            // `scope` above only ever parses to Agent/Project/Global here —
            // Thread already returned above, and AgentProject is reserved
            // for a future writer.
            ScopeContext::AgentProject { .. } | ScopeContext::Thread { .. } => unreachable!(
                "MemoryWrite only resolves scope from {{agent, project, global}} past this point"
            ),
        };

        // 4. Read live entries once for dedup check and scope cap check.
        let existing = match &scope_ctx {
            ScopeContext::Agent { agent_id } => store.list(agent_id).await?,
            ScopeContext::Global => store.list_global().await?,
            ScopeContext::Project { hash, .. } => store.list_project(hash).await?,
            ScopeContext::AgentProject { .. } | ScopeContext::Thread { .. } => unreachable!(
                "MemoryWrite only resolves scope from {{agent, project, global}}"
            ),
        };

        // 5. Dedup check.
        if let Some(dup) = existing.iter().find(|e| e.content == content) {
            return Ok(ToolOutput::structured(json!({
                "id": dup.id,
                "scope": scope_str,
                "deduplicated": true,
            })));
        }

        // 5.5. Contradiction check. Byte-equal dedup above only
        // catches an exact resubmission; this catches a reworded restatement or
        // contradiction of a live entry. The FTS5 index is queried first for
        // candidate near-duplicates (an upgrade past pure string
        // similarity — see `contradiction.rs` for why the index only ever
        // widens recall rather than replacing the strict pass).
        //
        // A match's source decides what happens next:
        // - Verified agent-authored (`Agent`/`GlobalPromotion`): this write may
        //   proceed, and the old entry gets marked `Superseded` below once the
        //   new entry's id exists (step 8.5). This is a narrower, already-
        //   verified-safe carve-out this call site owns — the
        //   generic gate below is never even told about it as a contradiction
        //   (see `ao_engine_tools_core::trust_gate`'s module doc for why the
        //   gate deliberately stays out of "which contradictions are pre-
        //   verified safe" decisions).
        // - Anything else — `Manual`, or unknown (`None`, e.g. a project-scope
        //   row written before scope carried a source) — HARD RULE:
        //   never silently supersede it. Unknown provenance is treated as
        //   cautiously as `Manual` because there is no way to prove it is safe
        //   to override. This applies no matter which pass (plain similarity or
        //   FTS5-corroborated) surfaced the match. Routed through the shared
        //   trust gate below as `overwrites_manual`, which always resolves to
        //   the `NeverAuto` hard block.
        let fts_scope_key: Option<&str> = match &scope_ctx {
            ScopeContext::Agent { agent_id } => Some(agent_id.as_str()),
            ScopeContext::Global => None,
            ScopeContext::Project { hash, .. } => Some(hash.as_str()),
            ScopeContext::AgentProject { .. } | ScopeContext::Thread { .. } => unreachable!(
                "MemoryWrite only resolves scope from {{agent, project, global}}"
            ),
        };
        let fts_candidate_ids = store
            .search_similar_ids(scope.clone(), fts_scope_key, content, FTS_CANDIDATE_LIMIT)
            .await;
        let contradiction = find_contradiction_with_fts_candidates(
            &existing,
            content,
            default_scorer().as_ref(),
            &fts_candidate_ids,
        );
        let mut superseded_entry_id: Option<String> = None;
        let mut overwrites_manual = false;
        if let Some(m) = &contradiction {
            let verified_agent_authored = matches!(
                m.entry.source,
                Some(MemorySource::Agent) | Some(MemorySource::GlobalPromotion)
            );
            if verified_agent_authored {
                superseded_entry_id = Some(m.entry.id.clone());
            } else {
                overwrites_manual = true;
            }
        }

        // 5.6. Trust gate: every
        // MemoryWrite call is a model tool call — there is no path through
        // this tool for direct human authorship — so the candidate's origin
        // is always SelfAuthored. This single call covers BOTH remaining
        // rules: (3) never-auto for a contradiction this call site could not
        // verify safe (`overwrites_manual`, set above), and (2d)/(2c)
        // stage-for-review for any write outside agent scope, contradiction
        // or not — closing the gap where a plain project/global write used
        // to apply live with no gating at all.
        let candidate_scope = match &scope_ctx {
            ScopeContext::Agent { .. } => CandidateScope::Agent,
            ScopeContext::Project { .. } => CandidateScope::Project,
            ScopeContext::Global => CandidateScope::Global,
            ScopeContext::AgentProject { .. } | ScopeContext::Thread { .. } => unreachable!(
                "MemoryWrite only resolves scope from {{agent, project, global}}"
            ),
        };
        let decision = stage_candidate(StagingRequest {
            artifact_type: ArtifactType::Memory,
            origin: CandidateOrigin::SelfAuthored,
            scope: candidate_scope,
            contradicts_existing: overwrites_manual,
            overwrites_manual,
        });
        if !decision.auto_enable() {
            let mut result = json!({
                "scope": scope_str,
                "staged": true,
                "applied": false,
                "tier": if decision.tier == StagingTier::NeverAuto {
                    "never_auto"
                } else {
                    "stage_for_review"
                },
                "reason": decision.reason,
            });
            if let Some(m) = &contradiction {
                result["id"] = json!(m.entry.id);
                result["contradicts"] = json!(m.entry.id);
                result["message"] = json!(format!(
                    "Not written: this content is a likely restatement of or contradiction \
                     with an existing memory ({}) that this agent cannot verify is safe to \
                     override. Ask the user to confirm before changing it, or use MemoryEdit \
                     on it directly once they agree.",
                    m.entry.id
                ));
            } else {
                result["message"] = json!(format!(
                    "Not written: writes to {scope_str} scope require human review before \
                     going live under the current trust boundary. Ask the user to confirm \
                     this memory directly, or write it to agent scope instead.",
                ));
            }

            // `StageForReview` is the exact set the review
            // queue must render — persist it, or a human reviewing later has
            // nothing but this transient tool result to go on. `NeverAuto`
            // is deliberately excluded: it never reaches a one-click confirm
            // queue (see `ao_engine_tools_core::trust_gate`'s module doc),
            // so staging it here would let `keep`/`edit`/`pin` silently
            // resolve a hard block the gate exists to prevent. Best-effort —
            // a `reflection_staging` outage must not fail the tool call that
            // is the caller's only signal this content was even proposed.
            if decision.tier == StagingTier::StageForReview {
                if let Some(staging) = &ctx.reflection_staging {
                    let target_scope_key = match &scope_ctx {
                        ScopeContext::Agent { agent_id } => Some(agent_id.clone()),
                        ScopeContext::Global => None,
                        ScopeContext::Project { hash, .. } => Some(hash.clone()),
                        ScopeContext::AgentProject { .. } | ScopeContext::Thread { .. } => unreachable!(
                            "MemoryWrite only resolves scope from {{agent, project, global}}"
                        ),
                    };
                    let candidate = ReflectionCandidate {
                        id: Uuid::new_v4().to_string(),
                        kind: ArtifactKind::Memory,
                        agent_id: ctx.agent_id.clone(),
                        source_thread_id: ctx.session_id.clone(),
                        content: content.to_string(),
                        status: ReflectionCandidateStatus::Pending,
                        target_scope: scope.clone(),
                        target_scope_key,
                        // Safe by construction: `overwrites_manual` already
                        // sent every unverified/Manual-provenance
                        // contradiction to `NeverAuto` above, so any
                        // `superseded_entry_id` reaching this branch is a
                        // verified agent-authored (or global-promotion)
                        // match this call site already treats as safe to
                        // resolve once a human confirms it (step 8.5 mirrors
                        // this same resolution on the auto-confirm path).
                        contradicts: superseded_entry_id.clone(),
                        reason: decision.reason.clone(),
                        created_at: Utc::now(),
                    };
                    result["candidate_id"] = json!(candidate.id);
                    if let Err(e) = staging.stage(&ctx.agent_id, &candidate).await {
                        tracing::warn!(
                            "MemoryWrite: failed to persist staged candidate for review queue: {}",
                            e
                        );
                    }
                }
            }

            return Ok(ToolOutput::structured(result));
        }

        // 6. Scope entry hard cap check. `Archived`/`Superseded`
        // entries are no longer live guidance (see `MemoryStatus`), so they
        // don't count against the cap — only `Active` entries do. When the
        // active count is at the hard cap, evict the lowest-scoring eligible
        // entry to `Archived` instead of rejecting the write outright; this
        // turns the cap from a wall into a sliding window. `Manual` entries
        // are never eviction candidates, so if every active entry is
        // Manual there is nothing safe to evict and the write is still
        // rejected.
        let mut active_count = existing.iter().filter(|e| e.status == MemoryStatus::Active).count();
        let mut evicted_entry_id: Option<String> = None;
        if active_count >= hard_cap {
            let scope_path = match &scope_ctx {
                ScopeContext::Agent { agent_id } => store.agent_scope_path(agent_id),
                ScopeContext::Global => store.global_scope_path(),
                ScopeContext::Project { hash, .. } => store.project_scope_path(hash),
                ScopeContext::AgentProject { .. } | ScopeContext::Thread { .. } => unreachable!(
                    "MemoryWrite only resolves scope from {{agent, project, global}}"
                ),
            };
            let usage = memory_usage::load(&scope_path).await;
            let candidate = select_eviction_candidate(&existing, &usage, Utc::now())
                .map(|e| e.id.clone());

            match candidate {
                Some(evict_id) => {
                    match &scope_ctx {
                        ScopeContext::Agent { agent_id } => {
                            store.archive(agent_id, &evict_id).await?
                        }
                        ScopeContext::Global => store.archive_global(&evict_id).await?,
                        ScopeContext::Project { hash, .. } => {
                            store.archive_project(hash, &evict_id).await?
                        }
                        ScopeContext::AgentProject { .. } | ScopeContext::Thread { .. } => unreachable!(
                            "MemoryWrite only resolves scope from {{agent, project, global}}"
                        ),
                    }
                    active_count -= 1;
                    evicted_entry_id = Some(evict_id);
                }
                None => {
                    return Ok(ToolOutput::structured(json!({
                        "error": format!(
                            "Memory scope is full ({active_count}/{hard_cap} entries) and every \
                             live entry is Manual, so none is eligible for automatic eviction. \
                             Use MemoryList + MemoryDelete to free space."
                        )
                    })));
                }
            }
        }

        // 7. Scope entry soft cap warning.
        let scope_warning = if active_count >= soft_cap {
            Some(format!(
                "⚠ {} memory at {active_count}/{soft_cap}. Consider deleting stale or contradicted entries before adding more.",
                scope_str
            ))
        } else {
            None
        };

        // 8. Write.
        let (id, deduplicated) = match &scope_ctx {
            ScopeContext::Agent { agent_id } => {
                let entry = store.add(agent_id, content, MemorySource::Agent).await?;
                (entry.id, false)
            }
            ScopeContext::Global => {
                let entry = store.add_global(content, MemorySource::Agent).await?;
                (entry.id, false)
            }
            ScopeContext::Project { hash, .. } => {
                let result = store.add_project(hash, content, MemorySource::Agent).await?;
                (result.id, result.deduplicated)
            }
            ScopeContext::AgentProject { .. } | ScopeContext::Thread { .. } => unreachable!(
                "MemoryWrite only resolves scope from {{agent, project, global}}"
            ),
        };

        // 8.5. Contradiction resolution. Reaching here means the write
        // above either had no contradiction, or matched a verified
        // agent-authored entry this call site already treated as safe to
        // supersede (step 5.5) — anything else returned earlier at the gate
        // check. Now that the new entry's id exists, mark the superseded
        // entry so provenance is preserved instead of two near-duplicate
        // entries silently coexisting.
        if let Some(old_id) = &superseded_entry_id {
            match &scope_ctx {
                ScopeContext::Agent { agent_id } => {
                    store.supersede(agent_id, old_id, &id).await?;
                }
                ScopeContext::Global => {
                    store.supersede_global(old_id, &id).await?;
                }
                ScopeContext::Project { hash, .. } => {
                    store.supersede_project(hash, old_id, &id).await?;
                }
                ScopeContext::AgentProject { .. } | ScopeContext::Thread { .. } => unreachable!(
                    "MemoryWrite only resolves scope from {{agent, project, global}}"
                ),
            }
        }

        // 9. Build result, merging char and scope warnings.
        let warning = char_warning.or(scope_warning);
        let mut result = json!({
            "id": id,
            "scope": scope_str,
            "deduplicated": deduplicated,
        });
        if let Some(w) = warning {
            result["warning"] = json!(w);
        }
        if let Some(old_id) = &superseded_entry_id {
            result["superseded"] = json!(old_id);
        }
        if let Some(evicted_id) = &evicted_entry_id {
            result["evicted"] = json!(evicted_id);
        }
        Ok(ToolOutput::structured(result))
    }
}

/// Write path for `Thread` scope — deliberately separate from the durable
/// (`Agent`/`Project`/`Global`) pipeline above rather than one more branch
/// threaded through it, because thread memory skips three things every
/// durable write does: the contradiction guard (no FTS lookup, no
/// supersede), the trust-gate staging queue (every thread write applies
/// immediately — there is no "review this before it goes live" step for a
/// tier that disappears with the thread anyway), and the usage-scored
/// evictor (`select_eviction_candidate`). Hitting the thread's hard cap
/// never rejects the write: [`thread_eviction_sweep`] drops the oldest live
/// entries to make room, silently — no `Archived` tombstone, no `evicted`
/// field in the result, because there is nothing left to keep a reviewable
/// record of once the whole tier is gone at end of thread. This auto-evict
/// is mandatory and unconditional for `Thread` scope: unlike the durable
/// scopes' one narrow residual rejection case (every live entry is
/// `Manual`), thread entries are never `Manual`-sourced, so there is no path
/// through this function that rejects a write for being over cap.
///
/// `pub` (and re-exported from [`crate::memory`]) because it has a second
/// caller outside the `MemoryWrite` tool's own `invoke`: the reflection
/// pass's low-confidence routing (`ao_engine::reflection_subscriber`) writes
/// a candidate straight through this same path so a candidate lands in
/// thread memory through the exact dedup/cap/evict rules a tool-invoked
/// thread write would apply — never a second, drifting copy of that logic.
pub async fn write_thread_entry(
    store: &Arc<ao_persistence::memory::MemoryStore>,
    thread_id: &str,
    content: &str,
    char_warning: Option<String>,
) -> Result<ToolOutput, AoError> {
    let existing = store.list_thread(thread_id).await?;

    if let Some(dup) = existing.iter().find(|e| e.content == content) {
        return Ok(ToolOutput::structured(json!({
            "id": dup.id,
            "scope": "thread",
            "deduplicated": true,
        })));
    }

    let mut active_count = existing.len();
    if active_count >= THREAD_HARD_CAP {
        let evict_ids = thread_eviction_sweep(&existing, THREAD_HARD_CAP.saturating_sub(1));
        for evict_id in &evict_ids {
            store.delete_thread(thread_id, evict_id).await?;
        }
        active_count -= evict_ids.len();
    }

    let scope_warning = if active_count >= THREAD_SOFT_CAP {
        Some(format!(
            "⚠ thread memory at {active_count}/{THREAD_SOFT_CAP}. This scope is ephemeral and \
             clears itself when the thread ends, but older notes may be dropped sooner to make room."
        ))
    } else {
        None
    };

    let entry = store.add_thread(thread_id, content, MemorySource::Agent).await?;

    let warning = char_warning.or(scope_warning);
    let mut result = json!({
        "id": entry.id,
        "scope": "thread",
        "deduplicated": false,
    });
    if let Some(w) = warning {
        result["warning"] = json!(w);
    }
    Ok(ToolOutput::structured(result))
}
