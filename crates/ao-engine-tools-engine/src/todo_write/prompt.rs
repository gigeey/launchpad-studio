use serde_json::{json, Value};

pub const DESCRIPTION: &str = "Replace the agent's todo list. Pass `todos` as an array of items; \
     each item requires `id` (unique string), `content` (string), and \
     `status` (\"pending\", \"in_progress\", or \"completed\"). \
     Optional: `active_form` (display form; defaults to `content`) and \
     `priority` (\"low\", \"medium\", or \"high\").";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "todos": {
                "type": "array",
                "description": "The complete new todo list. Replaces any existing items.",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "content": { "type": "string" },
                        "status": {
                            "type": "string",
                            "enum": ["pending", "in_progress", "completed"]
                        },
                        "active_form": { "type": "string" },
                        "priority": {
                            "type": "string",
                            "enum": ["low", "medium", "high"]
                        }
                    },
                    "required": ["id", "content", "status"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["todos"],
        "additionalProperties": false
    })
}
