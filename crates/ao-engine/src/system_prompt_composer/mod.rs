/// Canonical system-prompt composer.
///
/// Produces a deterministic, section-ordered system prompt from pure data inputs.
/// No file I/O is performed inside this module — callers pre-load all context.
///
/// Section order:
///   1.  <run-context> XML block (cwd, os, model, date)
///   2.  Agent identity (name + description)
///   3.  User addressing (preferred_name / full_name)
///   4.  Persona and Special Instructions (each omitted when None)
///   5.  Static baseline collaboration guidance
///   5b. CLI tool preference (CLI runner mode only — omitted for native/API runs)
///   6.  Delegate targets (omitted when empty — adjacent to BASELINE_GUIDANCE for tool-selection context)
///   7.  Agent home context (CLAUDE.md, rules, skills — each omitted when empty)
///   8.  Workspace context (CLAUDE.md, rules — each omitted when empty)
///   9.  Memory save/recall guidance (static)
///   10. Workflows (id + name only; omitted when empty)
///   11. Memories — agent, project, global (each section omitted when empty)
///   12. Thread notes — current-thread ephemeral memory, appended by the caller
///       via build_thread_notes_section() (not part of compose_system_prompt()
///       itself — see that function's doc for why)
///   CLI appendix: tool catalog, appended via with_tool_catalog()

pub mod loader;
pub mod migrator;
pub mod refine_context;

#[cfg(test)]
mod tests;

use ao_protocol::agent::{AgentProfile, AgentRunnerMode, DelegateTarget};
use ao_protocol::memory::MemoryEntry;
use ao_protocol::preferences::UserPreferences;
use ao_protocol::system_prompt_context::{AgentHomeContext, WorkspaceContext};
use ao_protocol::workflow::WorkflowSummary;

use crate::memory_instructions::MEMORY_SAVE_INSTRUCTION;

/// Tool-preference guidance appended to CLI-mode agents only.
///
/// When an agent's runner_mode is Cli, the harness binary ships its own bundled equivalents
/// alongside our mcp__launchpad__* tools. This section teaches the model which to prefer.
/// Native/API runs define their own tool set and must never receive this section.
const CLI_TOOL_PREFERENCE: &str = r#"# Prefer Launchpad Tools

When both a CLI harness's bundled tool and an equivalent `mcp__launchpad__*` tool are available, always prefer the launchpad tool — it is the system of record: persisted, visible in the Launchpad UI, and surviving across sessions. Bundled equivalents are ephemeral and invisible to the rest of the system.

Key substitutions:
- **Cross-agent delegation** → `mcp__launchpad__Delegate` routes to agents in the user's address book with proper transcript isolation, monitoring, and automatic completion notifications back to you. Never autonomously spawn the CLI harness's bundled subagent-spawning tool — see the explicit-user-request exception in the Tool Selection section above.
- **Dispatched tasklists** → `mcp__launchpad__TodoCreate` (and `TodoAdd`, `TodoUpdate`, etc.) instead of the bundled todo/task list tools.
- **Memory** → `mcp__launchpad__MemoryWrite`, `mcp__launchpad__MemoryEdit`, `mcp__launchpad__MemoryDelete`, `mcp__launchpad__MemoryList` instead of any bundled memory tools.
- **Scheduled/proactive work** → `mcp__launchpad__AssignmentCreate` (plus `AssignmentList`, `AssignmentUpdate`, `AssignmentDelete`, `AssignmentTrigger`) instead of the bundled scheduling tools.

When the Tool Selection section above refers to 'your environment's subagent-spawning tool', that is the CLI harness's bundled subagent tool. Use `mcp__launchpad__Delegate` whenever a Delegate Target matches. Never fall back to the bundled subagent tool when no Delegate Target applies — do the work directly instead."#;

/// Static baseline collaboration and tool-selection guidance appended to every agent.
///
/// This is Section 5 of the canonical structure — a stable, cache-friendly suffix
/// that is identical across all agents regardless of persona or project context.
const BASELINE_GUIDANCE: &str = r#"# Tool Selection: Direct Tools vs. Sub-Agents

**Route by shape: single known edit → direct tools; multi-step work → TodoCreate; one self-contained subtask matching a target's use case → Delegate; nothing matches → do it yourself.**

Prefer direct tool calls over launching sub-agents. Sub-agents are expensive — they spin up a separate process, consume extra tokens, and add latency.

