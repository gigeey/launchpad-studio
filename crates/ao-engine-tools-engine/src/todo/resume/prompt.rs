use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Resumes the agent's most recent Failed tasklist by resetting every Failed task \
back to Pending and flipping the tasklist to Active. Not to be confused with \
TodoWrite, which is an ephemeral in-memory scratchpad.

Use TodoResume after fixing the root cause of a tasklist failure (for example, \
correcting a broken tool call, updating a file the tasks depended on, or \
providing missing context) to retry the failed tasks without recreating the \
entire list from scratch. Tasks that already completed or were skipped are left \
untouched.

Returns the tasklist_id and the count of tasks that were reset to Pending so \
you know exactly what will re-dispatch.

Errors with a recoverable message if:
- The agent has no failed tasklist (use TodoCreate to start fresh).
- Another Active or Paused tasklist is already running (cancel or complete it \
  first with TodoCancel).";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}
