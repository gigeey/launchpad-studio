use ao_engine_tools_core::background_agents::SubagentRegistry;
use serde_json::{json, Value};

/// Static fallback description used when the tool is registered without a
/// spawner (the `Delegate::new()` stub). The state-wired instance replaces
/// this with [`build_description`], which additionally covers
/// `share_context`. Kept keyword-rich so ToolSearch can surface the tool
/// even before the dynamic description is built.
pub const DESCRIPTION: &str = "\
Delegate a self-contained task to a subagent and get its result back.

`target` names an agent from this agent's address book — a user-configured \
delegate that may carry its own tools, skills, and instructions tuned for a \
particular job.

Omit `target` to spawn a fresh instance of yourself instead — the child runs \
with your own profile (same provider, model, runner mode, skills, workflows, \
and prompt). This is the natural way to fan out your own work in parallel: \
issue several Delegate calls with no `target` to have multiple copies of \
yourself work on independent pieces at once. If your own profile can't be \
resolved, there is no agent to clone and the call returns a recoverable \
error — retry with an explicit `target` naming an address-book agent.

`mode` is `sync` (block until the child finishes and return its final output, \
with a `[stats: duration=Xms, turns=N]` line appended when timing is available) \
or `async` (launch in the background and return a `delegation_id` immediately so \
you can continue other work). Defaults to `sync`.

**Sync is fine even for long-running tasks** — the transport keeps the connection \
alive with periodic keepalives, so you'll see an in-flight chip the whole time. \
Use async when you want to continue other work in parallel, not merely because \
the task takes a long time.

**Async monitoring:** when an async delegate finishes, a completion message is \
automatically queued to you and you will be re-invoked with it — you do NOT need \
to poll. Alternatively, call DelegateOutput with `wait_seconds` for a bounded \
blocking wait. Results survive server restarts via the persisted sidechain \
transcript.

Use direct tools instead when the task is small enough to finish inline.";

/// Build the model-facing description for the state-wired Delegate instance.
///
/// `registry` is accepted for signature stability but currently unused: no
/// built-in catalog ships with the engine, so there is nothing in the
/// [`SubagentRegistry`] to enumerate for the model today (entries, when a
/// feature registers any, are consulted at resolution time in
/// `delegate/mod.rs`, not surfaced here). Address-book targets are not
/// listed here either — they are per-agent and discovered at resolution
/// time — but the description tells the model they exist and how to use
/// one.
pub fn build_description(_registry: &SubagentRegistry) -> String {
    let mut lines = String::new();
    lines.push_str(
        "Delegate a self-contained task to a subagent and get its result back.\n\n",
    );
    lines.push_str(
        "`target` names an agent from this agent's address book — a \
         user-configured delegate that may carry its own tools, skills, and \
         instructions tuned for a particular job.\n\n",
    );
    lines.push_str(
        "Omit `target` to spawn a fresh instance of yourself instead — the \
         child runs with your own profile (same provider, model, runner \
         mode, skills, workflows, and composed prompt). This is the natural \
         way to fan out your own work in parallel: issue several Delegate \
         calls with no `target` to have multiple copies of yourself work on \
         independent pieces at once. If your own profile can't be resolved, \
         there is no agent to clone and the call returns a recoverable \
         error — retry with an explicit `target` naming an address-book \
         agent.\n\n",
    );
    lines.push_str(
        "`mode` is `sync` (block until the child finishes and return its final \
         output, with a `[stats: duration=Xms, turns=N]` line appended when \
         timing is available) or `async` (launch in the background and return \
         a `delegation_id` immediately so you can continue other work while the \
         delegate runs). Defaults to `sync`.\n\n",
    );
    lines.push_str(
        "**Sync is acceptable even for long-running tasks.** When the parent is \
         reached over an HTTP-based transport, the server streams keepalive events \
         while the child runs, so the connection stays open and the parent shows \
         an in-flight chip the whole time. Choose async when you genuinely want \
         to continue other work in parallel while the delegate runs, not merely \
         because the task takes a long time.\n\n\
         **Async monitoring:** when an async delegate finishes, a completion \
         message is automatically queued to you and you will be re-invoked with \
         it — you do NOT need to poll. Alternatively, call DelegateOutput with \
         `wait_seconds` for a bounded blocking wait. Results survive server \
         restarts via the persisted sidechain transcript (`transcript_path` is \
         returned alongside `delegation_id` for reference).\n\n",
    );
    lines.push_str(
        "Set `share_context: true` to forward the current conversation \
         transcript to the child. For address-book targets this requires \
         share_context_allowed on the entry. For clone-parent spawns (no \
         target) it is honored directly.\n\n",
    );
    lines.push_str(
        "Use direct tools instead when the task is small enough to finish \
         inline.",
    );
    lines
}

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "required": ["directive"],
        "properties": {
            "directive": {
                "type": "string",
                "description": "The self-contained task to hand off. The child cannot ask follow-up questions, so include everything it needs."
            },
            "target": {
                "type": "string",
                "description": "Name of an agent from this agent's address book. Omit to spawn a fresh instance of the calling agent using its own profile (provider, model, skills, workflows) — the default way to fan out your own work in parallel. If omitted and the calling agent's own profile can't be resolved, there is no agent to clone: the call returns a recoverable error, so retry with an explicit target naming an address-book agent."
            },
            "mode": {
                "type": "string",
                "enum": ["sync", "async"],
                "description": "sync: block until the delegate finishes and return its output. async: launch in the background and return a delegation_id immediately; when done a completion message is automatically queued back to you, or call DelegateOutput with wait_seconds for a bounded blocking wait. Default: sync.",
                "default": "sync"
            },
            "share_context": {
                "type": "boolean",
                "description": "Forward the current conversation transcript to the child. Honored for address-book targets (requires share_context_allowed) and for clone-parent spawns (no target). Default: false.",
                "default": false
            },
            "description": {
                "type": "string",
                "description": "Optional short label for this delegation, shown in progress UI."
            }
        },
        "additionalProperties": false
    })
}
