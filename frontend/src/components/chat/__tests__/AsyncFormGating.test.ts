// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { useChatStore, pendingFormForThread, isFormMinimized } from "../../../stores/chatStore";
import { submitAsyncFormAnswer, dismissAsyncForm } from "../../../lib/api";
import type { AgentSnapshot, PendingForm } from "../../../types/api";
import type { FormRequestPayload } from "../../../types/form";

// ---------------------------------------------------------------------------
// chatStore — pending_forms gating via pendingFormForThread / clearPendingAsyncForm
// ---------------------------------------------------------------------------

function makeSnapshot(overrides: Partial<AgentSnapshot> = {}): AgentSnapshot {
  return {
    agent_id: "a1",
    name: "Test Agent",
    message_count: 0,
    has_active_run: false,
    queue_depth: 0,
    thread_id: null,
    created_at: "2025-01-01T00:00:00Z",
    last_activity_at: null,
    ...overrides,
  };
}

function makeForm(overrides: Partial<PendingForm> = {}): PendingForm {
  return {
    thread_id: null,
    form_id: "form-123",
    spec: null,
    ...overrides,
  };
}

describe("Async form gating — chatStore", () => {
  beforeEach(() => {
    useChatStore.getState().reset();
  });

  it("reads the default-thread pending form from agent snapshot", () => {
    useChatStore.setState({ agents: [makeSnapshot({ pending_forms: [makeForm({ form_id: "form-123" })] })] });
    const state = useChatStore.getState();
    const agent = state.agents.find((a) => a.agent_id === "a1");
    expect(pendingFormForThread(agent?.pending_forms, undefined)?.form_id).toBe("form-123");
  });

  it("gating is active when the active thread's pending form matches form_id", () => {
    const formId = "form-abc";
    useChatStore.setState({ agents: [makeSnapshot({ pending_forms: [makeForm({ form_id: formId })] })] });
    const agent = useChatStore.getState().agents.find((a) => a.agent_id === "a1");
    const isPending = pendingFormForThread(agent?.pending_forms, undefined)?.form_id === formId;
    expect(isPending).toBe(true);
  });

  it("gating is inactive when pending_forms is empty", () => {
    useChatStore.setState({ agents: [makeSnapshot({ pending_forms: [] })] });
    const agent = useChatStore.getState().agents.find((a) => a.agent_id === "a1");
    expect(pendingFormForThread(agent?.pending_forms, undefined)).toBeUndefined();
  });

  // Regression guard for the thread-scoping bug: a form pending on a
  // *different* thread of the same agent must never gate the composer (or
  // render) for the thread actually on screen.
  it("gating is inactive for a form pending on a different thread", () => {
    useChatStore.setState({
      agents: [makeSnapshot({ pending_forms: [makeForm({ thread_id: "thread-b", form_id: "form-b" })] })],
    });
    const agent = useChatStore.getState().agents.find((a) => a.agent_id === "a1");
    // Viewing the default thread (undefined) while the form is on thread-b.
    expect(pendingFormForThread(agent?.pending_forms, undefined)).toBeUndefined();
    // Switching to thread-b resolves it.
    expect(pendingFormForThread(agent?.pending_forms, "thread-b")?.form_id).toBe("form-b");
  });

  // Regression guard: `pending_forms` also carries sync
  // (`AskUserQuestionWithForm` mode="sync") entries, persisted purely for
  // `pendingFormByAgent` rehydration — see `hydratePendingSyncFormsFromAgents`.
  // A sync entry must never be mistaken for an async one here: doing so would
  // offer it through the async answer/dismiss UI, which POSTs to
  // `async-forms/.../answer` and queues a NEW agent turn instead of resolving
  // the parked tool call the sync form is actually blocking on.
  it("a sync-mode pending_forms entry is invisible to pendingFormForThread", () => {
    useChatStore.setState({
      agents: [
        makeSnapshot({
          pending_forms: [
            makeForm({
              form_id: "sync-form-1",
              spec: { form_id: "sync-form-1", mode: "sync", spec: { form_id: "sync-form-1", title: "Q", fields: [] } },
            }),
          ],
        }),
      ],
    });
    const agent = useChatStore.getState().agents.find((a) => a.agent_id === "a1");
    expect(pendingFormForThread(agent?.pending_forms, undefined)).toBeUndefined();
  });

  it("an async-mode pending_forms entry on the same thread is still found alongside an unrelated sync one", () => {
    useChatStore.setState({
      agents: [
        makeSnapshot({
          pending_forms: [
            makeForm({
              thread_id: "thread-b",
              form_id: "sync-form-1",
              spec: { form_id: "sync-form-1", mode: "sync", spec: { form_id: "sync-form-1", title: "Q", fields: [] } },
            }),
            makeForm({
              form_id: "async-form-1",
              spec: { form_id: "async-form-1", mode: "async", spec: { form_id: "async-form-1", title: "Q2", fields: [] } },
            }),
          ],
        }),
      ],
    });
    const agent = useChatStore.getState().agents.find((a) => a.agent_id === "a1");
    expect(pendingFormForThread(agent?.pending_forms, undefined)?.form_id).toBe("async-form-1");
    expect(pendingFormForThread(agent?.pending_forms, "thread-b")).toBeUndefined();
  });

  it("clearPendingAsyncForm clears the matching form (ChatInput restored after submit/dismiss)", () => {
    useChatStore.setState({ agents: [makeSnapshot({ pending_forms: [makeForm({ form_id: "form-xyz" })] })] });
    useChatStore.getState().clearPendingAsyncForm("a1", "form-xyz");
    const agent = useChatStore.getState().agents.find((a) => a.agent_id === "a1");
    expect(agent?.pending_forms).toEqual([]);
  });

  it("clearPendingAsyncForm does not disturb a sibling thread's pending form on the same agent", () => {
    useChatStore.setState({
      agents: [
        makeSnapshot({
          pending_forms: [makeForm({ form_id: "form-default" }), makeForm({ thread_id: "thread-b", form_id: "form-b" })],
        }),
      ],
    });
    useChatStore.getState().clearPendingAsyncForm("a1", "form-default");
    const agent = useChatStore.getState().agents.find((a) => a.agent_id === "a1");
    expect(agent?.pending_forms).toEqual([makeForm({ thread_id: "thread-b", form_id: "form-b" })]);
  });

  it("clearPendingAsyncForm does not disturb other agents", () => {
    useChatStore.setState({
      agents: [
        makeSnapshot({ agent_id: "a1", pending_forms: [makeForm({ form_id: "form-1" })] }),
        makeSnapshot({ agent_id: "a2", pending_forms: [makeForm({ form_id: "form-2" })] }),
      ],
    });
    useChatStore.getState().clearPendingAsyncForm("a1", "form-1");
    const a1 = useChatStore.getState().agents.find((a) => a.agent_id === "a1");
    const a2 = useChatStore.getState().agents.find((a) => a.agent_id === "a2");
    expect(a1?.pending_forms).toEqual([]);
    expect(pendingFormForThread(a2?.pending_forms, undefined)?.form_id).toBe("form-2");
  });
});

