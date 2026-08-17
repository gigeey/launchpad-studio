use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Force a zombie InProgress task back to Pending so the feeder can re-dispatch it.

A task becomes a zombie when its runner crashes or is killed while the task is
InProgress — status stays InProgress but no runner is actually working on it.
TodoRequeue clears the assignment and resets the task to Pending without losing
its position in the list. The feeder then picks it up on the next advance cycle.

Only valid on InProgress tasks. Completed, Failed, Skipped, and Pending tasks
are rejected. Safe to call when the runner is already dead — idempotent from
the feeder's perspective since nothing is dispatched until advance() fires.

Use TodoList to confirm the task is actually InProgress before calling.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": {
                "type": "string",
                "description": "ID of the InProgress task to reset back to Pending."
            }
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
}
