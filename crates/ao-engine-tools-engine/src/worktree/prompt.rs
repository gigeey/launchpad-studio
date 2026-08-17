use serde_json::{json, Value};

pub const ENTER_DESCRIPTION: &str = "\
Create a new git worktree and switch the session into it.

When to use: only when the user explicitly requests working in an isolated \
worktree — for example \"start a worktree for this task\" or \"work in a \
fresh branch without touching the main tree\".

When NOT to use: for ordinary branch switching, routine bug fixes, or any \
case where the user has not mentioned worktrees.

Requirements:
- The current directory must be inside a git repository.
- Only one worktree level is allowed per session; call ExitWorktree before \
entering another.

Behavior: resolves the canonical git root, creates a new branch named \
worktree/<slug> from the current HEAD, checks it out at \
<git_root>/.launchpad_studio/worktrees/<slug>, then switches the session \
working directory into that path.

Parameters:
- name (optional): a short label that becomes part of the branch and directory \
name. Omit to auto-generate a unique slug.";

pub const EXIT_DESCRIPTION: &str = "\
Exit the current worktree session and restore the prior working directory.

Scope: applies exclusively to a worktree that this session entered via \
EnterWorktree; if none is active it fails with an explicit error rather than \
quietly doing nothing.

When to use: when the user asks to exit, leave, or go back from the current \
worktree, or when a task inside the worktree is finished.

Parameters:
- action (required): one of 'keep' or 'remove'.
  - keep: leave the worktree branch and directory on disk; only restores the \
working directory.
  - remove: delete the worktree branch and directory after confirming that there \
are no uncommitted changes and no unmerged commits ahead of the base. Refused \
with a descriptive error if the worktree is not clean.";

pub fn enter_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Optional label for the worktree. Used as the branch and directory name suffix (slugified). Auto-generated when omitted."
            }
        },
        "additionalProperties": false
    })
}

pub fn exit_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["keep", "remove"],
                "description": "'keep' preserves the worktree branch and directory; 'remove' deletes both after verifying the tree is clean."
            }
        },
        "required": ["action"],
        "additionalProperties": false
    })
}
