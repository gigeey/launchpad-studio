/**
 * Pins the "jumpy tool indicator" fix: a classic (native tool-calling, no
 * `action_id`) chip is no longer removed from `activeToolCalls` the instant
 * its own `tool_call_completed` fires. It's marked `done` in place instead
 * (`markInFlightToolCallDone`, née `popInFlightToolCall`) so the bubble
 * doesn't shrink-then-regrow between one tool finishing and the next
 * starting or text beginning. Actual removal only happens via:
 *  - a new classic chip pushing the stack past the 5-chip cap (oldest done
 *    chip evicted first — see `capClassicToolCalls`)
 *  - text_delta's existing classic-chip flush (unchanged, tested elsewhere)
 *  - finalize / run_ended's clearInFlightToolCalls (unchanged)
 *
 * `action_id`-keyed chips (tool_use/agent_action) are untouched by any of
 * this — see chatStore.toolUse.test.ts for their coexistence coverage.
 */

import { describe, it, expect, beforeEach } from "vitest";
import { useChatStore } from "../chatStore";

const AGENT_ID = "agent-tool-call-stack-test";

function store() {
  return useChatStore.getState();
}

function calls() {
  return useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.activeToolCalls ?? [];
}

beforeEach(() => {
  useChatStore.getState().reset();
  useChatStore.getState().ensureInFlight(AGENT_ID);
});

describe("markInFlightToolCallDone keeps the chip stacked", () => {
  it("marks the oldest not-done classic chip done instead of removing it", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Read" });
    store().markInFlightToolCallDone(AGENT_ID);

    expect(calls()).toHaveLength(1);
    expect(calls()[0].tool).toBe("Read");
    expect(calls()[0].done).toBe(true);
  });

  it("stacks multiple sequential tool calls, each marked done as it completes", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Read" });
    store().markInFlightToolCallDone(AGENT_ID);
    store().addInFlightToolCall(AGENT_ID, { tool: "Grep" });
    store().markInFlightToolCallDone(AGENT_ID);
    store().addInFlightToolCall(AGENT_ID, { tool: "Edit" });

    const c = calls();
    expect(c.map((tc) => tc.tool)).toEqual(["Read", "Grep", "Edit"]);
    expect(c.map((tc) => !!tc.done)).toEqual([true, true, false]);
  });

  it("is a no-op when there is no not-done classic chip to mark", () => {
    store().markInFlightToolCallDone(AGENT_ID);
    expect(calls()).toHaveLength(0);
  });
});

describe("classic tool-call stacking cap (5)", () => {
  it("evicts the oldest done chip once a 6th classic chip is added", () => {
    for (let i = 0; i < 5; i++) {
      store().addInFlightToolCall(AGENT_ID, { tool: `Tool${i}` });
      store().markInFlightToolCallDone(AGENT_ID);
    }
    expect(calls()).toHaveLength(5);
    expect(calls().map((tc) => tc.tool)).toEqual(["Tool0", "Tool1", "Tool2", "Tool3", "Tool4"]);

    store().addInFlightToolCall(AGENT_ID, { tool: "Tool5" });

    const c = calls();
    expect(c).toHaveLength(5);
    // Oldest (Tool0, already done) evicted; the rest shift up, newest at the end.
    expect(c.map((tc) => tc.tool)).toEqual(["Tool1", "Tool2", "Tool3", "Tool4", "Tool5"]);
  });

  it("never evicts a still-active chip while a done chip exists on the stack", () => {
    // markInFlightToolCallDone marks the OLDEST not-done classic chip (FIFO —
    // inherited from the original popInFlightToolCall's slice(1)), so
    // "StillRunning" must be added *last*, after everything ahead of it is
    // already done, for it to remain the sole active chip.
    for (let i = 0; i < 4; i++) {
      store().addInFlightToolCall(AGENT_ID, { tool: `Tool${i}` });
      store().markInFlightToolCallDone(AGENT_ID);
    }
    store().addInFlightToolCall(AGENT_ID, { tool: "StillRunning" }); // never marked done
    expect(calls()).toHaveLength(5);

    // 6th classic chip pushes the stack over the cap.
    store().addInFlightToolCall(AGENT_ID, { tool: "Tool4" });

    const c = calls();
    expect(c).toHaveLength(5);
    // "StillRunning" (still active) survives; the oldest *done* chip
    // (Tool0) is evicted instead.
    expect(c.find((tc) => tc.tool === "StillRunning")).toBeDefined();
    expect(c.find((tc) => tc.tool === "Tool0")).toBeUndefined();
  });

  it("does not count action_id-keyed chips against the classic cap", () => {
    for (let i = 0; i < 5; i++) {
      store().addInFlightToolCall(AGENT_ID, { tool: `Tool${i}` });
      store().markInFlightToolCallDone(AGENT_ID);
    }
    store().addInFlightToolUse(AGENT_ID, "tu-1", "DateTime");
    store().addInFlightToolUse(AGENT_ID, "tu-2", "RecallHistory");

    // 5 classic (at cap) + 2 action_id-keyed — cap only applies to classic.
    expect(calls()).toHaveLength(7);
  });
});

describe("repeated same-tool call after completion doesn't revive the done chip", () => {
  it("gives a second Read call after the first is done its own fresh chip", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Read", input: { file_path: "/a.ts" } });
    store().markInFlightToolCallDone(AGENT_ID);

    // A later, unrelated Read call for a different file must not merge into
    // (and un-hide the "done" state of) the already-completed chip.
    store().addInFlightToolCall(AGENT_ID, { tool: "Read", label: "Reading b.ts" });

    const c = calls();
    expect(c).toHaveLength(2);
    expect(c[0].input).toEqual({ file_path: "/a.ts" });
    expect(c[0].done).toBe(true);
    expect(c[1].label).toBe("Reading b.ts");
    expect(c[1].done).toBeFalsy();
  });
});
