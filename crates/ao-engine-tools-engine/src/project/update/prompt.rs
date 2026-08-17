use serde_json::{json, Value};

/// Description shown to the model so it understands when and why to call this tool.
pub const DESCRIPTION: &str = "\
Partially update the project you are managing. All fields are optional — only \
the fields you provide are changed.

**Fields you can update:**
- `name` — display name shown in the sidebar
- `emoji` — single emoji character used as the project icon
- `spec` — the full project specification in Markdown, assembled during the \
  interview phase. Write the complete spec, not an incremental patch; each call \
  replaces whatever was stored before.
- `working_dir` — absolute path to the primary working directory for file \
  operations in this project (set once you know it from the user)
- `activate` — set to `true` to transition the project from `Interviewing` → \
  `Active`. This is the single call you make at the end of the interview once you \
  have a complete spec and the user has confirmed they are ready to start.

**Status transition rules:**
- `activate: true` is only legal when the current status is `Interviewing`. \
  Attempting to activate a project that is already `Active`, `Completed`, or \
  `Archived` returns a recoverable error — do not retry without the user's input.
- `ProjectComplete` is the dedicated tool for the `Active → Completed` transition; \
  do not attempt to set status directly here.

**Interview → activate pattern:**
1. Call `AskUserQuestionWithForm` to gather requirements.
2. Accumulate the answers into a structured spec (Markdown).
3. When the user confirms the spec, call `ProjectUpdate` with `spec=<final spec>` \
   and `activate=true` in a single call to record the spec and start the project \
   atomically.

After activation the project enters `Active` status and you switch from interviewing \
to orchestrating — use `TodoCreate`, `Delegate`, and other tools to drive the goal \
described in the spec.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "New display name for the project."
            },
            "emoji": {
                "type": ["string", "null"],
                "description": "Single emoji to use as the project icon. Pass null to clear."
            },
            "spec": {
                "type": ["string", "null"],
                "description": "Full project specification in Markdown. Replaces the existing spec. Pass null to clear."
            },
            "working_dir": {
                "type": ["string", "null"],
                "description": "Absolute path to the project's primary working directory. Pass null to clear."
            },
            "activate": {
                "type": "boolean",
                "description": "Set to true to transition status from Interviewing → Active. Only legal from Interviewing status."
            }
        },
        "additionalProperties": false
    })
}
