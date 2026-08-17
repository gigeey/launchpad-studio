use serde_json::{json, Value};

/// How to use TodoCreate and when.
pub const DESCRIPTION: &str = "\
Creates a persistent, dispatched tasklist for this agent. \
Not to be confused with TodoWrite, which is an ephemeral in-memory scratchpad.

**When to use:** work genuinely splits into MULTIPLE focused chunks — minimum 2 items. \
Never create a 1-item tasklist. If there is only one subtask, use Delegate instead.

Each item becomes a task dispatched to this agent (or a delegate when `owner` is set). \
The executing agent receives only the item's `brief` — no parent context is passed through. \
This means YOU (the planner) must decompose completely before dispatching: each `brief` \
must be fully self-contained with all file paths, constraints, and acceptance criteria the \
executor needs. The tasklist's value is keeping each executor's context focused on ONE chunk \
while you hold the big picture. Single-item lists and vague briefs defeat the purpose.

**Dispatch modes (`dispatch_mode`):**
- `sync` (default): the tool call blocks until the tasklist reaches a terminal \
  state and returns an aggregated result with per-task summaries. Use this when \
  you want to reason about the completed work as a single tool call.
- `async`: the tool returns immediately once the tasklist is created and the \
  dispatcher starts. Use this when you want to continue other work — or start \
  planning the next phase — while the tasklist runs in the background.

**Async monitoring:** when an async tasklist reaches a terminal state, a \
completion message summarizing every item is automatically queued to you and \
you are re-invoked with it — you do NOT need to poll TodoList to find out when \
it finished. (Sync mode blocks and returns the aggregated result inline, so \
polling never applies there either.)

**Guards:**
- The agent must have `max_instances >= 2` (one slot for the tasklist dispatcher, \
  one for the user chat thread).
- Only one active tasklist per agent is allowed at a time. To run work in \
  phases, create and start them one at a time: when the active tasklist \
  completes you are re-invoked with your transcript intact, so author the next \
  phase's tasklist then — you do not lose the reasoning or research behind the \
  earlier phase across that handoff.
- Cannot be called from inside a subagent context (no persistent message queue).

After creation, use TodoList to inspect the list, TodoAdd to append items, \
TodoComplete to mark items done, and TodoUpdate to change fields.";

/// Attached to the sync-dispatch result so the model knows the payload is the
/// concluded outcome of the tasklist and what to do with it. The per-task
/// `summary`/`details` fields carry each item's result; this frames the
/// follow-up. Kept short so it doesn't crowd the structured data.
pub const SYNC_COMPLETION_GUIDANCE: &str = "\
This is the final result of the tasklist you launched. Each task carries the \
executing agent's own `summary` (and optional `details`) of what it concluded. \
Synthesize those summaries into a single coherent result for the user rather \
than relaying them verbatim. For any task with status `failed`, read its \
`output_path` to learn why, then retry it (TodoAdd) or surface the blocker — \
never silently drop a failure. Read a task's `output_path` only when its \
summary isn't enough to act on.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Short display name for the tasklist (e.g. 'Q2 refactor')."
            },
            "items": {
                "type": "array",
                "minItems": 1,
                "description": "Ordered list of work items. Each item is dispatched as a separate task run.",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "One-line task title shown in the UI."
                        },
                        "brief": {
                            "type": "string",
                            "description": "Detailed prompt sent to the executing agent for this item."
                        },
                        "owner": {
                            "type": "string",
                            "description": "Optional: agent_id of the delegate to assign. Defaults to the coordinator agent."
                        }
                    },
                    "required": ["title", "brief"],
                    "additionalProperties": false
                }
            },
            "mode": {
                "type": "string",
                "enum": ["seq", "par"],
                "description": "Execution mode: 'seq' runs items one-by-one (default); 'par' runs all concurrently."
            },
            "dispatch_mode": {
                "type": "string",
                "enum": ["sync", "async"],
                "default": "sync",
                "description": "Dispatch mode: 'sync' (default) blocks until the tasklist completes and returns aggregated results; 'async' returns immediately after creation."
            }
        },
        "required": ["name", "items"],
        "additionalProperties": false
    })
}
