//! Production [`ReflectionTriggerSubscriber`]: the
//! OBSERVE producer that turns a thread's untrimmed transcript delta into
//! staged candidate memories/skills.
//!
//! This module owns the *orchestration* around the reflection pass — trigger
//! → thread resolution → delta read → agent-profile resolution → staging.
//! The actual model call is delegated to
//! [`ao_engine_tools_runner::reflection::ReflectionProposalEngine`], mirroring
//! `verification`/`thread_summary`'s split between "drive a provider client"
//! (that crate) and "resolve which provider client to drive, from an
//! `AgentProfile`" (this crate — see [`crate::provider_client_for_profile`],
//! which only `ao-engine` can do without a circular dependency on
//! `agent_runner`/`CliProviderClient`).

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use ao_engine_tools_core::trust_gate::{
    stage_candidate, ArtifactType, CandidateOrigin, CandidateScope, StagingRequest,
};
use ao_engine_tools_engine::memory::contradiction::{
    default_scorer, find_contradiction, CONTRADICTION_THRESHOLD,
};
use ao_engine_tools_engine::memory::{write_thread_entry, THREAD_ENTRY_CHAR_HARD};
use ao_engine_tools_runner::provider::ProviderClient;
use ao_engine_tools_runner::reflection::{ProviderReflectionProposer, ReflectionProposalEngine};
use ao_persistence::PersistenceLayer;
use ao_protocol::agent::AgentProfile;
use ao_protocol::memory::{MemoryScope, MemorySource, MemoryStatus};
use ao_protocol::outcome::ArtifactKind;
use ao_protocol::reflection_candidate::{ReflectionCandidate, ReflectionCandidateStatus};
use ao_protocol::reflection_trigger::{
    ReflectionTrigger, ReflectionTriggerReason, ReflectionTriggerSubscriber,
};
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

/// Resolves the [`ProviderClient`] to drive for a given [`AgentProfile`].
///
/// Production wiring injects [`crate::build_reflection_provider`] (itself a
/// thin wrapper over the same profile→client resolution
/// [`build_quick_verification_engine`](crate::build_quick_verification_engine)
/// and [`build_thread_summarization_engine`](crate::build_thread_summarization_engine)
/// already use). Tests inject a closure that hands back a scripted
/// `MockProviderClient` instead, so [`ReflectionSubscriber`]'s own
/// orchestration logic is exercised without a live provider — and so a test
/// can assert this resolver is the ONLY path a provider client reaches this
/// subscriber through (see `reflection_subscriber::tests`).
pub type ProviderResolver =
    Arc<dyn Fn(&AgentProfile) -> Option<Arc<dyn ProviderClient>> + Send + Sync>;

/// Cap on delta characters handed to the one-shot proposal call. A coarse
/// proxy for tokens (avoids depending on a tokenizer here), sized generously
/// since a reflection delta is normally much smaller than a full thread
/// (bounded by the watermark, not the whole transcript).
const MAX_DELTA_CHARS: usize = 40_000;

/// The front-tier routing threshold. A `Memory`-kind proposal
/// whose [`ReflectionProposal::confidence`](ao_engine_tools_runner::reflection::ReflectionProposal)
/// is strictly below this lands in the originating thread's ephemeral
/// [`MemoryScope::Thread`] instead of [`stage_proposal`]'s usual
/// `ReflectionStagingStore` path — cheap, injectable, no review-queue entry.
/// At or above it, nothing changes: the candidate is staged exactly as every
/// `Reflected` candidate is today. Set at the scale's midpoint: a candidate
/// the model itself is no more sure of than not is exactly the "maybe
/// useful right now, not yet worth a human's review time" case this tier
/// exists for. Promoting a thread note back out to staging is a separate,
/// later pass this constant has no say over.
const THREAD_ROUTE_CONFIDENCE_THRESHOLD: f32 = 0.5;

