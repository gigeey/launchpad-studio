/**
 * Pins `InFlightAgentMessage.thinkingShown` — the flag that keeps
 * `useInFlightThinking` returning a non-null snapshot between two sequential
 * thinking blocks in the same turn, instead of flipping to `null` the
 * instant a block closes with no buffered text (the `display: "omitted"`
 * case). Before this flag, `ThinkingPill` unmounted/remounted between
 * blocks — a visible "bubble jumps up and down" bug. Now the pill stays
 * mounted and just flips its icon between lit (`Lightbulb`, active) and dim
 * (`LightbulbOff`, inactive) — see `ThinkingPill` in StreamingMessage.tsx.
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

const AGENT_ID = "agent-thinking-persist-test";

function store() {
  return useChatStore.getState();
}

function entry() {
  return useChatStore.getState().inFlightByAgent.get(AGENT_ID);
}

beforeEach(() => {
  useChatStore.getState().reset();
});

describe("thinkingShown persistence across sequential blocks", () => {
  it("is false before any thinking block opens", () => {
    store().ensureInFlight(AGENT_ID);
    expect(entry()?.thinkingShown).toBe(false);
  });

  it("flips true on thinking_started and stays true after thinking_ended (no buffered text)", () => {
    store().ensureInFlight(AGENT_ID);
    store().startInFlightThinking(AGENT_ID);
    expect(entry()?.thinkingActive).toBe(true);
    expect(entry()?.thinkingShown).toBe(true);

    store().endInFlightThinking(AGENT_ID, 1200);
    expect(entry()?.thinkingActive).toBe(false);
    expect(entry()?.thinkingBuffer).toBe("");
    // The whole point: still shown (pill stays mounted, dim) even though
    // there's no active block and no buffered text to fall back on.
    expect(entry()?.thinkingShown).toBe(true);
    expect(entry()?.thinkingElapsedMs).toBe(1200);
  });

  it("re-lights (thinkingActive true) on a second block without ever losing thinkingShown", () => {
    store().ensureInFlight(AGENT_ID);
    store().startInFlightThinking(AGENT_ID);
    store().endInFlightThinking(AGENT_ID, 800);
    expect(entry()?.thinkingShown).toBe(true);

    // Second thinking block of the same turn.
    store().startInFlightThinking(AGENT_ID);
    expect(entry()?.thinkingActive).toBe(true);
    expect(entry()?.thinkingShown).toBe(true);

    store().endInFlightThinking(AGENT_ID, 300);
    expect(entry()?.thinkingActive).toBe(false);
    // "Thought for Ns" reflects the *last* block, not a cumulative sum.
    expect(entry()?.thinkingElapsedMs).toBe(300);
    expect(entry()?.thinkingShown).toBe(true);
  });

  it("appendInFlightThinkingDelta also sets thinkingShown (delta-only stream, no explicit start)", () => {
    store().ensureInFlight(AGENT_ID);
    store().appendInFlightThinkingDelta(AGENT_ID, "reasoning...");
    expect(entry()?.thinkingShown).toBe(true);
  });

  it("a text_delta that auto-closes a still-active block preserves thinkingShown", () => {
    store().ensureInFlight(AGENT_ID);
    store().startInFlightThinking(AGENT_ID);
    // Model moved straight to output without an explicit thinking_ended.
    store().appendInFlightDelta(AGENT_ID, "Here's the answer.");
    expect(entry()?.thinkingActive).toBe(false);
    expect(entry()?.thinkingShown).toBe(true);
  });

  it("resets to false only on finalize (next turn starts with a clean reasoning channel)", () => {
    store().ensureInFlight(AGENT_ID);
    store().startInFlightThinking(AGENT_ID);
    store().endInFlightThinking(AGENT_ID, 500);
    expect(entry()?.thinkingShown).toBe(true);

    store().finalizeInFlightText(AGENT_ID, "final reply");
    expect(entry()?.thinkingShown).toBe(false);
    expect(entry()?.thinkingActive).toBe(false);
    expect(entry()?.thinkingBuffer).toBe("");
  });
});
