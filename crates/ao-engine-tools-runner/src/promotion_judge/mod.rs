//! Skeptical, genericity-forcing promotion judge.
//!
//! [`ProviderPromotionJudge`] is the sole implementation of the "does this
//! thread-scope note actually generalize beyond the thread it was captured
//! in?" model call: a single uncached call through the existing
//! `ProviderClient` seam, no tools, no chat history — mirrors
//! `verification::ProviderVerificationEngine` and
//! `thread_summary::ProviderThreadSummarizer` deliberately: never
//! a bespoke provider path" rule. The verdict type it produces
//! ([`PromotionVerdict`]) is defined in `ao_engine_tools_engine`, not here —
//! that crate owns turning a `Promote` verdict into a staged candidate, and
//! this crate already depends on it, so the verdict shape has exactly one
//! source of truth rather than two crates agreeing on a duplicate.
//!
//! Deliberately a SEPARATE system prompt from the reflection pass's own
//! proposal prompt (`crate::reflection::REFLECTION_SYSTEM_PROMPT`) and its
//! skill-generalization prompt (`crate::reflection::
//! GENERALIZATION_SYSTEM_PROMPT`) — the whole point of a second, skeptical
//! pass is that it does not inherit the first pass's blind spots. Where the
//! reflection pass asks "is this worth remembering at all?", this judge
//! asks a narrower, harder question about something ALREADY proposed:
//! "does this actually generalize, or did the process that wrote it
//! mistake a one-off detail for a lasting fact?" — the same distillation
//! move skill distillation makes, applied here to memory, and
//! adversarially rather than generatively.

#[cfg(test)]
mod tests;

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use ao_engine_tools_engine::memory::promotion::PromotionVerdict;

use crate::{
    message::{ContentBlock, Message},
    provider::{CompletionEvent, CompletionRequest, ProviderClient},
};

const JUDGE_SYSTEM_PROMPT: &str = "\
You are a skeptical auditor, not the author of the note below. A separate, \
more optimistic process already wrote it and judged it worth keeping — your \
job is NOT to agree with that judgment, but to catch cases where it \
over-generalized something that only made sense inside the one conversation \
thread it was captured in.

The only question that decides your verdict: would this note genuinely help \
across MANY FUTURE threads, or is it specific to the thread it came from? \
Judge it against that question directly. Confident or general-sounding \
phrasing is not evidence of genuine generality — restate the underlying \
claim in your own head and ask whether it would still be true and useful in \
a completely unrelated conversation.

Guidelines:
- Default to \"reject\" when you are genuinely unsure. A missed promotion \
  costs nothing; a wrongly promoted thread-specific note pollutes durable \
  memory for every future thread that reads it.
- Reject anything that only makes sense with knowledge of one particular \
  file, task, ticket, error message, person, or conversation — even when it \
  is phrased as a general-sounding statement.
- Promote only a note stating a durable fact, preference, or convention \
  that would still be true and useful in a completely unrelated \
  conversation.
