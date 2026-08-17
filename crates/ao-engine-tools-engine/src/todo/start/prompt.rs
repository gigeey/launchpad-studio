use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Transitions the agent's active Paused tasklist to Active so the feeder begins \
dispatching tasks. Not to be confused with TodoWrite, which is an ephemeral \
in-memory scratchpad.

Use TodoStart after staging tasks on a Paused tasklist (for example, one \
created with TodoCreate followed by several TodoAdd calls) when you are ready \
to commit the work and begin execution. The feeder will dispatch the first \
eligible task immediately after the call returns.

Idempotent on an already-Active list: it re-kicks the feeder without returning \
an error, so it is safe to call defensively.

The response reflects what this call actually did, not a fixed success \
message: `outcome` is one of `dispatched` (with `dispatched_task_ids`), \
`already_running` (a task was already in flight; nothing new dispatched), or \
`no_pending` (nothing left to dispatch). If the feeder could not be reached \
or failed to dispatch a ready task, this call errors instead of reporting a \
fake success.

Errors with a recoverable message if no Active or Paused tasklist exists — \
use TodoCreate to start a new one.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}
