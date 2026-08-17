use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Cancel the agent's active tasklist immediately.

All tasks that have not yet started (Pending or Blocked) are marked Skipped. \
Tasks already in flight are allowed to complete naturally — they will not be \
interrupted. The tasklist transitions to Cancelled and a cancelled block is \
appended to progress.jsonl in the shared workspace.

Use this tool when you decide the remaining work is no longer needed or when \
you want to stop a long-running async tasklist before it finishes on its own. \
There is no recovery path once a tasklist is cancelled — create a new one with \
TodoCreate if you need to restart.

Returns the tasklist_id, the count of tasks that were skipped, and the count \
of tasks still in flight when the cancel was issued.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}
