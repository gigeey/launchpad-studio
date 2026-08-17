//! Thread summarization engine: production back-end for the `SummarizeThread`
//! tool.
//!
//! The trait is defined in `ao-engine-tools-core` so `RunnerContext` can hold
//! it without a circular dependency. [`ProviderThreadSummarizer`] is the sole
//! implementation: a single uncached model call through the existing
//! `ProviderClient` seam, with no tools and no chat history — the target
//! thread's own transcript text is the entire input. This mirrors
//! `verification::ProviderVerificationEngine`'s shape closely, but returns
//! free-form prose instead of a parsed JSON verdict.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use ao_engine_tools_core::{ThreadSummarizationEngine, ThreadSummarizationInput};

use crate::{
    message::{ContentBlock, Message},
    provider::{CompletionEvent, CompletionRequest, ProviderClient},
};

const SUMMARIZER_SYSTEM_PROMPT: &str = "\
You are summarizing one chat thread on behalf of the same agent, so a version \
of it working in a different thread can quickly catch up on what happened \
here without reading the whole transcript.

Guidelines:
- Be concrete: name decisions made, open questions, and any next steps or \
  commitments, rather than vague generalities.
- If the transcript notes that earlier content was omitted for length, say so \
  in your summary rather than presenting it as the complete history.
- Write plain prose (a short paragraph, or a few bullet points for a long or \
  eventful thread). Do not restate the raw transcript or wrap your reply in \
  JSON or code fences.
- If a specific focus question is provided, prioritize answering it, but \
  still note any other major decisions the reader would otherwise miss.";

/// Production implementation: makes a single uncached model call through the
/// existing `ProviderClient` seam.
pub struct ProviderThreadSummarizer {
    provider: Arc<dyn ProviderClient>,
}

impl ProviderThreadSummarizer {
    pub fn new(provider: Arc<dyn ProviderClient>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl ThreadSummarizationEngine for ProviderThreadSummarizer {
    async fn summarize(&self, input: ThreadSummarizationInput) -> Result<String, String> {
        let user_content = build_user_message(&input);

        let messages = vec![Message::User {
            content: vec![ContentBlock::Text { text: user_content }],
        }];

        let request = CompletionRequest {
            messages,
            system_prompt: Some(SUMMARIZER_SYSTEM_PROMPT.to_string()),
            tools: vec![],
            ..Default::default()
        };

        let cancel = CancellationToken::new();
        let mut stream = self
            .provider
            .complete(request, cancel)
            .await
            .map_err(|e| format!("thread summarization provider error: {e}"))?;

        let mut text = String::new();
        loop {
            match stream.recv().await {
                None => break,
                Some(Ok(CompletionEvent::AssistantText(chunk))) => text.push_str(&chunk),
                Some(Ok(CompletionEvent::TurnComplete { .. })) => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("thread summarization stream error: {e}")),
            }
        }

        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("thread summarization call returned no text".to_string());
        }
        Ok(trimmed.to_string())
    }
}

fn build_user_message(input: &ThreadSummarizationInput) -> String {
    let mut msg = String::from("# Thread transcript to summarize\n\n");

    if let Some(ref title) = input.thread_title {
        msg.push_str(&format!("Thread title: {title}\n\n"));
    }

    if let Some(ref focus) = input.focus {
        if !focus.trim().is_empty() {
            msg.push_str(&format!("Focus the summary on this question: {focus}\n\n"));
        }
    }

    msg.push_str("## Transcript\n\n");
    msg.push_str(&input.transcript_text);
    msg.push_str("\n\nRespond with the summary only — no preamble, no JSON, no code fences.");
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{MockProviderClient, StopReason};

    fn input(transcript: &str, focus: Option<&str>) -> ThreadSummarizationInput {
        ThreadSummarizationInput {
            thread_title: Some("Pricing discussion".to_string()),
            focus: focus.map(str::to_string),
            transcript_text: transcript.to_string(),
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

    #[tokio::test]
    async fn summarizes_transcript_into_trimmed_prose() {
        let provider = Arc::new(MockProviderClient::new(vec![turn(
            "  We settled on tiered pricing with a free trial.  ",
        )]));
        let engine = ProviderThreadSummarizer::new(provider);

        let summary = engine
            .summarize(input("user: what pricing?\nagent: tiered w/ trial", None))
            .await
            .unwrap();

        assert_eq!(summary, "We settled on tiered pricing with a free trial.");
    }

    #[tokio::test]
    async fn includes_focus_question_in_prompt() {
        let provider = Arc::new(MockProviderClient::new(vec![turn("Yes, trial is 14 days.")]));
        let engine = ProviderThreadSummarizer::new(provider);

        let out = engine
            .summarize(input(
                "user: what pricing?\nagent: tiered w/ trial",
                Some("How long is the trial?"),
            ))
            .await
            .unwrap();

        assert_eq!(out, "Yes, trial is 14 days.");
    }

    #[tokio::test]
    async fn empty_reply_is_an_error() {
        let provider = Arc::new(MockProviderClient::new(vec![turn("   ")]));
        let engine = ProviderThreadSummarizer::new(provider);

        let err = engine.summarize(input("hi", None)).await.unwrap_err();
        assert!(err.contains("no text"));
    }

    #[tokio::test]
    async fn provider_error_propagates() {
        // Empty script → ScriptExhausted on the first complete() call.
        let provider = Arc::new(MockProviderClient::new(vec![]));
        let engine = ProviderThreadSummarizer::new(provider);

        let err = engine.summarize(input("hi", None)).await.unwrap_err();
        assert!(err.contains("thread summarization"));
    }
}
