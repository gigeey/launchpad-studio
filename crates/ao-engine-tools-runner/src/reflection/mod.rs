//! OBSERVE reflection pass: the model-invoking half of the self-improvement
//! loop.
//!
//! [`ProviderReflectionProposer`] is the sole implementation of the
//! "propose candidate memories/skills from a transcript delta" step: a
//! single uncached model call through the existing `ProviderClient` seam,
//! with no tools and no chat history — the delta text handed in is the
//! entire input. This mirrors `thread_summary::ProviderThreadSummarizer`
//! and `verification::ProviderVerificationEngine` deliberately: this requires
//! every out-of-band pass that invokes a model to drive it through the same
//! proven `Arc<dyn ProviderClient>` seam those two already use, rather than
//! standing up a new provider/API path. The orchestration around this call
//! (resolving *which* profile's provider client to hand in, reading the
//! untrimmed transcript delta, staging results through the trust gate,
//! advancing the watermark) lives above this crate, in `ao-engine`, since
//! only that crate can resolve an `AgentProfile` into a concrete provider
//! client without a circular dependency.

#[cfg(test)]
mod tests;

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use ao_protocol::outcome::ArtifactKind;

use crate::{
    message::{ContentBlock, Message},
    provider::{CompletionEvent, CompletionRequest, ProviderClient},
};

/// One candidate the model proposed from a transcript delta, before it has been
/// through the trust gate or (for memory) the contradiction check — both happen
/// in the orchestration layer that calls [`ReflectionProposalEngine::propose`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReflectionProposal {
    pub kind: ArtifactKind,
    pub content: String,
    /// The model's own estimate, in `[0.0, 1.0]`, of how durable and
    /// generally useful this candidate is — as opposed to something tied
    /// narrowly to this one conversation. The orchestration layer (not this
    /// module) decides what a low score means for where the candidate ends
    /// up; this module only reads it off the model's reply and clamps it
    /// into range. Defaults to [`DEFAULT_PROPOSAL_CONFIDENCE`] when the
    /// model's reply omits or malforms the field.
    pub confidence: f32,
}

/// Pluggable "read a transcript delta, propose candidates" back-end.
///
/// Defined as a trait (rather than exposing [`ProviderReflectionProposer`]
/// directly) so the orchestration layer can inject a scripted test double
/// without depending on this crate's provider-driven implementation, mirroring
/// `ThreadSummarizationEngine`/`VerificationEngine`'s split between trait and
/// provider-backed implementation.
#[async_trait]
pub trait ReflectionProposalEngine: Send + Sync {
    async fn propose(&self, delta_text: &str) -> Result<Vec<ReflectionProposal>, String>;
}

const REFLECTION_SYSTEM_PROMPT: &str = "\
You are reviewing a slice of an agent's conversation history that is about to \
fall out of its working context window. Your job is to spot two kinds of \
durable takeaway worth rescuing before this content is forgotten:

- memory: a standalone fact, preference, decision, or correction worth \
  remembering independently of this conversation (e.g. a stated preference, a \
  project convention, a corrected misunderstanding).
- skill: a concrete multi-step procedure the agent carried out that looks \
  reusable if the same kind of task comes up again (e.g. a specific \
  build-verify-fix sequence). Describe the procedure as it was actually \
  carried out here — a later pass turns it into a reusable template.

Guidelines:
- Only propose a candidate with real evidence in the text below; never invent \
  one. Proposing nothing is correct and expected for most slices of \
  conversation — most turns produce no durable takeaway at all.
- Keep each candidate's content short, self-contained, and understandable \
  without the surrounding conversation.
- For each candidate, also give a \"confidence\" from 0.0 to 1.0: how sure \
  you are this would help in OTHER future conversations, not just this one. \
  A specific one-off detail tied to this conversation is low confidence \
  (e.g. 0.2); a clearly recurring preference, convention, or correction is \
  high confidence (e.g. 0.9). When unsure, guess low rather than high.
- Respond with ONLY valid JSON — no prose before or after — as an array \
  (an empty array `[]` when nothing is worth keeping) in this exact shape:

