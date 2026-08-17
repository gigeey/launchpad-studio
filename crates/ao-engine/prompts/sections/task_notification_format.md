## Task Notification Format

When you finish a tasklist task, emit a `<task action="…">` tag with a `<task-item-notification>` block **as its body**. The notification is nested inside the task tag — not a sibling — so the two pieces are sent as one indivisible XML element. The runtime parses both together; a missing or malformed notification is treated as a parse failure and triggers an auto-reprompt.

### Required shape (nested)

```xml
<task action="complete" task_id="t-xyz">
  <task-item-notification>
    <status>complete</status>
    <summary>One-line human-readable summary of what changed.</summary>
    <details>Optional longer body. Free-form text or markdown. May be omitted.</details>
  </task-item-notification>
</task>
```

**Do NOT** emit the self-closing form `<task action="complete" task_id="…" />` for `complete` or `fail`. The notification must be present as the body, so the body form is required.

### Fields inside `<task-item-notification>`

- `status` (required) — one of `complete`, `failed`, `needs_clarification`. Mirror the action you used in the wrapping `<task action="…">` tag.
- `summary` (required) — a single line, no more than ~200 characters, describing the outcome in past tense (e.g. `"Wrote splunk_logs.json with 1,284 entries from the last 24h."`).
- `details` (optional) — additional context for downstream readers: file paths produced, links, anomalies found, follow-up recommendations. Free-form; markdown is fine. Omit the element entirely if you have nothing to add.

### Example

A complete final message might look like:

```
I finished collecting the splunk logs and wrote the summary.

<task action="complete" task_id="t-collect">
  <task-item-notification>
    <status>complete</status>
    <summary>Wrote splunk_logs.json (1,284 entries) and summary.md to the workspace.</summary>
    <details>
    Time range: last 24h.
    Top error class: timeout (37%).
    Files: splunk_logs.json, summary.md.
    </details>
  </task-item-notification>
</task>
```

If the wrapping `<task>` tag is self-closing, or its body does not contain a valid `<task-item-notification>` block, the runtime will not mark the task complete; instead it re-queues a structured followup asking you to re-emit a valid nested block, capped at a small number of retries before falling back to a synthesized entry.
