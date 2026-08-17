//! Description text and input schema shown to the model for the Bash tool.

pub const DESCRIPTION: &str = "Runs a shell command in a fresh `/bin/bash -c` subprocess and returns stdout, stderr, \
the exit code, and structured metadata.

Simple read-only inspection commands — `ls`, `cat`, `grep`, `git status`, `git diff`, `git log`, \
and similar — run without a permission prompt. Commands that write files, chain operators \
(`&&`, `;`), redirect output (`>`), use command substitution (`$(...)` or backticks), or \
touch the network or git history will prompt for approval before executing.

Session mutations (variables set, `export` statements, `source venv/bin/activate`, traps) do NOT \
persist between calls — each invocation is independent. Aliases and functions defined in the user's \
shell config files (e.g. `~/.bashrc`) ARE available; they are captured once at startup. Only the \
working directory carries over across calls (see below).

# Working directory

The shell's working directory after each command completes — including any `cd` calls the command \
performed — is captured and carried forward to the next Bash call automatically. Multi-step \
directory changes work correctly: `mkdir x && cd x`, `cd a && cd b`, and conditional cds all \
persist the final directory. No special command structure is required.

Caveat: if a command explicitly calls `exit`, the final working directory will not be captured \
for that invocation and the cwd will be unchanged for the next call.

# Background mode

Pass `run_in_background: true` to spawn a process without blocking — the response returns a \
`process_id` immediately. Use this for dev servers, build daemons, file-system watchers, or any \
command expected to run longer than a few seconds.

- **BashStatus** — poll a background process: returns its lifecycle state (running / exited) and \
  a tail of the accumulated output buffer.
- **BashKill** — send SIGTERM (with SIGKILL fallback) to stop the process when it is no longer needed.

# Sleep and polling loops

Avoid bare `sleep N` (N ≥ 2 s) in foreground mode — it holds the tool slot for the full duration \
with nothing accomplished. Foreground bare sleeps of 2 s or more are blocked with a recoverable error.

For poll-until-done patterns:
1. Spawn the work with `run_in_background: true`.
2. Call **BashStatus** on the returned `process_id` to check state and read output.
3. Call **BashKill** if you need to abort early or the work is complete.

# Large output

Combined stdout + stderr is middle-truncated at 30 KB inline — a `[output truncated: N bytes elided]` \
marker is inserted so you see both the start and end of the output.

When combined output exceeds ~100 KB the full content is written to disk and the result carries a \
`<persisted-output>` envelope with: the file path, byte count, line count, and a head/tail preview. \
Use the **Read** tool with that path to access the complete content.

# Output structure

The result payload includes:
- `stdout` / `stderr` — captured separately; each stderr line is prefixed with `stderr: ` in rendered \
  text output.
- `exit_status` — the shell exit code (0 = success).
- `signal` — signal number if the process was killed at the OS level rather than exiting via a code.
- `timed_out` / `cancelled` — set when the tool forcibly ended the process.
- `is_error` — true only for cancelled or timed-out commands; a non-zero exit is NOT an `is_error`.
- `exit_code_note` — a short human-readable explanation for well-known non-zero codes: \
  127 (command not found), 126 (not executable), 130/137/143 (common signal terminations), \
  and direct signal kills.

# Quoting and escaping

- Single quotes (`'`) protect literal strings — no substitutions occur inside them.
- Double quotes (`\"`) allow `$VAR` and `$(cmd)` substitution.
- Embed a literal single quote inside a single-quoted string using: `'text'\\''more'`.
- For multi-line scripts, prefer a bash heredoc or a temporary script file over complex inline quoting.

# Environment

Variables whose names begin with `AO_`, `LAUNCHPAD_`, or `CLAUDE_` are stripped before the \
subprocess sees the environment. `set -o pipefail` is injected so that a piped command reflects \
the first failure's exit code rather than the last stage's.

Aliases, shell functions, and the PATH defined in the user's rc files (`~/.bashrc`, \
`~/.bash_profile`) are captured once at startup and are available to every command. \
Session mutations — `export FOO=bar`, `alias x=y` run inside a command, \
`source venv/bin/activate` — do NOT carry over to the next call.

# Interactive commands

stdin is connected to `/dev/null` — commands that read from stdin see EOF immediately. \
Editor- and pager-spawning commands are neutralized so they will not hang: `GIT_EDITOR`, \
`EDITOR`, `VISUAL`, and `GIT_SEQUENCE_EDITOR` are set to `true` (a no-op binary that exits \
immediately), and `GIT_PAGER`/`PAGER` are set to `cat`. For example, `git commit` without \
`-m` invokes the no-op editor and produces an empty commit message (which git may reject) — \
always pass `-m` explicitly. git will not block for credentials (`GIT_TERMINAL_PROMPT=0`). \
REPLs and programs that require a live interactive terminal (e.g. `python`, `node`, `psql` \
in interactive mode) are unsupported — pipe input via stdin redirection \
(`echo input | cmd`) or use non-interactive flags instead.

# Parameters

- `command` (required): the shell command to run, exactly as you would type it in a terminal.
- `timeout` (ms, default 120000, max 600000): how long to wait before sending SIGTERM and, after a \
  5-second grace period, SIGKILL. Use 0 to accept the default.
- `description` (5–10 words): a short human-readable label shown in the permission UI.
- `run_in_background` (default false): when true, spawn in the background and return `process_id` \
  immediately. Poll via BashStatus; stop via BashKill.";

pub const INPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "command": {
      "type": "string",
      "description": "The bash command to execute."
    },
    "timeout": {
      "type": "integer",
      "description": "Timeout in milliseconds. Default 120000, max 600000. Use 0 to accept the default."
    },
    "description": {
      "type": "string",
      "description": "Short description of the command (5-10 words) shown in the permission UI."
    },
    "run_in_background": {
      "type": "boolean",
      "default": false,
      "description": "When true, spawn the process in the background and return a process_id immediately."
    }
  },
  "required": ["command"],
  "additionalProperties": false
}"#;