[
  {\"kind\": \"memory\", \"content\": \"...\", \"confidence\": 0.8},
  {\"kind\": \"skill\", \"content\": \"...\", \"confidence\": 0.6}
]";

/// Production implementation: makes a single uncached model call through the
/// existing `ProviderClient` seam.
pub struct ProviderReflectionProposer {
    provider: Arc<dyn ProviderClient>,
}

impl ProviderReflectionProposer {
    pub fn new(provider: Arc<dyn ProviderClient>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl ReflectionProposalEngine for ProviderReflectionProposer {
    async fn propose(&self, delta_text: &str) -> Result<Vec<ReflectionProposal>, String> {
        let user_content = format!(
            "# Transcript delta to review\n\n{delta_text}\n\nRespond with the JSON array only."
        );

        let messages = vec![Message::User {
            content: vec![ContentBlock::Text { text: user_content }],
        }];

        let request = CompletionRequest {
            messages,
            system_prompt: Some(REFLECTION_SYSTEM_PROMPT.to_string()),
            tools: vec![],
            ..Default::default()
        };

        let cancel = CancellationToken::new();
        let mut stream = self
            .provider
            .complete(request, cancel)
            .await
            .map_err(|e| format!("reflection provider error: {e}"))?;

        let mut text = String::new();
        loop {
            match stream.recv().await {
                None => break,
                Some(Ok(CompletionEvent::AssistantText(chunk))) => text.push_str(&chunk),
                Some(Ok(CompletionEvent::TurnComplete { .. })) => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("reflection stream error: {e}")),
            }
        }

        // A malformed or empty reply degrades to "no candidates" rather than
        // a hard error — the system prompt already tells the model an empty
        // array is the expected, common case, so this pass should never let
        // a formatting slip block the trigger's caller (the "does not
        // add latency" contract: this is off-turn already, but a hard error
        // here would still lose the whole delta's candidates instead of just
        // skipping the ones that made it into unparseable output).
        Ok(parse_proposals(&text))
    }
}

/// Maximum candidates accepted from a single proposal call. A defensive
/// bound against a runaway or repetitive model response — not expected to be
/// hit in practice given the system prompt's "most turns propose nothing"
/// framing.
const MAX_CANDIDATES_PER_PASS: usize = 20;

/// Confidence assumed for a proposal item whose reply omits the
/// `confidence` field or gives a value that doesn't parse as a number —
/// e.g. a reply from before this field existed. Deliberately the top of the
/// range: a candidate this module can't read a confidence for must not be
/// silently treated as low-confidence by a caller that thresholds on it, so
/// an unparseable field degrades to "trust it" rather than "distrust it".
const DEFAULT_PROPOSAL_CONFIDENCE: f32 = 1.0;

fn parse_proposals(raw: &str) -> Vec<ReflectionProposal> {
    let trimmed = raw.trim();
    let Some(value) = extract_array_value(trimmed) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|item| {
            let kind = match item.get("kind").and_then(|v| v.as_str()) {
                Some("memory") => ArtifactKind::Memory,
                Some("skill") => ArtifactKind::Skill,
                _ => return None,
            };
            let content = item.get("content").and_then(|v| v.as_str())?.trim();
            if content.is_empty() {
                return None;
            }
            let confidence = item
                .get("confidence")
                .and_then(|v| v.as_f64())
                .map(|c| c.clamp(0.0, 1.0) as f32)
                .unwrap_or(DEFAULT_PROPOSAL_CONFIDENCE);
            Some(ReflectionProposal {
                kind,
                content: content.to_string(),
                confidence,
            })
        })
        .take(MAX_CANDIDATES_PER_PASS)
        .collect()
}

/// Locate a JSON array inside arbitrary model output, tolerating a fenced
/// code block or light prose wrapping. Mirrors
/// `verification::extract_verdict_value`'s "try fenced, then raw, then first
/// balanced substring" strategy, adapted to `[...]` instead of `{...}`.
fn extract_array_value(trimmed: &str) -> Option<serde_json::Value> {
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(fenced) = fenced_block(trimmed) {
        candidates.push(fenced);
    }
    candidates.push(trimmed);
    if let Some(bracketed) = first_balanced_array(trimmed) {
        candidates.push(bracketed);
    }

    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
            if value.is_array() {
                return Some(value);
            }
        }
    }
    None
}

fn fenced_block(text: &str) -> Option<&str> {
    let (open_len, start) = if let Some(idx) = text.find("```json") {
        (7, idx)
    } else if let Some(idx) = text.find("```") {
        (3, idx)
    } else {
        return None;
    };
    let after = &text[start + open_len..];
    let end = after.find("```")?;
    Some(after[..end].trim())
}

/// Return the first balanced top-level `[...]` substring, honoring string
/// literals so a `]` inside a quoted value doesn't close it early.
fn first_balanced_array(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('[')?;
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (i, &c) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

// --- distillation: generalizing a repeated procedure into a template ---
//
// A repetition trigger (multiple similar `Skill`-kind `ReflectionProposal`s
// observed across turns/sessions — see `ao_engine::skill_distillation`) is
// evidence a procedure recurs, but the concrete text of any one observation
// is still tied to whatever specific values that instance happened to use.
// [`SkillGeneralizationEngine`] is the second, separate model call this
// module offers: given several concrete observations of what is believed to
// be the same procedure, turn them into one reusable template with the
// varying parts parameterized. It is a distinct call shape from
// [`ReflectionProposalEngine`] (different prompt, different response
// contract) but drives the model through the exact same seam — same
// `Arc<dyn ProviderClient>`, same "no tools, one uncached call" pattern —
// deliberately: this must not open a second model-invocation path.

/// A reusable skill template the generalization pass produced from a group
/// of repeated concrete procedure observations.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneralizedSkill {
    /// A short, slug-friendly name suggestion. The caller still owns final
    /// validation/sanitization (`skill_registry::dispatch::validate_skill_name`)
    /// since a model reply is not a trusted input.
    pub name: String,
    pub description: String,
    /// The generalized procedure body — instructions with the varying parts
    /// called out as parameters rather than the concrete values any single
    /// observation used.
    pub body: String,
}

