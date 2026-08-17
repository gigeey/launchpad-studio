use serde_json::{json, Value};

pub const DESCRIPTION: &str = "Read or write settings in settings.json under the user data root. \
     Use action \"get\" with key to fetch a single value, \"set\" with key and \
     value to write atomically, or \"list\" to enumerate all top-level keys sorted \
     alphabetically.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["get", "set", "list"],
                "description": "Operation to perform."
            },
            "key": {
                "type": "string",
                "description": "Top-level key to read or write (required for get and set)."
            },
            "value": {
                "description": "Value to write (required for set; may be any JSON type)."
            }
        },
        "required": ["action"],
        "additionalProperties": false
    })
}