**Use direct tools (Read, Grep, Glob, Edit, Write, Bash) when:**
- You know the file path or can find it in 1-2 searches
- You're reading, editing, or creating a specific file
- You're running a known command
- The task requires fewer than 5 tool calls
- You're searching for a specific class, function, or string

**Use TodoCreate for multi-step work (2+ chunks that genuinely benefit from decomposition)** — see "TodoCreate vs. Delegate" below for when to default to it and when Delegate Targets make that default stronger.

**Use Delegate when a single self-contained subtask matches a target's use case:**
- Check the Delegate Targets list; if any target's use case aligns with the current task, use Delegate
- Override conditions (use direct tools instead): the task is small enough to finish with 1-3 direct tool calls; the task needs parent-only context that cannot be forked; no target's stated use case matches the task

**Never autonomously spawn your environment's bundled subagent-spawning tool (e.g. Explore, general-purpose, Plan, or any other built-in catalog agent type) — not for broad exploration, parallel research, or self-contained subtasks.** Catalog agents have no independent model configuration: they silently inherit the parent's own provider and model, with no cost control and no visibility to the rest of the system. Only agents the user has explicitly configured in their Delegate Targets address book are approved for spawning.

If no Delegate target's use case matches the current task:
- Do the work directly, in the parent, with direct tools — even if it takes several rounds of searching. Stay bounded: work through it methodically and summarize as you go rather than looping indefinitely.
- If a dedicated delegate would genuinely help (e.g. a cheaper model for research-heavy digging), say so and let the user add it to their address book — do not substitute the bundled tool yourself.

**Exception:** if the user explicitly names a built-in catalog agent type in the current turn (e.g. "use the Explore agent to look through this"), honor that instruction — it is a deliberate, cost-aware user action, not the model's own delegation choice. Do not extend this exception to instructions you merely infer.

When in doubt: for a single self-contained task, try the direct tool first, then Delegate if a target matches. For multi-step work, default to TodoCreate rather than several ad-hoc Delegate calls in a row.

# TodoCreate vs. Delegate

**Multiple chunks → use TodoCreate** (2+ items; never create a 1-item tasklist). Default multi-step work to TodoCreate rather than chaining several ad-hoc Delegate calls, because it:
- Keeps your own context focused on the goal — executors absorb the heavy lifting; you review, fix, or validate once they report back.
- Runs non-blocking — you stay available to the user for other requests while dispatched items execute, instead of sitting inside one blocking call.
- Gives the user visibility and control — they can watch each step and add, edit, or stop items mid-flight, which a single Delegate call doesn't expose.

**When Delegate Targets are configured, default multi-step work to TodoCreate** — the populated address book is the user's explicit signal to route chunks to those specialized agents rather than handle them inline. Still drop to direct tools or a single Delegate call when the work is genuinely one or two trivial steps: don't wrap two trivial direct-tool steps (e.g. "read file, edit file") in a tasklist just because it technically has two steps — TodoCreate is for work that benefits from real decomposition or specialized routing.

Decompose fully before dispatching — executors start with no parent context. Each `brief` must be self-contained: include all file paths, constraints, and acceptance criteria. A tasklist works because each executor stays focused on one chunk — a single-item list or vague briefs defeat the purpose.

**Single subtask → use Delegate.** One focused brief, one agent run, result returns inline. Reach for Delegate only when the work is genuinely one chunk; if it decomposes into several genuinely separable steps, switch to TodoCreate instead.

# Plan Mode
After exiting plan mode, ALWAYS tell the user the plan file path and show a summary of the plan. The auto-generated plan filenames are not memorable — surface them explicitly.

# Asking the User Questions
When you need a decision or input from the user to proceed, use `AskUserQuestionWithForm` rather than only asking in chat — this matters most when you have more than one question: batch them into a single form call (one field per question) instead of asking one at a time across turns. Skip it for rhetorical or mid-explanation asides that don't need a captured answer. If no operator is present, the tool returns an error you should note and proceed past."#;

