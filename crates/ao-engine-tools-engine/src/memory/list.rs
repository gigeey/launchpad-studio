use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::{error::AoError, memory::MemoryScope};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::store::{
    resolve_scope_context, resolve_working_dir, ScopeContext, AGENT_HARD_CAP, AGENT_SOFT_CAP,
    GLOBAL_HARD_CAP, GLOBAL_SOFT_CAP, PROJECT_HARD_CAP, PROJECT_SOFT_CAP,
    THREAD_HARD_CAP, THREAD_SOFT_CAP,
};

const PAGE_SIZE: usize = 100;
const PREVIEW_CHARS: usize = 200;

pub struct MemoryList;

#[async_trait]
impl IoTool for MemoryList {
    fn name(&self) -> &str {
        "MemoryList"
    }

    fn description(&self) -> &str {
        super::prompt::LIST_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["agent", "project", "global", "thread"],
                    "description": "Which memory scope to list. 'thread' is ephemeral working memory scoped to the current thread only."
                },
                "offset": {
                    "type": "number",
                    "description": "Number of entries to skip (default 0). Use for pagination."
                },
                "working_dir": {
                    "type": "string",
                    "description": "Optional. Override directory used to resolve the project key for scope='project'. Supports '~' expansion and accepts absolute or relative paths (relative is joined onto the runner cwd). Ignored for 'agent' and 'global' scopes. Pass this when the agent has navigated outside the runner's launch directory (e.g., into a sibling repo) so project memories key off the repo you're actually working in."
                }
            },
            "required": ["scope"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let scope_str = match input.get("scope").and_then(Value::as_str) {
            Some(s) => s,
            None => return Ok(ToolOutput::error("Missing required field: scope", false)),
        };
        let offset = input
            .get("offset")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(0);

        let scope = match scope_str {
            "agent" => MemoryScope::Agent,
            "project" => MemoryScope::Project,
            "global" => MemoryScope::Global,
            "thread" => MemoryScope::Thread,
            other => {
                return Ok(ToolOutput::error(
                    format!(
                        "Invalid scope '{other}'. Must be one of: agent, project, global, thread."
                    ),
                    false,
                ));
            }
        };

        let store = match &ctx.memory_store {
            Some(s) => s.clone(),
            None => {
                return Ok(ToolOutput::error(
                    "Memory store not available in this context.",
                    false,
                ));
            }
        };

        let cwd = ctx.cwd.read().unwrap().clone();
        let explicit_working_dir = input
            .get("working_dir")
            .and_then(Value::as_str)
            .map(|s| resolve_working_dir(Some(s), &cwd));
        let scope_ctx = match resolve_scope_context(
            &scope,
            &ctx.agent_id,
            explicit_working_dir.as_deref(),
            ctx.parent_current_cwd.as_deref(),
            &cwd,
            ctx.thread_id.as_deref(),
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolOutput::error(
                    format!("Failed to resolve scope context: {e}"),
                    false,
                ));
            }
        };

        let (soft_cap, hard_cap) = match &scope_ctx {
            ScopeContext::Agent { .. } => (AGENT_SOFT_CAP, AGENT_HARD_CAP),
            ScopeContext::Project { .. } => (PROJECT_SOFT_CAP, PROJECT_HARD_CAP),
            ScopeContext::Global => (GLOBAL_SOFT_CAP, GLOBAL_HARD_CAP),
            ScopeContext::Thread { .. } => (THREAD_SOFT_CAP, THREAD_HARD_CAP),
            // `scope_str` above only ever parses to Agent/Project/Global/Thread,
            // so `resolve_scope_context` can never hand this tool an
            // `AgentProject` context — reserved for a future writer.
            ScopeContext::AgentProject { .. } => unreachable!(
                "MemoryList only resolves scope from {{agent, project, global, thread}}"
            ),
        };

        // Load all live entries, sorted by updated_at descending.
        let mut all_entries = match &scope_ctx {
            ScopeContext::Agent { agent_id } => store.list(agent_id).await?,
            ScopeContext::Global => store.list_global().await?,
            ScopeContext::Project { hash, .. } => store.list_project(hash).await?,
            ScopeContext::Thread { thread_id } => store.list_thread(thread_id).await?,
            ScopeContext::AgentProject { .. } => unreachable!(
                "MemoryList only resolves scope from {{agent, project, global, thread}}"
            ),
        };
        all_entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        let total = all_entries.len();
        let page: Vec<Value> = all_entries
            .into_iter()
            .skip(offset)
            .take(PAGE_SIZE)
            .map(|e| {
                let preview: String = e.content.chars().take(PREVIEW_CHARS).collect();
                json!({
                    "id": e.id,
                    "content_preview": preview,
                    "created_at": e.created_at.to_rfc3339(),
                    "updated_at": e.updated_at.to_rfc3339(),
                })
            })
            .collect();

        let has_more = total > offset + PAGE_SIZE;

        Ok(ToolOutput::structured(json!({
            "scope": scope_str,
            "entries": page,
            "total": total,
            "offset": offset,
            "has_more": has_more,
            "scope_summary": {
                "count": total,
                "soft_cap": soft_cap,
                "hard_cap": hard_cap,
            }
        })))
    }
}
