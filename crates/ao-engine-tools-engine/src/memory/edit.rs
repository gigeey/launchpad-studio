use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::{error::AoError, memory::MemoryScope};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::store::{
    resolve_scope_context, resolve_working_dir, ScopeContext, ENTRY_CHAR_HARD,
    THREAD_ENTRY_CHAR_HARD,
};

pub struct MemoryEdit;

#[async_trait]
impl IoTool for MemoryEdit {
    fn name(&self) -> &str {
        "MemoryEdit"
    }

    fn description(&self) -> &str {
        super::prompt::EDIT_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The ID of the memory entry to edit. Use MemoryList to find valid IDs."
                },
                "scope": {
                    "type": "string",
                    "enum": ["agent", "project", "global", "thread"],
                    "description": "Which memory scope the entry belongs to. 'thread' is ephemeral working memory scoped to the current thread only."
                },
                "content": {
                    "type": "string",
                    "description": "The updated memory content."
                },
                "working_dir": {
                    "type": "string",
                    "description": "Optional. Override directory used to resolve the project key for scope='project'. Supports '~' expansion and accepts absolute or relative paths (relative is joined onto the runner cwd). Ignored for 'agent' and 'global' scopes. Pass this when the agent has navigated outside the runner's launch directory (e.g., into a sibling repo) so project memories key off the repo you're actually working in."
                }
            },
            "required": ["id", "scope", "content"],
            "additionalProperties": false
        })
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let id = match input.get("id").and_then(Value::as_str) {
            Some(s) => s,
            None => return Ok(ToolOutput::error("Missing required field: id", false)),
        };
        let scope_str = match input.get("scope").and_then(Value::as_str) {
            Some(s) => s,
            None => return Ok(ToolOutput::error("Missing required field: scope", false)),
        };
        let content = match input.get("content").and_then(Value::as_str) {
            Some(s) => s,
            None => return Ok(ToolOutput::error("Missing required field: content", false)),
        };

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

        // Check content char hard cap. Thread scope uses its own, smaller cap.
        let entry_char_hard = if scope == MemoryScope::Thread {
            THREAD_ENTRY_CHAR_HARD
        } else {
            ENTRY_CHAR_HARD
        };
        let char_len = content.chars().count();
        if char_len > entry_char_hard {
            return Ok(ToolOutput::structured(json!({
                "error": format!(
                    "Entry is too long ({char_len} chars). Maximum is {entry_char_hard} chars."
                )
            })));
        }

        // Get store.
        let store = match &ctx.memory_store {
            Some(s) => s.clone(),
            None => {
                return Ok(ToolOutput::error(
                    "Memory store not available in this context.",
                    false,
                ));
            }
        };

        // Resolve scope context.
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

        // Call the appropriate edit method; map "not found" errors to structured output.
        let result = match &scope_ctx {
            ScopeContext::Agent { agent_id } => store.edit(agent_id, id, content).await,
            ScopeContext::Global => store.edit_global(id, content).await,
            ScopeContext::Project { hash, .. } => store.edit_project(hash, id, content).await,
            ScopeContext::Thread { thread_id } => store.edit_thread(thread_id, id, content).await,
            // `scope` above only ever parses to Agent/Project/Global/Thread, so
            // `resolve_scope_context` can never hand this tool an
            // `AgentProject` context — reserved for a future writer.
            ScopeContext::AgentProject { .. } => unreachable!(
                "MemoryEdit only resolves scope from {{agent, project, global, thread}}"
            ),
        };

        match result {
            Ok(_) => Ok(ToolOutput::structured(json!({
                "id": id,
                "scope": scope_str,
            }))),
            Err(AoError::Internal(msg)) if msg.contains("not found") => {
                Ok(ToolOutput::structured(json!({
                    "error": format!(
                        "Memory entry {id} not found in {scope_str} scope. Use MemoryList to find valid IDs."
                    )
                })))
            }
            Err(e) => Err(e),
        }
    }
}
