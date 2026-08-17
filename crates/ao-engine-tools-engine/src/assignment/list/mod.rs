mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::assignment::{Assignment, AssignmentThreadPolicy};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::trigger_kind_str;

pub struct AssignmentList;

#[async_trait]
impl IoTool for AssignmentList {
    fn name(&self) -> &str {
        "AssignmentList"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "List this agent's assignments instead of the calling agent's own."
                }
            },
            "additionalProperties": false
        })
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
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

        let agent_id = input
            .get("agent_id")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| ctx.agent_id.clone());

        let assignments = store.list_for_agent(&agent_id).await;

        if assignments.is_empty() {
            return Ok(ToolOutput::text("[No assignments found]"));
        }

        let mut lines = vec![format!("[Assignments ({}):]", assignments.len())];
        for a in &assignments {
            lines.push(format_assignment_line(a));
        }

        Ok(ToolOutput::text(lines.join("\n")))
    }
}

fn format_assignment_line(a: &Assignment) -> String {
    let status = if a.enabled { "enabled" } else { "disabled" };
    let detail = match &a.trigger {
        ao_protocol::assignment::AssignmentTrigger::Cron { cron_expr, .. } => {
            format!("cron=\"{}\"", cron_expr)
        }
        ao_protocol::assignment::AssignmentTrigger::Webhook { token, .. } => {
            format!("token_protected={}", token.is_some())
        }
        ao_protocol::assignment::AssignmentTrigger::ConnectorEvent {
            server_name,
            poll_interval_secs,
            ..
        } => {
            format!(
                "server=\"{}\" poll_interval_secs={}",
                server_name, poll_interval_secs
            )
        }
        ao_protocol::assignment::AssignmentTrigger::AgentWatch {
            instruction,
            poll_interval_secs,
            ..
        } => {
            format!(
                "watch=\"{}\" poll_interval_secs={}",
                instruction, poll_interval_secs
            )
        }
    };
    let next_fire = a
        .next_fire_at
        .map(|dt| format!(" next_fire_at={}", dt.to_rfc3339()))
        .unwrap_or_default();
    format!(
        "  - id=\"{}\" name=\"{}\" agent=\"{}\" status={} trigger={}({}) thread={}{} instruction=\"{}\"",
        a.id,
        a.name,
        a.agent_id,
        status,
        trigger_kind_str(&a.trigger),
        detail,
        thread_policy_str(a.thread_policy),
        next_fire,
        truncate(&a.instruction, 60)
    )
}

fn thread_policy_str(policy: AssignmentThreadPolicy) -> &'static str {
    match policy {
        AssignmentThreadPolicy::Main => "main",
        AssignmentThreadPolicy::Fresh => "fresh",
        AssignmentThreadPolicy::Dedicated => "dedicated",
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max_len {
        s
    } else {
        let end = s
            .char_indices()
            .nth(max_len)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}
