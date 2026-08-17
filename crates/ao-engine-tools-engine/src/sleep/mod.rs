mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};

use ao_engine_tools_core::{EngineTool, Registry, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};

const MIN_SECS: u64 = 1;
const MAX_SECS: u64 = 3600;

pub struct Sleep;

#[async_trait]
impl EngineTool for Sleep {
    fn name(&self) -> &str {
        "Sleep"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "duration_seconds": {
                    "type": "number",
                    "description": "Number of seconds to wait. Must be between 1 and 3600."
                }
            },
            "required": ["duration_seconds"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let raw = &input["duration_seconds"];
        let duration_secs: Option<u64> = raw
            .as_u64()
            .or_else(|| raw.as_str().and_then(|s| s.parse::<u64>().ok()));

        let duration_secs = match duration_secs {
            Some(n) => n,
            None => {
                return Ok(ToolOutput::text(format!(
                    "duration_seconds must be a positive integer, got: {}",
                    raw
                )));
            }
        };

        if duration_secs < MIN_SECS {
            return Ok(ToolOutput::text(format!(
                "duration_seconds must be at least {} second, got {}",
                MIN_SECS, duration_secs
            )));
        }
        if duration_secs > MAX_SECS {
            return Ok(ToolOutput::text(format!(
                "duration_seconds must be at most {} seconds (1 hour), got {}",
                MAX_SECS, duration_secs
            )));
        }

        let started = Instant::now();
        let result = tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(duration_secs)) => {
                Ok(ToolOutput::text(format!("Waited {} seconds", duration_secs)))
            }
            _ = ctx.cancel.cancelled() => {
                let elapsed = started.elapsed().as_secs();
                Ok(ToolOutput::text(format!("Interrupted after {} seconds", elapsed)))
            }
        };
        // Signal the drain loop that Sleep ran this turn so it can defer
        // low-priority injections (e.g. background completion notices) until
        // the first subsequent turn where no Sleep occurs.
        ctx.set_sleep_ran();
        result
    }
}

/// Register the Sleep tool into `registry`.
///
/// Sleep is an autonomous-agent primitive. Register this only when building a
/// registry for an autonomous session — not for interactive sessions where a
/// human is watching.
pub fn register(registry: &mut Registry) {
    registry.register_engine(Arc::new(Sleep));
}
