/**
 * Pins the distinction between `isTyping` and `useIsAgentTurnActive`.
 *
 * Symptom this guards against: a queued follow-up message (see
 * `useQueuedMessageSend`) landing in the transcript while the agent's reply
 * is still in progress. `ChatView` used to gate the queue's flush on
 * `isTyping`, but `isTyping` legitimately flips false at every
 * `finalizeInFlightText` call — which fires on every `text_complete`,
 * including the ones that land mid-turn before a tool call, not just the
 * true end of a turn. `useQueuedMessageSend` treats any true→false edge as
 * "the run finished, flush the queue", so a multi-segment reply (text, tool
 * call, more text) flushed the queued message after the FIRST segment,
 * well before the agent was actually done.
 *
 * `finalizeInFlightText` deliberately keeps the in-flight entry itself alive
 * across these mid-turn boundaries (and across the RunEnded→RunStarted
 * skill-load/tool-continuation handoff — see `IN_FLIGHT_TEARDOWN_DELAY_MS`)
 * so the bubble reads as one continuous reply; the entry is only removed via
 * `scheduleInFlightTeardown` / `deleteInFlight`. `useIsAgentTurnActive` reads
 * entry *existence* rather than `isTyping`, so it stays true across exactly
 * those boundaries — the tests below pin that behaviour directly against the
 * store.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { useChatStore, inFlightKey } from "../chatStore";

// finalizeInFlightText (exercised below) fires a fire-and-forget
// `fetchAgents()` sidebar refresh that isn't under test here — without a
// mock it falls through to a real `fetch("/agents")` and trips the global
// unmocked-fetch guard (`src/test/setupFetchGuard.ts`) as an unhandled
// rejection.
vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return { ...actual, getAgents: async () => [] };
});

const AGENT_ID = "agent-turn-active-test";

function store() {
  return useChatStore.getState();
}

// Mirrors `useIsAgentTurnActive`'s selector body exactly (entry existence,
// not `isTyping`) — inlined here the same way `chatStore.runEndedDrain.test.ts`
// mirrors the `run_ended` SSE handler, since the real thing is a React
// selector hook and these tests drive the store directly without a DOM
// harness.
function readIsAgentTurnActive(agentId: string, threadId?: string): boolean {
  return useChatStore.getState().inFlightByAgent.has(inFlightKey(agentId, threadId));
}

beforeEach(() => {
  useChatStore.getState().reset();
});

describe("useIsAgentTurnActive vs isTyping", () => {
  it("stays true across a mid-turn text_complete that isTyping clears", () => {
    store().ensureInFlight(AGENT_ID);
    expect(readIsAgentTurnActive(AGENT_ID)).toBe(true);
    expect(store().inFlightByAgent.get(AGENT_ID)?.isTyping).toBe(true);

    // First text segment streams and finalizes (e.g. right before a tool
    // call) — isTyping drops, but the turn is NOT actually over.
    store().appendInFlightDelta(AGENT_ID, "Let me check that.");
    store().finalizeInFlightText(AGENT_ID, "Let me check that.");

    expect(store().inFlightByAgent.get(AGENT_ID)?.isTyping).toBe(false);
    // The entry itself must still be present — this is the signal the
    // queued-message flush relies on to avoid firing early.
    expect(readIsAgentTurnActive(AGENT_ID)).toBe(true);

    // Agent resumes with a second segment after the tool call.
    store().appendInFlightDelta(AGENT_ID, "The result is 42.");
    expect(store().inFlightByAgent.get(AGENT_ID)?.isTyping).toBe(true);
    expect(readIsAgentTurnActive(AGENT_ID)).toBe(true);
  });

  it("goes false only once the entry actually tears down", () => {
    store().ensureInFlight(AGENT_ID);
    store().finalizeInFlightText(AGENT_ID, "done");
    expect(readIsAgentTurnActive(AGENT_ID)).toBe(true);

    // Mirrors the Cancelled path of the run_ended handler, which deletes
    // immediately rather than debouncing.
    store().deleteInFlight(AGENT_ID);
    expect(readIsAgentTurnActive(AGENT_ID)).toBe(false);
  });

  it("is false for an agent with no in-flight entry at all", () => {
    expect(readIsAgentTurnActive("no-such-agent")).toBe(false);
  });
});
