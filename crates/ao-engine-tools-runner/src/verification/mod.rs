//! Verification engines: production back-ends for the `ProjectVerify` tool.
//!
//! The trait is defined in `ao-engine-tools-core` so `RunnerContext` can hold
//! it without a circular dependency. This module provides two implementations:
//!
//! - [`ProviderVerificationEngine`] — the quick engine (mode=`"quick"`). A
//!   single uncached model call that judges the goal against tasklist summaries.
//! - [`InspectionVerifier`] — the full engine (mode=`"full"`). Spawns an
//!   isolated, read-only child session that opens the working directory, reads
//!   diffs, and runs the test suite before issuing its verdict.

pub mod inspection;

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use ao_engine_tools_core::{VerificationEngine, VerificationInput, VerificationVerdict};

use crate::{
    message::{ContentBlock, Message},
    provider::{CompletionEvent, CompletionRequest, ProviderClient},
};
use inspection::{inconclusive_fail, VERDICT_RETRY_NUDGE};

const JUDGE_SYSTEM_PROMPT: &str = "\
You are an impartial goal verifier. You receive a project goal, optionally a \
detailed spec, and objective evidence of work performed (tasklist completion \
summaries). Your task is to decide whether the stated goal has been genuinely met.

Guidelines:
- Be skeptical. A goal is only \"pass\" when the evidence demonstrates it was \
  concretely achieved, not merely attempted or planned.
- List specific, actionable gaps when the verdict is \"fail\". A gap is something \
  missing or incomplete relative to the stated goal or spec.
- Do not penalise a project for gaps that were previously flagged and are now \
  remediated — focus on the current evidence.
- Respond with ONLY valid JSON — no prose before or after — in this exact shape:

{
  \"verdict\": \"pass\" | \"fail\",
  \"confidence\": \"high\" | \"medium\" | \"low\",
  \"gaps\": [\"gap 1\", \"gap 2\"],
  \"rationale\": \"one paragraph explaining the verdict\"
}";

/// Production implementation: makes a single uncached model call through the
/// existing `ProviderClient` seam.
pub struct ProviderVerificationEngine {
    provider: Arc<dyn ProviderClient>,
}

impl ProviderVerificationEngine {
    pub fn new(provider: Arc<dyn ProviderClient>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl VerificationEngine for ProviderVerificationEngine {
    async fn verify(&self, input: VerificationInput) -> Result<VerificationVerdict, String> {
        let user_content = build_user_message(&input);

        // First attempt. A hard provider/transport failure (e.g. the CLI judge
        // produced no output) degrades to an inconclusive verdict rather than a
        // raw error, so the caller always records a structured round and is not
        // misled into thinking the failure was transient and retryable.
        let first_text = match self.run_completion(&user_content).await {
            Ok(t) => t,
            Err(e) => return Ok(inconclusive_fail(format!("quick verification call failed: {e}"))),
        };

        if let Ok(verdict) = parse_verdict(&first_text) {
            return Ok(verdict);
        }

        // The judge's reply was not parseable. Retry exactly once, echoing the
        // unparseable reply back with an explicit format nudge. The provider may
        // be a stateless CLI binary that retains nothing between invocations, so
        // the retry must restate the full context and the required JSON shape.
        let retry_content = format!(
            "{user_content}\n\n## Previous reply (could not be parsed as JSON)\n\n\
             {first_text}\n\n{VERDICT_RETRY_NUDGE}"
        );
        let retry_text = match self.run_completion(&retry_content).await {
            Ok(t) => t,
            Err(e) => {
                return Ok(inconclusive_fail(format!(
                    "quick verification retry failed: {e}"
                )))
            }
        };

        match parse_verdict(&retry_text) {
            Ok(verdict) => Ok(verdict),
            Err(e) => Ok(inconclusive_fail(format!(
                "quick verifier did not return a parseable JSON verdict after a retry: {e}"
            ))),
        }
    }
}

impl ProviderVerificationEngine {
    /// Issue a single judge completion and drain the stream into one text blob.
    ///
    /// Returns `Err` only on a provider/transport/stream failure; an empty but
    /// successful turn yields `Ok("")` so the caller's verdict parser decides
    /// what to do with it.
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
            .map_err(|e| format!("verification provider error: {e}"))?;

        let mut text = String::new();
        loop {
            match stream.recv().await {
                None => break,
                Some(Ok(CompletionEvent::AssistantText(chunk))) => text.push_str(&chunk),
                Some(Ok(CompletionEvent::TurnComplete { .. })) => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("verification stream error: {e}")),
            }
        }

        Ok(text)
    }
}

