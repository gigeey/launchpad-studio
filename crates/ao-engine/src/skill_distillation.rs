//! Distillation trigger: the second half of the OBSERVE
//! → distill pipeline the reflection pass (`crate::reflection_subscriber`)
//! leaves a seam for.
//!
//! The reflection pass already turns an untrimmed transcript delta into
//! candidate memories/skills and stages every one of them behind the
//! trust gate (`ao_engine_tools_core::trust_gate`) — see its own module doc
//! for why nothing it produces is ever live. A single staged `Skill`
//! candidate, though, is still tied to the concrete occasion it was observed
//! on: one specific set of file paths, error messages, commands. [`SkillDistiller`]
//! answers the question the reflection pass deliberately leaves open: *has
//! this concrete procedure recurred enough times to be worth turning into a
//! reusable template, and if so, what does the general form of it look like?*
//!
//! Two responsibilities:
//! 1. **Detection** — group the agent's still-pending `Skill` candidates by
//!    content similarity and require at least [`SKILL_REPETITION_THRESHOLD`]
//!    members before treating a group as a real recurring procedure.
//!    Repetition is the evidence bar (signal over
//!    noise, not a single mid-turn hunch) — a lone observation never
//!    triggers distillation, no matter how procedure-like it reads.
//! 2. **Generalization** — turn a qualifying group's concrete observations
//!    into one reusable template via
//!    [`ao_engine_tools_runner::reflection::ProviderSkillGeneralizer`]. This
//!    is a model-invoking step, and it MUST drive the model
//!    through the exact same execution-engine seam the reflection pass uses:
//!    an injected [`crate::reflection_subscriber::ProviderResolver`]
//!    resolving an `AgentProfile` (the optional `reflection_agent_id`
//!    preference, falling back to the agent whose procedure is being
//!    distilled), driven via the app's existing `Arc<dyn ProviderClient>`
//!    path. There is no second model-invocation path, no bespoke HTTP
//!    client, and no new credential source here.
//!
//! [`SkillDistiller::generalize_single`] is a second, human-initiated entry
//! point onto the same generalization call: a person promoting one specific
//! observation straight to a skill without waiting for it to repeat. The
//! person's choice to promote *is* the evidence bar for that path, so it
//! intentionally bypasses [`SKILL_REPETITION_THRESHOLD`] — but it shares the
//! exact same model-invocation seam and the exact same parked-write path as
//! automatic detection (see [`SkillDistiller::write_parked_skill`]), so
//! nothing it produces skips the trust gate either.
//!
//! The resulting template is registered through the exact same primitives
//! `SkillRegister` uses to write a skill (`ao_engine_tools_core::skill_registry
//! ::dispatch::write_skill_to_user_pool`), with the trust gate's verdict
//! forced into the frontmatter exactly like `SkillRegister` does — see
//! [`SkillDistiller::run`]. A distilled skill is `CandidateOrigin::Reflected`,
//! which `stage_candidate` always stages for review (nothing
//! proposed out-of-band skips review, and a `Skill` artifact never
//! auto-enables regardless), so it lands on disk with
//! `disable-model-invocation: true` and is never model-invocable until a
//! human confirms it.

#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};

use regex::Regex;
use uuid::Uuid;

use ao_engine_tools_core::skill_registry::dispatch::{
    validate_skill_description, validate_skill_name, write_skill_to_user_pool,
};
use ao_engine_tools_core::skill_registry::{
    set_disable_model_invocation, set_distilled_from, set_distilled_origin, SkillRegistry,
};
use ao_engine_tools_core::trust_gate::{
    stage_candidate, ArtifactType, CandidateOrigin, CandidateScope, StagingRequest,
};
use ao_engine_tools_engine::memory::contradiction::{default_scorer, SimilarityScorer};
use ao_engine_tools_runner::provider::ProviderClient;
use ao_engine_tools_runner::reflection::{
    GeneralizedSkill, ProviderSkillGeneralizer, SkillGeneralizationEngine,
};
use ao_persistence::PersistenceLayer;
use ao_protocol::agent::AgentProfile;
use ao_protocol::outcome::ArtifactKind;
use ao_protocol::reflection_candidate::{ReflectionCandidate, ReflectionCandidateStatus};

use crate::reflection_subscriber::ProviderResolver;

