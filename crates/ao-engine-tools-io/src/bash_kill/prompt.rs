//! Description text and input schema shown to the model for the BashKill tool.

pub const DESCRIPTION: &str = "Terminate a running background shell command.

Use BashKill to stop a command that was started with Bash using `run_in_background: true`.
Pass the `process_id` returned by that Bash call. The whole process group receives SIGTERM,
then SIGKILL after a 5-second grace period, so child processes the command spawned are
stopped too. A killed command cannot be resumed.

Returns an error if the process id is unknown, or if the command has already exited or been
killed. Use BashStatus first if you are unsure whether the command is still running.

This call waits for the process to actually stop, so check the `status` it returns:
- `killed` — confirmed stopped; BashStatus will also report `killed`.
- `exited` — the command finished on its own before the signal landed. Nothing was killed
  and `exit_code` is its real exit code.
- `kill_requested` — the signal was sent but the process was not confirmed stopped within
  10 seconds. It may still be running; re-check with BashStatus.";

pub const INPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "process_id": {
      "type": "string",
      "description": "The background command id returned by Bash (e.g. \"bash_1\")."
    }
  },
  "required": ["process_id"],
  "additionalProperties": false
}"#;
