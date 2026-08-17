/**
 * Pins the defensive textBuffer-drain behaviour at run_ended.
 *
 * Symptom this guards against: a partially-streamed agent reply where the
 * `text_complete` event never lands (lost in transit, suppressed by a
 * mid-turn tool failure, dropped by a panicking runner). Before the drain
 * landed in `useSSE.ts::run_ended`, `scheduleInFlightTeardown` would clear
 * the in-flight entry 400ms later with the streaming text still buffered,
 * so the user saw the message disappear and only reappear after a
 * navigate-away / navigate-back refetched it from disk (where
 * `persist_pending` had already done its job).
 *
 * The tests below simulate the SSE handler's behaviour directly against
 * the store, since the actual handler lives in a React effect and is hard
 * to drive without a full DOM harness.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { useChatStore } from "../chatStore";

// finalizeInFlightText (called by simulateRunEndedHandler below) fires a
// fire-and-forget `fetchAgents()` sidebar refresh that isn't under test
// here — without a mock it falls through to a real `fetch("/agents")` and
// trips the global unmocked-fetch guard (`src/test/setupFetchGuard.ts`) as
// an unhandled rejection.
vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return { ...actual, getAgents: async () => [] };
});

const AGENT_ID = "agent-drain-test";

function store() {
  return useChatStore.getState();
}

beforeEach(() => {
  useChatStore.getState().reset();
  useChatStore.getState().ensureInFlight(AGENT_ID);
  useChatStore.setState({ selectedAgentId: AGENT_ID });
});

/**
 * Mirrors the body of the `run_ended` SSE handler in `useSSE.ts` — drain
 * any unfinalized buffer into the transcript before tearing down. Kept
 * inline in the test so we exercise the exact ordering rather than
 * importing internals.
 */
function simulateRunEndedHandler(agentId: string, reason: string) {
  const inFlightEntry = useChatStore.getState().inFlightByAgent.get(agentId);
  if (inFlightEntry && inFlightEntry.textBuffer.length > 0) {
    store().finalizeInFlightText(agentId, inFlightEntry.textBuffer);
  }
  store().clearInFlightToolCalls(agentId);
  if (reason === "Cancelled") {
    store().deleteInFlight(agentId);
  }
  // (omitting scheduleInFlightTeardown here — it's a 400ms timer that
  // doesn't affect the assertions in this test.)
}

describe("run_ended drains pending textBuffer into transcript", () => {
  it("appends buffered text to messages when text_complete was skipped", () => {
    // Stream three deltas — buffer fills, but text_complete never arrives.
    store().appendInFlightDelta(AGENT_ID, "Hey ");
    store().appendInFlightDelta(AGENT_ID, "Axew — ");
    store().appendInFlightDelta(AGENT_ID, "no prompt this turn.");

    const buffered = useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.textBuffer;
    expect(buffered).toBe("Hey Axew — no prompt this turn.");
    expect(useChatStore.getState().messages).toHaveLength(0);

    // run_ended fires; drain should finalize the buffer before teardown.
    simulateRunEndedHandler(AGENT_ID, "Completed");

    const messages = useChatStore.getState().messages;
    expect(messages).toHaveLength(1);
    expect(messages[0].content).toBe("Hey Axew — no prompt this turn.");
    // Buffer is cleared by finalizeInFlightText so a future delta starts fresh.
    expect(useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.textBuffer).toBe("");
  });

  it("no-op when buffer is empty (text_complete arrived as expected)", () => {
    // Normal flow: text_complete fired and ran finalizeInFlightText, leaving
    // an empty buffer when run_ended arrives.
    store().appendInFlightDelta(AGENT_ID, "all done");
    store().finalizeInFlightText(AGENT_ID, "all done");
    expect(useChatStore.getState().messages).toHaveLength(1);

    simulateRunEndedHandler(AGENT_ID, "Completed");

    // Still one message; no duplicate from the drain.
    expect(useChatStore.getState().messages).toHaveLength(1);
    expect(useChatStore.getState().messages[0].content).toBe("all done");
  });

  it("drains even on Cancelled — the streamed text was real work", () => {
    // User clicked Stop mid-stream; we still want what was already typed
    // to land in the transcript instead of evaporating.
    store().appendInFlightDelta(AGENT_ID, "I was about to say something useful");

    simulateRunEndedHandler(AGENT_ID, "Cancelled");

    const messages = useChatStore.getState().messages;
    expect(messages).toHaveLength(1);
    expect(messages[0].content).toBe("I was about to say something useful");
    // deleteInFlight ran (Cancelled path); entry should be gone.
    expect(useChatStore.getState().inFlightByAgent.has(AGENT_ID)).toBe(false);
  });
});
