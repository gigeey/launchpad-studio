mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, LoadPolicy, Registry, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use ao_protocol::thread::{default_thread_id, Thread};
use async_trait::async_trait;
use serde_json::Value;

/// List every thread belonging to the acting agent's own chat.
///
/// Companion to [`crate::summarize_thread::SummarizeThread`] — this tool is
/// the discovery half of cross-thread lookups, always scoped to
/// `ctx.agent_id`'s own threads via `ThreadStore::list_for_agent` (which
/// filters on `ThreadScope::AgentChat { agent_id }`), so it can never surface
/// another agent's, a team's, or a delegation's threads.
pub struct ListThreads;

#[async_trait]
impl EngineTool for ListThreads {
    fn name(&self) -> &str {
        "ListThreads"
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
        true
    }

    async fn invoke(&self, _input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let store = match &ctx.thread_store {
            Some(s) => s,
            None => {
                return Ok(ToolOutput::error(
                    "Thread store not available in this context.",
                    false,
                ));
            }
        };

        let threads = match store.list_for_agent(&ctx.agent_id).await {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to list threads: {e}"),
                    false,
                ));
            }
        };

        // `ctx.thread_id` is only set for a run explicitly scoped to a
        // non-default thread (see `RunnerContext::thread_id` docs); an
        // unset id means this run is on the agent's default thread, mirroring
        // `ThreadStore::resolve_or_default`'s `None` case.
        let current_id = ctx
            .thread_id
            .clone()
            .unwrap_or_else(|| default_thread_id(&ctx.agent_id));

        let items: Vec<Value> = threads
            .iter()
            .map(|t| thread_summary_json(t, &current_id))
            .collect();

        Ok(ToolOutput::structured(serde_json::json!({
            "threads": items,
            "count": items.len(),
        })))
    }
}

fn thread_summary_json(t: &Thread, current_id: &str) -> Value {
    serde_json::json!({
        "thread_id": t.id,
        "title": display_title(t),
        "kind": t.kind,
        "created_at": t.created_at,
        "updated_at": t.updated_at,
        "is_current": t.id == current_id,
    })
}

/// Same precedence as the frontend tab strip: explicit `title`, then the
/// auto-derived `auto_title`, then a placeholder for a never-touched thread.
pub(crate) fn display_title(t: &Thread) -> String {
    t.title
        .clone()
        .or_else(|| t.auto_title.clone())
        .unwrap_or_else(|| "(untitled)".to_string())
}

/// Register the ListThreads tool into `registry`.
///
/// Not part of [`crate::register_all`] — like `RenameThread`, this tool is
/// conditionally injected per run by session-init logic (native runner) only
/// when the acting agent has more than one thread, so a single-thread agent
/// never pays the token cost of a tool that could only ever list itself.
pub fn register(registry: &mut Registry) {
    registry.register_engine(Arc::new(ListThreads));
}
