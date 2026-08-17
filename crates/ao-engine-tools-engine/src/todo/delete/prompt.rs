use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Remove a single unstarted task from this agent's active tasklist.

Only tasks in the `pending` state (not yet dispatched to a subagent) can be \
removed. Attempting to remove an in-progress, completed, failed, or already-\
skipped task returns an error.

Use TodoList to look up task IDs before calling TodoDelete. The task is \
marked Skipped and will no longer be dispatched. This action cannot be undone \
— create a new task with TodoAdd if you need it back.

Returns the task_id of the removed task on success.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": {
                "type": "string",
                "description": "ID of the pending task to remove."
            }
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
}