/// Pluggable "turn N concrete observations of the same procedure into one
/// template" back-end. Split from its provider-backed implementation for the
/// same reason [`ReflectionProposalEngine`] is: so orchestration can inject a
/// scripted test double without depending on a live provider.
#[async_trait]
pub trait SkillGeneralizationEngine: Send + Sync {
    async fn generalize(&self, observations: &[String]) -> Result<GeneralizedSkill, String>;
}

const GENERALIZATION_SYSTEM_PROMPT: &str = "\
You are given several concrete descriptions of what looks like the same \
multi-step procedure, each observed on a different occasion. Write ONE \
reusable template that captures the procedure in general form, with the \
parts that varied between observations called out as parameters instead of \
hard-coded to whichever concrete value one occasion happened to use.

Respond with ONLY valid JSON — no prose before or after — in this exact \
shape:

{\"name\": \"short-slug-like-name\", \"description\": \"one-line summary, under 200 characters\", \"body\": \"the generalized step-by-step procedure\"}

The name should be lowercase words separated by hyphens. The body should \
read as instructions someone (or an agent) could follow the next time this \
kind of task comes up, not as a report of what happened on any one occasion.";

/// Production implementation: makes a single uncached model call through the
/// existing `ProviderClient` seam — the same seam [`ProviderReflectionProposer`]
/// uses.
pub struct ProviderSkillGeneralizer {
    provider: Arc<dyn ProviderClient>,
}

impl ProviderSkillGeneralizer {
    pub fn new(provider: Arc<dyn ProviderClient>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl SkillGeneralizationEngine for ProviderSkillGeneralizer {
    async fn generalize(&self, observations: &[String]) -> Result<GeneralizedSkill, String> {
        if observations.is_empty() {
            return Err("cannot generalize from zero observations".to_string());
        }

        let mut user_content = String::from("# Repeated procedure observations\n\n");
        for (i, obs) in observations.iter().enumerate() {
            user_content.push_str(&format!("Observation {}: {}\n\n", i + 1, obs));
        }
        user_content.push_str("Respond with the JSON object only.");

        let messages = vec![Message::User {
            content: vec![ContentBlock::Text { text: user_content }],
        }];

        let request = CompletionRequest {
            messages,
            system_prompt: Some(GENERALIZATION_SYSTEM_PROMPT.to_string()),
            tools: vec![],
            ..Default::default()
        };

        let cancel = CancellationToken::new();
        let mut stream = self
            .provider
            .complete(request, cancel)
            .await
            .map_err(|e| format!("skill generalization provider error: {e}"))?;

        let mut text = String::new();
        loop {
            match stream.recv().await {
                None => break,
                Some(Ok(CompletionEvent::AssistantText(chunk))) => text.push_str(&chunk),
                Some(Ok(CompletionEvent::TurnComplete { .. })) => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("skill generalization stream error: {e}")),
            }
        }

        parse_generalized_skill(&text)
            .ok_or_else(|| format!("could not parse a generalized skill from reply: {text}"))
    }
}

fn parse_generalized_skill(raw: &str) -> Option<GeneralizedSkill> {
    let trimmed = raw.trim();
    let value = extract_object_value(trimmed)?;
    let name = value.get("name").and_then(|v| v.as_str())?.trim();
    let description = value.get("description").and_then(|v| v.as_str())?.trim();
    let body = value.get("body").and_then(|v| v.as_str())?.trim();
    if name.is_empty() || description.is_empty() || body.is_empty() {
        return None;
    }
    Some(GeneralizedSkill {
        name: name.to_string(),
        description: description.to_string(),
        body: body.to_string(),
    })
}

/// Locate a JSON object inside arbitrary model output. Same tolerance
/// strategy as [`extract_array_value`] (fenced block, then raw, then first
/// balanced substring), adapted to `{...}`.
fn extract_object_value(trimmed: &str) -> Option<serde_json::Value> {
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(fenced) = fenced_block(trimmed) {
        candidates.push(fenced);
    }
    candidates.push(trimmed);
    if let Some(braced) = first_balanced_object(trimmed) {
        candidates.push(braced);
    }

    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
            if value.is_object() {
                return Some(value);
            }
        }
    }
    None
}

/// Return the first balanced top-level `{...}` substring, honoring string
/// literals so a `}` inside a quoted value doesn't close it early.
fn first_balanced_object(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (i, &c) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}