// ---------------------------------------------------------------------------
// chatStore — minimizedFormByKey (setFormMinimized / isFormMinimized)
// ---------------------------------------------------------------------------

function makeSyncForm(overrides: Partial<FormRequestPayload> = {}): FormRequestPayload {
  return {
    form_id: "sync-form-1",
    agent_id: "a1",
    session_id: "session-1",
    title: "Pick one",
    fields: [],
    ...overrides,
  };
}

describe("minimizedFormByKey — chatStore", () => {
  beforeEach(() => {
    useChatStore.getState().reset();
  });

  it("setFormMinimized(true) then isFormMinimized reads true; setFormMinimized(false) rounds back to false", () => {
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", undefined)).toBe(false);
    useChatStore.getState().setFormMinimized("a1", undefined, true);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", undefined)).toBe(true);
    useChatStore.getState().setFormMinimized("a1", undefined, false);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", undefined)).toBe(false);
    // Sparse map: unminimizing deletes the key rather than storing `false`.
    expect(useChatStore.getState().minimizedFormByKey).toEqual({});
  });

  it("default-thread and named-thread keys for the same agent do not collide", () => {
    useChatStore.getState().setFormMinimized("a1", undefined, true);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", undefined)).toBe(true);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", "thread-b")).toBe(false);

    useChatStore.getState().setFormMinimized("a1", "thread-b", true);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", undefined)).toBe(true);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", "thread-b")).toBe(true);

    // Unminimizing one thread's slot leaves the other untouched.
    useChatStore.getState().setFormMinimized("a1", undefined, false);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", undefined)).toBe(false);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", "thread-b")).toBe(true);
  });

  it("clearPendingForm wipes the minimized flag for that (agent, thread) slot", () => {
    useChatStore.getState().setPendingForm("a1", makeSyncForm());
    useChatStore.getState().setFormMinimized("a1", undefined, true);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", undefined)).toBe(true);

    useChatStore.getState().clearPendingForm("a1", undefined);

    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", undefined)).toBe(false);
    expect(useChatStore.getState().minimizedFormByKey).toEqual({});
  });

  it("markPendingFormOrphaned force-expands: wipes the minimized flag so the orphaned state is visible", () => {
    useChatStore.getState().setPendingForm("a1", makeSyncForm({ thread_id: "thread-b" }));
    useChatStore.getState().setFormMinimized("a1", "thread-b", true);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", "thread-b")).toBe(true);

    useChatStore.getState().markPendingFormOrphaned("a1", "thread-b");

    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", "thread-b")).toBe(false);
    // The form itself is still there, now flagged orphaned — only the
    // minimized flag was cleared.
    expect(useChatStore.getState().pendingFormByAgent["a1::thread:thread-b"]?.orphaned).toBe(true);
  });

  // Arrives-expanded invariant: `inFlightKey(agentId, threadId)` is stable
  // for the thread's whole lifetime, not per-form, so a NEW form landing on
  // a slot that still carries a stale minimized flag (a still-pending form
  // replaced without an intervening `clearPendingForm`, e.g. a
  // reconnect/replay/orphan-recovery re-set) must not inherit it.
  it("setPendingForm wipes a stale minimized flag so a new form on the same slot arrives expanded", () => {
    useChatStore.getState().setPendingForm("a1", makeSyncForm());
    useChatStore.getState().setFormMinimized("a1", undefined, true);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", undefined)).toBe(true);

    // A new form posted to the same (agent, thread) slot without an
    // intervening clear — e.g. the agent asks another question right away.
    useChatStore.getState().setPendingForm("a1", makeSyncForm({ form_id: "sync-form-2" }));

    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "a1", undefined)).toBe(false);
    expect(useChatStore.getState().minimizedFormByKey).toEqual({});
  });
});

