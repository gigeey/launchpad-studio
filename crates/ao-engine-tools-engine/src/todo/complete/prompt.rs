use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Marks a specific task in this agent's active dispatched tasklist as completed. \
Not to be confused with TodoWrite, which is an ephemeral in-memory scratchpad.

Calling TodoComplete on a sequential (SEQ) tasklist advances the feeder to \
dispatch the next pending item. On a parallel (PAR) tasklist, it records \
completion without side effects on other running tasks.

Use TodoList to look up task IDs before calling TodoComplete.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": {
                "type": "string",
                "description": "ID of the task to mark as completed."
            },
            "summary": {
                "type": "string",
                "description": "Optional one-line summary of what was accomplished."
            }
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
}
