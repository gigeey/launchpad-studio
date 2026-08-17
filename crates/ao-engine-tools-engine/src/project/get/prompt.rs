use serde_json::{json, Value};

/// Description shown to the model so it understands when and why to call this tool.
pub const DESCRIPTION: &str = "\
Retrieve the current state of the project you are managing.

Returns the project's goal, name, emoji, working directory, status, spec \
(the structured specification you build during the interview), and the list of \
file attachments the user has provided.

**When to use:**
- At the start of a session to re-orient yourself on the project's goal and \
  status before taking any action.
- After any external event (e.g. a user message or a completed tasklist) to \
  confirm the project record reflects the latest state.
- Whenever you need to verify the current spec before deciding what to do next.

This tool reads only — it makes no changes. Use `ProjectUpdate` to modify the \
project record, and `ProjectComplete` to mark the project finished.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}
