use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Updates fields on a specific task in this agent's active dispatched tasklist. \
Not to be confused with TodoWrite, which is an ephemeral in-memory scratchpad.

Use TodoUpdate to edit the task prompt, reassign the owning delegate, or \
adjust expected outputs before the task has been dispatched. Changes to \
in-progress or completed tasks are persisted but may not affect the current run.

Reassigning `owner` on a task in an agent-owned tasklist pins that task to \
the new owner going forward (the auto-router will never re-assign it), \
mirroring the owner pin set when a task is created with an explicit owner.

Use TodoList to look up task IDs before calling TodoUpdate.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": {
                "type": "string",
                "description": "ID of the task to update."
            },
            "prompt": {
                "type": "string",
                "description": "Replacement prompt text for the task. Omit to leave unchanged."
            },
            "owner": {
                "type": "string",
                "description": "agent_id to reassign this task to. Omit to leave unchanged."
            },
            "expected_outputs": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Replacement expected-output list. Omit to leave unchanged."
            }
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
}
