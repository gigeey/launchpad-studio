use serde_json::{json, Value};

pub const DESCRIPTION: &str = "\
Sends an email through this agent's connected email account (an enabled \
Email channel binding). Works from any thread — not just an email-bridge \
conversation — so use it any time the user asks you to email someone, or \
follow up on an email you saw earlier in this conversation.

**Composing a new email:** pass `to`, `subject`, and `body`. Omit \
`in_reply_to_message_id`.

**Replying to an email you received:** an inbound email delivered to you \
through the email channel includes a line like `Message-ID (pass this as \
in_reply_to_message_id when replying with SendEmail): <id>` — pass that \
value as `in_reply_to_message_id` so the reply threads correctly in the \
recipient's mail client (In-Reply-To/References headers). The subject is \
automatically prefixed with 'Re:' in this case if it isn't already.

`body` is plain text. `to` and `cc` accept either a single address string or \
an array of addresses. `binding_id` selects which email account to send \
from when more than one is configured; omit it when the agent has exactly \
one enabled Email binding.

Returns `{success, message_id}` on success. On failure returns an error \
noting whether the failure is retryable (e.g. a transient network problem) \
or not (e.g. a rejected recipient) — do not treat a non-retryable failure as \
something a bare retry will fix.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "to": {
                "description": "Recipient address, or an array of addresses.",
                "oneOf": [
                    { "type": "string" },
                    { "type": "array", "items": { "type": "string" } }
                ]
            },
            "subject": {
                "type": "string",
                "description": "Email subject. When replying (in_reply_to_message_id set), 'Re: ' is prepended automatically if not already present."
            },
            "body": {
                "type": "string",
                "description": "Plain-text email body."
            },
            "cc": {
                "description": "Optional Cc address, or an array of addresses.",
                "oneOf": [
                    { "type": "string" },
                    { "type": "array", "items": { "type": "string" } }
                ]
            },
            "in_reply_to_message_id": {
                "type": "string",
                "description": "The Message-ID of the email being replied to, as surfaced in that email's ingest text. Sets In-Reply-To/References for correct threading."
            },
            "binding_id": {
                "type": "string",
                "description": "Which Email channel binding to send from. Defaults to the agent's single enabled Email binding; required if more than one is configured."
            }
        },
        "required": ["to", "subject", "body"]
    })
}