fn build_user_message(input: &VerificationInput) -> String {
    let mut msg = format!("# Project goal\n\n{}\n", input.goal);

    if let Some(ref spec) = input.spec {
        if !spec.trim().is_empty() {
            msg.push_str(&format!("\n# Spec\n\n{}\n", spec));
        }
    }

    if input.tasklist_evidence.is_empty() {
        msg.push_str("\n# Work evidence\n\nNo completed tasklists yet.\n");
    } else {
        msg.push_str("\n# Work evidence\n\n");
        for item in &input.tasklist_evidence {
            msg.push_str(&format!("## {}\n\n{}\n\n", item.title, item.summary));
        }
    }

    if !input.prior_verdicts.is_empty() {
        msg.push_str("\n# Prior verification rounds\n\n");
        for pv in &input.prior_verdicts {
            msg.push_str(&format!(
                "Round {}: {} — gaps: {}\n",
                pv.round,
                pv.verdict,
                if pv.gaps.is_empty() {
                    "none".to_string()
                } else {
                    pv.gaps.join("; ")
                }
            ));
        }
    }

    if let Some(ref extra) = input.extra_evidence {
        if !extra.trim().is_empty() {
            msg.push_str(&format!("\n# Additional evidence\n\n{}\n", extra));
        }
    }

    msg.push_str("\nRespond with the JSON verdict only.");
    msg
}

pub fn parse_verdict(raw: &str) -> Result<VerificationVerdict, String> {
    let trimmed = raw.trim();

    let v = extract_verdict_value(trimmed).ok_or_else(|| {
        "no parseable JSON verdict object found in model output".to_string()
    })?;

    let verdict = v
        .get("verdict")
        .and_then(|x| x.as_str())
        .filter(|s| *s == "pass" || *s == "fail")
        .ok_or_else(|| "missing or invalid 'verdict' field (expected 'pass' or 'fail')".to_string())?
        .to_string();

    let confidence = v
        .get("confidence")
        .and_then(|x| x.as_str())
        .filter(|s| matches!(*s, "high" | "medium" | "low"))
        .unwrap_or("medium")
        .to_string();

    let gaps = v
        .get("gaps")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|g| g.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let rationale = v
        .get("rationale")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();

    Ok(VerificationVerdict {
        verdict,
        confidence,
        gaps,
        rationale,
    })
}

/// Locate a JSON verdict object inside arbitrary model output.
///
/// Models — especially CLI binaries answering in print mode — frequently wrap
/// the verdict in a fenced code block, a sentence of preamble, or a trailing
/// remark. Three candidates are tried in order, and the first one that parses
/// into a JSON object carrying a `verdict` field wins:
///
/// 1. the contents of a fenced code block, if one is present;
/// 2. the whole trimmed string (the strict "JSON only" case);
/// 3. the first balanced `{...}` object embedded anywhere in the text.
fn extract_verdict_value(trimmed: &str) -> Option<serde_json::Value> {
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(fenced) = fenced_block(trimmed) {
        candidates.push(fenced);
    }
    candidates.push(trimmed);
    candidates.extend(top_level_json_objects(trimmed));

    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
            // Require the object to actually look like a verdict; this avoids
            // latching onto an unrelated JSON object that happens to appear
            // earlier in the prose. Field validation happens in the caller.
            if value.is_object() && value.get("verdict").is_some() {
                return Some(value);
            }
        }
    }

    None
}

/// Return the trimmed contents of the first fenced code block, preferring a
/// ```` ```json ```` fence over a bare ```` ``` ```` fence. Returns `None` when
/// no closed fence is present.
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

/// Collect every balanced top-level `{...}` substring, in document order.
///
/// String literals are honored so a `}` inside a quoted value (e.g. a
/// rationale) does not close the object prematurely. Returning every top-level
/// object — not just the first — lets the caller skip an unrelated leading
/// object and still find the real verdict later in the text.
///
/// The braces, quotes, and backslash this scans for are all ASCII, and any
/// multi-byte UTF-8 content between them only ever uses continuation bytes
/// outside the ASCII range — so byte scanning never splits a character and the
/// returned slices always land on valid `char` boundaries.
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

/// Given a byte index pointing at an opening `{`, return the index of the
/// matching closing `}`, honoring string literals and escapes. Returns `None`
/// when the object never closes.
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