/// Minimum number of similar `Skill`-kind reflection candidates required
/// before a group is treated as a recurring procedure worth distilling. A
/// plain repetition-count threshold is the accepted first cut when wiring full outcome-based ranking is heavier than the
/// milestone calls for — see the `TODO(outcome-ranking)` below for what a
/// richer version would add.
pub const SKILL_REPETITION_THRESHOLD: usize = 2;

/// Two `Skill` candidates are treated as observations of "the same
/// procedure" when their content similarity clears this bar. Reuses
/// `ao_engine_tools_engine::memory::contradiction`'s normalized-token-overlap
/// scorer — the algorithm itself (Jaccard similarity over a lowercase
/// alphanumeric token set) has nothing memory-specific about it, so this
/// reuses the existing string-similarity seam rather than growing a second
/// one for skills. Set independently from that module's own
/// `CONTRADICTION_THRESHOLD` since procedure descriptions and memory facts
/// have different length/vocabulary shapes; tune if real distillation
/// candidates cluster too eagerly or not eagerly enough.
pub const SKILL_SIMILARITY_THRESHOLD: f32 = 0.5;

/// Result of one [`SkillDistiller::run`] call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DistillationOutcome {
    /// Names of the skills written this pass, one per qualifying group.
    pub skills_distilled: Vec<String>,
}

impl DistillationOutcome {
    fn no_op() -> Self {
        Self::default()
    }
}

/// Result of one [`SkillDistiller::generalize_single`] call: the same
/// `{name, description, body}` triple [`GeneralizedSkill`] already carries
/// for the cluster path, plus the slug the resulting parked `SKILL.md` was
/// actually written under. `written_as` can differ from `name` — the model's
/// suggested name is still run through the same sanitization and
/// collision-breaking [`unique_skill_name`] applies on the cluster path, so
/// a caller (e.g. a review UI) needs `written_as` to know which skill on
/// disk this call produced.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneralizedTemplate {
    pub name: String,
    pub description: String,
    pub body: String,
    /// The on-disk skill name the parked `SKILL.md` was written under.
    pub written_as: String,
}

/// Orchestrates distillation for one agent's pending `Skill` reflection
/// candidates. Holds the exact same two collaborators
/// `reflection_subscriber::ReflectionSubscriber` does — `PersistenceLayer`
/// and a `ProviderResolver` — so production wiring (`crate::state`) always
/// constructs both from the same `build_reflection_provider` function; see
/// that module's construction site for the shared rationale.
#[derive(Clone)]
pub struct SkillDistiller {
    persistence: Arc<PersistenceLayer>,
    resolve_provider: ProviderResolver,
}

impl SkillDistiller {
    pub fn new(persistence: Arc<PersistenceLayer>, resolve_provider: ProviderResolver) -> Self {
        Self {
            persistence,
            resolve_provider,
        }
    }

