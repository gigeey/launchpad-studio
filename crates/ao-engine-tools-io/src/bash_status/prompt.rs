//! Description text and input schema shown to the model for the BashStatus tool.

pub const DESCRIPTION: &str = "Check the status and recent output of a background shell command.

Use BashStatus to poll a command that was started with Bash using `run_in_background: true`.
Pass the `process_id` returned by that Bash call to retrieve:
- Current lifecycle state: `running`, `exited` (with exit code), `killed`, or `failed`.
- Recent combined stdout+stderr output from the in-memory ring buffer.
- A path to the on-disk log file where all output is persisted for the session.

## Incremental reads

Pass `offset` on subsequent calls to receive only the bytes written since the previous
check. The response always includes a `next_offset` value — save it and use it as `offset`
on the next BashStatus call to avoid re-reading output you have already seen.

`offset` counts total bytes written to the process's combined output stream since it started.
If the process has been running long enough that early bytes were evicted from the ring buffer,
the response will note the earliest available byte position and start from there instead.

If the process is still running, poll periodically with a delay between calls. Do not
busy-loop — a few seconds between polls is appropriate for most commands.";

pub const INPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "process_id": {
      "type": "string",
      "description": "The background command id returned by Bash (e.g. \"bash_1\")."
    },
    "offset": {
      "type": "integer",
      "description": "Byte offset into the combined output stream. Pass the next_offset from a previous BashStatus call to receive only new output. Defaults to 0 (return all buffered output from the start of the buffer)."
    }
  },
  "required": ["process_id"],
  "additionalProperties": false
}"#;