// ---------------------------------------------------------------------------
// API — submitAsyncFormAnswer
// ---------------------------------------------------------------------------

describe("submitAsyncFormAnswer", () => {
  let origFetch: typeof globalThis.fetch;

  beforeEach(() => {
    origFetch = globalThis.fetch;
  });

  afterEach(() => {
    globalThis.fetch = origFetch;
  });

  it("POSTs to /agents/:id/async-forms/:form_id/answer with { values }", async () => {
    let capturedUrl = "";
    let capturedBody: unknown = null;
    globalThis.fetch = vi.fn(async (url: string | URL | Request, init?: RequestInit) => {
      capturedUrl = url instanceof Request ? url.url : String(url);
      capturedBody = JSON.parse(init?.body as string);
      return new Response(JSON.stringify({ message_id: "m1", status: "queued" }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    });

    const values = { name: { kind: "text", value: "Alice" } };
    const result = await submitAsyncFormAnswer("agent-1", "form-abc", values);

    expect(capturedUrl).toContain("/agents/agent-1/async-forms/form-abc/answer");
    expect(capturedBody).toEqual({ values });
    expect(result.message_id).toBe("m1");
    expect(result.status).toBe("queued");
  });

  it("uses POST method with Content-Type: application/json", async () => {
    let capturedMethod = "";
    let capturedContentType = "";
    globalThis.fetch = vi.fn(async (_url: string | URL | Request, init?: RequestInit) => {
      capturedMethod = init?.method ?? "";
      capturedContentType = (init?.headers as Record<string, string>)?.["Content-Type"] ?? "";
      return new Response(JSON.stringify({ message_id: "m2", status: "queued" }), { status: 200 });
    });

    await submitAsyncFormAnswer("agent-1", "form-abc", {});

    expect(capturedMethod).toBe("POST");
    expect(capturedContentType).toBe("application/json");
  });

  it("throws on non-2xx response", async () => {
    globalThis.fetch = vi.fn(async () =>
      new Response(JSON.stringify({ error: "form_id '...' is not the current pending form" }), { status: 400 }),
    );

    await expect(submitAsyncFormAnswer("agent-1", "stale-form", {})).rejects.toThrow("API 400");
  });
});

// ---------------------------------------------------------------------------
// API — dismissAsyncForm
// ---------------------------------------------------------------------------

describe("dismissAsyncForm", () => {
  let origFetch: typeof globalThis.fetch;

  beforeEach(() => {
    origFetch = globalThis.fetch;
  });

  afterEach(() => {
    globalThis.fetch = origFetch;
  });

  it("POSTs to /agents/:id/async-forms/:form_id/dismiss", async () => {
    let capturedUrl = "";
    let capturedMethod = "";
    globalThis.fetch = vi.fn(async (url: string | URL | Request, init?: RequestInit) => {
      capturedUrl = url instanceof Request ? url.url : String(url);
      capturedMethod = init?.method ?? "";
      return new Response(null, { status: 200 });
    });

    await dismissAsyncForm("agent-1", "form-abc");

    expect(capturedUrl).toContain("/agents/agent-1/async-forms/form-abc/dismiss");
    expect(capturedMethod).toBe("POST");
  });

  it("resolves successfully on 200", async () => {
    globalThis.fetch = vi.fn(async () => new Response(null, { status: 200 }));
    await expect(dismissAsyncForm("agent-1", "form-abc")).resolves.toBeUndefined();
  });

  it("throws on non-2xx response", async () => {
    globalThis.fetch = vi.fn(async () =>
      new Response("not found", { status: 404 }),
    );

    await expect(dismissAsyncForm("agent-1", "bad-form")).rejects.toThrow("API 404");
  });
});