    /// Detect and distill repeated procedures among `agent_id`'s currently
    /// pending `Skill` reflection candidates.
    ///
    /// Cheap no-op when nothing clears [`SKILL_REPETITION_THRESHOLD`]: no
    /// provider is resolved and no model call is made, mirroring the
    /// reflection pass's own "empty delta -> no provider call" contract.
    pub async fn run(&self, agent_id: &str) -> Result<DistillationOutcome, String> {
        let pending = self
            .persistence
            .reflection_staging
            .list_pending(agent_id)
            .await
            .map_err(|e| format!("failed to load pending reflection candidates: {e}"))?;

        let skill_candidates: Vec<&ReflectionCandidate> = pending
            .iter()
            .filter(|c| c.kind == ArtifactKind::Skill)
            .collect();

        let scorer = default_scorer();
        let mut groups = group_by_similarity(&skill_candidates, scorer.as_ref(), SKILL_SIMILARITY_THRESHOLD);

        // TODO(outcome-ranking): once `OutcomeRecord` can be
        // joined back to the turn a `Skill` candidate was observed on,
        // weight a group by how often that turn's signal was
        // positive/implicit-success rather than by repetition count alone —
        // a procedure that reliably worked should outrank one that recurred
        // only because earlier attempts needed correction. Sorting by group
        // size is the accepted first cut.
        groups.retain(|g| g.len() >= SKILL_REPETITION_THRESHOLD);
        groups.sort_by(|a, b| b.len().cmp(&a.len()));

        if groups.is_empty() {
            return Ok(DistillationOutcome::no_op());
        }

        // Resolve the SAME way the reflection pass does — the optional
        // `reflection_agent_id` preference, falling back to the agent whose
        // candidates these are. `list_pending` already scoped every
        // candidate here to `agent_id`, so `agent_id` plays the role "the
        // thread's own agent" plays in `ReflectionSubscriber::run` (every
        // observation in every group came from this agent's own threads).
        let provider = self.resolve_generalizer_provider(agent_id).await?;
        let generalizer = ProviderSkillGeneralizer::new(provider);

        // The distilled skill is registered under the agent whose procedure
        // is being generalized, not whichever profile the preference above
        // picked to propose it — that preference only selects which
        // (possibly cheaper) model proposes the generalization, never who
        // the resulting artifact belongs to. Reassigned after every
        // successful write below (to the updated profile
        // `write_parked_skill` returns) so a second qualifying group in the
        // same pass sees the first group's newly registered skill in its own
        // collision check.
        let mut owner_profile = self.load_agent_profile(agent_id).await?;

        let mut skills_distilled = Vec::new();
        for group in groups {
            let observations: Vec<String> = group.iter().map(|c| c.content.clone()).collect();
            let generalized = match generalizer.generalize(&observations).await {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!("skill distillation: generalization call failed: {e}");
                    continue;
                }
            };

            let source_candidate_ids: Vec<String> = group.iter().map(|c| c.id.clone()).collect();
            match self
                .write_parked_skill(agent_id, &owner_profile, &generalized, &source_candidate_ids)
                .await
            {
                Ok((skill_name, updated_profile)) => {
                    owner_profile = updated_profile;
                    skills_distilled.push(skill_name);
                }
                Err(e) => tracing::warn!("skill distillation: {e}"),
            }
        }

