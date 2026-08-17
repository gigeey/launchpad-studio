use serde_json::{json, Value};

pub const ENTER_DESCRIPTION: &str = "Switch to plan mode. While active, filesystem-mutating \
     tools are denied. Use ExitPlanMode to restore the prior permission mode. Idempotent.";

pub const EXIT_DESCRIPTION: &str = "Restore the permission mode saved by the most recent \
     EnterPlanMode call. Idempotent when not in plan mode.";

pub fn enter_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

pub fn exit_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}
