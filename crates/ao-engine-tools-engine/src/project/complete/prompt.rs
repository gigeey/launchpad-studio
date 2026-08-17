use serde_json::{json, Value};

/// Description shown to the model so it understands when and why to call this tool.
pub const DESCRIPTION: &str = "\
Mark the project as Completed and record a final summary of what was accomplished.

This is the terminal action for a project — call it after all major work items have \
been finished, the goal stated in the spec has been met, and you have confirmed with \
the user that nothing else remains.

**Required field:**
- `summary` — a concise plain-text or Markdown summary of what was achieved. This is \
  the permanent record of the project's outcome, visible in the project history.

**Status transition rules:**
- Only legal from `Active` status. Attempting to complete a project that is \
  `Interviewing`, `Completed`, or `Archived` returns a recoverable error.
- Use `ProjectUpdate` with `activate=true` to transition from `Interviewing → Active` \
  before calling this tool.

**After completion:**
The project enters `Completed` status and cannot be modified further through these \
tools. The summary is persisted alongside the spec so the user can refer back to what \
was done.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {
                "type": "string",
                "description": "Concise summary of what the project accomplished. Written in past tense. Required."
            }
        },
        "required": ["summary"],
        "additionalProperties": false
    })
}
