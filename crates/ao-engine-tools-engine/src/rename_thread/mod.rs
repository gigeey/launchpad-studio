mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, Registry, RunnerContext, ToolOutput, UserEvent};
use ao_protocol::error::AoError;
use ao_protocol::thread::{derive_auto_title, ThreadKind, ThreadScope};
use async_trait::async_trait;
use serde_json::Value;

/// Give the acting thread a title.
///
/// Only ever registered for a run when the thread is eligible (personal,
/// non-default, and not yet named — see [`ao_protocol::thread::Thread::offers_rename_tool`]),
/// so a thread that already has a title never pays the token/latency cost of
/// this tool's definition being present at all. The checks in [`Self::invoke`]
/// are a second, backend-authoritative line of defense against a stale
/// registration (e.g. a long-running turn where the thread was renamed by a
/// human mid-call) — they are not the primary gate.
pub struct RenameThread;

#[async_trait]
impl EngineTool for RenameThread {
    fn name(&self) -> &str {
        "RenameThread"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::input_schema()
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let raw_title = match input.get("title").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return Ok(ToolOutput::error(
                    "RenameThread requires a `title` string.",
                    true,
                ));
            }
        };
        let title = match derive_auto_title(raw_title) {
            Some(t) => t,
            None => {
                return Ok(ToolOutput::error(
                    "`title` cannot be blank.",
                    true,
                ));
            }
        };

        let thread_id = match &ctx.thread_id {
            Some(id) => id.clone(),
            None => {
                return Ok(ToolOutput::error(
                    "RenameThread is only available inside a thread-scoped session. \
                     This run has no thread scope.",
                    false,
                ));
            }
        };

        let store = match &ctx.thread_store {
            Some(s) => s,
            None => {
                return Ok(ToolOutput::error(
                    "Thread store not available in this context.",
                    false,
                ));
            }
        };

        let thread = match store.get(&thread_id).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Ok(ToolOutput::error(
                    &format!("thread '{}' not found", thread_id),
                    false,
                ));
            }
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to load thread: {e}"),
                    false,
                ));
            }
        };

        if thread.kind == ThreadKind::Default {
            return Ok(ToolOutput::error(
                "The default thread's name is fixed and cannot be renamed.",
                true,
            ));
        }
        if !matches!(thread.scope, ThreadScope::AgentChat { .. }) {
            return Ok(ToolOutput::error(
                "RenameThread is only available for personal chat threads, not team or \
                 delegation threads.",
                true,
            ));
        }
        if let Some(existing) = &thread.title {
            return Ok(ToolOutput::error(
                &format!(
                    "This thread is already named \"{existing}\" — no need to call \
                     RenameThread again."
                ),
                true,
            ));
        }

        let updated = match store.rename(&thread_id, Some(title.clone())).await {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to rename thread: {e}"),
                    false,
                ));
            }
        };

        let _ = ctx
            .event_sink
            .emit(UserEvent::ThreadRenamed {
                thread_id: updated.id.clone(),
                title: title.clone(),
            })
            .await;

        Ok(ToolOutput::structured(serde_json::json!({
            "thread_id": updated.id,
            "title": title,
        })))
    }
}

/// Register the RenameThread tool into `registry`.
///
/// Not part of [`crate::register_all`] — this tool is conditionally injected
/// per run by session-init logic (native runner) / per-request context
/// building (MCP route), gated on `Thread::offers_rename_tool`, exactly like
/// [`crate::sleep::register`] is conditionally injected for autonomous
/// sessions. Always registering it and relying solely on the in-`invoke`
/// guards above would mean every turn on an already-named thread pays for a
/// tool definition it can never legally use.
pub fn register(registry: &mut Registry) {
    registry.register_engine(Arc::new(RenameThread));
}
