mod prompt;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Arc;

use ao_engine_tools_core::{
    EngineTool, LoadPolicy, Registry, RunnerContext, ThreadSummarizationInput, ToolOutput,
};
use ao_protocol::error::AoError;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use async_trait::async_trait;
use serde_json::Value;

/// Cap on transcript characters handed to the one-shot summarization call.
/// Keeps the call well inside typical context limits even for a very long
/// thread. A char count is a coarse proxy for tokens, but avoids depending on
/// a tokenizer here; it errs generous rather than truncating too eagerly.
const MAX_TRANSCRIPT_CHARS: usize = 60_000;

/// Number of leading messages always kept verbatim when truncating, so a long
/// thread's original goal/framing survives even when most of the middle is
/// elided. A pure "most recent N" window loses exactly this, which is the
/// classic failure mode for conversation truncation.
const HEAD_MESSAGES_KEPT: usize = 5;

/// Summarize another thread in the acting agent's own chat via a fresh,
/// tool-less model call over its transcript.
///
/// Companion to [`crate::list_threads::ListThreads`]. Ownership is enforced
/// structurally, not by a manual field comparison: `thread_id` is resolved
/// through `ThreadStore::list_for_agent(ctx.agent_id)` rather than the
/// unscoped `ThreadStore::get`, so a miss here means "doesn't exist, belongs
/// to another agent, or is a team/delegation thread" — all of which read
/// identically to the caller as "not found", with nothing leaked about which.
pub struct SummarizeThread;

#[async_trait]
impl EngineTool for SummarizeThread {
    fn name(&self) -> &str {
        "SummarizeThread"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::input_schema()
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let thread_id = match input.get("thread_id").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => {
                return Ok(ToolOutput::error(
                    "SummarizeThread requires a non-empty `thread_id` string. \
                     Call ListThreads first to find one.",
                    true,
                ));
            }
        };
        let focus = input
            .get("focus")
            .and_then(Value::as_str)
            .map(str::to_string);

        let thread_store = match &ctx.thread_store {
            Some(s) => s,
            None => {
                return Ok(ToolOutput::error(
                    "Thread store not available in this context.",
                    false,
                ));
            }
        };
        let transcript_store = match &ctx.transcript_store {
            Some(s) => s,
            None => {
                return Ok(ToolOutput::error(
                    "Transcript store not available in this context.",
                    false,
                ));
            }
        };
        let engine = match &ctx.thread_summarization_engine {
            Some(e) => e.clone(),
            None => {
                return Ok(ToolOutput::error(
                    "Thread summarization is not available in this session — no provider is \
                     configured. Ensure a provider API key is set up and try again.",
                    false,
                ));
            }
        };

        // Deliberately NOT `thread_store.get(&thread_id)` — that lookup is a
        // bare id match with no ownership filter. `list_for_agent` already
        // restricts to `ThreadScope::AgentChat { agent_id }` rows for THIS
        // agent, so resolving through it is the only way this check can't be
        // accidentally skipped later.
        let threads = match thread_store.list_for_agent(&ctx.agent_id).await {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to list threads: {e}"),
                    false,
                ));
            }
        };
        let thread = match threads.into_iter().find(|t| t.id == thread_id) {
            Some(t) => t,
            None => {
                return Ok(ToolOutput::error(
                    &format!(
                        "Thread '{thread_id}' not found in your own chat. Call ListThreads to \
                         see valid ids."
                    ),
                    true,
                ));
            }
        };

        let entries = match transcript_store
            .read_all_at(Path::new(&thread.transcript_path))
            .await
        {
            Ok(e) => e,
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to read thread transcript: {e}"),
                    false,
                ));
            }
        };

        let display = crate::list_threads::display_title(&thread);

        if entries.is_empty() {
            return Ok(ToolOutput::structured(serde_json::json!({
                "thread_id": thread.id,
                "title": display,
                "summary": "This thread has no messages yet.",
                "message_count": 0,
                "truncated": false,
            })));
        }

        let (transcript_text, truncated) = format_transcript(&entries);

        let summary = match engine
            .summarize(ThreadSummarizationInput {
                thread_title: Some(display.clone()),
                focus,
                transcript_text,
            })
            .await
        {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("thread summarization call failed: {e}"),
                    true,
                ));
            }
        };

        Ok(ToolOutput::structured(serde_json::json!({
            "thread_id": thread.id,
            "title": display,
            "summary": summary,
            "message_count": entries.len(),
            "truncated": truncated,
        })))
    }
}

fn role_label(role: &TranscriptRole) -> &str {
    match role {
        TranscriptRole::System(s) => s.as_str(),
        TranscriptRole::Agent { agent } => agent.as_str(),
        TranscriptRole::Schedule { .. } => "schedule",
    }
}

/// Render entries into one text blob for the summarizer, windowed to
/// `MAX_TRANSCRIPT_CHARS` so a very long thread doesn't blow the one-shot
/// call's context. The first [`HEAD_MESSAGES_KEPT`] entries — which usually
/// carry the thread's original goal/framing — are always kept verbatim, then
/// as many of the most recent entries as fit in the remaining budget. Returns
/// the text plus whether anything was elided.
fn format_transcript(entries: &[TranscriptEntry]) -> (String, bool) {
    let lines: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                "[{}] {}: {}",
                e.ts.format("%Y-%m-%d %H:%M"),
                role_label(&e.role),
                e.content
            )
        })
        .collect();

    let total_len: usize = lines.iter().map(|l| l.len() + 1).sum();
    if total_len <= MAX_TRANSCRIPT_CHARS {
        return (lines.join("\n"), false);
    }

    let head_count = HEAD_MESSAGES_KEPT.min(lines.len());
    let head = &lines[..head_count];
    let head_len: usize = head.iter().map(|l| l.len() + 1).sum();
    let budget = MAX_TRANSCRIPT_CHARS.saturating_sub(head_len);

    let mut tail: Vec<&String> = Vec::new();
    let mut used = 0usize;
    for l in lines[head_count..].iter().rev() {
        if used + l.len() + 1 > budget {
            break;
        }
        used += l.len() + 1;
        tail.push(l);
    }
    tail.reverse();

    let omitted = lines.len().saturating_sub(head_count + tail.len());
    let mut out = String::new();
    for l in head {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&format!(
        "\n[... {omitted} earlier messages omitted for length ...]\n\n"
    ));
    for l in &tail {
        out.push_str(l);
        out.push('\n');
    }

    (out, true)
}

/// Register the SummarizeThread tool into `registry`.
///
/// Not part of [`crate::register_all`] — like `RenameThread` and
/// `ListThreads`, this tool is conditionally injected per run by session-init
/// logic (native runner) only when the acting agent has more than one thread.
pub fn register(registry: &mut Registry) {
    registry.register_engine(Arc::new(SummarizeThread));
}
