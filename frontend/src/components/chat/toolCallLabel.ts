/** Shared tool-call chip label logic — imported by all chat surfaces. */

/** Strip MCP namespacing (`mcp__<server>__`) from a tool name so chips show
 *  the underlying tool rather than the transport. Handles nested forms like
 *  `mcp__launchpad__mcp__everything__echo` → `echo`. */
export function stripMcpPrefix(tool: string): string {
  let s = tool;
  while (s.startsWith("mcp__")) {
    const sep = s.indexOf("__", 5);
    if (sep < 0) break;
    s = s.slice(sep + 2);
  }
  return s;
}

/** True for task/agent output files (e.g. `.../tasks/<id>/output...`). */
export function isAgentOutputPath(path: string): boolean {
  return /\/tasks\/[^/]+\/output/.test(path);
}

function truncate(s: string): string {
  return s.length > 28 ? s.slice(0, 25) + "…" : s;
}

/** Map tool names to human-friendly chip labels, enriched by tool input. */
export function describeToolCall(
  rawTool: string,
  input?: Record<string, unknown>,
  completed?: boolean,
): { label: string; detail?: string } {
  const tool = stripMcpPrefix(rawTool);

  if (tool === "Delegate") {
    const rawTarget = input?.target;
    const target =
      typeof rawTarget === "string" && rawTarget.trim().length > 0
        ? rawTarget.trim()
        : null;
    if (target == null) return { label: completed ? "Delegated" : "Delegating…" };
    return { label: completed ? `Delegated to ${target}` : `Delegating to ${target}…` };
  }

  if (tool === "Agent") {
    const desc = (input?.description as string) ?? (input?.prompt as string);
    if (desc) return { label: "Using Agent", detail: desc };
    return { label: "Using Agent" };
  }

  if (tool === "RunSkill") {
    const skill = input?.skill as string | undefined;
    if (skill) return { label: `Loading skill: ${skill}` };
    return { label: "Loading skill" };
  }

  if (tool === "TodoCreate") {
    if (completed) return { label: "Used TodoList" };
    const name = input?.name as string | undefined;
    return { label: name ? `Using TodoList: ${name}` : "Using TodoList" };
  }

  const memoryLabels: Record<string, { pending: string; settled: string }> = {
    MemoryWrite: { pending: "Saving memory…", settled: "Saved memory" },
    MemoryEdit: { pending: "Editing memory…", settled: "Edited memory" },
    MemoryDelete: { pending: "Deleting memory…", settled: "Deleted memory" },
    MemoryList: { pending: "Listing memories…", settled: "Listed memories" },
  };
  if (tool in memoryLabels) {
    const { pending, settled } = memoryLabels[tool];
    return { label: completed ? settled : pending };
  }

  const filePath = (input?.file_path as string) ?? (input?.path as string);

  if (tool === "Read") {
    if (!filePath) return { label: "Reading" };
    if (isAgentOutputPath(filePath)) return { label: "Reading agent output" };
    return { label: `Reading ${truncate(filePath.split("/").pop() ?? filePath)}` };
  }

  if (tool === "Edit") {
    if (!filePath) return { label: "Editing" };
    return { label: `Editing ${truncate(filePath.split("/").pop() ?? filePath)}` };
  }

  if (tool === "Write") {
    if (!filePath) return { label: "Creating" };
    return { label: `Creating ${truncate(filePath.split("/").pop() ?? filePath)}` };
  }

  if (tool === "Bash") {
    const desc = input?.description as string | undefined;
    if (desc) return { label: `Running: ${truncate(desc)}` };
    const cmd = input?.command as string | undefined;
    if (cmd) return { label: `Running ${truncate(cmd.split("/").pop() ?? cmd)}` };
    return { label: "Running" };
  }

  if (tool === "Grep") {
    const pattern = input?.pattern as string | undefined;
    if (pattern) return { label: `Searching for ${truncate(pattern)}` };
    return { label: "Searching" };
  }

  if (tool === "Glob") {
    const pattern = input?.pattern as string | undefined;
    if (pattern) return { label: `Finding files: ${truncate(pattern)}` };
    return { label: "Finding files" };
  }

  if (tool === "WebSearch") {
    const query = input?.query as string | undefined;
    if (query) return { label: `Searching the web: ${truncate(query)}` };
    return { label: "Searching the web" };
  }

  if (tool === "WebFetch") {
    const url = input?.url as string | undefined;
    if (url) {
      try {
        const domain = new URL(url).hostname;
        return { label: `Fetching ${truncate(domain)}` };
      } catch {
        return { label: "Fetching" };
      }
    }
    return { label: "Fetching" };
  }

  if (tool === "ListDirectory") {
    if (filePath) return { label: `Browsing ${truncate(filePath.split("/").pop() ?? filePath)}` };
    return { label: "Browsing" };
  }

  return { label: `Using ${tool}` };
}
