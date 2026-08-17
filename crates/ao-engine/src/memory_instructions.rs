/// Instruction text appended to agent system prompts describing the Memory tool family.
pub const MEMORY_SAVE_INSTRUCTION: &str = r#"
# Memory Management
You have four tools for persisting information across sessions: MemoryWrite, MemoryEdit, MemoryDelete, and MemoryList.

## Scopes
- **agent** — private to this agent; use for preferences, corrections, and patterns specific to this assistant.
- **project** — shared across all agents in the same repository; use for project context, decisions, and conventions.
- **global** — shared across all agents and all projects; use only for universal preferences that apply everywhere.
- **thread** — private to the current conversation/thread; surfaced only in this thread's own Thread Notes section; applies immediately (no review/staging gate); cleared when the thread is deleted. Use for decisions, constraints, dead-ends, and user directives specific to THIS piece of work that would be noise in the agent/project pools.

## Caps (entries / chars)
- Agent: soft 60, hard 100 | Project: soft 80, hard 150 | Global: soft 25, hard 40 | Thread: soft 15, hard 25 (FIFO auto-evict)
- Per-entry: soft 2 000 chars, hard 8 000 chars — prefer short, focused entries.

## Workflow
Before calling MemoryWrite, call MemoryList to check for contradictory or stale entries and call MemoryDelete (or MemoryEdit) to remove or update them first.
Use MemoryEdit to update an existing entry — it preserves the original ID and timestamp.
Save: user preferences, corrections, project decisions, recurring patterns.
Do NOT save: transient task details, information already visible in the current context.
PROACTIVELY pin thread-local items — decisions, constraints, dead-ends — to thread scope as you work, and whenever the user says things like "remember for this thread" / "for this chat". Thread scope is the right home for working memory of this task.

## AgentAuthor vs. Memory
Memory and AgentAuthor both persist information beyond this turn, but they are not interchangeable, and Memory is the safer default when it's unclear which applies.
- The user explicitly asks you to update/change the system prompt, your persona, or your special instructions → use AgentAuthor (`update` op, your own agent id). Do not write this to Memory instead.
- The user asks you to "remember" something → use Memory, per the scopes and workflow above.
- Otherwise, judge the request itself: a preference, correction, fact, or one-off context is Memory territory, even if it changes what you do. A durable rule about how you should behave going forward — a fundamental behavioral change, not just a preference — is a candidate for AgentAuthor's special_instructions instead. Because an AgentAuthor self-edit takes effect immediately with no confirmation step, prefer to confirm with the user before restructuring your own behavior config this way unless they clearly asked for it.
- When genuinely unclear which bucket a request falls into, state which you chose and why, or ask — don't reflexively pick either tool."#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards against the thread scope silently dropping back out of the
    /// model-facing guidance — the composer's snapshot tests would still
    /// pass on a stale snapshot regen, so this asserts directly on the
    /// source constant.
    #[test]
    fn instruction_enumerates_thread_scope() {
        assert!(
            MEMORY_SAVE_INSTRUCTION.contains("**thread**"),
            "Scopes list must enumerate 'thread' alongside agent/project/global"
        );
        assert!(
            MEMORY_SAVE_INSTRUCTION.contains("PROACTIVELY pin thread-local items"),
            "must nudge the model to proactively use thread scope, not just document it"
        );
        // The literal "[Thread Notes]" marker is reserved for
        // `build_thread_notes_section`'s per-run heading (see
        // `system_prompt_composer::build_thread_notes_section`); tests there
        // assert it is *absent* from the composed prompt when a run has no
        // active thread. This block must describe that section without
        // repeating its bracketed marker, or those tests break whenever this
        // always-present block is spliced in ahead of it.
        assert!(
            !MEMORY_SAVE_INSTRUCTION.contains("[Thread Notes]"),
            "must not repeat the literal [Thread Notes] marker reserved for build_thread_notes_section's heading"
        );
    }
}
