use serde_json::{json, Value};

/// Description shown to the model so it understands when and why to call this tool.
///
/// This tool is only ever registered for a run when the acting thread has no
/// title yet (see `Thread::offers_rename_tool`), so the description doesn't
/// need to caution the model about checking that first — if the model can
/// see this tool, naming the thread is legitimate right now.
pub const DESCRIPTION: &str = "\
Give the current chat thread a short, descriptive title. Call this at most once \
per thread, as soon as you have a clear sense of what the conversation is about — \
typically after the first exchange, not before.

Write a concise label (a few words, under 48 characters) that would help the user \
recognize this thread in a tab strip later, e.g. \"Fix login redirect bug\" or \
\"Q3 roadmap planning\". Do not restate the whole request verbatim.

If the thread already has a title (set by the user or an earlier call), calling \
this again returns a recoverable error and has no effect — there's no need to \
call it more than once.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": {
                "type": "string",
                "description": "Short descriptive title for the thread, under 48 characters."
            }
        },
        "required": ["title"],
        "additionalProperties": false
    })
}