// Re-export types used by callers that build the verification engines.
pub use ao_engine_tools_core::VerificationEngine as VerificationEngineTrait;
pub use inspection::InspectionVerifier;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{MockProviderClient, StopReason};

    fn input() -> VerificationInput {
        VerificationInput {
            project_id: "proj".to_string(),
            goal: "Ship the thing".to_string(),
            spec: None,
            tasklist_evidence: vec![],
            prior_verdicts: vec![],
            extra_evidence: None,
            working_dir: None,
        }
    }

    fn turn(text: &str) -> Vec<CompletionEvent> {
        vec![
            CompletionEvent::AssistantText(text.to_string()),
            CompletionEvent::TurnComplete {
                stop_reason: StopReason::Natural,
            },
        ]
    }

    // --- parse_verdict robustness ------------------------------------------

    #[test]
    fn parses_plain_json() {
        let v = parse_verdict(
            r#"{"verdict":"pass","confidence":"high","gaps":[],"rationale":"done"}"#,
        )
        .unwrap();
        assert_eq!(v.verdict, "pass");
        assert_eq!(v.confidence, "high");
    }

    #[test]
    fn parses_fenced_json() {
        let text = concat!(
            "Here is my verdict:\n```json\n",
            r#"{"verdict":"fail","confidence":"low","gaps":["x"],"rationale":"y"}"#,
            "\n```\nLet me know if you need more."
        );
        let v = parse_verdict(text).unwrap();
        assert_eq!(v.verdict, "fail");
        assert_eq!(v.gaps, vec!["x"]);
    }

    #[test]
    fn parses_json_with_prose_preamble_and_trailer() {
        // The exact shape that broke the quick path: a bare object wrapped in
        // conversational prose, no code fence.
        let text = concat!(
            "Based on the evidence provided, the goal is met.\n\n",
            r#"{"verdict":"pass","confidence":"medium","gaps":[],"rationale":"All tasklists completed."}"#,
            "\n\nHappy to dig deeper if useful."
        );
        let v = parse_verdict(text).unwrap();
        assert_eq!(v.verdict, "pass");
        assert_eq!(v.confidence, "medium");
    }

    #[test]
    fn parses_object_whose_string_contains_braces() {
        // A `}` inside the rationale string must not close the object early.
        let text = concat!(
            "verdict follows:\n",
            r#"{"verdict":"fail","confidence":"high","gaps":["missing }"],"rationale":"saw a } and { in code"}"#,
            "\ndone"
        );
        let v = parse_verdict(text).unwrap();
        assert_eq!(v.verdict, "fail");
        assert_eq!(v.gaps, vec!["missing }"]);
        assert!(v.rationale.contains("{ in code"));
    }

    #[test]
    fn rejects_text_with_no_verdict_object() {
        assert!(parse_verdict("I could not reach a conclusion.").is_err());
    }

    #[test]
    fn ignores_unrelated_leading_object() {
        // An earlier, unrelated JSON object without a `verdict` field is skipped
        // in favor of the real verdict object later in the text.
        let text = concat!(
            r#"{"note":"scratch"}"#,
            "\n",
            r#"{"verdict":"pass","confidence":"low","gaps":[],"rationale":"ok"}"#,
        );
        let v = parse_verdict(text).unwrap();
        assert_eq!(v.verdict, "pass");
    }

    // --- ProviderVerificationEngine resilience -----------------------------

    /// A single prose-wrapped reply parses without any retry.
    #[tokio::test]
    async fn quick_engine_parses_prose_wrapped_reply_first_try() {
        let reply = concat!(
            "Looks complete.\n",
            r#"{"verdict":"pass","confidence":"high","gaps":[],"rationale":"good"}"#,
        );
        let provider = std::sync::Arc::new(MockProviderClient::new(vec![turn(reply)]));
        let engine = ProviderVerificationEngine::new(provider.clone());

        let v = engine.verify(input()).await.unwrap();
        assert_eq!(v.verdict, "pass");
        // Only the first turn was consumed — no retry fired.
        assert_eq!(provider.remaining_turns(), 0);
    }

    /// An unparseable first reply triggers exactly one retry, which succeeds.
    #[tokio::test]
    async fn quick_engine_retries_on_unparseable_reply() {
        let good = r#"{"verdict":"fail","confidence":"low","gaps":["no tests"],"rationale":"absent"}"#;
        let provider = std::sync::Arc::new(MockProviderClient::new(vec![
            turn("I'm honestly not sure how to format this."),
            turn(good),
        ]));
        let engine = ProviderVerificationEngine::new(provider.clone());

        let v = engine.verify(input()).await.unwrap();
        assert_eq!(v.verdict, "fail");
        assert_eq!(v.gaps, vec!["no tests"]);
        assert_eq!(provider.remaining_turns(), 0);
    }

    /// Two unparseable replies degrade to a structured inconclusive verdict
    /// rather than a hard error.
    #[tokio::test]
    async fn quick_engine_both_attempts_fail_returns_inconclusive() {
        let provider = std::sync::Arc::new(MockProviderClient::new(vec![
            turn("no json here"),
            turn("still no json"),
        ]));
        let engine = ProviderVerificationEngine::new(provider);

        let v = engine.verify(input()).await.unwrap();
        assert_eq!(v.verdict, "fail");
        assert_eq!(v.confidence, "low");
        assert!(v.gaps.iter().any(|g| g.contains("inconclusive")));
    }

    /// A provider/transport failure (e.g. the CLI judge produced no output)
    /// degrades to inconclusive instead of propagating an error.
    #[tokio::test]
    async fn quick_engine_provider_error_returns_inconclusive() {
        // Empty script → ScriptExhausted on the first complete() call.
        let provider = std::sync::Arc::new(MockProviderClient::new(vec![]));
        let engine = ProviderVerificationEngine::new(provider);

        let v = engine.verify(input()).await.unwrap();
        assert_eq!(v.verdict, "fail");
        assert_eq!(v.confidence, "low");
        assert!(v.gaps.iter().any(|g| g.contains("inconclusive")));
    }
}
