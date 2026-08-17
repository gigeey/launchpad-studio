mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_persistence::cron_util::compute_next_fire_at;
use ao_protocol::assignment::{Assignment, AssignmentTrigger, OutputMode};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{
    default_thread_policy_for_trigger, parse_bindings, parse_thread_policy, parse_trigger,
    trigger_kind_str,
};

pub struct AssignmentCreate;

#[async_trait]
impl IoTool for AssignmentCreate {
    fn name(&self) -> &str {
        "AssignmentCreate"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Human-readable label for the assignment."
                },
                "instruction": {
                    "type": "string",
                    "description": "The instruction injected into the agent each time this assignment fires."
                },
                "trigger": {
                    "type": "object",
                    "description": "What fires this assignment.",
                    "properties": {
                        "type": {
                            "type": "string",
                            "enum": ["schedule", "webhook", "connector_event"],
                            "description": "\"schedule\" (cron-based), \"webhook\" (inbound HTTP POST), or \"connector_event\" (poll a connector on a timer and fire when it reports something new)."
                        },
                        "cron_expr": {
                            "type": "string",
                            "description": "Standard 5-field cron expression (e.g. \"0 9 * * *\"). Required when type=schedule."
                        },
                        "is_recurring": {
                            "type": "boolean",
                            "description": "When false, the schedule fires once then disables itself. Defaults to true. Only used when type=schedule."
                        },
                        "token": {
                            "type": "string",
                            "description": "Optional shared secret the inbound POST must present. Only used when type=webhook."
                        },
                        "server_name": {
                            "type": "string",
                            "description": "The MCP/connector server to poll (matches a connected server's name, e.g. from AssignmentList / the MCP servers panel). Required when type=connector_event."
                        },
                        "poll": {
                            "type": "object",
                            "description": "What to poll and how to detect a new event. Required when type=connector_event.",
                            "properties": {
                                "tool_name": {
                                    "type": "string",
                                    "description": "The connector's MCP tool to call each poll (e.g. \"list_starred\", \"search_issues\")."
                                },
                                "arguments": {
                                    "type": "object",
                                    "description": "Arguments passed to the tool call. Defaults to {}."
                                },
                                "cursor_path": {
                                    "type": "string",
                                    "description": "Dot-path into the tool result used to detect changes (e.g. \"structuredContent.latest_id\"). Effectively required: if omitted, or if it never resolves against a poll result, this trigger's cursor never advances and it never fires."
                                }
                            },
                            "required": ["tool_name"]
                        },
                        "poll_interval_secs": {
                            "type": "integer",
                            "description": "Minimum seconds between polls. Required (> 0) when type=connector_event."
                        }
                    },
                    "required": ["type"]
                },
                "bindings": {
                    "type": "array",
                    "description": "Optional connectors this run may use, e.g. [{\"kind\": \"mcp_server\", \"ref_id\": \"gmail\"}].",
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
                    "description": "Which thread each fire lands in: \"main\" (the agent's default thread), \"fresh\" (a new disposable thread every fire), or \"dedicated\" (one reused thread across every fire). Defaults from the trigger type when omitted."
                },
                "working_directory": {
                    "type": "string",
                    "description": "Working directory / focus path for the fired run."
                },
                "expires_at": {
                    "type": "string",
                    "description": "Optional RFC 3339 datetime after which the assignment stops firing."
                },
                "agent_id": {
                    "type": "string",
                    "description": "Agent that owns this assignment. Defaults to the calling agent."
                }
            },
            "required": ["name", "instruction", "trigger"],
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

        let name = match input.get("name").and_then(Value::as_str).map(str::trim) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => return Ok(ToolOutput::error("name is required", true)),
        };

        let instruction = match input.get("instruction").and_then(Value::as_str) {
            Some(i) if !i.trim().is_empty() => i.to_string(),
            _ => return Ok(ToolOutput::error("instruction is required", true)),
        };

        let trigger_val = match input.get("trigger") {
            Some(t) => t,
            None => return Ok(ToolOutput::error("trigger is required", true)),
        };

        let trigger: AssignmentTrigger = match parse_trigger(trigger_val) {
            Ok(t) => t,
            Err(msg) => return Ok(ToolOutput::error(msg, true)),
        };

        if let Err(msg) = trigger.validate() {
            return Ok(ToolOutput::error(msg, true));
        }

        if let AssignmentTrigger::Cron { ref cron_expr, .. } = trigger {
            let timezone = super::resolve_timezone(ctx).await;
            if compute_next_fire_at(Some(cron_expr), timezone.as_deref()).is_none() {
                return Ok(ToolOutput::error(
                    format!(
                        "invalid cron expression \"{}\". Use standard 5-field cron syntax (e.g. \"0 9 * * *\").",
                        cron_expr
                    ),
                    true,
                ));
            }
        }

        let bindings = match input.get("bindings") {
            Some(v) => match parse_bindings(v) {
                Ok(b) => b,
                Err(msg) => return Ok(ToolOutput::error(msg, true)),
            },
            None => Vec::new(),
        };

        let thread_policy = input
            .get("thread_policy")
            .and_then(Value::as_str)
            .and_then(parse_thread_policy)
            .unwrap_or_else(|| default_thread_policy_for_trigger(&trigger));

        let working_directory = input
            .get("working_directory")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        let expires_at = match input.get("expires_at").and_then(Value::as_str) {
            Some(s) => match s.parse::<chrono::DateTime<Utc>>() {
                Ok(dt) => Some(dt),
                Err(_) => {
                    return Ok(ToolOutput::error(
                        format!("invalid expires_at \"{}\". Use RFC 3339 format.", s),
                        true,
                    ));
                }
            },
            None => None,
        };

        let agent_id = input
            .get("agent_id")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| ctx.agent_id.clone());

        if let Err(e) = store
            .enforce_agent_watch_cap(&agent_id, &trigger, true, false)
            .await
        {
            return Ok(ToolOutput::error(e.to_string(), true));
        }

        let timezone = super::resolve_timezone(ctx).await;
        let now = Utc::now();
        let next_fire_at = match &trigger {
            AssignmentTrigger::Cron { cron_expr, .. } => {
                compute_next_fire_at(Some(cron_expr), timezone.as_deref())
            }
            AssignmentTrigger::Webhook { .. } => None,
            // Poll ASAP after creation so the first tick seeds the dedup
            // baseline instead of waiting a full `poll_interval_secs`.
            AssignmentTrigger::ConnectorEvent { .. } => Some(now),
            AssignmentTrigger::AgentWatch { .. } => Some(now),
        };

        let assignment_id = Uuid::new_v4().to_string();
        let assignment = Assignment {
            id: assignment_id.clone(),
            agent_id,
            name,
            instruction,
            working_directory,
            trigger,
            bindings,
            output_mode: OutputMode::Background,
            thread_policy,
            dedicated_thread_id: None,
            enabled: true,
            expires_at,
            next_fire_at,
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        };

        store.add(assignment.clone()).await?;

        let next_fire_display = next_fire_at
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| "n/a".to_string());

        Ok(ToolOutput::text(format!(
            "[Assignment created: id=\"{}\" trigger={} next_fire_at={}]",
            assignment_id,
            trigger_kind_str(&assignment.trigger),
            next_fire_display
        )))
    }
}
