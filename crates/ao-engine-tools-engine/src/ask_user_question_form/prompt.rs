use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Present a structured form to the operator and collect their answers. \
Use this when you need the user to fill in one or more fields — checkboxes, radio buttons, \
free text, or file uploads — before proceeding. \
This is also the tool for a quick multiple-choice question: send a single `radio` field \
whose options are the candidate answers (a single-option radio works as an OK/confirm \
prompt). \
Include a clear `title` and use descriptive field labels so the operator knows exactly what \
information is needed. \
\n\n\
Choose `mode` based on how long the form may take to answer:\n\
- `\"sync\"` (default) — the agent pauses until the operator submits. Use for quick, \
  in-flow questions the user can answer right now.\n\
- `\"async\"` — the form is posted as a durable message in the conversation; the agent's \
  turn ends immediately with `{\"posted\":true,\"form_id\":\"...\"}`. Use when the form \
  requires research, uploads, or other work that may take minutes to hours. The operator \
  submits at their own pace and the answer re-enters as a new turn.\n\
\n\
If this tool returns 'no operator available to present form', there is no interactive user in \
this session. Proceed without waiting for user input.\
\n\n\
In `\"sync\"` mode, instead of submitting, the operator may click an action button on the form \
instead: the result then has an `\"action\"` field (no `\"answers\"`) with one of these values, \
and you should react to it rather than re-reading it as an empty submission:\n\
- `\"cancel\"` — the operator doesn't want to answer this right now. Don't ask the same \
  question again; move on or ask if there's something else you can help with.\n\
- `\"regenerate\"` — the questions weren't right. Call this tool again with different or \
  better-targeted questions (don't just resend the same form — if you're unsure what to change, \
  ask in chat what was wrong first, so you don't loop on the same form).\n\
- `\"other\"` — the operator wants something not covered by the fields you offered. Immediately \
  ask one open-ended question in chat to find out what, rather than guessing.\
\n\n\
`\"sync\"` mode has a deadline. If nobody answers before it elapses, the result is \
`{\"outcome\":\"form_timed_out\",\"timeout_secs\":<n>}` — no `\"answers\"`, no `\"action\"`. This \
means the operator never responded, NOT that they submitted an empty or default answer. \
ABORT the action this form was meant to inform: do not guess, infer, or proceed using a default \
value for the missing answer. Tell the user in chat that you asked but the form timed out before \
they responded, and stop there — wait for them to follow up rather than resubmitting the same \
form or continuing on your own assumption of what they would have said.\
\n\n\
A still-open `\"sync\"` form can also end in cancellation instead of a timeout — this is a \
distinct outcome from the deadline above, surfaced as a tool error whose message is exactly \
`\"cancelled\"` (not the `form_timed_out` shape, and not an `\"action\":\"cancel\"` response, \
which only happens when the operator was actually looking at the form and chose to dismiss it). \
A bare `\"cancelled\"` error means the wait was cut short from outside the form itself — e.g. the \
session ended — before anyone could respond either way. React exactly as you would to a timeout: \
ABORT the action this form was meant to inform, do not guess or proceed on an assumed answer, and \
tell the user the question went unanswered rather than resubmitting the same form.";

pub fn input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "required": ["title", "questions"],
        "additionalProperties": false,
        "properties": {
            "title": {
                "type": "string",
                "description": "Short heading shown above the form (≤200 chars)",
                "minLength": 1,
                "maxLength": 200
            },
            "intro": {
                "type": "string",
                "description": "Optional paragraph below the title providing context",
                "maxLength": 1000
            },
            "mode": {
                "type": "string",
                "enum": ["sync", "async"],
                "default": "sync",
                "description": "Delivery mode. \"sync\" (default) parks the agent until the operator submits. \"async\" posts the form as a durable message and returns immediately — use when the form may take minutes or hours to complete."
            },
            "questions": {
                "type": "array",
                "description": "One or more fields to present to the user",
                "minItems": 1,
                "maxItems": 8,
                "items": { "$ref": "#/definitions/FormField" }
            }
        },
        "definitions": {
            "FormField": {
                "type": "object",
                "required": ["id", "type", "label"],
                "additionalProperties": false,
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Stable identifier used to key the answer map",
                        "minLength": 1,
                        "maxLength": 64,
                        "pattern": "^[a-zA-Z0-9_-]+$"
                    },
                    "type": {
                        "type": "string",
                        "description": "Field type; drives the UI control rendered",
                        "enum": ["checkbox", "radio", "text", "textarea", "file"]
                    },
                    "label": {
                        "type": "string",
                        "description": "Question or label text shown to the user",
                        "minLength": 1,
                        "maxLength": 300
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional helper text shown below the label",
                        "maxLength": 500
                    },
                    "options": {
                        "type": "array",
                        "description": "Required for checkbox/radio; must be absent for other types",
                        "minItems": 1,
                        "maxItems": 12,
                        "items": { "$ref": "#/definitions/FormOption" }
                    },
                    "required": {
                        "type": "boolean",
                        "description": "Whether the user must provide an answer before submitting",
                        "default": false
                    },
                    "placeholder": {
                        "type": "string",
                        "description": "Ghost text inside text/textarea inputs; ignored for other types",
                        "maxLength": 200
                    },
                    "max_files": {
                        "type": "integer",
                        "description": "Maximum number of files for file fields (default 1); ignored for other types",
                        "minimum": 1,
                        "maximum": 10
                    },
                    "accept": {
                        "type": "string",
                        "description": "MIME-type filter string for file fields (e.g. 'image/*,application/pdf'); ignored for other types",
                        "maxLength": 200
                    }
                }
            },
            "FormOption": {
                "type": "object",
                "required": ["id", "label"],
                "additionalProperties": false,
                "properties": {
                    "id": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 64,
                        "pattern": "^[a-zA-Z0-9_-]+$"
                    },
                    "label": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 200
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional one-line hint shown below the option label",
                        "maxLength": 400
                    }
                }
            }
        }
    })
}
