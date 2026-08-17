//! BashStatus tool — query the status and buffered output of a background shell command.

use std::sync::Arc;

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

/// BashStatus — reads status and recent output for a registered background command.
#[derive(Default)]
pub struct BashStatus;

#[derive(Deserialize)]
struct BashStatusInput {
    process_id: String,
    #[serde(default)]
    offset: Option<u64>,
}

#[async_trait]
impl IoTool for BashStatus {
    fn name(&self) -> &str {
        "BashStatus"
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
        let input: BashStatusInput = serde_json::from_value(input)
            .map_err(|e| AoError::ValidationError(format!("invalid BashStatus input: {e}")))?;

        let id = BackgroundCommandId::from(input.process_id.as_str());
        let handle = ctx.background_commands.get(&id).await.ok_or_else(|| {
            AoError::ValidationError(format!(
                "unknown background command \"{}\". The id was not found in this session's \
                 registry — check that you are using the process_id returned by a Bash \
                 run_in_background call in this session.",
                input.process_id
            ))
        })?;

        let status = handle.status.lock().unwrap().clone();
        let status_str = match &status {
            BackgroundCommandStatus::Running => "running".to_string(),
            BackgroundCommandStatus::Exited { code } => format!("exited:{code}"),
            BackgroundCommandStatus::Killed => "killed".to_string(),
            BackgroundCommandStatus::Failed { reason } => format!("failed:{reason}"),
        };

        // Serve output from the in-memory ring buffer at the requested offset.
        let (output_text, next_offset, dropped_bytes) = {
            let buf = handle.output_buffer.lock().unwrap();
            let dropped = buf.dropped_bytes;
            let buf_len = buf.len() as u64;
            let total_written = dropped + buf_len;

            let requested_offset = input.offset.unwrap_or(0);

            let slice_start = if requested_offset <= dropped {
                // Requested bytes are gone; start from the current buffer head.
                0usize
            } else {
                let pos = (requested_offset - dropped) as usize;
                if pos >= buf.len() {
                    // No new bytes since last read.
                    return Ok(ToolOutput::structured(serde_json::json!({
                        "process_id": input.process_id,
                        "status": status_str,
                        "output": "",
                        "next_offset": total_written,
                        "output_path": handle.output_path.to_string_lossy().as_ref(),
                    })));
                }
                pos
            };

            let raw = &buf.as_bytes()[slice_start..];
            let text = String::from_utf8_lossy(raw).into_owned();
            (text, total_written, dropped)
        };

        let mut payload = serde_json::json!({
            "process_id": input.process_id,
            "status": status_str,
            "output": output_text,
            "next_offset": next_offset,
            "output_path": handle.output_path.to_string_lossy().as_ref(),
        });

        if dropped_bytes > 0 {
            payload["dropped_bytes"] = serde_json::Value::Number(dropped_bytes.into());
        }

        Ok(ToolOutput::structured(payload))
    }
}

/// Register [`BashStatus`] into `registry`.
pub fn register_bash_status(registry: &mut Registry) {
    registry.register_io(Arc::new(BashStatus));
}
