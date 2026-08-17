mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput, UserEvent};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

pub struct Brief;

#[async_trait]
impl EngineTool for Brief {
    fn name(&self) -> &str {
        "Brief"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::input_schema()
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let summary = match input.get("summary").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return Ok(ToolOutput::error(
                    "missing required field: summary",
                    true,
                ));
            }
        };

        let trimmed = summary.trim();
        if trimmed.is_empty() {
            return Ok(ToolOutput::error(
                "summary must contain at least one non-whitespace character",
                true,
            ));
        }

        let content = match input.get("details").and_then(|v| v.as_str()) {
            Some(details) => format!("{summary}\n\n{details}"),
            None => summary.clone(),
        };

        ctx.event_sink
            .emit(UserEvent::Brief {
                content: content.clone(),
            })
            .await
            .map_err(|e| AoError::Internal(format!("event sink error: {e}")))?;

        Ok(ToolOutput::text(summary))
    }
}
