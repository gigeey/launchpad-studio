use serde_json::{json, Value};

pub const DESCRIPTION: &str = "Send a short status message to the user. \
     Provide a `summary` (required, ≥1 visible character). \
     Optionally provide `details` for additional context appended after a blank line.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {
                "type": "string",
                "description": "Short status message shown to the user (required, must contain at least one non-whitespace character)."
            },
            "details": {
                "type": "string",
                "description": "Optional additional context, appended after a blank line."
            }
        },
        "required": ["summary"],
        "additionalProperties": false
    })
}
