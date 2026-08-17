/**
 * Regression tests for cancelRun: stopping a run must be scoped to the
 * thread the user is looking at, not fan out to every thread for the agent.
 *
 * Covers two bugs fixed together:
 *  1. `api.cancelAgentRun` must be called with the *current* thread id (or
 *     omitted for the default thread) so the backend only cancels that run.
 *  2. `deleteInFlight` must clear the in-flight entry keyed by the current
 *     thread (via `inFlightKey`), not the bare agentId — otherwise a named
 *     thread's own "in flight"/typing UI would survive its own cancel while
 *     an unrelated thread's entry could be touched instead.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";

const cancelAgentRun = vi.fn().mockResolvedValue(undefined);

vi.mock("../lib/api", () => ({
  cancelAgentRun: (...args: unknown[]) => cancelAgentRun(...args),
  getAgents: vi.fn().mockResolvedValue([]),
  getMessages: vi.fn().mockResolvedValue({ messages: [], cursor: null }),
  getAgent: vi.fn().mockResolvedValue(null),
}));

import { useChatStore, inFlightKey } from "./chatStore";

beforeEach(() => {
  useChatStore.getState().reset();
  cancelAgentRun.mockClear();
});

describe("cancelRun thread scoping", () => {
  it("omits thread_id when the default thread is selected", async () => {
    const agentId = "agent-1";
    useChatStore.setState({ selectedAgentId: agentId, selectedThreadIdByAgent: new Map() });

    await useChatStore.getState().cancelRun();

    expect(cancelAgentRun).toHaveBeenCalledWith(agentId, undefined);
  });

  it("passes the selected thread's id when a non-default thread is active", async () => {
    const agentId = "agent-2";
    const threadId = "thread-b";
    useChatStore.setState({
      selectedAgentId: agentId,
      selectedThreadIdByAgent: new Map([[agentId, threadId]]),
    });

    await useChatStore.getState().cancelRun();

    expect(cancelAgentRun).toHaveBeenCalledWith(agentId, threadId);
  });

  it("clears only the active thread's in-flight entry, leaving a sibling thread's run untouched", async () => {
    const agentId = "agent-3";
    const threadA = "thread-a";
    const threadB = "thread-b";
    const keyA = inFlightKey(agentId, threadA);
    const keyB = inFlightKey(agentId, threadB);

    // Simulate both threads mid-run.
    useChatStore.getState().ensureInFlight(keyA);
    useChatStore.getState().ensureInFlight(keyB);
    expect(useChatStore.getState().inFlightByAgent.has(keyA)).toBe(true);
    expect(useChatStore.getState().inFlightByAgent.has(keyB)).toBe(true);

    // User is looking at thread A and hits Stop.
    useChatStore.setState({
      selectedAgentId: agentId,
      selectedThreadIdByAgent: new Map([[agentId, threadA]]),
    });
    await useChatStore.getState().cancelRun();

    expect(cancelAgentRun).toHaveBeenCalledWith(agentId, threadA);
    expect(useChatStore.getState().inFlightByAgent.has(keyA)).toBe(false);
    // Thread B's in-flight state must survive — this is the reported bug.
    expect(useChatStore.getState().inFlightByAgent.has(keyB)).toBe(true);
  });

  it("clears the default thread's own in-flight entry (keyed by bare agentId), not a named thread's", async () => {
    const agentId = "agent-4";
    const namedThreadId = "thread-x";
    const defaultKey = inFlightKey(agentId);
    const namedKey = inFlightKey(agentId, namedThreadId);

    useChatStore.getState().ensureInFlight(defaultKey);
    useChatStore.getState().ensureInFlight(namedKey);

    useChatStore.setState({ selectedAgentId: agentId, selectedThreadIdByAgent: new Map() });
    await useChatStore.getState().cancelRun();

    expect(cancelAgentRun).toHaveBeenCalledWith(agentId, undefined);
    expect(useChatStore.getState().inFlightByAgent.has(defaultKey)).toBe(false);
    expect(useChatStore.getState().inFlightByAgent.has(namedKey)).toBe(true);
  });
});
