//! BashKill tool — terminate a registered background shell command.

use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::{
    BackgroundCommandId, BackgroundCommandStatus, IoTool, PermissionContext, PermissionDecision,
    Registry, RunnerContext, ToolOutput,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

pub mod prompt;
#[cfg(test)]
mod tests;

/// How long to wait for the drain task to confirm the child was reaped.
///
/// Sized to cover the full termination sequence with headroom: the SIGTERM
/// grace period before SIGKILL (`execute::TERMINATE_GRACE`, 5s) plus the
/// post-kill pipe drain (`POST_KILL_DRAIN_GRACE`, 2s). A command that ignores
/// SIGTERM therefore still reports a confirmed kill rather than timing out.
const KILL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);

/// BashKill — signals a registered background command to stop immediately.
#[derive(Default)]
pub struct BashKill;

#[derive(Deserialize)]
struct BashKillInput {
    process_id: String,
}

#[async_trait]
impl IoTool for BashKill {
    fn name(&self) -> &str {
        "BashKill"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        serde_json::from_str(prompt::INPUT_SCHEMA).expect("INPUT_SCHEMA is valid JSON")
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        _input: &Value,
        _ctx: &PermissionContext,
    ) -> PermissionDecision {
        PermissionDecision::Allow
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let input: BashKillInput = serde_json::from_value(input)
            .map_err(|e| AoError::ValidationError(format!("invalid BashKill input: {e}")))?;

        let id = BackgroundCommandId::from(input.process_id.as_str());
        let handle = ctx.background_commands.get(&id).await.ok_or_else(|| {
            AoError::ValidationError(format!(
                "unknown background command \"{}\". The id was not found in this session's \
                 registry — check that you are using the process_id returned by a Bash \
                 run_in_background call in this session.",
                input.process_id
            ))
        })?;

        // Reject terminal states up front. The drain task in `bash::background`
        // is the single writer of a terminal status, so this only reads: an
        // earlier version marked the handle `Killed` here, before signalling,
        // which made `BashStatus` report a kill that had not happened yet.
        {
            let st = handle.status.lock().unwrap();
            match &*st {
                BackgroundCommandStatus::Running => {}
                BackgroundCommandStatus::Exited { code } => {
                    let c = *code;
                    return Ok(ToolOutput::error(
                        format!(
                            "background command \"{}\" has already exited with code {c}",
                            input.process_id
                        ),
                        false,
                    ));
                }
                BackgroundCommandStatus::Killed => {
                    return Ok(ToolOutput::error(
                        format!(
                            "background command \"{}\" has already been killed",
                            input.process_id
                        ),
                        false,
                    ));
                }
                BackgroundCommandStatus::Failed { reason } => {
                    let r = reason.clone();
                    return Ok(ToolOutput::error(
                        format!(
                            "background command \"{}\" is in a failed state: {r}",
                            input.process_id
                        ),
                        false,
                    ));
                }
            }
        }

        // Fire the cancellation token. The drain task in background.rs watches
        // it and terminates the child's process group when it fires.
        handle.cancel.cancel();

        // Wait for the drain task to confirm the child was reaped instead of
        // reporting success the instant a token is signalled. Firing a token
        // is not evidence that anything died, and the model has no other way
        // to tell a completed kill from a pending one — `process_id` is the
        // only handle it holds. The budget covers `terminate_child`'s SIGTERM
        // grace plus the post-kill pipe drain, with headroom.
        let confirmed = tokio::time::timeout(
            KILL_CONFIRM_TIMEOUT,
            handle.terminated.cancelled(),
        )
        .await
        .is_ok();

        if !confirmed {
            return Ok(ToolOutput::structured(serde_json::json!({
                "process_id": input.process_id,
                "status": "kill_requested",
                "note": format!(
                    "Termination was signalled but the process was not confirmed \
                     stopped within {}s. It may still be running — re-check with \
                     BashStatus before assuming it has stopped.",
                    KILL_CONFIRM_TIMEOUT.as_secs()
                ),
            })));
        }

        // Report the status the drain task actually recorded. A command that
        // exited on its own in the window between the check above and the
        // signal landing is a third outcome, distinct from both "killed" and
        // "still running"; collapsing it into "killed" would tell the model a
        // kill worked when nothing was killed.
        let observed = handle.status.lock().unwrap().clone();
        Ok(match observed {
            BackgroundCommandStatus::Exited { code } => ToolOutput::structured(serde_json::json!({
                "process_id": input.process_id,
                "status": "exited",
                "exit_code": code,
                "note": "The command finished on its own before the kill signal \
                         landed; nothing was terminated.",
            })),
            BackgroundCommandStatus::Failed { reason } => {
                ToolOutput::structured(serde_json::json!({
                    "process_id": input.process_id,
                    "status": "failed",
                    "reason": reason,
                }))
            }
            // `Running` is unreachable: the drain task sets a terminal status
            // before firing `terminated`. Reported rather than asserted so a
            // future change to that ordering surfaces as a visible oddity
            // instead of a panic in a background tool call.
            BackgroundCommandStatus::Killed | BackgroundCommandStatus::Running => {
                ToolOutput::structured(serde_json::json!({
                    "process_id": input.process_id,
                    "status": "killed",
                }))
            }
        })
    }
}

/// Register [`BashKill`] into `registry`.
pub fn register_bash_kill(registry: &mut Registry) {
    registry.register_io(Arc::new(BashKill));
}
