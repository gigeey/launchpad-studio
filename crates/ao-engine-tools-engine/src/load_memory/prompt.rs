use serde_json::{json, Value};

/// How to use LoadMemory and when.
pub const DESCRIPTION: &str = "\
Inject another repo's project memory into your context, without changing the repo you are \
currently working in.

**Why this exists, and how it differs from MemoryList:** MemoryList/MemoryWrite/MemoryEdit/ \
MemoryDelete already accept a `working_dir` override so you can reach a sibling repo's \
project scope, but MemoryList returns a paginated list of 200-char previews meant for \
browsing — you'd have to page through it and re-fetch full content yourself. LoadMemory \
is the ergonomic wrapper for the common case: you need repo B's accumulated project \
memory (its conventions, known gotchas, prior decisions) while you are answering a \
question about repo B, and you want it injected as usable content in one call, not paged.

**When to reach for this:** you started in one repo but the user asks about, or you \
need to act in, a *different* repo you have local access to (e.g. a sibling checkout, \
a second worktree). Pass that repo's path as `repo` — you do not need to `cd` or use \
EnterWorktree first.

**Arguments:**
- `repo` (required) — path to the target repo. Absolute, `~`-prefixed, or relative to \
your current working directory. Any path inside the repo works; the project scope is \
keyed off the repo's git toplevel (or the canonicalized path itself if it isn't a git repo).
- `task` (optional) — a short phrase describing what you're about to do. This is only \
consulted when the target scope is too large to inject in full: entries are ranked by \
keyword overlap with this text so you get the most relevant slice instead of an \
arbitrary one. Omit it to fall back to the most recently updated entries.
- `budget_chars` (optional, default 4000, min 500, max 20000) — soft cap on total \
injected content. Scopes at or under this size are always returned in full — the cap \
only starts trimming once the scope has grown past what's reasonable to inject wholesale.

**Return shape:** `entries` (full `id`/`content`/`created_at`/`updated_at` per memory, \
not previews), `entry_count` (how many live entries exist in the scope), \
`returned_count` / `chars_returned` (what actually came back), `truncated` (true if \
`entry_count` exceeds what was returned), and `filtered_by_task` (true if `task` \
keywords were used to rank the selection rather than plain recency).

**Read-only.** LoadMemory never writes, edits, or deletes anything — use MemoryWrite \
with an explicit `working_dir` if you want to record something back into that repo's \
project scope.

If the `repo` path does not exist on disk, a recoverable error is returned.";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "repo": {
                "type": "string",
                "description": "Path to the target repo whose project memory you want to load. Absolute, '~'-prefixed, or relative to your current working directory. Does not have to be the repo the session launched in."
            },
            "task": {
                "type": "string",
                "description": "Optional. A short phrase describing what you're about to do in that repo. Only used to rank entries when the scope is too large to return in full; omit it to fall back to most-recently-updated ordering."
            },
            "budget_chars": {
                "type": "number",
                "description": "Optional. Soft cap on total injected content, in characters (default 4000, min 500, max 20000). Scopes under this size are always returned in full.",
                "default": 4000,
                "minimum": 500,
                "maximum": 20000
            }
        },
        "required": ["repo"],
        "additionalProperties": false
    })
}