/// The periodic in-life promotion sweep: how often, at most, one
/// thread's accumulated thread-scope notes get re-checked against the
/// promotion judge outside of thread archival (see
/// [`ReflectionSubscriber::run_periodic_promotion_sweep`]). Piggybacks on
/// whatever reflection trigger already fired for the thread
/// (`AnchorRotated`/`IdleTimeout`) rather than a timer of its own, so a
/// thread with no conversation activity never sweeps at all — only the
/// debounce is time-based. 30 minutes is generous enough that a normal
/// working session's own `AnchorRotated`/`IdleTimeout` cadence drives the
/// sweep without adding a judge call to every single trigger.
const PROMOTION_SWEEP_INTERVAL: chrono::Duration = chrono::Duration::minutes(30);

/// Outcome of one [`ReflectionSubscriber::run`] call, returned so callers
/// (mainly tests) can assert on it without re-reading the staging store.
///
/// `candidates_staged` only counts a proposal once it was actually persisted
/// somewhere — either [`ReflectionStagingStore`](ao_persistence::reflection_staging::ReflectionStagingStore)
/// via [`ReflectionSubscriber::stage_proposal`], or the originating thread's
/// ephemeral memory via [`ReflectionSubscriber::route_to_thread_memory`].
/// `dropped_count` is the complement: a proposal [`stage_proposal`] or
/// [`route_to_thread_memory`] decided NOT to write anywhere (over the thread
/// entry char cap, or a near-duplicate of a candidate already `Pending`) —
/// tracked separately so that outcome is observable instead of silently
/// inflating `candidates_staged` for content that was actually discarded.
#[derive(Debug, Clone, PartialEq)]
pub struct ReflectionOutcome {
    pub candidates_staged: usize,
    pub dropped_count: usize,
    pub advanced_watermark_to: Option<DateTime<Utc>>,
}

impl ReflectionOutcome {
    fn no_op() -> Self {
        Self {
            candidates_staged: 0,
            dropped_count: 0,
            advanced_watermark_to: None,
        }
    }
}

/// Whether a proposal handed to [`ReflectionSubscriber::stage_proposal`] (or
/// its [`ReflectionSubscriber::route_to_thread_memory`] carve-out) actually
/// ended up persisted anywhere. See [`ReflectionOutcome`]'s doc comment for
/// why this distinction exists.
enum StagingOutcome {
    /// Written to `ReflectionStagingStore` or the thread's ephemeral memory.
    Staged,
    /// Not written anywhere — over the thread entry char cap, or a
    /// near-duplicate of a candidate already `Pending`.
    Dropped,
}

/// The concrete [`ReflectionTriggerSubscriber`] the trigger seam (anchor
/// rotation, idle timeout, explicit archive — all wired in
/// `ao_protocol::reflection_trigger` / `history::select` /
/// `ThreadStore::archive`) fires into.
#[derive(Clone)]
pub struct ReflectionSubscriber {
    persistence: Arc<PersistenceLayer>,
    resolve_provider: ProviderResolver,
    /// Distillation (`crate::skill_distillation::SkillDistiller`), run
    /// after every successful pass — see [`Self::with_distiller`]. `None` in
    /// every existing test in this module, so none of them gain a second
    /// provider call or any other new behavior from this field's addition.
    distiller: Option<Arc<crate::skill_distillation::SkillDistiller>>,
    /// Promotion judge (`crate::memory_promotion::MemoryPromotionJudge`)
    /// — see [`Self::with_promotion_judge`]. Run unconditionally for
    /// [`ReflectionTriggerReason::Archived`] triggers
    /// ([`Self::run_promotion_sweep`]), and debounced/gated for every other
    /// trigger reason ([`Self::run_periodic_promotion_sweep`]). `None` in
    /// every existing test in this module, so none of them gain a second
    /// provider call or any other new behavior from this field's addition.
    promotion_judge: Option<Arc<crate::memory_promotion::MemoryPromotionJudge>>,
}