- When you promote, do not restate the note verbatim. Rewrite it into the \
  general, reusable form: strip out any thread-specific instance details \
  (concrete file names, one-off values, this-conversation's particulars) \
  that would not survive outside this thread. If nothing generalizable \
  remains once those are stripped, reject it instead.

Respond with ONLY valid JSON — no prose before or after — in this exact \
shape:

{\"verdict\": \"promote\" | \"reject\", \"generalized_content\": \"the reusable, thread-agnostic rewrite — required and non-empty only when verdict is 'promote'\", \"rationale\": \"one sentence explaining the verdict\"}";

/// Nudge appended to a retry after an unparseable first reply. Distinct
/// wording from `verification::VERDICT_RETRY_NUDGE` so a future reader
/// grepping this crate for a nudge string lands on the judge that actually
/// emitted it.
const VERDICT_RETRY_NUDGE: &str =
    "Respond again with ONLY the JSON object described above — no prose, no code fences, no \
     commentary.";

/// Pluggable "decide whether one thread-scope note generalizes" back-end.
///
/// Defined as a trait (rather than exposing [`ProviderPromotionJudge`]
/// directly) so orchestration above this crate can inject a scripted test
/// double without depending on a live provider — the same split
/// `ReflectionProposalEngine`/`VerificationEngine`/`ThreadSummarizationEngine`
/// already use.
#[async_trait]
pub trait PromotionJudgeEngine: Send + Sync {
    async fn judge(&self, thread_entry_content: &str) -> Result<PromotionVerdict, String>;
}

/// Production implementation: makes a single uncached model call through the
/// existing `ProviderClient` seam — the injected `provider`, constructed
/// upstream from whichever `AgentProfile` the caller resolved (the
/// optional `reflection_agent_id` preference, else the thread's own agent).
/// This struct never resolves an `AgentProfile` or builds a provider client
/// itself; it only ever drives the one it is handed.
pub struct ProviderPromotionJudge {
    provider: Arc<dyn ProviderClient>,
}

impl ProviderPromotionJudge {
    pub fn new(provider: Arc<dyn ProviderClient>) -> Self {
        Self { provider }
    }

    /// Issue a single judge completion and drain the stream into one text
    /// blob. Returns `Err` only on a provider/transport/stream failure.
    async fn run_completion(&self, user_content: &str) -> Result<String, String> {
        let messages = vec![Message::User {
            content: vec![ContentBlock::Text {
                text: user_content.to_string(),
            }],
        }];

        let request = CompletionRequest {
            messages,
            system_prompt: Some(JUDGE_SYSTEM_PROMPT.to_string()),
            tools: vec![],
            ..Default::default()
        };

        let cancel = CancellationToken::new();
        let mut stream = self
            .provider
            .complete(request, cancel)
            .await
            .map_err(|e| format!("promotion judge provider error: {e}"))?;

        let mut text = String::new();
        loop {
            match stream.recv().await {
                None => break,
                Some(Ok(CompletionEvent::AssistantText(chunk))) => text.push_str(&chunk),
                Some(Ok(CompletionEvent::TurnComplete { .. })) => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("promotion judge stream error: {e}")),
            }
        }

        Ok(text)
    }
}

#[async_trait]
impl PromotionJudgeEngine for ProviderPromotionJudge {
    /// Judge one thread-scope entry's content. Never returns `Err` for a
    /// provider failure or an unparseable reply — both degrade to a safe
    /// [`PromotionVerdict::Reject`] instead (fail-safe: a promotion pass
    /// that cannot get a clean verdict must never accidentally promote).
    /// `Err` is reserved for cases that indicate a caller bug, of which
    /// there are none in this implementation today.
    async fn judge(&self, thread_entry_content: &str) -> Result<PromotionVerdict, String> {
        let user_content = build_user_message(thread_entry_content);

        let first_text = match self.run_completion(&user_content).await {
            Ok(t) => t,
            Err(e) => {
                return Ok(PromotionVerdict::Reject {
                    rationale: format!(
                        "promotion judge call failed, defaulting to reject: {e}"
                    ),
                })
            }
        };

        if let Ok(verdict) = parse_verdict(&first_text) {
            return Ok(verdict);
        }

        // The judge's reply was not parseable. Retry exactly once, echoing
        // the unparseable reply back with an explicit format nudge — mirrors
        // `verification::ProviderVerificationEngine`'s retry shape, since a
        // CLI-backed provider may be a stateless binary that retains nothing
        // between invocations.
        let retry_content = format!(
            "{user_content}\n\n## Previous reply (could not be parsed as JSON)\n\n{first_text}\n\n\
             {VERDICT_RETRY_NUDGE}"
        );
        let retry_text = match self.run_completion(&retry_content).await {
            Ok(t) => t,
            Err(e) => {
                return Ok(PromotionVerdict::Reject {
                    rationale: format!(
                        "promotion judge retry failed, defaulting to reject: {e}"
                    ),
                })
            }
        };

        match parse_verdict(&retry_text) {
            Ok(verdict) => Ok(verdict),
            Err(e) => Ok(PromotionVerdict::Reject {
                rationale: format!(
                    "promotion judge did not return a parseable verdict after a retry, \
                     defaulting to reject: {e}"
                ),
            }),
        }
    }
}

fn build_user_message(content: &str) -> String {
    format!(
        "# Thread-scope note proposed for promotion\n\n{content}\n\n\
         Respond with the JSON verdict only."
    )
}

fn parse_verdict(raw: &str) -> Result<PromotionVerdict, String> {
    let trimmed = raw.trim();
    let value = extract_object_value(trimmed)
        .ok_or_else(|| "no parseable JSON verdict object found in model output".to_string())?;

    let verdict = value
        .get("verdict")
        .and_then(|v| v.as_str())
        .filter(|s| *s == "promote" || *s == "reject")
        .ok_or_else(|| {
            "missing or invalid 'verdict' field (expected 'promote' or 'reject')".to_string()
        })?;

    let rationale = value
        .get("rationale")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if verdict == "reject" {
        return Ok(PromotionVerdict::Reject { rationale });
    }

    let generalized_content = value
        .get("generalized_content")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "verdict 'promote' requires a non-empty 'generalized_content' field".to_string()
        })?
        .to_string();

    Ok(PromotionVerdict::Promote {
        generalized_content,
        rationale,
    })
}

/// Locate a JSON object inside arbitrary model output, tolerating a fenced
/// code block or light prose wrapping. Mirrors
/// `verification::extract_verdict_value`'s "try fenced, then raw, then
/// first balanced substring" strategy — duplicated rather than shared, the
/// same way `reflection`'s array/object parsing helpers are already
/// duplicated per call shape in this crate.
fn extract_object_value(trimmed: &str) -> Option<serde_json::Value> {
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(fenced) = fenced_block(trimmed) {
        candidates.push(fenced);
    }
    candidates.push(trimmed);
    candidates.extend(top_level_json_objects(trimmed));

    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
            if value.is_object() && value.get("verdict").is_some() {
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

/// Collect every balanced top-level `{...}` substring, in document order,
/// honoring string literals so a `}` inside a quoted value does not close
/// an object early.
fn top_level_json_objects(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut objects = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = balanced_object_end(bytes, i) {
                objects.push(&text[i..=end]);
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }

    objects
}

fn balanced_object_end(bytes: &[u8], start: usize) -> Option<usize> {
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
                    return Some(i);
                }
            }
            _ => {}
        }
    }

    None
}