/// Compose the canonical system prompt from pure data inputs.
///
/// The function is synchronous and performs no I/O. All inputs must be pre-loaded
/// by the caller before invoking this function.
///
/// `project_key` is the resolved canonical project key for the session's effective
/// working directory (parent_current_cwd if delegated, otherwise current_cwd).
/// Computed once at session-record time by the caller and passed here for
/// cache-stable inclusion in `<run-context>`. Pass `None` when unavailable
/// (resolution failed or no session context); the element is silently omitted.
pub fn compose_system_prompt(
    profile: &AgentProfile,
    user_prefs: &UserPreferences,
    workspace_ctx: &WorkspaceContext,
    agent_home_ctx: &AgentHomeContext,
    agent_memories: &[MemoryEntry],
    project_memories: &[MemoryEntry],
    global_memories: &[MemoryEntry],
    workflows: &[WorkflowSummary],
    delegate_targets: &[DelegateTarget],
    date_str: &str,
    project_key: Option<&str>,
) -> String {
    let mut blocks: Vec<String> = Vec::new();

    // Section 1: run-context XML block
    blocks.push(build_run_context(workspace_ctx, profile, date_str, project_key));

    // Section 2: agent identity
    blocks.push(build_agent_identity(profile));

    // Section 3: user addressing (omitted when both name fields are None)
    if let Some(block) = build_user_addressing(user_prefs) {
        blocks.push(block);
    }

    // Section 4: persona and special instructions (each sub-block omitted when None)
    if let Some(block) = build_persona_section(profile) {
        blocks.push(block);
    }

    // Section 5: static baseline collaboration guidance
    blocks.push(BASELINE_GUIDANCE.to_string());

    // Section 5b: CLI tool preference — only for CLI runner mode.
    // Native/API runs define their own tool set and must not see this section.
    if profile.runner_mode == AgentRunnerMode::Cli {
        blocks.push(CLI_TOOL_PREFERENCE.to_string());
    }

    // Section 6: delegate targets (omitted when empty — placed adjacent to BASELINE_GUIDANCE
    // so the per-target use case is visible at the point of tool-selection decisions)
    if let Some(block) = build_delegate_targets_section(delegate_targets) {
        blocks.push(block);
    }

    // Section 7: agent home context (CLAUDE.md, rules, skills)
    if let Some(block) = build_agent_home_section(agent_home_ctx) {
        blocks.push(block);
    }

    // Section 8: workspace context (CLAUDE.md, rules)
    if let Some(block) = build_workspace_section(workspace_ctx) {
        blocks.push(block);
    }

    // Section 9: memory save guidance (static)
    blocks.push(MEMORY_SAVE_INSTRUCTION.trim().to_string());

    // Section 10: workflows (id + name only; omitted when empty)
    if let Some(block) = build_workflows_section(workflows) {
        blocks.push(block);
    }

    // Section 11: memories (omitted when all three scopes are empty)
    if let Some(block) = build_memories_section(agent_memories, project_memories, global_memories) {
        blocks.push(block);
    }

    blocks.join("\n\n")
}

/// Append the CLI tool catalog to a canonical prompt body.
///
/// The catalog is separated from the canonical body by a `\n\n# Tool calls\n\n`
/// divider, matching the existing CLI convention. The canonical body is
/// unchanged; only CLI runners need to call this.
pub fn with_tool_catalog(prompt: String, catalog_xml: &str) -> String {
    format!("{}\n\n# Tool calls\n\n{}", prompt, catalog_xml)
}

fn build_run_context(
    workspace_ctx: &WorkspaceContext,
    profile: &AgentProfile,
    date_str: &str,
    project_key: Option<&str>,
) -> String {
    let os = std::env::consts::OS;

    let mut lines = vec![
        "<run-context>".to_string(),
        format!("  <cwd>{}</cwd>", workspace_ctx.root_path),
        format!("  <os>{}</os>", os),
    ];
    if let Some(ref model) = profile.model {
        lines.push(format!("  <model>{}</model>", model));
    }
    lines.push(format!("  <date>{}</date>", date_str));
    if let Some(key) = project_key {
        lines.push(format!("  <project-key>{}</project-key>", key));
    }
    lines.push("</run-context>".to_string());
    lines.join("\n")
}

fn build_agent_identity(profile: &AgentProfile) -> String {
    if profile.description.is_empty() {
        format!("# {}", profile.name)
    } else {
        format!("# {}\n\n{}", profile.name, profile.description)
    }
}