impl ReflectionSubscriber {
    pub fn new(persistence: Arc<PersistenceLayer>, resolve_provider: ProviderResolver) -> Self {
        Self {
            persistence,
            resolve_provider,
            distiller: None,
            promotion_judge: None,
        }
    }

    /// Chain a [`crate::skill_distillation::SkillDistiller`] pass onto every
    /// [`Self::on_reflection_trigger`] run, closing the OBSERVE → distill
    /// loop off the same trigger the reflection pass itself fires on — no
    /// separate scheduler, no extra latency on the user's turn (the trigger
    /// is already handled off-turn; see [`Self::on_reflection_trigger`]).
    pub fn with_distiller(mut self, distiller: Arc<crate::skill_distillation::SkillDistiller>) -> Self {
        self.distiller = Some(distiller);
        self
    }

    /// Chain a [`crate::memory_promotion::MemoryPromotionJudge`] onto every
    /// [`Self::on_reflection_trigger`] run. A
    /// [`ReflectionTriggerReason::Archived`] trigger — the point a thread's
    /// learnings are considered final — runs an unconditional sweep over
    /// every live thread-scope entry ([`Self::run_promotion_sweep`]). Every
    /// other reason (`AnchorRotated`/`IdleTimeout`) runs the debounced,
    /// survival-gated periodic sweep instead
    /// ([`Self::run_periodic_promotion_sweep`]): a thread that is still
    /// active may keep adding, editing, or contradicting its own
    /// thread-scope notes, so an entry only becomes judge-eligible once it
    /// has sat uncontradicted for at least
    /// [`crate::memory_promotion::MIN_PROMOTION_SURVIVAL`] — otherwise
    /// judging it risks promoting something the thread itself later
    /// supersedes. This periodic path is what lets a `Default` thread's
    /// notes reach durable memory at all, since a `Default` thread can never
    /// archive (see `ThreadStore::archive`).
    pub fn with_promotion_judge(
        mut self,
        judge: Arc<crate::memory_promotion::MemoryPromotionJudge>,
    ) -> Self {
        self.promotion_judge = Some(judge);
        self
    }