        Ok(DistillationOutcome { skills_distilled })
    }

    /// Run the exact same generalization model call [`Self::run`]'s cluster
    /// path uses, but against a single reflection candidate — the
    /// human-initiated "promote this one observation" path a reviewer can
    /// trigger straight from one `Skill` candidate without waiting for it to
    /// repeat [`SKILL_REPETITION_THRESHOLD`] times.
    ///
    /// Resolves its model/credentials through the exact same
    /// [`crate::reflection_subscriber::ProviderResolver`] /
    /// `Arc<dyn ProviderClient>` seam `run` does — there is no second
    /// provider-resolution path here. The resulting template is written to
    /// disk through [`Self::write_parked_skill`], the same helper `run`
    /// uses, so a manually promoted skill is parked exactly like an
    /// automatically distilled one (`disable-model-invocation: true`,
    /// `origin: distilled`, `distilled-from: [observation.id]`) and needs
    /// the same human review step before it becomes model-invocable.
    pub async fn generalize_single(
        &self,
        observation: &ReflectionCandidate,
    ) -> Result<GeneralizedTemplate, String> {
        if observation.kind != ArtifactKind::Skill {
            return Err(format!(
                "reflection candidate '{}' is not a Skill-kind candidate and cannot be generalized into a skill",
                observation.id
            ));
        }
        // A non-`Pending` candidate has already been through the pipeline
        // once (most importantly `Distilled`, meaning some earlier pass —
        // cluster or single — already turned it into a parked skill): a
        // second call here would spend a model call to mint a second parked
        // skill from the same observation. Reject rather than silently
        // re-processing.
        if observation.status != ReflectionCandidateStatus::Pending {
            return Err(format!(
                "reflection candidate '{}' is not pending (status: {:?}) and cannot be generalized again",
                observation.id, observation.status
            ));
        }
        if observation.content.trim().is_empty() {
            return Err(format!(
                "reflection candidate '{}' has no content to generalize from",
                observation.id
            ));
        }

        let provider = self.resolve_generalizer_provider(&observation.agent_id).await?;
        let generalizer = ProviderSkillGeneralizer::new(provider);
        let generalized = generalizer
            .generalize(std::slice::from_ref(&observation.content))
            .await
            .map_err(|e| format!("generalization call failed: {e}"))?;

        let owner_profile = self.load_agent_profile(&observation.agent_id).await?;
        let (written_as, _updated_profile) = self
            .write_parked_skill(
                &observation.agent_id,
                &owner_profile,
                &generalized,
                std::slice::from_ref(&observation.id),
            )
            .await?;

        Ok(GeneralizedTemplate {
            name: generalized.name,
            description: generalized.description,
            body: generalized.body,
            written_as,
        })
    }

    /// Resolve which provider/profile proposes a generalization for
    /// `source_agent_id`'s procedure: the optional `reflection_agent_id`
    /// preference if set, else `source_agent_id` itself — the same fallback
    /// `ReflectionSubscriber` uses. Shared by both [`Self::run`] and
    /// [`Self::generalize_single`] so there is exactly one place this
    /// fallback logic lives.
    ///
    /// Hard rule: never construct a provider client directly here — only
    /// ever consume whatever the injected resolver hands back, the same
    /// contract `ReflectionSubscriber` upholds.
    async fn resolve_generalizer_provider(
        &self,
        source_agent_id: &str,
    ) -> Result<Arc<dyn ProviderClient>, String> {
        let prefs = self
            .persistence
            .preferences
            .get()
            .await
            .map_err(|e| format!("failed to load preferences for distillation: {e}"))?
            .unwrap_or_default();
        let generalizer_profile_id = prefs.reflection_agent_id.unwrap_or_else(|| source_agent_id.to_string());
        let generalizer_profile = self
            .persistence
            .agents
            .get(&generalizer_profile_id)
            .await
            .map_err(|e| format!("failed to load agent profile '{generalizer_profile_id}': {e}"))?
            .ok_or_else(|| format!("distillation agent profile '{generalizer_profile_id}' not found"))?;

        (self.resolve_provider)(&generalizer_profile).ok_or_else(|| {
            format!("no provider configured for distillation agent profile '{generalizer_profile_id}'")
        })
    }

    async fn load_agent_profile(&self, agent_id: &str) -> Result<AgentProfile, String> {
        self.persistence
            .agents
            .get(agent_id)
            .await
            .map_err(|e| format!("failed to load agent profile '{agent_id}': {e}"))?
            .ok_or_else(|| format!("agent profile '{agent_id}' not found"))
    }

    /// Validate, gate, and persist a generalization result as a **parked**
    /// `SKILL.md` under `owner_agent_id`'s skill pool. This is the single
    /// write path both [`Self::run`]'s cluster generalization and
    /// [`Self::generalize_single`]'s one-observation generalization share —
    /// the trust gate and provenance stamping below only need to exist
    /// once, and both callers are guaranteed to produce a skill in the exact
    /// same parked state.
    ///
    /// On success, marks every id in `source_candidate_ids` `Distilled` in
    /// the reflection staging pool (best-effort: a failure to update status
    /// is logged but does not undo the already-written skill, matching
    /// `run`'s prior behavior) and returns the skill's final on-disk name
    /// together with the agent profile `write_skill_to_user_pool` updated
    /// (now listing the new skill).
    async fn write_parked_skill(
        &self,
        owner_agent_id: &str,
        owner_profile: &AgentProfile,
        generalized: &GeneralizedSkill,
        source_candidate_ids: &[String],
    ) -> Result<(String, AgentProfile), String> {
        let data_dir = self.persistence.data_root.root();
        let registry = SkillRegistry::load(data_dir, owner_profile);
        let skill_name = unique_skill_name(&generalized.name, &registry);
        let description = sanitize_description(&generalized.description);

        validate_skill_name(&skill_name)
            .map_err(|msg| format!("generated an invalid skill name, skipping: {msg}"))?;
        validate_skill_description(&description)
            .map_err(|msg| format!("generated an invalid description, skipping: {msg}"))?;

        let raw_body = render_skill_markdown(&skill_name, &description, &generalized.body);

        // Identical gating shape to `SkillRegister` — a distilled
        // skill is `CandidateOrigin::Reflected` (the origin reserved for
        // both the reflection pass and this distillation producer), which
        // always stages for review regardless of scope or contradiction,
        // and a `Skill` artifact never auto-enables anyway (tier 2a) — so
        // this always quarantines regardless of whatever
        // `disable-model-invocation` value the model's own reply implies.
        let decision = stage_candidate(StagingRequest {
            artifact_type: ArtifactType::Skill,
            origin: CandidateOrigin::Reflected,
            scope: CandidateScope::Agent,
            contradicts_existing: false,
            overwrites_manual: false,
        });
        debug_assert!(!decision.auto_enable(), "a distilled skill must always quarantine");

        let gated_body = set_disable_model_invocation(&raw_body, !decision.auto_enable())
            .map_err(|e| format!("could not apply the trust gate: {e}"))?;
        // Provenance: `origin: distilled` marks *that*
        // this skill was machine-authored from observed behavior;
        // `distilled-from` records *which* reflection candidate(s) it
        // generalizes, so a human reviewing the staging gate can trace it
        // back to the concrete turn(s) it came from. A brand-new distilled
        // skill always starts at version 1 — `unique_skill_name` guarantees
        // no collision with an existing skill, so there is nothing to bump.
        let provenanced_body = set_distilled_origin(&gated_body)
            .map_err(|e| format!("could not stamp provenance: {e}"))?;
        let provenanced_body = set_distilled_from(&provenanced_body, source_candidate_ids)
            .map_err(|e| format!("could not stamp source candidate ids: {e}"))?;

        let (_, updated_profile) = write_skill_to_user_pool(
            data_dir,
            owner_agent_id,
            &skill_name,
            &provenanced_body,
            false,
            &registry,
        )
        .await
        .map_err(|e| format!("failed to register skill '{skill_name}': {e}"))?;

        if let Err(e) = self
            .persistence
            .reflection_staging
            .update_status(owner_agent_id, source_candidate_ids, ReflectionCandidateStatus::Distilled)
            .await
        {
            tracing::warn!(
                "wrote skill '{skill_name}' but failed to mark its source candidates \
                 distilled — they may be re-clustered next pass: {e}"
            );
        }

        Ok((skill_name, updated_profile))
    }
}

