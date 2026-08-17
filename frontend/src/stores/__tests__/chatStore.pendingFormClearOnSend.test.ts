/**
 * Regression test: the `?` question badge (`ThreadTabStrip`/`HomeSidebar`,
 * via `resolveThreadActivity`) got stuck forever once a sync
 * `AskUserQuestionWithForm` form's question was superseded any way other
 * than the operator answering it through the form overlay (which calls
 * `clearPendingForm` directly) or the run ending (which `useSSE`'s
 * `run_ended` handler already clears via `clearPendingForm`).
 *
 * The common miss: the operator ignores the form and just types a new
 * message instead. Nothing cleared `pendingFormByAgent` for that thread, so
 * the badge kept rendering indefinitely even though the operator had clearly
 * moved on.
 *
 * Fix: `sendMessage` now clears the target thread's pending sync-form slot
 * up front, before posting. Scoped to that one (agentId, threadId) key only —
 * a still-pending form on a *different* thread of the same agent (sync or
 * async) must survive, same guarantee `run_ended`'s clear already gives.
 *
 * These drive the store directly (no DOM harness), same pattern as
 * `chatStore.optimisticTyping.test.ts`.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";

const AGENT_ID = "agent-clear-on-send";
const THREAD_A = "thread-clear-a";
const THREAD_B = "thread-clear-b";

const mockSendMessage = vi.fn();
const mockGetAgents = vi.fn();

vi.mock("../../lib/api", () => ({
  getAgents: (...args: unknown[]) => mockGetAgents(...args),
  getAgent: vi.fn().mockResolvedValue(null),
  getMessages: vi.fn().mockResolvedValue({ messages: [], cursor: null }),
  listThreads: vi.fn().mockResolvedValue([]),
  sendMessage: (...args: unknown[]) => mockSendMessage(...args),
}));

import { useChatStore, pendingSyncFormForThread, hasPendingSyncFormForThread } from "../chatStore";
import type { FormRequestPayload } from "../../types/form";
import type { AgentSnapshot, PendingForm, Thread } from "../../types/api";

function store() {
  return useChatStore.getState();
}

function makeForm(overrides: Partial<FormRequestPayload> = {}): FormRequestPayload {
  return {
    form_id: "form-1",
    agent_id: AGENT_ID,
    session_id: "session-1",
    title: "Pick one",
    fields: [],
    ...overrides,
  };
}

function makeThread(id: string, agentId: string, kind: Thread["kind"]): Thread {
  return {
    id,
    title: null,
    scope: { type: "AgentChat", agent_id: agentId },
    transcript_path: "",
    kind,
    created_at: "",
    updated_at: "",
  };
}

beforeEach(() => {
  useChatStore.getState().reset();
  useChatStore.setState({ agents: [] });
  vi.clearAllMocks();
  mockGetAgents.mockResolvedValue([]);
  mockSendMessage.mockResolvedValue({ message_id: "msg-1", status: "queued" });
  useChatStore.setState({ selectedAgentId: AGENT_ID });
});

describe("sendMessage clears the target thread's pending sync form", () => {
  it("clears a pending form on the default thread when sending to the default thread", async () => {
    store().setPendingForm(AGENT_ID, makeForm({ thread_id: undefined }));
    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined)?.form_id).toBe("form-1");

    await store().sendMessage("never mind, let's do something else");

    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined)).toBeUndefined();
  });

  it("clears the badge on the default thread once `loadThreads` has resolved and `selectedThreadIdByAgent` holds the default thread's REAL backend id (not the pre-load `default-{agentId}` sentinel) — the live-app state for virtually the entire time a user has Main thread open", async () => {
    const DEFAULT_REAL_ID = "real-default-thread-uuid-123";
    useChatStore.setState((s) => {
      const nextThreads = new Map(s.threadsByAgent);
      nextThreads.set(AGENT_ID, [makeThread(DEFAULT_REAL_ID, AGENT_ID, "default")]);
      const nextSelected = new Map(s.selectedThreadIdByAgent);
      nextSelected.set(AGENT_ID, DEFAULT_REAL_ID);
      return { threadsByAgent: nextThreads, selectedThreadIdByAgent: nextSelected };
    });

    // The backend never tags default-thread SSE events with a thread_id (see
    // `useSSE.ts`'s `parsePayloadData`), so the real `form_request` handler
    // always calls `setPendingForm` with `thread_id: undefined` here — this
    // is the actual runtime key `useSSE.ts` uses, not a convenient shortcut.
    store().setPendingForm(AGENT_ID, makeForm({ thread_id: undefined }));
    expect(hasPendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined)).toBe(true);

    await store().sendMessage("never mind, let's do something else");

    expect(hasPendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined)).toBe(false);
  });

  it("clears a pending form on a non-default thread when sending to that thread", async () => {
    useChatStore.setState((s) => {
      const next = new Map(s.selectedThreadIdByAgent);
      next.set(AGENT_ID, THREAD_A);
      return { selectedThreadIdByAgent: next };
    });
    store().setPendingForm(AGENT_ID, makeForm({ thread_id: THREAD_A }));
    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, THREAD_A)?.form_id).toBe("form-1");

    await store().sendMessage("I'll just tell you directly");

    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, THREAD_A)).toBeUndefined();
  });

  it("GUARD: does not clear a still-pending sync form on a different thread of the same agent", async () => {
    useChatStore.setState((s) => {
      const next = new Map(s.selectedThreadIdByAgent);
      next.set(AGENT_ID, THREAD_A);
      return { selectedThreadIdByAgent: next };
    });
    store().setPendingForm(AGENT_ID, makeForm({ form_id: "form-a", thread_id: THREAD_A }));
    store().setPendingForm(AGENT_ID, makeForm({ form_id: "form-b", thread_id: THREAD_B }));

    await store().sendMessage("answering thread A's question inline");

    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, THREAD_A)).toBeUndefined();
    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, THREAD_B)?.form_id).toBe("form-b");
  });

  it("GUARD: does not clear a legitimately-pending async form on a different thread", async () => {
    const asyncForm: PendingForm = {
      thread_id: THREAD_B,
      form_id: "async-form-1",
      spec: {
        form_id: "async-form-1",
        mode: "async",
        spec: { form_id: "async-form-1", title: "Still running in the background", fields: [] },
      },
    };
    const snapshot: AgentSnapshot = {
      agent_id: AGENT_ID,
      name: "Agent",
      last_activity_at: null,
      message_count: 0,
      has_active_run: false,
      queue_depth: 0,
      thread_id: null,
      created_at: "2026-01-01T00:00:00Z",
      pending_forms: [asyncForm],
    };
    useChatStore.setState({ agents: [snapshot] });
    mockGetAgents.mockResolvedValue([snapshot]);

    useChatStore.setState((s) => {
      const next = new Map(s.selectedThreadIdByAgent);
      next.set(AGENT_ID, THREAD_A);
      return { selectedThreadIdByAgent: next };
    });

    await store().sendMessage("moving on with thread A");

    // Thread B's async pending form (an unrelated, still-open question) must
    // survive a message sent to thread A.
    const refreshedAgent = store().agents.find((a) => a.agent_id === AGENT_ID);
    expect(refreshedAgent?.pending_forms?.some((f) => f.form_id === "async-form-1")).toBe(true);
  });

  it("clearing the default thread's form leaves a non-default thread's form untouched", async () => {
    store().setPendingForm(AGENT_ID, makeForm({ form_id: "form-default", thread_id: undefined }));
    store().setPendingForm(AGENT_ID, makeForm({ form_id: "form-a", thread_id: THREAD_A }));

    // selectedThreadIdByAgent left unset => sends to the default thread.
    await store().sendMessage("done with the default thread's question");

    expect(store().pendingFormByAgent[AGENT_ID]).toBeUndefined();
    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, THREAD_A)?.form_id).toBe("form-a");
  });
});