    /// Run one reflection pass for `trigger`. Public and `async` — unlike the
    /// trait's sync entry point below — so tests (and any future caller that
    /// wants to await completion) don't have to race a spawned task.
    pub async fn run(&self, trigger: ReflectionTrigger) -> Result<ReflectionOutcome, String> {
        // Resolve the owning Thread row. `agent_id` + `transcript_path` is
        // the trigger's identity tuple (no `thread_id` — see
        // `ao_protocol::reflection_trigger`).
        let thread = self
            .persistence
            .threads
            .find_by_transcript_path(&trigger.agent_id, &trigger.transcript_path)
            .await
            .map_err(|e| format!("failed to resolve thread for reflection trigger: {e}"))?
            .ok_or_else(|| {
                format!(
                    "no thread found for agent '{}' at transcript path '{}'",
                    trigger.agent_id, trigger.transcript_path
                )
            })?;

        // Hard rule: read the UNTRIMMED transcript from disk — never the
        // trimmed in-memory history window. `read_all_at` reads the whole
        // JSONL file at `thread.transcript_path`; the watermark filter below
        // is what turns that into "only the delta".
        let all_entries = self
            .persistence
            .transcripts
            .read_all_at(Path::new(&thread.transcript_path))
            .await
            .map_err(|e| format!("failed to read transcript for reflection: {e}"))?;

        let watermark = thread.distilled_through_ts;
        let delta: Vec<TranscriptEntry> = all_entries
            .into_iter()
            .filter(|e| watermark.is_none_or(|w| e.ts > w))
            .collect();

        if delta.is_empty() {
            // Nothing new since the last pass — cheap no-op, no model call,
            // watermark left untouched (there is no "end of delta" to
            // advance to). This is what makes a rotation that finds nothing
            // new idempotent rather than a wasted provider call every time.
            return Ok(ReflectionOutcome::no_op());
        }

        // Resolve which AgentProfile drives the proposal call — the
        // optional `reflection_agent_id` preference, falling back to the
        // thread's own agent (whatever ran the turn).
        let prefs = self
            .persistence
            .preferences
            .get()
            .await
            .map_err(|e| format!("failed to load preferences for reflection: {e}"))?
            .unwrap_or_default();
        let profile_id = prefs
            .reflection_agent_id
            .unwrap_or_else(|| trigger.agent_id.clone());
        let profile = self
            .persistence
            .agents
            .get(&profile_id)
            .await
            .map_err(|e| format!("failed to load agent profile '{profile_id}': {e}"))?
            .ok_or_else(|| format!("reflection agent profile '{profile_id}' not found"))?;

        // Hard rule: drive the model through the app's existing
        // provider/runner seam — never a bespoke client. This subscriber
        // never constructs a `ProviderClient` itself; it only ever consumes
        // whatever `resolve_provider` (production: `build_reflection_provider`)
        // hands back.
        let provider = (self.resolve_provider)(&profile).ok_or_else(|| {
            format!("no provider configured for reflection agent profile '{profile_id}'")
        })?;

        let delta_text = format_delta(&delta);
        let proposals = ProviderReflectionProposer::new(provider)
            .propose(&delta_text)
            .await?;

        // Stage every candidate. Nothing here is ever written to a live
        // memory/skill store — see `stage_proposal`.
        let mut staged_count = 0usize;
        let mut dropped_count = 0usize;
        for proposal in proposals {
            match self
                .stage_proposal(&trigger.agent_id, &thread.id, proposal)
                .await?
            {
                StagingOutcome::Staged => staged_count += 1,
                StagingOutcome::Dropped => dropped_count += 1,
            }
        }

        // Advance the watermark to the end of the consumed delta. Only
        // reached after every candidate staged successfully, so a failure
        // above leaves the watermark untouched and the SAME delta is
        // reconsidered on the next trigger — idempotent by construction
        // (byte-equal dedup on the memory-write path, if a candidate is ever
        // confirmed and applied, is the backstop against any double-staging
        // that slips through anyway).
        let delta_end = delta
            .last()
            .map(|e| e.ts)
            .expect("delta already checked non-empty above");
        self.persistence
            .threads
            .advance_distillation_watermark(&thread.id, delta_end)
            .await
            .map_err(|e| format!("failed to advance distillation watermark: {e}"))?;

        Ok(ReflectionOutcome {
            candidates_staged: staged_count,
            dropped_count,
            advanced_watermark_to: Some(delta_end),
        })
    }

