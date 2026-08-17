// @vitest-environment jsdom
//
// Regression guard for the "minimize pending form to input bar" invariant: a
// form must ALWAYS arrive expanded, never inheriting a stale minimized flag
// left on the same (agent, thread) slot by a previous form. `setPendingForm`
// (chatStore.ts) already enforces this for the sync-form path; `form_posted`
// upserts the async-form entry into `agents` state directly via an inline
// `useChatStore.setState` call in useSSE.ts, bypassing that store action, so
// it needs the same `minimizedFormByKey` cleanup inlined into its own
// `setState` update.
//
// Events are injected through the real `useSSE` hook via the SSE hub's
// `__dispatchForTest` seam (same approach as `useSSE.artifactWrite.test.ts`),
// so this exercises the production listener body end-to-end rather than
// re-implementing its logic in the test.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { useChatStore, isFormMinimized } from "../../stores/chatStore";
import { useSSE } from "../useSSE";
import { __dispatchForTest } from "../../lib/sseHub";
import type { AgentSnapshot } from "../../types/api";

vi.mock("../sseUtils", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../sseUtils")>();
  return {
    ...actual,
    createManagedEventSource: vi.fn(() => ({ close: vi.fn() })),
  };
});

// `form_posted` fires `fetchAgents`/`selectAgent` as a side effect (see the
// handler's own comment) — mocked here purely so those fire-and-forget calls
// don't hit a real network in jsdom; nothing under test reads their results.
vi.mock("../../lib/api", () => ({
  getAgents: vi.fn().mockResolvedValue([]),
  getAgent: vi.fn().mockResolvedValue(null),
  getMessages: vi.fn().mockResolvedValue({ messages: [], cursor: null }),
  systemStreamUrl: vi.fn(() => "http://test/system/stream"),
}));

const AGENT_ID = "form-posted-agent";

function makeSnapshot(overrides: Partial<AgentSnapshot> = {}): AgentSnapshot {
  return {
    agent_id: AGENT_ID,
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

let mountedRoots: Array<{ root: Root; container: HTMLDivElement }> = [];

function mountHarness(agentId: string): void {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  function Harness() {
    useSSE(agentId);
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

function injectFormPosted(
  agentId: string,
  formId: string,
  threadId?: string,
  spec?: Record<string, unknown>
): void {
  act(() => {
    __dispatchForTest({
      agent_id: agentId,
      run_id: "run-1",
      thread_id: threadId ?? null,
      eventName: "form_posted",
      raw: JSON.stringify({
        agent_id: agentId,
        run_id: "run-1",
        thread_id: threadId,
        payload: { type: "form_posted", data: { form_id: formId, spec } },
      }),
    });
  });
}

beforeEach(() => {
  useChatStore.getState().reset();
});

afterEach(() => {
  unmountAllHooks();
});

describe("form_posted → minimizedFormByKey cleanup", () => {
  it("clears a stale minimized flag on the same (agent, default-thread) slot when a new async form posts", () => {
    useChatStore.setState({ agents: [makeSnapshot()] });
    useChatStore.getState().setFormMinimized(AGENT_ID, undefined, true);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, AGENT_ID, undefined)).toBe(true);

    mountHarness(AGENT_ID);
    injectFormPosted(AGENT_ID, "form-new-1");

    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, AGENT_ID, undefined)).toBe(false);
    const agent = useChatStore.getState().agents.find((a) => a.agent_id === AGENT_ID);
    expect(agent?.pending_forms?.some((f) => f.form_id === "form-new-1")).toBe(true);
  });

  it("clears a stale minimized flag scoped to the event's own thread, leaving other threads untouched", () => {
    const threadId = "thread-form-posted";
    useChatStore.setState({ agents: [makeSnapshot()] });
    useChatStore.getState().setFormMinimized(AGENT_ID, threadId, true);
    useChatStore.getState().setFormMinimized(AGENT_ID, undefined, true);

    mountHarness(AGENT_ID);
    injectFormPosted(AGENT_ID, "form-new-2", threadId);

    // Only the thread-scoped slot the event targeted was cleared.
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, AGENT_ID, threadId)).toBe(false);
    // The unrelated default-thread slot for the same agent is untouched.
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, AGENT_ID, undefined)).toBe(true);
  });
});

describe("form_posted → pending_forms[].spec", () => {
  // The optimistic insert used to hardcode `spec: null` and rely on the
  // async render path scanning the transcript to fill it in. Now that the
  // backend's `form_posted` event carries the full spec (see
  // `ao-protocol::event::AgentEventPayload::FormPosted`), the optimistic
  // entry must carry it too, wrapped in the same `{form_id, spec, mode}`
  // envelope (`PendingFormRequestMeta`) the backend's own `pending_forms`
  // snapshot pointer uses.
  it("carries the event's spec into the optimistic pending_forms entry, non-null", () => {
    useChatStore.setState({ agents: [makeSnapshot()] });
    mountHarness(AGENT_ID);

    injectFormPosted(AGENT_ID, "form-with-spec", undefined, {
      form_id: "form-with-spec",
      title: "Rate this",
      intro: "Quick check-in",
      fields: [{ id: "q1", kind: "text", label: "Comments", required: false }],
    });

    const agent = useChatStore.getState().agents.find((a) => a.agent_id === AGENT_ID);
    const pending = agent?.pending_forms?.find((f) => f.form_id === "form-with-spec");
    expect(pending?.spec).not.toBeNull();
    expect(pending?.spec?.mode).toBe("async");
    expect(pending?.spec?.spec.title).toBe("Rate this");
    expect(pending?.spec?.spec.fields).toHaveLength(1);
  });

  it("falls back to a null spec when the event omits it (back-compat with an older server)", () => {
    useChatStore.setState({ agents: [makeSnapshot()] });
    mountHarness(AGENT_ID);

    injectFormPosted(AGENT_ID, "form-no-spec");

    const agent = useChatStore.getState().agents.find((a) => a.agent_id === AGENT_ID);
    const pending = agent?.pending_forms?.find((f) => f.form_id === "form-no-spec");
    expect(pending?.spec).toBeNull();
  });
});
