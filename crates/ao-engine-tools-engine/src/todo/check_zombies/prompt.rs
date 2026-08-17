use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Scan the active tasklist for zombie tasks — tasks marked InProgress whose \
assigned runner has no active runs. Returns one entry per zombie: task_id, \
a short title excerpt, how long ago the task was dispatched, and the \
assigned agent. Does NOT modify any task state.

Optional auto_requeue flag: when true, each zombie is reset to Pending so \
the feeder can re-dispatch it. Use this only after confirming the runner \
truly died — auto-requeue is irreversible while the tasklist is Active.

A grace_secs parameter (default 60) prevents falsely flagging a runner that \
is still starting. Tasks dispatched more recently than grace_secs are always \
skipped, even if no run has registered yet.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "grace_secs": {
                "type": "integer",
                "description": "Seconds after dispatch before a task is considered zombie-eligible. Default 60.",
                "minimum": 0
            },
            "auto_requeue": {
                "type": "boolean",
                "description": "When true, reset each detected zombie back to Pending for re-dispatch. Defaults to false."
            }
        },
        "required": []
    })
}
