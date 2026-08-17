mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput, UserEvent};
use ao_engine_tools_core::permissions::PermissionMode;
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

pub struct EnterPlanMode;

#[async_trait]
impl EngineTool for EnterPlanMode {
    fn name(&self) -> &str {
        "EnterPlanMode"
    }

    fn description(&self) -> &str {
        prompt::ENTER_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::enter_input_schema()
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    async fn invoke(&self, _input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let prior = ctx.permissions.mode();
        ctx.permissions.enter_plan_mode();
        if prior != PermissionMode::Plan {
            ctx.event_sink
                .emit(UserEvent::PermissionModeChanged {
                    from: prior,
                    to: PermissionMode::Plan,
                })
                .await
                .map_err(|e| AoError::Internal(format!("event sink error: {e}")))?;
        }
        Ok(ToolOutput::text("plan mode"))
    }
}

pub struct ExitPlanMode;

#[async_trait]
impl EngineTool for ExitPlanMode {
    fn name(&self) -> &str {
        "ExitPlanMode"
    }

    fn description(&self) -> &str {
        prompt::EXIT_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::exit_input_schema()
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    async fn invoke(&self, _input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let prior_current = ctx.permissions.mode();
        ctx.permissions.exit_plan_mode();
        let new_current = ctx.permissions.mode();
        if prior_current == PermissionMode::Plan && new_current != PermissionMode::Plan {
            ctx.event_sink
                .emit(UserEvent::PermissionModeChanged {
                    from: PermissionMode::Plan,
                    to: new_current,
                })
                .await
                .map_err(|e| AoError::Internal(format!("event sink error: {e}")))?;
        }
        Ok(ToolOutput::text("default mode"))
    }
}
