/**
 * Tests for tool_use_started / tool_use_completed chip lifecycle in chatStore.
 *
 * Covers:
 * - addInFlightToolUse adds a chip with the correct tool name and action_id
 * - The chip survives a text_delta clear (action_id protects it)
 * - removeInFlightAgentAction(agentId, toolUseId) dismisses the chip
 * - Full sequence: text → tool_use_started → tool_use_completed → text
 * - Native tool_call_started chips and tool_use chips coexist without interference
 */

import { describe, it, expect, beforeEach } from "vitest";
import { useChatStore } from "../chatStore";

const AGENT_ID = "test-agent-001";

function store() {
  return useChatStore.getState();
}

function getToolCalls() {
  return useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.activeToolCalls ?? [];
}

beforeEach(() => {
  useChatStore.getState().reset();
  useChatStore.getState().ensureInFlight(AGENT_ID);
});

describe("addInFlightToolUse", () => {
  it("adds a chip with the given tool name", () => {
    store().addInFlightToolUse(AGENT_ID, "tu-1", "DateTime");
    const calls = getToolCalls();
    expect(calls).toHaveLength(1);
    expect(calls[0].tool).toBe("DateTime");
  });

  it("sets action_id to toolUseId so chip survives text_delta clear", () => {
    store().addInFlightToolUse(AGENT_ID, "tu-1", "DateTime");
    const calls = getToolCalls();
    expect(calls[0].action_id).toBe("tu-1");
  });

  it("stores optional input on the chip", () => {
    const input = { timezone: "UTC" };
    store().addInFlightToolUse(AGENT_ID, "tu-2", "DateTime", input);
    expect(getToolCalls()[0].input).toEqual(input);
  });

  it("is idempotent: duplicate toolUseId is ignored", () => {
    store().addInFlightToolUse(AGENT_ID, "tu-1", "DateTime");
    store().addInFlightToolUse(AGENT_ID, "tu-1", "DateTime");
    expect(getToolCalls()).toHaveLength(1);
  });
});

describe("tool_use chip survives text_delta clear", () => {
  it("tool_use chip (action_id set) is not cleared on text_delta", () => {
    // Simulate tool_call_started (no action_id → would be cleared by text_delta)
    store().addInFlightToolCall(AGENT_ID, { tool: "Read" });
    // Simulate tool_use_started (action_id set → survives text_delta)
    store().addInFlightToolUse(AGENT_ID, "tu-1", "DateTime");

    expect(getToolCalls()).toHaveLength(2);

    // Simulate text_delta clear: filter out entries without action_id
    useChatStore.setState((state) => {
      const current = state.inFlightByAgent.get(AGENT_ID);
      if (!current) return state;
      const filtered = current.activeToolCalls.filter((tc) => tc.action_id != null);
      const next = new Map(state.inFlightByAgent);
      next.set(AGENT_ID, { ...current, activeToolCalls: filtered });
      return { inFlightByAgent: next };
    });

    const remaining = getToolCalls();
    expect(remaining).toHaveLength(1);
    expect(remaining[0].tool).toBe("DateTime");
    expect(remaining[0].action_id).toBe("tu-1");
  });
});

describe("removeInFlightAgentAction dismisses tool_use chip", () => {
  it("removes chip by toolUseId", () => {
    store().addInFlightToolUse(AGENT_ID, "tu-1", "DateTime");
    store().addInFlightToolUse(AGENT_ID, "tu-2", "RecallHistory");
    expect(getToolCalls()).toHaveLength(2);

    store().removeInFlightAgentAction(AGENT_ID, "tu-1");

    const remaining = getToolCalls();
    expect(remaining).toHaveLength(1);
    expect(remaining[0].tool).toBe("RecallHistory");
  });
});

