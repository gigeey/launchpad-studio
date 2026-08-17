use serde_json::{json, Value};

/// Description shown to the model so it understands when this tool helps.
///
/// Only ever registered for a run when the acting agent has more than one
/// thread (see the eligibility check in `native.rs`'s session-init logic) —
/// with a single thread there is nothing else to list.
pub const DESCRIPTION: &str = "\
List every thread in your own chat — the one you're in right now plus any \
others the user split off (via forking/branching a conversation) or started \
fresh. Use this when the user references something \"from before\" or \"in \
the other thread\" that isn't in your current context, before asking them to \
repeat themselves.

Each entry reports the thread's id, display title, kind (\"default\", \
\"fresh\", or \"branch\"), created/updated timestamps, and whether it's the \
thread you're currently running in. Once you find a promising id, call \
SummarizeThread with it to catch up on that thread's content.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}
