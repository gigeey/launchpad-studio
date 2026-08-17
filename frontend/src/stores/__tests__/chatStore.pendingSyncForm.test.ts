/**
 * Regression suite: per-thread isolation of the sync-form
 * (`AskUserQuestionWithForm`) pending state.
 *
 * Bug: `pendingFormByAgent` was keyed purely by agent id. Once an agent could
 * run turns on multiple threads concurrently, three symptoms fell out of that
 * single shared slot:
 *  1. The form rendered under whichever thread the user happened to be
 *     viewing, not the thread that actually requested it.
 *  2. An unrelated thread's `run_ended` unconditionally wiped the slot
 *     (`clearPendingForm(id)`), so a still-pending form on another thread
 *     appeared then vanished.
 *  3. `addFormAnswerEntry`'s optimistic bubble was gated only on
 *     `selectedAgentId === agentId`, so answering a background thread's form
 *     dropped a bubble into whichever thread was on screen.
 *
 * Fix: `pendingFormByAgent` is now keyed the same way as `inFlightByAgent`
 * (see `inFlightKey`) — plain agent id for the default thread,
 * `inFlightKey(agentId, threadId)` otherwise. `setPendingForm` composes the
 * key from `form.thread_id`; `clearPendingForm` takes an explicit
 * `threadId` so a run's own explicit-action clears (send/cancel) only ever
 * touch its own thread's slot; `addFormAnswerEntry` gates its optimistic
 * bubble on `isEventForActiveThread`, mirroring `finalizeInFlightText`.
 *
 * A follow-up fix changed what `run_ended` itself does to its own thread's
 * slot: it used to delete it outright, which made a still-unanswered form
 * vanish with no trace the instant its run ended. It now calls
 * `markPendingFormOrphaned` instead, which flags the slot `orphaned: true`
 * in place — see the `run_ended` tests below.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import {
  useChatStore,
  inFlightKey,
  pendingSyncFormForThread,
  hasPendingSyncFormForThread,
  agentHasPendingSyncForm,
} from "../chatStore";
import { useSSE } from "../../hooks/useSSE";
import { __dispatchForTest } from "../../lib/sseHub";
import type { Thread } from "../../types/api";
import type { FormRequestPayload } from "../../types/form";

vi.mock("../../hooks/sseUtils", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../hooks/sseUtils")>();
  return {
    ...actual,
    createManagedEventSource: vi.fn(() => ({ close: vi.fn() })),
  };
});

function chatStore() {
  return useChatStore.getState();
}

function makeForm(overrides: Partial<FormRequestPayload> = {}): FormRequestPayload {
  return {
    form_id: "form-1",
    agent_id: "agent-1",
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
});

describe("pendingFormByAgent — thread-scoped set/clear", () => {
  const AGENT = "agent-sync-form";
  const THREAD_A = "thread-sync-a";
  const THREAD_B = "thread-sync-b";

  it("setPendingForm buckets by (agentId, form.thread_id)", () => {
    chatStore().setPendingForm(AGENT, makeForm({ thread_id: THREAD_A }));
    expect(chatStore().pendingFormByAgent[inFlightKey(AGENT, THREAD_A)]?.form_id).toBe("form-1");
    expect(chatStore().pendingFormByAgent[AGENT]).toBeUndefined();
  });

  it("a form with no thread_id resolves to the default-thread (plain agent id) bucket", () => {
    chatStore().setPendingForm(AGENT, makeForm({ thread_id: undefined }));
    expect(chatStore().pendingFormByAgent[AGENT]?.form_id).toBe("form-1");
  });

  it("two threads of the same agent hold independent pending forms", () => {
    chatStore().setPendingForm(AGENT, makeForm({ form_id: "form-a", thread_id: THREAD_A }));
    chatStore().setPendingForm(AGENT, makeForm({ form_id: "form-b", thread_id: THREAD_B }));

    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.form_id).toBe("form-a");
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_B)?.form_id).toBe("form-b");
  });

  it("clearPendingForm(agentId, threadId) clears only the owning thread's slot", () => {
    chatStore().setPendingForm(AGENT, makeForm({ form_id: "form-a", thread_id: THREAD_A }));
    chatStore().setPendingForm(AGENT, makeForm({ form_id: "form-b", thread_id: THREAD_B }));

    chatStore().clearPendingForm(AGENT, THREAD_A);

    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)).toBeUndefined();
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_B)?.form_id).toBe("form-b");
  });

  it("clearPendingForm(agentId) with no threadId clears the default-thread slot only, matching pre-threads behavior", () => {
    chatStore().setPendingForm(AGENT, makeForm({ form_id: "form-default", thread_id: undefined }));
    chatStore().setPendingForm(AGENT, makeForm({ form_id: "form-a", thread_id: THREAD_A }));

    chatStore().clearPendingForm(AGENT);

    expect(chatStore().pendingFormByAgent[AGENT]).toBeUndefined();
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.form_id).toBe("form-a");
  });

  describe("markPendingFormOrphaned", () => {
    it("flags the owning thread's slot orphaned in place, without deleting it", () => {
      chatStore().setPendingForm(AGENT, makeForm({ form_id: "form-a", thread_id: THREAD_A }));

      chatStore().markPendingFormOrphaned(AGENT, THREAD_A);

      const form = pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A);
      expect(form?.form_id).toBe("form-a");
      expect(form?.orphaned).toBe(true);
    });

    it("does not touch a different thread's slot", () => {
      chatStore().setPendingForm(AGENT, makeForm({ form_id: "form-a", thread_id: THREAD_A }));
      chatStore().setPendingForm(AGENT, makeForm({ form_id: "form-b", thread_id: THREAD_B }));

      chatStore().markPendingFormOrphaned(AGENT, THREAD_A);

      expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_B)?.orphaned).toBeFalsy();
    });

    it("no-ops when the slot is already empty (the common answered-before-run_ended case)", () => {
      chatStore().markPendingFormOrphaned(AGENT, THREAD_A);
      expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)).toBeUndefined();
    });

    it("is idempotent against an already-orphaned slot", () => {
      chatStore().setPendingForm(AGENT, makeForm({ form_id: "form-a", thread_id: THREAD_A }));
      chatStore().markPendingFormOrphaned(AGENT, THREAD_A);
      chatStore().markPendingFormOrphaned(AGENT, THREAD_A);

      const form = pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A);
      expect(form?.form_id).toBe("form-a");
      expect(form?.orphaned).toBe(true);
    });
  });
});

describe("pendingSyncFormForThread / hasPendingSyncFormForThread / agentHasPendingSyncForm", () => {
  const AGENT = "agent-selectors";
  const OTHER_AGENT = "agent-other";
  const THREAD_A = "thread-sel-a";

  it("hasPendingSyncFormForThread reflects presence/absence per thread", () => {
    chatStore().setPendingForm(AGENT, makeForm({ thread_id: THREAD_A }));
    expect(hasPendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)).toBe(true);
    expect(hasPendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, undefined)).toBe(false);
  });

  it("agentHasPendingSyncForm is true if ANY thread of the agent has a pending form", () => {
    expect(agentHasPendingSyncForm(chatStore().pendingFormByAgent, AGENT)).toBe(false);
    chatStore().setPendingForm(AGENT, makeForm({ thread_id: THREAD_A }));
    expect(agentHasPendingSyncForm(chatStore().pendingFormByAgent, AGENT)).toBe(true);
    // Does not leak into an unrelated agent.
    expect(agentHasPendingSyncForm(chatStore().pendingFormByAgent, OTHER_AGENT)).toBe(false);
  });

  it("agentHasPendingSyncForm goes false again once the only pending thread clears", () => {
    chatStore().setPendingForm(AGENT, makeForm({ thread_id: THREAD_A }));
    chatStore().clearPendingForm(AGENT, THREAD_A);
    expect(agentHasPendingSyncForm(chatStore().pendingFormByAgent, AGENT)).toBe(false);
  });
});

describe("addFormAnswerEntry — thread-scoped optimistic bubble", () => {
  const AGENT = "agent-answer";
  const THREAD_A = "thread-answer-a";
  const THREAD_B = "thread-answer-b";
  const DEFAULT_THREAD_ID = `default-${AGENT}`;
  const threads: Thread[] = [
    makeThread(DEFAULT_THREAD_ID, AGENT, "default"),
    makeThread(THREAD_A, AGENT, "fresh"),
    makeThread(THREAD_B, AGENT, "fresh"),
  ];

  it("appends the optimistic bubble when the form's thread matches the one currently viewed", () => {
    useChatStore.setState({
      selectedAgentId: AGENT,
      threadsByAgent: new Map([[AGENT, threads]]),
      selectedThreadIdByAgent: new Map([[AGENT, THREAD_A]]),
      messages: [],
      allMessages: [],
    });

    chatStore().addFormAnswerEntry(AGENT, {
      form: makeForm({ thread_id: THREAD_A }),
      answers: {},
    });

    expect(chatStore().messages).toHaveLength(1);
    expect(chatStore().messages[0].event_type).toBe("form_answer");
  });

  it("does NOT append the optimistic bubble into the viewed thread when the form belongs to a different thread", () => {
    useChatStore.setState({
      selectedAgentId: AGENT,
      // User is viewing thread A...
      threadsByAgent: new Map([[AGENT, threads]]),
      selectedThreadIdByAgent: new Map([[AGENT, THREAD_A]]),
      messages: [],
      allMessages: [],
    });

    // ...but the form being answered belongs to thread B.
    chatStore().addFormAnswerEntry(AGENT, {
      form: makeForm({ thread_id: THREAD_B }),
      answers: {},
    });

    expect(chatStore().messages).toEqual([]);
    expect(chatStore().allMessages).toEqual([]);
  });

  it("back-compat: a form with no thread_id appends when the default thread is active", () => {
    useChatStore.setState({
      selectedAgentId: AGENT,
      threadsByAgent: new Map([[AGENT, threads]]),
      selectedThreadIdByAgent: new Map([[AGENT, DEFAULT_THREAD_ID]]),
      messages: [],
      allMessages: [],
    });

    chatStore().addFormAnswerEntry(AGENT, {
      form: makeForm({ thread_id: undefined }),
      answers: {},
    });

    expect(chatStore().messages).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// Integration: real useSSE handlers, driven through the SSE hub's test seam
// (mirrors hooks/__tests__/useSSE.terminalArtifact.test.ts).
// ---------------------------------------------------------------------------

let mountedRoots: Array<{ root: Root; container: HTMLDivElement }> = [];

function mountHook(useHook: () => unknown): void {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  function Harness() {
    useHook();
    return null;
  }
  act(() => {
    root.render(React.createElement(Harness));
  });
  mountedRoots.push({ root, container });
}

function unmountAllHooks(): void {
  act(() => {
    for (const { root } of mountedRoots) root.unmount();
  });
  for (const { container } of mountedRoots) document.body.removeChild(container);
  mountedRoots = [];
}

function rawEvent(
  agentId: string,
  eventName: string,
  data: Record<string, unknown> = {},
  threadId?: string,
): string {
  return JSON.stringify({
    agent_id: agentId,
    run_id: "run-1",
    ...(threadId ? { thread_id: threadId } : {}),
    payload: { type: eventName, data },
  });
}

function inject(agentId: string, eventName: string, data: Record<string, unknown> = {}, threadId?: string): void {
  act(() => {
    __dispatchForTest({
      agent_id: agentId,
      run_id: "run-1",
      thread_id: threadId ?? null,
      eventName,
      raw: rawEvent(agentId, eventName, data, threadId),
    });
  });
}

describe("useSSE integration — form_request / run_ended across threads", () => {
  const AGENT = "agent-sse-form";
  const THREAD_A = "thread-sse-a";
  const THREAD_B = "thread-sse-b";

  afterEach(() => {
    unmountAllHooks();
  });

  it("thread A's pending form survives thread B's run_ended (appear-then-vanish regression)", () => {
    mountHook(() => useSSE(AGENT));

    inject(AGENT, "form_request", {
      form_id: "form-a",
      agent_id: AGENT,
      session_id: "s-1",
      title: "Question on A",
      fields: [],
    }, THREAD_A);

    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.form_id).toBe("form-a");

    // An unrelated run on thread B ends — must not touch thread A's form.
    inject(AGENT, "run_ended", { reason: "Completed" }, THREAD_B);

    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.form_id).toBe("form-a");
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.orphaned).toBeFalsy();
  });

  it("a run_ended for a completely different agent leaves this agent's pending form untouched", () => {
    const OTHER_AGENT = "agent-sse-form-other";
    mountHook(() => useSSE(AGENT));
    mountHook(() => useSSE(OTHER_AGENT));

    inject(AGENT, "form_request", {
      form_id: "form-a",
      agent_id: AGENT,
      session_id: "s-1",
      title: "Question on A",
      fields: [],
    }, THREAD_A);

    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.form_id).toBe("form-a");

    // A run ending for an entirely different agent (even on a same-named
    // thread id) must not touch this agent's pending form at all.
    inject(OTHER_AGENT, "run_ended", { reason: "Completed" }, THREAD_A);

    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.form_id).toBe("form-a");
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.orphaned).toBeFalsy();
  });

  it("run_ended on thread A marks only thread A's form orphaned, leaving thread B's untouched", () => {
    mountHook(() => useSSE(AGENT));

    inject(AGENT, "form_request", {
      form_id: "form-a",
      agent_id: AGENT,
      session_id: "s-1",
      title: "Question on A",
      fields: [],
    }, THREAD_A);
    inject(AGENT, "form_request", {
      form_id: "form-b",
      agent_id: AGENT,
      session_id: "s-1",
      title: "Question on B",
      fields: [],
    }, THREAD_B);

    inject(AGENT, "run_ended", { reason: "Completed" }, THREAD_A);

    // Thread A's form is still present — its OWNING run ended while it was
    // still unanswered, which is the orphaned case (see `markPendingFormOrphaned`
    // in chatStore.ts). It must not silently vanish.
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.form_id).toBe("form-a");
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.orphaned).toBe(true);
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_B)?.form_id).toBe("form-b");
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_B)?.orphaned).toBeFalsy();
  });

  it("form_request on thread B while thread A's form is pending keeps both isolated", () => {
    mountHook(() => useSSE(AGENT));

    inject(AGENT, "form_request", {
      form_id: "form-a",
      agent_id: AGENT,
      session_id: "s-1",
      title: "Question on A",
      fields: [],
    }, THREAD_A);
    inject(AGENT, "form_request", {
      form_id: "form-b",
      agent_id: AGENT,
      session_id: "s-1",
      title: "Question on B",
      fields: [],
    }, THREAD_B);

    // Neither collapses into the default (undefined-thread) bucket.
    expect(chatStore().pendingFormByAgent[AGENT]).toBeUndefined();
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.form_id).toBe("form-a");
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_B)?.form_id).toBe("form-b");
  });

  // Regression coverage for the default/main thread specifically — every
  // other case above uses two non-default threads, which never exercises
  // the `thread_id` omitted-on-the-wire (`undefined`) case a real default-
  // thread form arrives as. `pendingSyncFormForThread(..., undefined)` is
  // exactly what ChatView's `streamingThreadId`-scoped read resolves to when
  // the operator is viewing the default thread (see `resolveStreamingThreadId`).
  it("form fired on the default thread (no thread_id on the wire) resolves via the undefined-threadId bucket", () => {
    mountHook(() => useSSE(AGENT));

    inject(AGENT, "form_request", {
      form_id: "form-default",
      agent_id: AGENT,
      session_id: "s-1",
      title: "Question on default thread",
      fields: [],
    } /* no threadId => omitted from the wire, matching the backend's default-thread convention */);

    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, undefined)?.form_id).toBe("form-default");
    // Bare agentId is the literal bucket key for the default thread.
    expect(chatStore().pendingFormByAgent[AGENT]?.form_id).toBe("form-default");
  });

  it("a default-thread form and a non-default thread's form stay isolated from each other", () => {
    mountHook(() => useSSE(AGENT));

    inject(AGENT, "form_request", {
      form_id: "form-default",
      agent_id: AGENT,
      session_id: "s-1",
      title: "Question on default thread",
      fields: [],
    });
    inject(AGENT, "form_request", {
      form_id: "form-a",
      agent_id: AGENT,
      session_id: "s-1",
      title: "Question on A",
      fields: [],
    }, THREAD_A);

    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, undefined)?.form_id).toBe("form-default");
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.form_id).toBe("form-a");

    // run_ended on the non-default thread must not wipe the default thread's
    // form, and must mark its OWN thread's form orphaned in place rather
    // than deleting it (that form's owning run ended while still unanswered).
    inject(AGENT, "run_ended", { reason: "Completed" }, THREAD_A);
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, undefined)?.form_id).toBe("form-default");
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, undefined)?.orphaned).toBeFalsy();
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.form_id).toBe("form-a");
    expect(pendingSyncFormForThread(chatStore().pendingFormByAgent, AGENT, THREAD_A)?.orphaned).toBe(true);
  });
});
