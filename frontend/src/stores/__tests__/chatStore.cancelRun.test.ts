/**
 * `cancelRun` clearing the pending SYNC form for the stopped thread.
 *
 * Split out of a sibling test file that otherwise pinned the backend
 * "is this form still the latest thing in its thread" flag's old
 * staleness-gating behavior — removed once the composer gate stopped
 * auto-releasing a pending form on skip (it now blocks until the form is
 * explicitly answered/dismissed/superseded). This coverage is unrelated to
 * that removal and stays live.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";

const AGENT_ID = "agent-latest-in-thread";
const THREAD_A = "thread-latest-a";

const mockCancelAgentRun = vi.fn();
const mockGetAgents = vi.fn();

vi.mock("../../lib/api", () => ({
  getAgents: (...args: unknown[]) => mockGetAgents(...args),
  getAgent: vi.fn().mockResolvedValue(null),
  getMessages: vi.fn().mockResolvedValue({ messages: [], cursor: null }),
  listThreads: vi.fn().mockResolvedValue([]),
  cancelAgentRun: (...args: unknown[]) => mockCancelAgentRun(...args),
}));

import { useChatStore, pendingSyncFormForThread } from "../chatStore";
import type { FormRequestPayload } from "../../types/form";

function store() {
  return useChatStore.getState();
}

function makeSyncForm(overrides: Partial<FormRequestPayload> = {}): FormRequestPayload {
  return {
    form_id: "form-1",
    agent_id: AGENT_ID,
    session_id: "session-1",
    title: "Pick one",
    fields: [],
    ...overrides,
  };
}

beforeEach(() => {
  useChatStore.getState().reset();
  vi.clearAllMocks();
  mockGetAgents.mockResolvedValue([]);
  mockCancelAgentRun.mockResolvedValue(undefined);
});

describe("cancelRun clears the pending sync form for the stopped thread", () => {
  it("clears the default thread's pending sync form on stop, without any run_ended SSE event", async () => {
    useChatStore.setState({ selectedAgentId: AGENT_ID });
    store().setPendingForm(AGENT_ID, makeSyncForm({ thread_id: undefined }));
    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined)?.form_id).toBe("form-1");

    await store().cancelRun();

    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined)).toBeUndefined();
    expect(mockCancelAgentRun).toHaveBeenCalledWith(AGENT_ID, undefined);
  });

  it("clears a non-default thread's pending sync form on stop, scoped to that thread only", async () => {
    useChatStore.setState((s) => {
      const next = new Map(s.selectedThreadIdByAgent);
      next.set(AGENT_ID, THREAD_A);
      return { selectedAgentId: AGENT_ID, selectedThreadIdByAgent: next };
    });
    store().setPendingForm(AGENT_ID, makeSyncForm({ form_id: "form-a", thread_id: THREAD_A }));
    store().setPendingForm(AGENT_ID, makeSyncForm({ form_id: "form-b", thread_id: "thread-other" }));

    await store().cancelRun();

    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, THREAD_A)).toBeUndefined();
    // A different thread's still-pending form must survive this stop.
    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, "thread-other")?.form_id).toBe("form-b");
  });
});
