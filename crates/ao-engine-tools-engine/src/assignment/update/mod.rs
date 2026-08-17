mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_persistence::cron_util::compute_next_fire_at;
use ao_protocol::assignment::{carry_forward_watch_contract, AssignmentTrigger};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use tracing::info;

use super::{parse_bindings, parse_thread_policy, parse_trigger};

pub struct AssignmentUpdate;

#[async_trait]
impl IoTool for AssignmentUpdate {
    fn name(&self) -> &str {
        "AssignmentUpdate"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "assignment_id": {
                    "type": "string",
                    "description": "ID of the assignment to update."
                },
                "name": {
                    "type": "string",
                    "description": "New human-readable label."
                },
                "instruction": {
                    "type": "string",
                    "description": "New instruction to run on each fire."
                },
                "trigger": {
                    "type": "object",
                    "description": "Full trigger replacement. See AssignmentCreate for the shape. type=\"agent_watch\" updates an existing agent-driven watch's instruction/poll_interval_secs/connector_scope in place — it cannot create a new watch. Its bound watch contract is preserved when instruction and connector_scope are both unchanged, and cleared for re-authoring when either changes.",
                    "properties": {
                        "type": {
                            "type": "string",
                            "enum": ["schedule", "webhook", "connector_event", "agent_watch"]
                        },
                        "cron_expr": {"type": "string"},
                        "is_recurring": {"type": "boolean"},
                        "token": {"type": "string"},
                        "server_name": {"type": "string"},
                        "poll": {"type": "object"},
                        "poll_interval_secs": {"type": "integer"},
                        "instruction": {
                            "type": "string",
                            "description": "The watch condition text. Only used when type=agent_watch."
                        },
                        "connector_scope": {
                            "type": "string",
                            "description": "Restrict the watch to one MCP server's tools, or omit for every configured server. Only used when type=agent_watch."
                        }
                    },
                    "required": ["type"]
                },
                "bindings": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": {"type": "string"},
                            "ref_id": {"type": "string"}
                        },
                        "required": ["kind", "ref_id"]
                    }
                },
                "thread_policy": {
                    "type": "string",
                    "enum": ["main", "fresh", "dedicated"],
                    "description": "Which thread each fire lands in."
                },
                "working_directory": {
                    "type": "string",
                    "description": "New working directory / focus path."
                },
                "expires_at": {
                    "type": "string",
                    "description": "New RFC 3339 expiry datetime."
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Enable or disable the assignment."
                }
            },
            "required": ["assignment_id"],
            "additionalProperties": false
        })
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        if let Some(err) = super::reject_if_subagent(ctx) {
            return Ok(err);
        }

        let store = match &ctx.assignment_store {
            Some(s) => s.clone(),
            None => {
                return Ok(ToolOutput::Error {
                    recoverable: false,
                    message: "Assignment store not available in this context.".into(),
                });
            }
        };

        let assignment_id = match input.get("assignment_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Ok(ToolOutput::error("assignment_id is required", true)),
        };

        let mut updated = match store.get(&assignment_id).await {
            Some(a) => a,
            None => {
                return Ok(ToolOutput::Error {
                    recoverable: true,
                    message: format!("[Assignment error: \"{}\" not found]", assignment_id),
                });
            }
        };

        let was_already_active_agent_watch =
            updated.enabled && matches!(updated.trigger, AssignmentTrigger::AgentWatch { .. });

        if let Some(name) = input.get("name").and_then(Value::as_str) {
            if name.trim().is_empty() {
                return Ok(ToolOutput::error("name cannot be empty", true));
            }
            updated.name = name.trim().to_string();
        }

        if let Some(instruction) = input.get("instruction").and_then(Value::as_str) {
            updated.instruction = instruction.to_string();
        }

        if let Some(trigger_val) = input.get("trigger") {
            let is_agent_watch = trigger_val.get("type").and_then(Value::as_str) == Some("agent_watch");
            let trigger: AssignmentTrigger = if is_agent_watch {
                match parse_agent_watch_trigger_update(trigger_val) {
                    Ok(t) => t,
                    Err(msg) => return Ok(ToolOutput::error(msg, true)),
                }
            } else {
                match parse_trigger(trigger_val) {
                    Ok(t) => t,
                    Err(msg) => return Ok(ToolOutput::error(msg, true)),
                }
            };

            if let Err(msg) = trigger.validate() {
                return Ok(ToolOutput::error(msg, true));
            }

            let timezone = super::resolve_timezone(ctx).await;
            updated.next_fire_at = match &trigger {
                AssignmentTrigger::Cron { cron_expr, .. } => {
                    let nfa = compute_next_fire_at(Some(cron_expr), timezone.as_deref());
                    if nfa.is_none() {
                        return Ok(ToolOutput::error(
                            format!(
                                "invalid cron expression \"{}\". Use standard 5-field cron syntax (e.g. \"0 9 * * *\").",
                                cron_expr
                            ),
                            true,
                        ));
                    }
                    nfa
                }
                AssignmentTrigger::Webhook { .. } => None,
                // Poll ASAP after the trigger changes so the first tick seeds
                // the dedup baseline instead of waiting a full
                // `poll_interval_secs` (mirrors AssignmentCreate).
                AssignmentTrigger::ConnectorEvent { .. } => Some(Utc::now()),
                AssignmentTrigger::AgentWatch { .. } => Some(Utc::now()),
            };
            let (trigger, cleared_reason) = carry_forward_watch_contract(&updated.trigger, trigger);
            if let Some(reason) = cleared_reason {
                info!(assignment_id = %assignment_id, reason, "agent watch: clearing watch contract on update");
            }
            updated.trigger = trigger;
        }

        if let Some(v) = input.get("bindings") {
            match parse_bindings(v) {
                Ok(b) => updated.bindings = b,
                Err(msg) => return Ok(ToolOutput::error(msg, true)),
            }
        }

        if let Some(tp) = input
            .get("thread_policy")
            .and_then(Value::as_str)
            .and_then(parse_thread_policy)
        {
            updated.thread_policy = tp;
        }

        if let Some(wd) = input.get("working_directory").and_then(Value::as_str) {
            updated.working_directory = Some(wd.to_string());
        }

        if let Some(exp_str) = input.get("expires_at").and_then(Value::as_str) {
            match exp_str.parse::<chrono::DateTime<Utc>>() {
                Ok(dt) => updated.expires_at = Some(dt),
                Err(_) => {
                    return Ok(ToolOutput::error(
                        format!("invalid expires_at \"{}\". Use RFC 3339 format.", exp_str),
                        true,
                    ));
                }
            }
        }

        if let Some(enabled) = input.get("enabled").and_then(Value::as_bool) {
            updated.enabled = enabled;
        }

        if let Err(e) = store
            .enforce_agent_watch_cap(
                &updated.agent_id,
                &updated.trigger,
                updated.enabled,
                was_already_active_agent_watch,
            )
            .await
        {
            return Ok(ToolOutput::error(e.to_string(), true));
        }

        updated.updated_ts = Utc::now();

        let next_fire_display = updated
            .next_fire_at
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| "n/a".to_string());

        store.update(updated).await?;

        Ok(ToolOutput::text(format!(
            "[Assignment \"{}\" updated next_fire_at={}]",
            assignment_id, next_fire_display
        )))
    }
}

/// Parses a `type="agent_watch"` trigger replacement. Unlike
/// [`super::parse_trigger`]'s other cases, this only ever updates fields on
/// an existing `AgentWatch` trigger — `contract`/`extraction`/
/// `extraction_tool`/`extraction_output_schema_declared` aren't exposed here,
/// since they're derived from `instruction`/`connector_scope` rather than
/// settable directly (see [`carry_forward_watch_contract`]).
fn parse_agent_watch_trigger_update(value: &Value) -> Result<AssignmentTrigger, String> {
    let instruction = value
        .get("instruction")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "trigger.instruction is required for an agent_watch trigger".to_string())?
        .to_string();
    let poll_interval_secs = value
        .get("poll_interval_secs")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "trigger.poll_interval_secs is required for an agent_watch trigger".to_string()
        })?;
    let connector_scope = value
        .get("connector_scope")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(AssignmentTrigger::AgentWatch {
        instruction,
        poll_interval_secs,
        connector_scope,
        contract: None,
        extraction: None,
        extraction_tool: None,
        extraction_args: None,
        extraction_output_schema_declared: false,
    })
}
