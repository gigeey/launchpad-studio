use serde_json::{json, Value};

/// How to use DelegateOutput and when.
pub const DESCRIPTION: &str = "\
Retrieve output or await completion of an async delegation.

After launching an async **Delegate** (mode=async), you have two options:
1. Wait for the automatic re-invocation — when the delegate finishes, a completion message \
   is queued to you and you will be re-invoked with it. No polling needed.
2. Call **DelegateOutput** when you want a bounded blocking wait — useful when you have \
   finished other independent work and want to collect the result immediately.

Each call returns only the events emitted since the previous call, so successive calls can \
stream live output without re-reading prior content.

`id` is the `delegation_id` returned by an async-mode **Delegate** call.

**Waiting for completion:**
`wait_seconds` (optional, default 0, max 120) controls how long to block before returning:
- `0` (default) — instant snapshot: returns immediately with whatever events are buffered so far.
- `> 0` — blocking wait: waits up to `wait_seconds` for the delegation to reach a terminal \
  state, then returns. If it finishes in time you get the terminal result directly; if the \
  deadline is reached first you get `status: \"running\"` with a `hint` field prompting you \
  to call again.

**Recommended pattern:** launch the delegation, continue other independent work, then call \
DelegateOutput once with a generous `wait_seconds` (e.g. 30–60) to collect the result as \
soon as it is ready without polling in a tight loop.

**Return values:**
- `status: \"running\"` — the delegate is still executing; `events` contains any new \
  output since the last call. When returned after a `wait_seconds` deadline, a `hint` \
  field is also present. Call again later (with `wait_seconds` if desired) to check for more.
- `status: \"completed\"` — the delegate finished normally; `final_result` contains its \
  last assistant message. A `stats` object (`duration_ms`, `num_turns`) is included when \
  the runner captured timing. The handle is reaped after this call.
- `status: \"failed\"` — the delegate errored before completing; `error` contains the \
  failure reason (e.g. provider misconfiguration or a failed model call). `stats` is \
  included when available. The handle is reaped after this call.
- `status: \"cancelled\"` — the delegate was cancelled (e.g. via DelegateStop); `events` \
  contains the tail of its output. `stats` is included when available. The handle is \
  reaped after this call.
- `status: \"indeterminate\"` — **the outcome is not known yet, and this is not a failure.** \
  Returned when the live handle is gone and the persisted transcript contains no terminal \
  event. `reason` distinguishes the cases: \
  `running-or-orphaned-no-terminal-event` (a transcript exists and events have landed, but \
  the run has not ended — it is most likely still working) or `no-transcript-found` (nothing \
  persisted yet). `last_event_at`, `last_activity_age_seconds`, and `event_count` report what \
  was actually observed, and `final_result` carries any partial output. \
  **Do not treat this as failure and do not re-dispatch the work** — a duplicate delegate \
  would edit the same files concurrently. Poll again with `wait_seconds`; if \
  `last_activity_age_seconds` keeps growing with no terminal event, escalate rather than \
  assume an outcome.

**Durability:** results survive server restarts. The live handle is frequently absent for \
ordinary reasons — it is dropped at each continuation step, not just on restart — so \
DelegateOutput falls back to the persisted sidechain transcript and returns the terminal \
result with a `recovered_from_transcript` note. `failed` is reported **only** when the \
transcript records an actual failure event; a still-running or orphaned delegate is reported \
as `indeterminate`, never as `failed`.

An error is returned only when `id` is missing or is not a well-formed delegation id. A \
valid id with no transcript is not an error — it is `indeterminate`.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "The delegation_id returned by an async-mode Delegate call."
            },
            "wait_seconds": {
                "type": "number",
                "description": "How long to block waiting for the delegation to finish (0–120 seconds, default 0). \
                    When 0 the call returns immediately with buffered events. When > 0 the call waits up to this \
                    many seconds: if the delegation completes first you get the terminal result; if the deadline \
                    is reached you get status=running with accumulated events and a hint to call again.",
                "default": 0,
                "minimum": 0,
                "maximum": 120
            }
        },
        "required": ["id"]
    })
}