/// Group candidates by mutual content similarity (single-linkage: a
/// candidate joins the first existing group containing any member whose
/// score against it clears `threshold`). Small-`n` friendly (O(n^2)) —
/// pending `Skill` candidates for one agent are expected to number in the
/// tens at most, not thousands, since the reflection pass itself only stages
/// a handful of candidates per delta.
///
/// Similarity is scored on [`normalize_for_clustering`]'s output, not the
/// raw `content`, so two observations of the same procedure still cluster
/// when the only thing that differs between them is which file/line the
/// procedure happened to touch that occasion. The un-normalized `content` on
/// each candidate is untouched by this — grouping is the only thing that
/// sees the normalized form.
fn group_by_similarity<'a>(
    candidates: &[&'a ReflectionCandidate],
    scorer: &dyn SimilarityScorer,
    threshold: f32,
) -> Vec<Vec<&'a ReflectionCandidate>> {
    let normalized: Vec<String> = candidates
        .iter()
        .map(|c| normalize_for_clustering(&c.content))
        .collect();

    let mut groups: Vec<Vec<usize>> = Vec::new();
    'candidates: for i in 0..candidates.len() {
        for group in groups.iter_mut() {
            let joins = group
                .iter()
                .any(|&member_idx| scorer.score(&normalized[member_idx], &normalized[i]) >= threshold);
            if joins {
                group.push(i);
                continue 'candidates;
            }
        }
        groups.push(vec![i]);
    }
    groups
        .into_iter()
        .map(|idxs| idxs.into_iter().map(|i| candidates[i]).collect())
        .collect()
}

/// Strip incidental, instance-specific tokens from an observation's content
/// before it is compared for clustering, so two observations of the same
/// underlying procedure still land in the same similarity group even when
/// the concrete file(s)/line(s) they happened to touch differ. This is a
/// clustering-only transform: the caller must keep feeding the model the
/// *original*, un-normalized `content` (the generalization prompt needs the
/// real concrete instances, not a scrubbed version), and this function's
/// output is never persisted anywhere.
///
/// Conservatively strips, in order:
/// 1. Path-like tokens — any whitespace-delimited run of characters that
///    contains a `/` (e.g. `frontend/src/components/SettingsView.tsx:120`).
/// 2. Bare filenames with a common source/config extension and no path
///    separator (e.g. `AppShell.tsx`, `main.rs`).
/// 3. Line-number mentions — `line 42`/`line: 42` (case-insensitive),
///    `:42`-style trailing line references, and `L42`-style references.
///
/// Everything else (prose, verbs, error text, command names) is left as-is —
/// this only targets the two token classes flagged as "incidental
/// instance tokens", not a general-purpose redaction pass.
///
/// Known, accepted imprecision: the `:\d+` line-reference pattern also
/// matches non-line-number colon-number text (a ratio like `3:1`, a time
/// like `12:30`). Left as-is deliberately — this function's output only
/// ever feeds a similarity *threshold* (never the model, never disk), so an
/// over-eager strip here can only ever nudge two observations toward
/// clustering, never corrupt anything a human or the model sees.
fn normalize_for_clustering(content: &str) -> String {
    static PATH_RE: OnceLock<Regex> = OnceLock::new();
    static FILENAME_RE: OnceLock<Regex> = OnceLock::new();
    static LINE_RE: OnceLock<Regex> = OnceLock::new();

    let path_re = PATH_RE.get_or_init(|| Regex::new(r"\S*/\S+").expect("valid regex"));
    let filename_re = FILENAME_RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b[\w-]+\.(rs|tsx?|jsx?|json|toml|ya?ml|md|py|go|rb|css|s?html|vue|svelte|sh|txt)\b",
        )
        .expect("valid regex")
    });
    let line_re = LINE_RE.get_or_init(|| {
        Regex::new(r"(?i)\bline\s*#?\s*\d+\b|\bl\d+\b|:\d+\b").expect("valid regex")
    });

    let stripped = path_re.replace_all(content, " ");
    let stripped = filename_re.replace_all(&stripped, " ");
    let stripped = line_re.replace_all(&stripped, " ");
    stripped.into_owned()
}

