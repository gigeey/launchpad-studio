use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Appends a new group of items to this agent's active dispatched tasklist. \
Not to be confused with TodoWrite, which is an ephemeral in-memory scratchpad.

Use TodoAdd when work emerges mid-flight and needs to be tracked and dispatched \
alongside existing items. The new items are added as a separate group on the \
active tasklist; they inherit the chosen mode (sequential or parallel).

Requires an active tasklist created via TodoCreate. Use TodoList to inspect \
the current list state before adding.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "minItems": 1,
                "description": "New items to append as a group to the active tasklist.",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "One-line task title shown in the UI."
                        },
                        "brief": {
                            "type": "string",
                            "description": "Detailed prompt sent to the executing agent for this item."
                        },
                        "owner": {
                            "type": "string",
                            "description": "Optional: agent_id of the delegate to assign. Defaults to the coordinator agent."
                        }
                    },
                    "required": ["title", "brief"],
                    "additionalProperties": false
                }
            },
            "mode": {
                "type": "string",
                "enum": ["seq", "par"],
                "description": "Execution mode for the new group: 'seq' runs items one-by-one (default); 'par' runs all concurrently."
            }
        },
        "required": ["items"],
        "additionalProperties": false
    })
}
