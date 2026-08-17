/// Shared scope guidance appended to each Memory tool description.
pub const SCOPE_GUIDANCE: &str = "Scope values: 'agent' (private to this agent across sessions), 'project' (shared across agents in the same git repo), 'global' (shared across all agents and repos), 'thread' (ephemeral working memory scoped to the current thread only, gone once the thread ends). If you were spawned by another agent, prefer project scope for facts about the work (visible to parent and siblings working in the same repo) and agent scope for personal preferences.";

pub const EDIT_DESCRIPTION: &str = "\
Update the content of an existing memory entry by ID. Prefer this over MemoryDelete + MemoryWrite \
when revising existing information — it preserves the original entry ID and created_at timestamp.

Use MemoryList to find the ID of the entry you want to update. Returns an error if the ID is \
not found in the specified scope.

Scope values:
- 'agent'   — private to this agent, visible across all its sessions
- 'project' — shared across all agents working in the same git repository
- 'global'  — shared across all agents and repositories
- 'thread'  — ephemeral working memory scoped to the current thread only; gone once the thread ends

If you were spawned by another agent, prefer project scope for facts about the work \
(visible to parent and siblings working in the same repo) and agent scope for personal preferences.

For scope='project', pass working_dir if the entry was written from a different directory than the \
runner's launch cwd — e.g. a sibling repo you navigated into. Without this override, the project \
key resolves against the runner cwd and may miss the entry. Tilde ('~') expansion and relative \
paths are supported.

Returns { id, scope } on success.";

pub const DELETE_DESCRIPTION: &str = "\
Remove a memory entry by ID. Use MemoryList to find the ID of the entry you want to delete. \
Prefer MemoryEdit over MemoryDelete + MemoryWrite when you only need to update content.

Use this tool to free space before writing new memories when a scope is near its cap, or to \
remove stale and contradicted information.

Scope values:
- 'agent'   — private to this agent, visible across all its sessions
- 'project' — shared across all agents working in the same git repository
- 'global'  — shared across all agents and repositories
- 'thread'  — ephemeral working memory scoped to the current thread only; gone once the thread ends

If you were spawned by another agent, prefer project scope for facts about the work \
(visible to parent and siblings working in the same repo) and agent scope for personal preferences.

For scope='project', pass working_dir if the entry was written from a different directory than the \
runner's launch cwd — e.g. a sibling repo you navigated into. Without this override, the project \
key resolves against the runner cwd and may miss the entry. Tilde ('~') expansion and relative \
paths are supported.

Returns { id, scope, deleted: true } on success, or an error if the entry is not found.";

pub const LIST_DESCRIPTION: &str = "\
List memory entries in a scope. Returns up to 100 entries per call, sorted by most-recently \
updated first. Use the offset parameter to page through large scopes.

Each entry includes: id (use with MemoryEdit/MemoryDelete), content_preview (first 200 chars), \
created_at, and updated_at. The result also includes scope_summary with count, soft_cap, and \
hard_cap so you can gauge how full the scope is.

Scope values:
- 'agent'   — private to this agent, visible across all its sessions
- 'project' — shared across all agents working in the same git repository
- 'global'  — shared across all agents and repositories
- 'thread'  — ephemeral working memory scoped to the current thread only; gone once the thread ends

If you were spawned by another agent, prefer project scope for facts about the work \
(visible to parent and siblings working in the same repo) and agent scope for personal preferences.

For scope='project', pass working_dir to pin the project key to a specific directory — e.g. a \
sibling repo you've navigated into via Bash cd. Without this override, the project key resolves \
against the runner cwd. Tilde ('~') expansion and relative paths are supported.";

pub const WRITE_DESCRIPTION: &str = "\
Save a piece of information to persistent memory so it is available in future sessions.

Content that closely restates or contradicts an existing entry is caught automatically — you \
do not need to MemoryList + MemoryDelete the old one yourself. If the match was written by this \
agent, the old entry is marked superseded and the new one is saved. If the match was authored by \
the user (or its authorship can't be verified), the write is not applied — it's staged for human \
review instead, since an agent must never silently override a user's own correction. A staged \
response looks like { staged: true, applied: false, contradicts: <id> }; if you hit this and are \
confident the user wants the change, ask them to confirm, or use MemoryEdit on that entry directly. \
Prefer short, focused entries over long prose — entries over 2000 chars trigger a warning; over \
8000 chars are rejected.

Scope values:
- 'agent'   — private to this agent, visible across all its sessions
- 'project' — shared across all agents working in the same git repository
- 'global'  — shared across all agents and repositories
- 'thread'  — ephemeral working memory scoped to the current thread only; gone once the thread ends

If you were spawned by another agent, prefer project scope for facts about the work \
(visible to parent and siblings working in the same repo) and agent scope for personal preferences.

For scope='project', pass working_dir to pin the project key to a specific repository — e.g. when \
you've navigated into a different repo than where the runner launched. The project key is derived \
from the git toplevel of working_dir (falling back to the canonicalized path if no .git is found). \
Tilde ('~') expansion and relative paths are supported.

Returns { id, scope, deduplicated, warning?, superseded? }. If the exact content already exists \
in the scope, returns deduplicated: true without writing a duplicate. If this write superseded an \
older agent-authored entry, superseded holds that entry's id.";