    /// Route one proposal through the trust gate (plus the similarity check
    /// for memory) and persist it as a [`ReflectionCandidate`]. Never writes to
    /// a live memory or skill store.
    ///
    /// The carve-out, checked first: a `Memory` proposal below
    /// [`THREAD_ROUTE_CONFIDENCE_THRESHOLD`] never reaches the trust gate or
    /// `ReflectionStagingStore` at all — it goes straight to the originating
    /// thread's ephemeral memory via [`Self::route_to_thread_memory`] and
    /// returns early. `Skill` proposals are unaffected by confidence: there
    /// is no thread-scope tier for skills yet (distillation reads
    /// `Skill` candidates out of the staging queue), so every `Skill`
    /// proposal keeps going through the unchanged path below regardless of
    /// its confidence.
    async fn stage_proposal(
        &self,
        agent_id: &str,
        thread_id: &str,
        proposal: ao_engine_tools_runner::reflection::ReflectionProposal,
    ) -> Result<StagingOutcome, String> {
        if proposal.kind == ArtifactKind::Memory
            && proposal.confidence < THREAD_ROUTE_CONFIDENCE_THRESHOLD
        {
            return self.route_to_thread_memory(thread_id, &proposal.content).await;
        }

        let artifact_type = match proposal.kind {
            ArtifactKind::Memory => ArtifactType::Memory,
            ArtifactKind::Skill => ArtifactType::Skill,
        };

        // Memory only: flag — never silently apply — a likely
        // restatement of or contradiction with an existing live entry.
        // Nothing is written regardless of this check's result (every
        // reflection candidate stages for review at minimum, per the gate
        // call below); the tag is what a future confirm step must consult
        // before ever applying the candidate, so it can enforce "never
        // clobber a Manual entry" at that point too.
        // Extracted to an owned `(id, source)` pair immediately: the match
        // itself borrows from `existing`, which only lives inside this `if`
        // branch's scope.
        let contradiction: Option<(String, Option<MemorySource>)> = if proposal.kind
            == ArtifactKind::Memory
        {
            // Dedup against candidates already sitting `Pending` in
            // `ReflectionStagingStore` — not just live memory below. Without
            // this, a repeated observation across triggers piles up a fresh
            // near-duplicate `ReflectionCandidate` every pass instead of
            // being recognized as the same review-queue entry. `Skill`
            // proposals are deliberately excluded: repeated observations of
            // the SAME skill are exactly what the distillation pass
            // clusters (see `with_distiller`'s doc), so deduping them here
            // would starve that repetition count.
            let pending = self
                .persistence
                .reflection_staging
                .list_pending(agent_id)
                .await
                .map_err(|e| format!("failed to load pending candidates for dedup check: {e}"))?;
            let scorer = default_scorer();
            let duplicates_pending = pending
                .iter()
                .filter(|c| c.kind == ArtifactKind::Memory)
                .any(|c| scorer.score(&c.content, &proposal.content) >= CONTRADICTION_THRESHOLD);
            if duplicates_pending {
                tracing::info!(
                    agent_id = %agent_id,
                    thread_id = %thread_id,
                    "skipping reflection candidate: near-duplicate of a candidate already pending review"
                );
                return Ok(StagingOutcome::Dropped);
            }

            let existing = self
                .persistence
                .memory
                .list(agent_id)
                .await
                .map_err(|e| format!("failed to load existing memory for contradiction check: {e}"))?;
            find_contradiction(&existing, &proposal.content, default_scorer().as_ref())
                .map(|m| (m.entry.id.clone(), m.entry.source.clone()))
        } else {
            None
        };
        let contradicts = contradiction.as_ref().map(|(id, _)| id.clone());
        // Same "verified agent-authored vs. everything else" split
        // `ao_engine_tools_engine::memory::write` uses:
        // unverified provenance (`Manual`, or unknown/legacy rows) is never
        // safe to treat as a routine supersede.
        let overwrites_manual = contradiction.as_ref().is_some_and(|(_, source)| {
            !matches!(source, Some(MemorySource::Agent) | Some(MemorySource::GlobalPromotion))
        });

        // Every reflection candidate is CandidateOrigin::Reflected,
        // which the gate always stages for review at minimum — never
        // auto-confirms, regardless of scope or contradiction (out-of-band
        // candidates never skip review, unlike `SkillRegister`'s in-turn
        // path). A candidate that also contradicts unverified/Manual
        // provenance escalates further, to the never-auto hard block.
        let decision = stage_candidate(StagingRequest {
            artifact_type,
            origin: CandidateOrigin::Reflected,
            scope: CandidateScope::Agent,
            contradicts_existing: contradicts.is_some(),
            overwrites_manual,
        });
        debug_assert!(
            !decision.auto_enable(),
            "CandidateOrigin::Reflected must never auto-confirm"
        );

        let candidate = ReflectionCandidate {
            id: Uuid::new_v4().to_string(),
            kind: proposal.kind,
            agent_id: agent_id.to_string(),
            source_thread_id: thread_id.to_string(),
            content: proposal.content,
            status: ReflectionCandidateStatus::Pending,
            // The reflection pass only ever proposes agent-scope candidates
            // (`scope: CandidateScope::Agent` above) — see the trust-gate
            // call just above this block.
            target_scope: MemoryScope::Agent,
            target_scope_key: Some(agent_id.to_string()),
            contradicts,
            reason: decision.reason,
            created_at: Utc::now(),
        };

        // TODO(skill-generalization): a Skill candidate lands here exactly
        // as the concrete slice it was observed as. The generalization pass
        // reads `ReflectionStagingStore::list_pending` and turns it into a
        // reusable template — through this SAME execution-engine seam
        // (`ProviderResolver` / `Arc<dyn ProviderClient>`, resolved the same
        // "reflection_agent_id, else the thread's own profile" way), not a
        // second model-invocation path.
        self.persistence
            .reflection_staging
            .stage(agent_id, &candidate)
            .await
            .map_err(|e| format!("failed to stage reflection candidate: {e}"))?;

        Ok(StagingOutcome::Staged)
    }