describe("full tool_use sequence", () => {
  it("text → ToolUseStarted → ToolUseCompleted → text lifecycle", () => {
    // Step 1: text arrives (TextDelta)
    store().appendInFlightDelta(AGENT_ID, "Let me check.");
    expect(useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.textBuffer).toBe("Let me check.");
    expect(getToolCalls()).toHaveLength(0);

    // Step 2: ToolUseStarted — chip appears
    store().addInFlightToolUse(AGENT_ID, "tu-1", "DateTime");
    expect(getToolCalls()).toHaveLength(1);
    expect(getToolCalls()[0].tool).toBe("DateTime");

    // Step 3: ToolUseCompleted — chip dismissed
    store().removeInFlightAgentAction(AGENT_ID, "tu-1");
    expect(getToolCalls()).toHaveLength(0);

    // Step 4: follow-up text arrives
    store().appendInFlightDelta(AGENT_ID, " It's 3pm.");
    expect(useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.textBuffer).toBe("Let me check. It's 3pm.");
  });
});

describe("addInFlightToolCall Layer-2 label", () => {
  it("lands a label passed alongside tool+input on the ActiveToolCall", () => {
    store().addInFlightToolCall(AGENT_ID, {
      tool: "RunSkill",
      input: { skill: "systematic-debugging" },
      label: "Loading skill: Systematic Debugging",
    });
    const calls = getToolCalls();
    expect(calls).toHaveLength(1);
    expect(calls[0].label).toBe("Loading skill: Systematic Debugging");
  });

  it("carries a label forward when input arrives on a previously-input-less chip", () => {
    // First event: input-less chip already carries a Layer-2 override label.
    store().addInFlightToolCall(AGENT_ID, { tool: "Read", label: "Reading plan" });
    expect(getToolCalls()).toHaveLength(1);
    expect(getToolCalls()[0].input).toBeUndefined();

    // Second event: same tool gains input but no label — the merge must keep
    // the label the input-less chip carried rather than dropping it.
    store().addInFlightToolCall(AGENT_ID, { tool: "Read", input: { file_path: "/docs/plan.md" } });

    const calls = getToolCalls();
    expect(calls).toHaveLength(1);
    expect(calls[0].input).toEqual({ file_path: "/docs/plan.md" });
    expect(calls[0].label).toBe("Reading plan");
  });

  it("prefers the input-bearing event's label over the input-less chip's on merge", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Read", label: "Reading" });
    store().addInFlightToolCall(AGENT_ID, {
      tool: "Read",
      input: { file_path: "/docs/plan.md" },
      label: "Reading plan",
    });

    const calls = getToolCalls();
    expect(calls).toHaveLength(1);
    expect(calls[0].label).toBe("Reading plan");
  });
});

describe("native tool_call and tool_use coexistence", () => {
  it("native chip and tool_use chip coexist without interference", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Read", input: { file_path: "/foo/bar.ts" } });
    store().addInFlightToolUse(AGENT_ID, "tu-1", "DateTime");

    const calls = getToolCalls();
    expect(calls).toHaveLength(2);
    expect(calls.find((c) => c.tool === "Read")).toBeDefined();
    expect(calls.find((c) => c.tool === "DateTime")).toBeDefined();

    // markInFlightToolCallDone marks only the native chip done — it stays in
    // the array (no shrink/regrow) rather than being removed. See the
    // "jumpy tool indicator" fix: a classic chip now only leaves the array
    // via text_delta's flush, finalize, run_ended, or the stacking cap.
    store().markInFlightToolCallDone(AGENT_ID);
    const afterDone = getToolCalls();
    expect(afterDone).toHaveLength(2);
    expect(afterDone.find((c) => c.tool === "Read")?.done).toBe(true);
    expect(afterDone.find((c) => c.tool === "DateTime")?.done).toBeFalsy();

    // removeInFlightAgentAction removes only the tool_use chip
    store().removeInFlightAgentAction(AGENT_ID, "tu-1");
    const after = getToolCalls();
    expect(after).toHaveLength(1);
    expect(after[0].tool).toBe("Read");
  });
});
