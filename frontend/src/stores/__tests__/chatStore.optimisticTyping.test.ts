/**
 * Regression tests for the optimistic typing indicator in `sendMessage`.
 *
 * Symptom this guards against: perceived send lag. The typing indicator is
 * driven off the in-flight entry's `isTyping`, which historically only flipped
 * true when the server's `run_started` (or a replayed `agent_busy`) travelled
 * back down the single shared SSE stream. When that one connection is
 * mid-reconnect (laptop wake, network blip, or an ao-server restart in dev),
 * the live event is missed and the dots don't appear until the stream
 * re-establishes — which reads as "I sent a message and nothing happened".
 *
 * The fix raises the indicator optimistically the instant a message is posted,
 * with two guardrails pinned here:
 *   1. a failed send retracts the indicator (no run will start), and
 *   2. a bounded watchdog retracts a stuck indicator if the run never actually
 *      starts — but only while the entry is still in its untouched optimistic
 *      state, so a genuinely-slow run that has begun streaming is left alone.
 *
 * These drive the store directly (no DOM harness) the same way the sibling
 * chatStore.* tests do.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

const AGENT_ID = "agent-optimistic-typing";
const FRESH_THREAD_ID = "fresh-thread-xyz";

const mockSendMessage = vi.fn();
const mockGetAgents = vi.fn();

vi.mock("../../lib/api", () => ({
  getAgents: (...args: unknown[]) => mockGetAgents(...args),
  getAgent: vi.fn().mockResolvedValue(null),
  getMessages: vi.fn().mockResolvedValue({ messages: [], cursor: null }),
  listThreads: vi.fn().mockResolvedValue([]),
  sendMessage: (...args: unknown[]) => mockSendMessage(...args),
}));

import { useChatStore, inFlightKey } from "../chatStore";

function store() {
  return useChatStore.getState();
}

beforeEach(() => {
  useChatStore.getState().reset();
  vi.clearAllMocks();
  mockGetAgents.mockResolvedValue([]);
  mockSendMessage.mockResolvedValue({ message_id: "msg-1", status: "queued" });
  useChatStore.setState({ selectedAgentId: AGENT_ID });
});

afterEach(() => {
  vi.useRealTimers();
});

describe("optimistic typing indicator on send", () => {
  it("shows the typing indicator immediately on send, before any SSE event", async () => {
    await store().sendMessage("Hello");

    // The default-thread in-flight key is the bare agent id. The indicator
    // must already be up purely from the send — no run_started was delivered.
    const entry = store().inFlightByAgent.get(inFlightKey(AGENT_ID));
    expect(entry?.isTyping).toBe(true);
  });

  it("keys the optimistic entry to the active thread", async () => {
    useChatStore.setState((s) => {
      const next = new Map(s.selectedThreadIdByAgent);
      next.set(AGENT_ID, FRESH_THREAD_ID);
      return { selectedThreadIdByAgent: next };
    });

    await store().sendMessage("Hello from a branch thread");

    // A non-default thread gets its own composite key — the indicator must
    // land there, not in the agent's default bucket.
    expect(store().inFlightByAgent.get(inFlightKey(AGENT_ID, FRESH_THREAD_ID))?.isTyping).toBe(true);
    expect(store().inFlightByAgent.has(inFlightKey(AGENT_ID))).toBe(false);
  });

  it("is idempotent with a later run_started — buffer and typing survive", async () => {
    await store().sendMessage("Hello");
    const key = inFlightKey(AGENT_ID);

    // A real run_started arriving over SSE calls ensureInFlight again; it must
    // not wipe the entry or double-toggle anything.
    store().ensureInFlight(key);
    expect(store().inFlightByAgent.get(key)?.isTyping).toBe(true);

    // First streamed text lands — normal flow continues from the optimistic
    // entry rather than a fresh one.
    store().appendInFlightDelta(key, "Hi there");
    expect(store().inFlightByAgent.get(key)?.textBuffer).toBe("Hi there");
  });

  it("retracts the indicator when the send itself fails", async () => {
    mockSendMessage.mockRejectedValueOnce(new Error("network down"));

    await expect(store().sendMessage("Hello")).rejects.toThrow("network down");

    // No run will ever start for a failed send — the dots must not linger.
    expect(store().inFlightByAgent.has(inFlightKey(AGENT_ID))).toBe(false);
  });
});

describe("optimistic typing watchdog", () => {
  it("retracts a stuck indicator when the run never starts", async () => {
    vi.useFakeTimers();
    try {
      await store().sendMessage("Hello");
      const key = inFlightKey(AGENT_ID);
      expect(store().inFlightByAgent.get(key)?.isTyping).toBe(true);

      // No run_started, no delta, no tool call ever arrive — the entry stays
      // pristine. Past the watchdog window it must be torn down.
      await vi.advanceTimersByTimeAsync(60000);
      expect(store().inFlightByAgent.has(key)).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });

  it("leaves a genuinely-running turn alone when the watchdog fires", async () => {
    vi.useFakeTimers();
    try {
      await store().sendMessage("Hello");
      const key = inFlightKey(AGENT_ID);

      // The run actually started and produced text — the entry is no longer
      // pristine, so the watchdog's guard must fail and leave it intact.
      store().appendInFlightDelta(key, "streaming reply");

      await vi.advanceTimersByTimeAsync(60000);
      expect(store().inFlightByAgent.get(key)?.textBuffer).toBe("streaming reply");
      expect(store().inFlightByAgent.get(key)?.isTyping).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });
});
