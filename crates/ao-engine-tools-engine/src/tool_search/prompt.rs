/// Description used by [`ToolSearch`](super::ToolSearch) as its `EngineTool::description()`.
///
/// This is the primary lever for correct model behavior — read carefully before editing.
pub const DESCRIPTION: &str = "\
Search the deferred tool catalog by keyword, or activate tools by name using the \
select: operator.

**This tool only returns tools that are NOT currently loaded in the session.** \
Deferred tools are omitted from every request by default to save context window \
tokens; use ToolSearch to find and activate them on demand.

## Keyword search

Pass a query string to find tools whose name or description matches your keywords. \
An empty string (or whitespace-only) lists all currently-unloaded deferred tools, \
sorted alphabetically with score 0. Results are capped at max_results (default 5).

## select: activation

To activate one or more tools by name, prefix the query with `select:` (case-insensitive) \
followed by a comma-separated list of tool names:

    select:Name1,Name2

What happens:
- Each named tool is looked up in the full registry (IO and engine tools).
- Found tools are added to the session's activated set; their full input schemas \
  are returned in the `activated` array so you can call them immediately.
- Activations **persist for the entire session** — you do not need to re-activate \
  a tool each turn.
- Activating a tool that is already loaded (always-load or previously activated) is \
  **idempotent**: the call succeeds, the schema is returned, and telemetry is emitted.
- Tool names that do not match any registered tool are returned in the `unresolved` \
  array; the call still returns Ok even when all names are unresolved.

After a successful select:, the activated tool schemas appear in subsequent turns' \
tool arrays alongside the always-loaded set.";