    /// Thread-memory landing spot for a low-confidence `Memory`
    /// proposal: writes straight through
    /// [`ao_engine_tools_engine::memory::write_thread_entry`] — the exact
    /// dedup/cap/evict path a tool-invoked `MemoryWrite{scope: "thread"}`
    /// call would take — so this pass never grows a second, drifting copy
    /// of that logic. No [`ReflectionCandidate`] is ever created for this
    /// path: it never reaches `ReflectionStagingStore`, matching its own
    /// "cheap, ephemeral, no review-queue entry" contract.
    async fn route_to_thread_memory(
        &self,
        thread_id: &str,
        content: &str,
    ) -> Result<StagingOutcome, String> {
        if content.chars().count() > THREAD_ENTRY_CHAR_HARD {
            // Thread scope enforces this cap only at the tool-invoke layer
            // (`memory::write::MemoryWrite::invoke`), which this path
            // bypasses. An over-cap proposal is dropped rather than
            // truncated or hard-failing the whole pass — consistent with
            // this module's existing "a malformed piece of model output
            // degrades to skipping just that piece" posture (see
            // `ProviderReflectionProposer::propose`'s doc). Reported back as
            // `StagingOutcome::Dropped` so the caller's `dropped_count`
            // reflects it instead of silently counting it as staged.
            tracing::warn!(
                thread_id = %thread_id,
                chars = content.chars().count(),
                "dropping low-confidence reflection candidate: exceeds thread entry char cap"
            );
            return Ok(StagingOutcome::Dropped);
        }

        write_thread_entry(&self.persistence.memory, thread_id, content, None)
            .await
            .map(|_| StagingOutcome::Staged)
            .map_err(|e| format!("failed to write low-confidence reflection candidate to thread memory: {e}"))
    }

    /// The promotion sweep: judge every still-`Active`
    /// `Thread`-scope memory entry the archived thread accumulated (via
    /// [`Self::route_to_thread_memory`] across however many prior triggers)
    /// and, for each, let `judge` decide whether it generalizes beyond this
    /// thread. Resolves the owning thread independently of [`Self::run`]'s
    /// delta-driven pass — deliberately so, since this must still run when
    /// the archive trigger's own transcript delta is empty (the thread's
    /// accumulated thread-scope notes predate this specific trigger; an
    /// early no-op return in `run` must never skip promotion).
    ///
    /// Never fails the caller: a per-entry judge failure is logged and
    /// skipped so one bad entry cannot block the rest of the sweep, matching
    /// this module's existing "a malformed piece degrades to skipping just
    /// that piece" posture. Returns `Err` only if the thread itself, or its
    /// thread-scope memory, cannot be read at all.
    async fn run_promotion_sweep(
        &self,
        judge: &crate::memory_promotion::MemoryPromotionJudge,
        agent_id: &str,
        transcript_path: &str,
    ) -> Result<(), String> {
        let thread = self
            .persistence
            .threads
            .find_by_transcript_path(agent_id, transcript_path)
            .await
            .map_err(|e| format!("failed to resolve thread for promotion sweep: {e}"))?
            .ok_or_else(|| {
                format!(
                    "no thread found for agent '{agent_id}' at transcript path '{transcript_path}' \
                     during promotion sweep"
                )
            })?;

        let thread_entries = self
            .persistence
            .memory
            .list_thread(&thread.id)
            .await
            .map_err(|e| format!("failed to load thread-scope memory for promotion sweep: {e}"))?;

        for entry in thread_entries.iter().filter(|e| e.status == MemoryStatus::Active) {
            if let Err(e) = judge.promote(agent_id, &thread.id, entry).await {
                tracing::warn!(
                    agent_id = %agent_id,
                    thread_id = %thread.id,
                    entry_id = %entry.id,
                    "promotion judge failed for one thread-scope entry: {e}"
                );
            }
        }

        Ok(())
    }

