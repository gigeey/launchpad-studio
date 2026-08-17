use ao_engine_tools_core::{memory_usage, IoTool, RunnerContext, ToolOutput};
use ao_protocol::{error::AoError, memory::MemoryScope};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::store::{resolve_scope_context, resolve_working_dir, ScopeContext};

/// After a durable-log delete succeeds, also remove the entry's row from the
/// `.usage.json` sidecar — hard invariant: a delete/tombstone
/// must clean BOTH files, never orphan one. Best-effort and logged on
/// failure rather than propagated, matching how `MemoryStore` already treats
/// its other derived index (`sync_index_delete`): the JSONL tombstone is the
/// already-committed source of truth, and the sidecar is a derived scoring
/// aid, not a second copy of the entry itself.
async fn clean_usage_sidecar(scope_path: &std::path::Path, id: &str) {
    if let Err(e) = memory_usage::remove_entry(scope_path, id).await {
        tracing::warn!("MemoryDelete: failed to clean usage sidecar for {}: {}", id, e);
    }
}

pub struct MemoryDelete;

#[async_trait]
impl IoTool for MemoryDelete {
    fn name(&self) -> &str {
        "MemoryDelete"
    }

    fn description(&self) -> &str {
        super::prompt::DELETE_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The ID of the memory entry to delete. Use MemoryList to find valid IDs."
                },
                "scope": {
                    "type": "string",
                    "enum": ["agent", "project", "global", "thread"],
                    "description": "Which memory scope the entry belongs to. 'thread' is ephemeral working memory scoped to the current thread only."
                },
                "working_dir": {
                    "type": "string",
                    "description": "Optional. Override directory used to resolve the project key for scope='project'. Supports '~' expansion and accepts absolute or relative paths (relative is joined onto the runner cwd). Ignored for 'agent' and 'global' scopes. Pass this when the agent has navigated outside the runner's launch directory (e.g., into a sibling repo) so project memories key off the repo you're actually working in."
                }
            },
            "required": ["id", "scope"],
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

        let not_found_error = || {
            ToolOutput::structured(json!({
                "error": format!(
                    "Memory entry {id} not found in {scope_str} scope."
                )
            }))
        };

        match &scope_ctx {
            ScopeContext::Agent { agent_id } => match store.delete(agent_id, id).await? {
                true => {
                    clean_usage_sidecar(&store.agent_scope_path(agent_id), id).await;
                    Ok(ToolOutput::structured(json!({
                        "id": id,
                        "scope": scope_str,
                        "deleted": true,
                    })))
                }
                false => Ok(not_found_error()),
            },
            ScopeContext::Global => match store.delete_global(id).await? {
                true => {
                    clean_usage_sidecar(&store.global_scope_path(), id).await;
                    Ok(ToolOutput::structured(json!({
                        "id": id,
                        "scope": scope_str,
                        "deleted": true,
                    })))
                }
                false => Ok(not_found_error()),
            },
            ScopeContext::Project { hash, .. } => {
                match store.delete_project(hash, id).await {
                    Ok(_) => {
                        clean_usage_sidecar(&store.project_scope_path(hash), id).await;
                        Ok(ToolOutput::structured(json!({
                            "id": id,
                            "scope": scope_str,
                            "deleted": true,
                        })))
                    }
                    Err(AoError::Internal(msg)) if msg.contains("not found") => {
                        Ok(not_found_error())
                    }
                    Err(e) => Err(e),
                }
            }
            // Thread scope has no usage sidecar to clean: thread entries
            // never accrue usage history in the first place (see
            // `write_thread_entry` in `memory/write.rs`), so a plain
            // tombstone is the whole operation.
            ScopeContext::Thread { thread_id } => match store.delete_thread(thread_id, id).await? {
                true => Ok(ToolOutput::structured(json!({
                    "id": id,
                    "scope": scope_str,
                    "deleted": true,
                }))),
                false => Ok(not_found_error()),
            },
            // `scope` above only ever parses to Agent/Project/Global/Thread,
            // so `resolve_scope_context` can never hand this tool an
            // `AgentProject` context — reserved for a future writer.
            ScopeContext::AgentProject { .. } => unreachable!(
                "MemoryDelete only resolves scope from {{agent, project, global, thread}}"
            ),
        }
    }
}
