use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::{
    error::AoError,
    transcript::{TranscriptEntry, TranscriptRole},
};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct RecallHistory;

#[async_trait]
impl IoTool for RecallHistory {
    fn name(&self) -> &str {
        "RecallHistory"
    }

    fn description(&self) -> &str {
        super::recall_prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "count": {
                    "type": "integer",
                    "description": "Number of messages to retrieve before the current context window. Defaults to 20, clamped to max 100.",
                    "minimum": 1,
                    "maximum": 100
                }
            },
            "additionalProperties": false
        })
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let count = input
            .get("count")
            .and_then(Value::as_u64)
            .unwrap_or(20) as usize;
        let count = count.min(100).max(1);

        let store = match &ctx.transcript_store {
            Some(s) => s.clone(),
            None => {
                return Ok(ToolOutput::Error {
                    recoverable: false,
                    message: "Transcript store not available in this context.".into(),
                });
            }
        };

        // When the runner pinned a thread-scoped transcript path on the
        // context, read from it directly so a non-default or branch thread
        // surfaces the correct prior history. Falls back to the agent-keyed
        // default file for single-thread agents (back-compat).
        let all_entries = match ctx.recall_transcript_path.as_deref() {
            Some(path) => store.read_all_at(path).await?,
            None => store.read_all(&ctx.agent_id).await?,
        };

        let before_window: Vec<TranscriptEntry> = match ctx.window_floor_ts {
            Some(floor) => all_entries.into_iter().filter(|e| e.ts < floor).collect(),
            None => all_entries,
        };

        if before_window.is_empty() {
            return Ok(ToolOutput::text(
                "[Already at beginning of session history. No earlier messages available.]",
            ));
        }

        let start = before_window.len().saturating_sub(count);
        let recalled = &before_window[start..];
        Ok(ToolOutput::text(format_recalled_context(recalled)))
    }
}

fn role_label(role: &TranscriptRole) -> &str {
    match role {
        TranscriptRole::System(s) => s.as_str(),
        TranscriptRole::Agent { agent } => agent.as_str(),
        TranscriptRole::Schedule { .. } => "schedule",
    }
}

/// Format recalled transcript entries into a [Recalled context] block.
/// Matches the format of `format_recalled_context` in ao-engine/src/context.rs (no query variant).
fn format_recalled_context(entries: &[TranscriptEntry]) -> String {
    if entries.is_empty() {
        return "[No matching history found]".to_string();
    }
    let header = format!("[Recalled context ({} messages)]", entries.len());
    let lines: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                "[{}] {}: {}",
                e.ts.format("%H:%M"),
                role_label(&e.role),
                e.content
            )
        })
        .collect();
    format!("{}\n{}", header, lines.join("\n"))
}