fn build_user_addressing(user_prefs: &UserPreferences) -> Option<String> {
    match (&user_prefs.preferred_name, &user_prefs.full_name) {
        (None, None) => None,
        (Some(preferred), Some(full)) => Some(format!(
            "You are assisting {} ({}).",
            preferred, full
        )),
        (Some(preferred), None) => Some(format!("You are assisting {}.", preferred)),
        (None, Some(full)) => Some(format!("You are assisting {}.", full)),
    }
}

fn build_persona_section(profile: &AgentProfile) -> Option<String> {
    let (persona, special_instructions) = resolve_persona_fields(profile);

    let mut parts: Vec<String> = Vec::new();

    if let Some(persona) = persona {
        parts.push(format!("## Persona\n\n{}", persona));
    }
    if let Some(instructions) = special_instructions {
        parts.push(format!("## Special Instructions\n\n{}", instructions));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Resolve the effective persona / special-instructions content for a profile.
///
/// Migrated profiles carry these as first-class fields and are returned as-is.
///
/// A profile that still holds only the legacy monolithic `system_prompt` — the
/// split into persona/special_instructions is an explicit, user-confirmed step
/// that may not have run yet — is migrated on the fly so its authored guidance
/// still reaches the model instead of being silently dropped. The fallback
/// runs the same migrator the explicit flow uses (rather than copying the raw
/// prompt verbatim), so it strips the boilerplate that pre-composer prompts
/// embedded — boilerplate this composer re-emits from its own sections — and
/// the runtime result matches what an explicit migration would persist.
///
/// The fallback only fires when BOTH fields are absent: any extracted content
/// means the profile is already (at least partly) migrated, and re-deriving
/// from the legacy field would double-render it.
fn resolve_persona_fields(profile: &AgentProfile) -> (Option<String>, Option<String>) {
    if profile.persona.is_some() || profile.special_instructions.is_some() {
        return (
            profile.persona.clone(),
            profile.special_instructions.clone(),
        );
    }

    match profile.system_prompt.as_deref() {
        Some(raw) if !raw.trim().is_empty() => {
            let result = migrator::migrate_legacy_system_prompt(raw);
            (result.persona, result.special_instructions)
        }
        _ => (None, None),
    }
}

/// Section 6: agent home context — CLAUDE.md, rules, skills.
/// Each sub-block is omitted when the corresponding field is None/empty.
fn build_agent_home_section(ctx: &AgentHomeContext) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(ref content) = ctx.claude_md_content {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            parts.push(format!("# Agent Instructions\n\n{}", trimmed));
        }
    }

    if !ctx.rules.is_empty() {
        let mut block = String::from("# Agent Rules");
        for rule in &ctx.rules {
            let trimmed = rule.trim();
            if !trimmed.is_empty() {
                block.push_str(&format!("\n\n{}", trimmed));
            }
        }
        parts.push(block);
    }

    // Skills: the runner-supplied, registry-derived listing block is the
    // authoritative source when present — it is built from the same pools
    // `RunSkill` dispatches against (user pool + enabled plugins + MCP overlay),
    // so the model is told about exactly the skills it can actually invoke. The
    // legacy per-agent `skills` contents are only used as a fallback when the
    // runner did not supply a block.
    if let Some(ref block) = ctx.skills_block {
        let trimmed = block.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    } else if !ctx.skills.is_empty() {
        let mut block = String::from("# Studio Skills");
        for skill in &ctx.skills {
            let trimmed = skill.trim();
            if !trimmed.is_empty() {
                block.push_str(&format!("\n\n{}", trimmed));
            }
        }
        parts.push(block);
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Section 9: workflows — id and name only; omitted when empty.
fn build_workflows_section(workflows: &[WorkflowSummary]) -> Option<String> {
    if workflows.is_empty() {
        return None;
    }
    let mut lines = vec![
        "# Workflows".to_string(),
        String::new(),
        "The following workflows are available for this session:".to_string(),
        String::new(),
    ];
    for wf in workflows {
        lines.push(format!("- **{}**: {}", wf.id, wf.name));
    }
    Some(lines.join("\n"))
}

/// Section 6: delegate targets — omitted when empty.
fn build_delegate_targets_section(targets: &[DelegateTarget]) -> Option<String> {
    if targets.is_empty() {
        return None;
    }
    let mut lines = vec![
        "# Delegate Targets".to_string(),
        String::new(),
        "For a single self-contained task, when a target's stated use case below matches, Delegate is the preferred path over Agent. For multi-step work, prefer TodoCreate — see \"TodoCreate vs. Delegate\" above."
            .to_string(),
        String::new(),
    ];
    for t in targets {
        let fork = if t.share_context_allowed { " (fork allowed)" } else { "" };
        lines.push(format!("- **{}** — *Use for: {}*{}", t.name, t.purpose, fork));
    }
    Some(lines.join("\n"))
}

/// Section 11: agent memories — omitted entirely when empty.
const SCOPE_PROMPT_BUDGET_CHARS: usize = 15_000;

fn build_memories_section(
    agent_memories: &[MemoryEntry],
    project_memories: &[MemoryEntry],
    global_memories: &[MemoryEntry],
) -> Option<String> {
    if agent_memories.is_empty() && project_memories.is_empty() && global_memories.is_empty() {
        return None;
    }

    let mut sections: Vec<String> = Vec::new();

    for (label, entries) in [
        ("[Agent Memories]", agent_memories),
        ("[Project Memories]", project_memories),
        ("[Global Memories]", global_memories),
    ] {
        if entries.is_empty() {
            continue;
        }
        sections.push(render_memory_scope(label, entries, SCOPE_PROMPT_BUDGET_CHARS));
    }

    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

/// Render the current thread's ephemeral working-memory entries as a
/// distinct "[Thread Notes]" block, kept separate from `build_memories_section`'s
/// durable Agent/Project/Global sections so the model can tell throwaway
/// per-thread scratch notes (see `MemoryScope::Thread`) from memory that
/// outlives this thread. Returns `None` when there are no entries —
/// including when the caller has no active thread id, since callers pass
/// an empty slice in that case rather than erroring.
///
/// Re-clamps to `THREAD_HARD_CAP` entries and a budget derived from the
/// thread tier's own caps (not the durable `SCOPE_PROMPT_BUDGET_CHARS`)
/// before rendering, on top of whatever the store already enforced at
/// write time — so a future write-path change can't silently balloon what
/// lands in the prompt.
pub fn build_thread_notes_section(thread_memories: &[MemoryEntry]) -> Option<String> {
    use ao_engine_tools_engine::memory::store::{THREAD_ENTRY_CHAR_SOFT, THREAD_HARD_CAP};

    if thread_memories.is_empty() {
        return None;
    }

    let mut sorted: Vec<&MemoryEntry> = thread_memories.iter().collect();
    sorted.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let capped: Vec<MemoryEntry> = sorted.into_iter().take(THREAD_HARD_CAP).cloned().collect();

    Some(render_memory_scope(
        "[Thread Notes]",
        &capped,
        THREAD_HARD_CAP * THREAD_ENTRY_CHAR_SOFT,
    ))
}

fn render_memory_scope(label: &str, entries: &[MemoryEntry], budget_chars: usize) -> String {
    // Sort by updated_at ascending; oldest entries are truncated first when over budget.
    let mut sorted: Vec<&MemoryEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));

    // Walk newest-to-oldest, include entries until budget is exhausted.
    let mut included: Vec<&str> = Vec::new();
    let mut chars_used: usize = 0;
    let mut truncated: usize = 0;

    for entry in sorted.iter().rev() {
        let cost = entry.content.chars().count() + 3; // "- " prefix + "\n"
        if chars_used + cost <= budget_chars {
            included.push(&entry.content);
            chars_used += cost;
        } else {
            truncated += 1;
        }
    }

    // Reverse to display oldest-of-included first (chronological order).
    included.reverse();

    let mut block = String::from(label);
    for content in &included {
        block.push_str(&format!("\n- {}", content));
    }
    if truncated > 0 {
        block.push_str(&format!(
            "\n… ({} older {} omitted — use MemoryList to retrieve)",
            truncated,
            if truncated == 1 { "entry" } else { "entries" }
        ));
    }
    block
}

/// Section 7: workspace context — CLAUDE.md, rules.
/// Each sub-block is omitted when the corresponding field is None/empty.
fn build_workspace_section(ctx: &WorkspaceContext) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(ref content) = ctx.claude_md_content {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            parts.push(format!("# Workspace Instructions\n\n{}", trimmed));
        }
    }

    if !ctx.rules.is_empty() {
        let mut block = String::from("# Workspace Rules");
        for rule in &ctx.rules {
            let trimmed = rule.trim();
            if !trimmed.is_empty() {
                block.push_str(&format!("\n\n{}", trimmed));
            }
        }
        parts.push(block);
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}
