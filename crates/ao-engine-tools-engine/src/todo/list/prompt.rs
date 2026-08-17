use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Lists all items in this agent's active dispatched tasklist with their current \
statuses and assignees. Not to be confused with TodoWrite, which is an ephemeral \
in-memory scratchpad.

Each task reports `assignee` (the agent id it will dispatch to, or null if \
unassigned) and `assignment_mode`: \"pinned\" means the owner was explicitly \
set (e.g. via TodoUpdate's owner field or at creation) and will not be \
reassigned by auto-classification; \"classified\" means the owner was chosen \
automatically and may still be re-routed; null means no assignment has been \
made yet.

Use TodoList to inspect the current state of the tasklist before calling \
TodoAdd, TodoUpdate, or TodoComplete. Returns an empty result if no active \
tasklist exists.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}
