pub mod create;
pub mod delete;
pub mod list;
pub mod trigger;
pub mod update;

pub use create::AssignmentCreate;
pub use delete::AssignmentDelete;
pub use list::AssignmentList;
pub use trigger::AssignmentTrigger;
pub use update::AssignmentUpdate;

use ao_engine_tools_core::{Registry, RunnerContext, ToolOutput};
use ao_protocol::assignment::{AssignmentBinding, AssignmentThreadPolicy};
use ao_protocol::assignment::AssignmentTrigger as AssignmentTriggerModel;
use serde_json::Value;
use std::sync::Arc;

/// Gate helper: returns Some(error ToolOutput) when the caller is a subagent
/// (depth > 0), None when it is the top-level agent. Called as the FIRST
/// line of every tool's invoke — an assignment is a standing automation the
/// user set up for their top-level agent, so a subagent spun up mid-task
/// must not be able to create, change, or fire one behind the user's back.
pub(super) fn reject_if_subagent(ctx: &RunnerContext) -> Option<ToolOutput> {
    if ctx.depth > 0 {
        Some(ToolOutput::Error {
            recoverable: true,
            message: "Assignment management is restricted to the top-level agent. Ask your parent agent to handle this.".into(),
        })
    } else {
        None
    }
}

/// Resolve the user's preferred timezone from preferences, best-effort.
/// Returns None if preferences are unavailable or no timezone is set.
pub(super) async fn resolve_timezone(ctx: &RunnerContext) -> Option<String> {
    if let Some(prefs_store) = &ctx.preferences {
        if let Ok(Some(prefs)) = prefs_store.get().await {
            return prefs.timezone;
        }
    }
    None
}

/// Parse a `thread_policy` tool-input string into its enum value. Returns
/// `None` for anything other than the three recognized wire values so callers
/// can distinguish "omitted/invalid" (fall back to the trigger-dependent
/// default) from an explicit choice.
pub(super) fn parse_thread_policy(s: &str) -> Option<AssignmentThreadPolicy> {
    match s {
        "main" => Some(AssignmentThreadPolicy::Main),
        "fresh" => Some(AssignmentThreadPolicy::Fresh),
        "dedicated" => Some(AssignmentThreadPolicy::Dedicated),
        _ => None,
    }
}

/// Trigger-dependent `thread_policy` default (mirrors the same rule the
/// `/assignments` HTTP route applies): a schedule feels like a reminder, so
/// it lands in the agent's main thread; a webhook or connector event is an
/// untrusted/external kick, so it gets a disposable thread that can never
/// interrupt a live chat.
pub(super) fn default_thread_policy_for_trigger(trigger: &AssignmentTriggerModel) -> AssignmentThreadPolicy {
    match trigger {
        AssignmentTriggerModel::Cron { .. } => AssignmentThreadPolicy::Main,
        AssignmentTriggerModel::Webhook { .. } => AssignmentThreadPolicy::Fresh,
        AssignmentTriggerModel::ConnectorEvent { .. } => AssignmentThreadPolicy::Fresh,
        AssignmentTriggerModel::AgentWatch { .. } => AssignmentThreadPolicy::Fresh,
    }
}

