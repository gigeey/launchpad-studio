mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::background_agents::{cancel_delegation, BackgroundAgentId, CancelOutcome};
use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

/// Engine tool that cancels a specific async delegation by id.
///
/// Delegates the actual lookup-and-fire to
/// [`cancel_delegation`](ao_engine_tools_core::background_agents::cancel_delegation),
/// the same primitive the `POST /delegates/{delegation_id}/cancel` HTTP
/// route uses, and maps its [`CancelOutcome`] onto this tool's JSON shape.
/// The handle stays in the registry either way so
/// [`DelegateOutput`](crate::delegate_output::DelegateOutput) can reap it on
/// the next poll. The call is idempotent — cancelling an already-cancelled
/// delegation returns `status: "already_cancelled"` rather than an error.
pub struct DelegateStop;

#[async_trait]
impl EngineTool for DelegateStop {
    fn name(&self) -> &str {
        "DelegateStop"
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

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let id_str = match input.get("id").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return Ok(ToolOutput::error("missing required field: id", true)),
        };

        let bg_id: BackgroundAgentId = match id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                return Ok(ToolOutput::error(
                    format!("invalid background agent id: {e}"),
                    false,
                ));
            }
        };

        match cancel_delegation(&ctx.background_agents, &bg_id).await {
            CancelOutcome::NotFound => Ok(ToolOutput::error(
                format!("unknown background agent id '{bg_id}'"),
                true,
            )),
            CancelOutcome::AlreadyCancelled => Ok(ToolOutput::structured(serde_json::json!({
                "status": "already_cancelled",
                "id": bg_id.to_string(),
            }))),
            CancelOutcome::Cancelled => Ok(ToolOutput::structured(serde_json::json!({
                "status": "cancelled",
                "id": bg_id.to_string(),
            }))),
        }
    }
}
