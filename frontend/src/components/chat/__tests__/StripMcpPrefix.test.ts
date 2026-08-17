/**
 * Pins the MCP-prefix stripping behavior used by the streaming chat surface.
 *
 * Tools that reach us through the MCP transport are namespaced by the claude
 * CLI convention `mcp__<server>__<tool>`. The chat pill should display the
 * underlying tool name (and the friendly verb mapping in `describeToolCall`)
 * rather than the transport. `stripMcpPrefix` is the small helper that powers
 * this — these tests cover the cases the streaming bubble actually has to
 * handle today:
 *
 *   bare native tool                  → unchanged
 *   one-level MCP-namespaced tool     → leading `mcp__<server>__` dropped
 *   nested MCP-namespaced tool        → all `mcp__<server>__` segments dropped
 *   server name with single `_`       → still stripped (delimiter is `__`)
 *   malformed / partial input         → returned as-is rather than crashing
 *
 * Also pins the integration with `describeToolCall` so the chip wording for a
 * known MCP-routed tool matches the wording for its bare native equivalent.
 */

import { describe, it, expect } from "vitest";
import { stripMcpPrefix, describeToolCall } from "../StreamingMessage";

describe("stripMcpPrefix", () => {
  it("returns bare tool names unchanged", () => {
    expect(stripMcpPrefix("Bash")).toBe("Bash");
    expect(stripMcpPrefix("Delegate")).toBe("Delegate");
    expect(stripMcpPrefix("WorkflowActionCreate")).toBe("WorkflowActionCreate");
  });

  it("drops a single `mcp__<server>__` prefix", () => {
    expect(stripMcpPrefix("mcp__launchpad__Bash")).toBe("Bash");
    expect(stripMcpPrefix("mcp__launchpad__Delegate")).toBe("Delegate");
    expect(stripMcpPrefix("mcp__launchpad__WorkflowActionReadState"))
      .toBe("WorkflowActionReadState");
  });

  it("drops nested `mcp__<server>__` prefixes", () => {
    // The launchpad MCP server can proxy through to the "everything" demo
    // server, producing a double-nested name. Both segments must come off.
    expect(stripMcpPrefix("mcp__launchpad__mcp__everything__echo"))
      .toBe("echo");
    expect(stripMcpPrefix("mcp__launchpad__mcp__everything__get-sum"))
      .toBe("get-sum");
  });

  it("handles server names containing single underscores", () => {
    // Real connectors use names like `claude_ai_Gmail`. The delimiter is `__`
    // (double underscore), so single underscores inside the server name must
    // not split the token.
    expect(stripMcpPrefix("mcp__claude_ai_Gmail__authenticate"))
      .toBe("authenticate");
    expect(stripMcpPrefix("mcp__claude_ai_Google_Calendar__complete_authentication"))
      .toBe("complete_authentication");
  });

  it("returns input unchanged when the closing `__` is missing", () => {
    // Defensive: a truncated stream or malformed wire could yield a partial
    // prefix. Don't crash — just leave it alone and let the chip render the
    // raw string so the bug is at least visible upstream.
    expect(stripMcpPrefix("mcp__launchpad")).toBe("mcp__launchpad");
    expect(stripMcpPrefix("mcp__")).toBe("mcp__");
  });

  it("returns the empty string unchanged", () => {
    expect(stripMcpPrefix("")).toBe("");
  });
});

describe("describeToolCall — MCP-namespaced tool chips", () => {
  it("renders the verb for the underlying tool, not the transport", () => {
    // Without the strip, Bash via MCP would render as "Using mcp__launchpad__Bash".
    expect(describeToolCall("mcp__launchpad__Bash").label).toBe("Running");
    expect(describeToolCall("mcp__launchpad__Read").label).toBe("Reading");
    expect(describeToolCall("mcp__launchpad__Edit").label).toBe("Editing");
    expect(describeToolCall("mcp__launchpad__Write").label).toBe("Creating");
    expect(describeToolCall("mcp__launchpad__Grep").label).toBe("Searching");
  });

  it("Delegate routed via MCP still picks up the target-aware label", () => {
    expect(
      describeToolCall("mcp__launchpad__Delegate", { target: "Reviewer" }).label,
    ).toBe("Delegating to Reviewer…");
    expect(
      describeToolCall("mcp__launchpad__Delegate", { target: "Reviewer" }, true).label,
    ).toBe("Delegated to Reviewer");
  });

  it("unknown MCP-routed tool falls back to 'Using <stripped>' rather than the namespaced form", () => {
    // Better to surface the bare tool name than the full transport string.
    expect(describeToolCall("mcp__launchpad__WorkflowActionCreate").label)
      .toBe("Using WorkflowActionCreate");
  });
});