/// Reduce a model-suggested name to something `validate_skill_name` accepts:
/// lowercase, non-`[a-z0-9_-]` runs collapsed to a single `-`, trimmed of
/// leading/trailing `-`, capped at 64 chars. Falls back to a UUID-suffixed
/// generic name if the input reduces to nothing usable (e.g. the model
/// replied in a script with no ASCII alphanumerics).
fn sanitize_skill_name(raw: &str) -> String {
    let mut out = String::new();
    let mut last_was_dash = false;
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            out.push(lower);
            last_was_dash = false;
        } else if !last_was_dash && !out.is_empty() {
            out.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-');
    let capped: String = trimmed.chars().take(64).collect();
    let capped = capped.trim_end_matches('-');

    if capped.is_empty() {
        format!("distilled-skill-{}", short_uuid())
    } else {
        capped.to_string()
    }
}

/// Append a short, collision-breaking suffix if `sanitize_skill_name(raw)`
/// already names a skill in `registry` — a distilled skill must never
/// silently overwrite an existing one (`write_skill_to_user_pool` is always
/// called with `override_existing: false` here).
fn unique_skill_name(raw: &str, registry: &SkillRegistry) -> String {
    let base = sanitize_skill_name(raw);
    if registry.get(&base).is_none() {
        return base;
    }
    // Leave room for the `-{8-char uuid}` suffix within the 64-char cap
    // `validate_skill_name` enforces.
    let truncated_base: String = base.chars().take(64 - 9).collect();
    let truncated_base = truncated_base.trim_end_matches('-');
    format!("{truncated_base}-{}", short_uuid())
}

fn short_uuid() -> String {
    Uuid::new_v4().to_string()[..8].to_string()
}

/// Render a `SKILL.md`-shaped string from already-sanitized `name` /
/// `description` and a generalized `body`. Builds the frontmatter through a
/// YAML mapping rather than hand-formatting a `name: {name}`-style string —
/// `description`/`body` are model output and may contain characters (a
/// colon-space, a quote, an embedded `---`) that would otherwise corrupt or
/// escape the intended frontmatter block if interpolated directly into raw
/// YAML text.
fn render_skill_markdown(name: &str, description: &str, body: &str) -> String {
    let mut mapping = serde_yaml::Mapping::new();
    mapping.insert(
        serde_yaml::Value::String("name".to_string()),
        serde_yaml::Value::String(name.to_string()),
    );
    mapping.insert(
        serde_yaml::Value::String("description".to_string()),
        serde_yaml::Value::String(description.to_string()),
    );
    mapping.insert(
        serde_yaml::Value::String("context".to_string()),
        serde_yaml::Value::String("inline".to_string()),
    );
    let yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .unwrap_or_default();
    let yaml = yaml.trim_end_matches('\n');
    format!("---\n{yaml}\n---\n{body}\n")
}

/// Cap a model-suggested description at the 240-char limit
/// `validate_skill_description` enforces, falling back to a generic
/// description if the model returned nothing usable.
fn sanitize_description(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "Distilled from a repeated procedure observed across turns.".to_string();
    }
    trimmed.chars().take(240).collect()
}
