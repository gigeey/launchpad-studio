use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Re-queue a Stopped task so the feeder dispatches it again.

TodoResumeTask transitions the target task from Stopped to Pending, clears
its assignment, and bumps the classifier token so stale CAS results from
any prior classifier run are rejected. The feeder then picks the task up on
its next advance cycle.

In SEQ groups the resume respects list ordering: the resumed task runs before
any tasks that were waiting behind it. In PAR groups the advance does not
disturb tasks that are already running.

State transition: Stopped → Pending
Only valid on a Stopped task; rejects any other status.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": {
                "type": "string",
                "description": "ID of the Stopped task to re-queue for dispatch."
            }
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
}
