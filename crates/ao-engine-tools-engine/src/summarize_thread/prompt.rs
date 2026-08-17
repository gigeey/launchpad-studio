use serde_json::{json, Value};

/// Description shown to the model so it understands when this tool helps.
///
/// Only ever registered for a run when the acting agent has more than one
/// thread (see the eligibility check in `native.rs`'s session-init logic),
/// alongside `ListThreads`.
pub const DESCRIPTION: &str = "\
Ask a fresh, tool-less model call to read another thread in your own chat, \
start to finish, and summarize it in prose. Use this after ListThreads to \
catch up on a thread the user is referencing that isn't in your current \
context — e.g. \"like we decided in the pricing thread\" when you're not IN \
the pricing thread.

Optionally pass `focus` to steer the summary toward a specific question \
(e.g. \"what did we decide about the trial length?\") instead of a general \
recap. A very long thread is summarized from its opening messages plus the \
most recent ones, with older middle content elided; the response's \
`truncated` field tells you when that happened. This can only summarize \
threads that belong to your own chat — it cannot read another agent's, a \
team's, or a delegation's thread.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "thread_id": {
                "type": "string",
                "description": "id of the thread to summarize, as returned by ListThreads."
            },
            "focus": {
                "type": "string",
                "description": "Optional question or topic to focus the summary on, instead of a general recap."
            }
        },
        "required": ["thread_id"],
        "additionalProperties": false
    })
}