/// Parse the `poll` object of a `connector_event` trigger into a
/// `ConnectorPollSpec`. Returns `Err(message)` on any malformed shape.
fn parse_poll_spec(value: &Value) -> Result<ao_protocol::assignment::ConnectorPollSpec, String> {
    let tool_name = value
        .get("tool_name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "trigger.poll.tool_name is required for a connector_event trigger".to_string())?
        .to_string();
    let arguments = value
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let cursor_path = value
        .get("cursor_path")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    Ok(ao_protocol::assignment::ConnectorPollSpec {
        tool_name,
        arguments,
        cursor_path,
    })
}

/// Parse the `trigger` object from tool input. Returns `Err(message)` with a
/// user-facing explanation on any malformed shape.
pub(super) fn parse_trigger(value: &Value) -> Result<AssignmentTriggerModel, String> {
    let trigger_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "trigger.type is required (one of: schedule, webhook, connector_event)".to_string())?;

    match trigger_type {
        "schedule" => {
            let cron_expr = value
                .get("cron_expr")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "trigger.cron_expr is required for a schedule trigger".to_string())?
                .to_string();
            let is_recurring = value
                .get("is_recurring")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            Ok(AssignmentTriggerModel::Cron {
                cron_expr,
                is_recurring,
            })
        }
        "webhook" => {
            let token = value
                .get("token")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            // Route/filter/prompt-template/deliver fields are not yet
            // exposed through this tool's input schema — the tool still
            // only creates the legacy token-checked shape.
            Ok(AssignmentTriggerModel::Webhook {
                token,
                route_name: None,
                secret_ref: None,
                events: vec![],
                filters: None,
                prompt_template: None,
                deliver: Default::default(),
            })
        }
        "connector_event" => {
            let server_name = value
                .get("server_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "trigger.server_name is required for a connector_event trigger".to_string())?
                .to_string();
            let poll_interval_secs = value.get("poll_interval_secs").and_then(Value::as_u64).unwrap_or(0);
            if poll_interval_secs == 0 {
                return Err("trigger.poll_interval_secs must be a positive integer for a connector_event trigger".to_string());
            }
            let poll_val = value
                .get("poll")
                .filter(|v| v.is_object())
                .ok_or_else(|| "trigger.poll (object) is required for a connector_event trigger".to_string())?;
            let poll = parse_poll_spec(poll_val)?;
            Ok(AssignmentTriggerModel::ConnectorEvent {
                server_name,
                poll,
                poll_interval_secs,
            })
        }
        other => Err(format!(
            "unknown trigger.type \"{}\" — must be one of: schedule, webhook, connector_event",
            other
        )),
    }
}

/// Parse the optional `bindings` array into `Vec<AssignmentBinding>`. Each
/// entry must be an object with string `kind` and `ref_id` fields.
pub(super) fn parse_bindings(value: &Value) -> Result<Vec<AssignmentBinding>, String> {
    let Some(items) = value.as_array() else {
        return Err("bindings must be an array".to_string());
    };
    items
        .iter()
        .map(|item| {
            let kind = item
                .get("kind")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "each binding requires a non-empty \"kind\"".to_string())?
                .to_string();
            let ref_id = item
                .get("ref_id")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "each binding requires a non-empty \"ref_id\"".to_string())?
                .to_string();
            Ok(AssignmentBinding { kind, ref_id })
        })
        .collect()
}

/// Human-readable label for a trigger, used in tool output text.
pub(super) fn trigger_kind_str(trigger: &AssignmentTriggerModel) -> &'static str {
    match trigger {
        AssignmentTriggerModel::Cron { .. } => "schedule",
        AssignmentTriggerModel::Webhook { .. } => "webhook",
        AssignmentTriggerModel::ConnectorEvent { .. } => "connector_event",
        AssignmentTriggerModel::AgentWatch { .. } => "agent_watch",
    }
}

pub fn register_assignment_tools(registry: &mut Registry) {
    registry.register_io(Arc::new(AssignmentCreate));
    registry.register_io(Arc::new(AssignmentList));
    registry.register_io(Arc::new(AssignmentUpdate));
    registry.register_io(Arc::new(AssignmentDelete));
    registry.register_io(Arc::new(AssignmentTrigger));
}

#[cfg(test)]
pub(crate) mod tests {
    use ao_persistence::{assignment_store::AssignmentStore, paths::DataRoot};
    use std::sync::Arc;

    /// Create a fresh AssignmentStore backed by a temp directory.
    /// Caller must keep the TempDir alive for the duration of the test.
    pub async fn temp_store() -> (tempfile::TempDir, Arc<AssignmentStore>) {
        let dir = tempfile::TempDir::new().unwrap();
        let data_root = DataRoot::new(dir.path());
        let store = Arc::new(AssignmentStore::load(data_root).await.unwrap());
        (dir, store)
    }
}
