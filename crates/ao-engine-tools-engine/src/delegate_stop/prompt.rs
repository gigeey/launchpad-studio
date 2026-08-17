use serde_json::{json, Value};

/// When and how to use DelegateStop.
pub const DESCRIPTION: &str = "\
Cancel a specific async delegation by id without affecting its siblings.

Call **DelegateStop** when you no longer need an async **Delegate** running in the background
and want to release it immediately. The call is idempotent: cancelling an already-cancelled
delegation returns `status: \"already_cancelled\"` rather than an error, so you can safely retry.

`id` must be the `delegation_id` returned by a previous async-mode **Delegate** call.

The cancelled delegation's handle remains in the registry until **DelegateOutput** polls it to
completion, at which point the handle is reaped and the final `cancelled` status is returned.

If the id is unknown or already reaped, a recoverable error is returned.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "The delegation_id of the async delegation to cancel."
            }
        },
        "required": ["id"]
    })
}