    /// Periodic counterpart to [`Self::run_promotion_sweep`]: runs on every
    /// non-`Archived` reflection trigger, but only actually does anything
    /// once every [`PROMOTION_SWEEP_INTERVAL`] per thread (tracked via
    /// [`ao_protocol::thread::Thread::promotion_swept_at`]), and only ever
    /// judges an entry that clears
    /// [`crate::memory_promotion::is_promotion_eligible`]. This is what lets
    /// a thread's thread-scope notes reach durable memory without the
    /// thread ever being archived — the gap a `Default` thread (which can
    /// never archive; see `ThreadStore::archive`) would otherwise fall
    /// into, since it can stay alive indefinitely while its accumulated
    /// notes just pile up in thread scope and get evicted oldest-first.
    ///
    /// A dormant thread never reaches this method at all: it is only called
    /// from [`Self::on_reflection_trigger`]'s spawned task, itself only
    /// invoked when a reflection trigger actually fires — no separate timer
    /// polls threads on its own.
    ///
    /// The debounce watermark advances whenever the sweep is due, whether or
    /// not any entry turned out eligible — that's what bounds this method to
    /// at most one `list_thread` read per thread per interval, on top of the
    /// empty-sweep guard (an empty eligible set below skips the judge call
    /// entirely, at whatever cadence the sweep runs).
    ///
    /// Never fails the caller, matching [`Self::run_promotion_sweep`]'s
    /// posture: a per-entry judge failure is logged and skipped. Returns
    /// `Err` only if the thread or its thread-scope memory cannot be read,
    /// or the debounce watermark cannot be persisted.
    async fn run_periodic_promotion_sweep(
        &self,
        judge: &crate::memory_promotion::MemoryPromotionJudge,
        agent_id: &str,
        transcript_path: &str,
    ) -> Result<(), String> {
        let thread = self
            .persistence
            .threads
            .find_by_transcript_path(agent_id, transcript_path)
            .await
            .map_err(|e| format!("failed to resolve thread for periodic promotion sweep: {e}"))?
            .ok_or_else(|| {
                format!(
                    "no thread found for agent '{agent_id}' at transcript path '{transcript_path}' \
                     during periodic promotion sweep"
                )
            })?;

        let now = Utc::now();
        let due = thread
            .promotion_swept_at
            .is_none_or(|last| now - last >= PROMOTION_SWEEP_INTERVAL);
        if !due {
            return Ok(());
        }

        let thread_entries = self
            .persistence
            .memory
            .list_thread(&thread.id)
            .await
            .map_err(|e| format!("failed to load thread-scope memory for periodic promotion sweep: {e}"))?;

        for entry in thread_entries
            .iter()
            .filter(|e| crate::memory_promotion::is_promotion_eligible(e, now))
        {
            if let Err(e) = judge.promote(agent_id, &thread.id, entry).await {
                tracing::warn!(
                    agent_id = %agent_id,
                    thread_id = %thread.id,
                    entry_id = %entry.id,
                    "periodic promotion sweep: judge failed for one thread-scope entry: {e}"
                );
            }
        }

        self.persistence
            .threads
            .advance_promotion_sweep_watermark(&thread.id, now)
            .await
            .map_err(|e| format!("failed to advance promotion sweep watermark: {e}"))?;

        Ok(())
    }
}

