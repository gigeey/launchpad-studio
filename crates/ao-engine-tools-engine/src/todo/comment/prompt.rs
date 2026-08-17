use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Attaches a durable comment to a specific task in this agent's active dispatched tasklist. \
The comment is persisted and surfaced in the task-detail UI alongside the task's prompt and status.

Use this when you want to annotate a task with additional context, progress notes, or findings \
without changing the task's status or completing it.

Agents whose output is processed via the queue manager may alternatively emit an inline tag:\n\
  <task_comment tasklist_id=\"...\" task_id=\"...\">body</task_comment>\n\
The queue manager extracts that tag deterministically from the agent's response text. \
This tool is the explicit-call equivalent and is preferred when direct invocation is available.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": {
                "type": "string",
                "description": "ID of the task to comment on."
            },
            "comment": {
                "type": "string",
                "description": "Comment text to attach to the task."
            }
        },
        "required": ["task_id", "comment"],
        "additionalProperties": false
    })
}
