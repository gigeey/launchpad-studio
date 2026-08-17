/**
 * Pins `InFlightAgentMessage.everShownThisTurn` — the store-resident latch
 * that replaced a `StreamingMessage`-local `useRef`.
 *
 * Bug this guards against: `StreamingMessage` used a local ref to remember
 * "has this turn shown any content yet" so the bubble stays mounted through
 * a momentary content gap (e.g. the beat between one tool call finishing and
 * the next starting, where `activeToolCalls` is briefly empty). But
 * `MessageList` remounts on every conversation switch
 * (`key={deferredConversationKey}` in `ChatView`), which reset that local
 * ref to `false`. If a remount landed during one of those normal gaps, the
 * whole bubble (avatar, name, idle dots, tool chip) rendered nothing until
 * the next SSE event arrived — even though the turn was still very much
 * alive in the store. Moving the latch into the store fixes this: it now
 * survives remounts and only resets when the in-flight entry itself tears
 * down, exactly mirroring how `activeToolCalls`/`textBuffer` already behave
 * (see `chatStore.turnActive.test.ts` for the sibling pattern this borrows).
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { useChatStore } from "../chatStore";

// finalizeInFlightText (exercised below) fires a fire-and-forget
// `fetchAgents()` sidebar refresh that isn't under test here — without a
// mock it falls through to a real `fetch("/agents")` and trips the global
// unmocked-fetch guard (`src/test/setupFetchGuard.ts`) as an unhandled
// rejection.
vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return { ...actual, getAgents: async () => [] };
});

const AGENT_ID = "agent-ever-shown-test";

function store() {
  return useChatStore.getState();
}

function everShown(agentId: string): boolean {
  return useChatStore.getState().inFlightByAgent.get(agentId)?.everShownThisTurn ?? false;
}

beforeEach(() => {
  useChatStore.getState().reset();
});

describe("everShownThisTurn latch", () => {
  it("is false immediately after ensureInFlight (typing dots alone don't count as content)", () => {
    store().ensureInFlight(AGENT_ID);
    expect(everShown(AGENT_ID)).toBe(false);
  });

  it("flips true on the first text delta and survives finalize + a new segment", () => {
    store().ensureInFlight(AGENT_ID);
    store().appendInFlightDelta(AGENT_ID, "Let me check that.");
    expect(everShown(AGENT_ID)).toBe(true);

    // finalizeInFlightText clears the buffer for a mid-turn boundary but
    // must not reset the latch — this is exactly the skill-load /
    // tool-continuation gap the latch is meant to survive.
    store().finalizeInFlightText(AGENT_ID, "Let me check that.");
    expect(everShown(AGENT_ID)).toBe(true);

    store().appendInFlightDelta(AGENT_ID, "The result is 42.");
    expect(everShown(AGENT_ID)).toBe(true);
  });

  it("flips true on a tool call, stays true once it's marked done, and stays true once the array is later cleared", () => {
    store().ensureInFlight(AGENT_ID);
    store().addInFlightToolCall(AGENT_ID, { tool: "Read" });
    expect(everShown(AGENT_ID)).toBe(true);
    expect(store().inFlightByAgent.get(AGENT_ID)?.activeToolCalls.length).toBe(1);

    // Tool call finishes — the chip is marked done in place, NOT removed
    // (see markInFlightToolCallDone / the "jumpy tool indicator" fix), so
    // the array no longer drains to empty at this point.
    store().markInFlightToolCallDone(AGENT_ID);
    expect(store().inFlightByAgent.get(AGENT_ID)?.activeToolCalls.length).toBe(1);
    expect(store().inFlightByAgent.get(AGENT_ID)?.activeToolCalls[0]?.done).toBe(true);
    expect(everShown(AGENT_ID)).toBe(true);

    // The array only actually empties via a real flush point (text_delta's
    // classic-chip clear, finalize, or run_ended's clearInFlightToolCalls —
    // simulated here). This is the beat a remount used to blank the bubble
    // under the old ref-based implementation.
    store().clearInFlightToolCalls(AGENT_ID);
    expect(store().inFlightByAgent.get(AGENT_ID)?.activeToolCalls.length).toBe(0);
    expect(everShown(AGENT_ID)).toBe(true);
  });

  it("flips true on an artifact id and on thinking activity", () => {
    store().ensureInFlight(AGENT_ID);
    store().appendInFlightArtifactId(AGENT_ID, "artifact-1");
    expect(everShown(AGENT_ID)).toBe(true);

    useChatStore.getState().reset();
    store().ensureInFlight(AGENT_ID);
    store().startInFlightThinking(AGENT_ID);
    expect(everShown(AGENT_ID)).toBe(true);
  });

  it("survives a simulated remount — a fresh read of the store (not a local ref) still sees it", () => {
    store().ensureInFlight(AGENT_ID);
    store().addInFlightToolCall(AGENT_ID, { tool: "Grep" });
    store().markInFlightToolCallDone(AGENT_ID);

    // "Remounting" a component that reads this via the useEverShownThisTurn
    // hook just re-subscribes to the same store state — unlike the old local
    // ref, there is no component-local memory to lose here.
    expect(everShown(AGENT_ID)).toBe(true);
  });

  it("resets to false only once the entry itself tears down and is recreated", () => {
    store().ensureInFlight(AGENT_ID);
    store().appendInFlightDelta(AGENT_ID, "hi");
    store().finalizeInFlightText(AGENT_ID, "hi");
    expect(everShown(AGENT_ID)).toBe(true);

    store().deleteInFlight(AGENT_ID);
    expect(store().inFlightByAgent.has(AGENT_ID)).toBe(false);

    store().ensureInFlight(AGENT_ID);
    expect(everShown(AGENT_ID)).toBe(false);
  });
});