impl ReflectionTriggerSubscriber for ReflectionSubscriber {
    /// Hands the trigger off to a spawned task and returns immediately —
    /// this is what keeps the pass entirely off the user's turn (the
    /// "adds zero latency" contract). `history::select()` and
    /// `ThreadStore::archive()` call this inline on the request-serving
    /// path, so it must never block or do the reflection work itself.
    fn on_reflection_trigger(&self, trigger: ReflectionTrigger) {
        let this = self.clone();
        tokio::spawn(async move {
            let agent_id = trigger.agent_id.clone();
            let transcript_path = trigger.transcript_path.clone();
            let reason = trigger.reason;
            match this.run(trigger).await {
                Ok(_) => {
                    // Check whether this agent's now-updated set of
                    // pending Skill candidates clears the repetition bar for
                    // distillation. Cheap when it doesn't — see
                    // `SkillDistiller::run`'s own no-op contract.
                    if let Some(distiller) = &this.distiller {
                        if let Err(e) = distiller.run(&agent_id).await {
                            tracing::warn!(
                                agent_id = %agent_id,
                                "skill distillation pass failed: {e}"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        reason = ?reason,
                        "reflection pass failed: {e}"
                    );
                }
            }

            // The promotion judge always runs off this same trigger,
            // never off a delta-driven condition — it fires regardless of
            // whether the `run()` pass above found a delta, succeeded, or
            // failed, since the thread's accumulated thread-scope notes are
            // independent of this specific trigger's transcript delta. An
            // `Archived` trigger — "this thread's learnings are final"
            // — gets an unconditional sweep over every live
            // entry. Every other reason runs the debounced, survival-gated
            // periodic sweep instead, which is what lets a `Default` thread
            // (never archived) promote at all.
            if let Some(judge) = &this.promotion_judge {
                if reason == ReflectionTriggerReason::Archived {
                    if let Err(e) = this.run_promotion_sweep(judge, &agent_id, &transcript_path).await {
                        tracing::warn!(
                            agent_id = %agent_id,
                            "promotion sweep failed: {e}"
                        );
                    }
                } else if let Err(e) = this
                    .run_periodic_promotion_sweep(judge, &agent_id, &transcript_path)
                    .await
                {
                    tracing::warn!(
                        agent_id = %agent_id,
                        "periodic promotion sweep failed: {e}"
                    );
                }
            }
        });
    }
}

fn role_label(role: &TranscriptRole) -> &str {
    match role {
        TranscriptRole::System(s) => s.as_str(),
        TranscriptRole::Agent { agent } => agent.as_str(),
        TranscriptRole::Schedule { .. } => "schedule",
    }
}

/// Render a transcript delta into one text blob for the proposal call,
/// capped at [`MAX_DELTA_CHARS`]. Entries are kept in chronological order
/// (oldest first) since the whole point is rescuing content about to be
/// forgotten — truncation (rare, given the delta is already watermark-bounded)
/// drops the newest tail rather than the oldest content.
fn format_delta(entries: &[TranscriptEntry]) -> String {
    let mut text = String::new();
    for entry in entries {
        let line = format!(
            "[{}] {}: {}\n",
            entry.ts.to_rfc3339(),
            role_label(&entry.role),
            entry.content
        );
        if text.chars().count() + line.chars().count() > MAX_DELTA_CHARS {
            text.push_str("...[remaining delta truncated]...\n");
            break;
        }
        text.push_str(&line);
    }
    text
}
