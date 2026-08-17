/**
 * Verifies that project SSE pill handlers (todo_list.complete, delegate.complete,
 * memory_saved) append the correct system entry to the project transcript store.
 *
 * Events are injected through the real `useProjectSSE` hook via the SSE hub's
 * `__dispatchForTest` seam (see `frontend/src/lib/sseHub.ts`), so these tests
 * exercise the production listener bodies instead of a hand-written duplicate
 * of them.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { useProjectStore } from "../projectStore";
import { useChatStore } from "../chatStore";
import { useProjectSSE } from "../../hooks/useProjectSSE";
import { __dispatchForTest } from "../../lib/sseHub";

// The hub lazily opens a real fetch-based connection on first subscription.
// Stub it out so mounting `useProjectSSE` in jsdom never attempts a network
// call — events are injected directly via `__dispatchForTest`.
vi.mock("../../hooks/sseUtils", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../hooks/sseUtils")>();
  return {
    ...actual,
    createManagedEventSource: vi.fn(() => ({ close: vi.fn() })),
  };
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const PROJECT_ID = "proj-pills-test";
const PROJECT_KEY = `project:${PROJECT_ID}`;

let mountedRoots: Array<{ root: Root; container: HTMLDivElement }> = [];

function mountProjectSSEHook(): void {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  function Harness() {
    useProjectSSE(PROJECT_ID);
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

function inject(eventName: string, data: Record<string, unknown> = {}): void {
  act(() => {
    __dispatchForTest({
      agent_id: PROJECT_KEY,
      run_id: "run-1",
      thread_id: null,
      eventName,
      raw: JSON.stringify({
        agent_id: PROJECT_KEY,
        run_id: "run-1",
        payload: { type: eventName, data },
      }),
    });
  });
}

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

beforeEach(() => {
  useChatStore.getState().reset();
  useProjectStore.getState().reset();
  mountProjectSSEHook();
});

afterEach(() => {
  unmountAllHooks();
});

// ---------------------------------------------------------------------------
// todo_list.complete
// ---------------------------------------------------------------------------

describe("todo_list.complete → project store pill", () => {
  it("appends a 'completed' pill with task counts", () => {
    inject("todo_list.complete", {
      tasklist_id: "tl-1",
      status: "completed",
      counts: { succeeded: 5, failed: 0, skipped: 0 },
    });

    const msgs = useProjectStore.getState().messages;
    expect(msgs).toHaveLength(1);
    expect(msgs[0].event_type).toBe("todo_list_complete");
    expect(msgs[0].content).toBe("Todo list completed · 5 done");
    expect(msgs[0].role).toBe("system");
  });

  it("appends a 'failed' pill with failure counts", () => {
    inject("todo_list.complete", {
      tasklist_id: "tl-2",
      status: "failed",
      counts: { succeeded: 2, failed: 3, skipped: 1 },
    });

    const { messages, allMessages } = useProjectStore.getState();
    expect(messages[0].content).toBe("Todo list ended with failures · 2 done, 3 failed, 1 skipped");
    expect(allMessages).toHaveLength(1);
  });

  it("appends a 'cancelled' pill", () => {
    inject("todo_list.complete", {
      tasklist_id: "tl-3",
      status: "cancelled",
      counts: { succeeded: 0 },
    });

    expect(useProjectStore.getState().messages[0].content).toBe(
      "Todo list was cancelled · 0 done",
    );
  });

  it("is a no-op when tasklist_id is missing", () => {
    inject("todo_list.complete", { status: "completed" });
    expect(useProjectStore.getState().messages).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// delegate.complete
// ---------------------------------------------------------------------------

describe("delegate.complete → project store pill", () => {
  it("appends a completion pill with duration", () => {
    inject("delegate.complete", {
      delegate_name: "researcher",
      status: "completed",
      duration_ms: 3500,
    });

    const msgs = useProjectStore.getState().messages;
    expect(msgs).toHaveLength(1);
    expect(msgs[0].event_type).toBe("delegate_complete");
    expect(msgs[0].content).toBe("Delegate 'researcher' completed · 3.5s");
    expect(msgs[0].metadata).toEqual({ status: "completed", delegate_name: "researcher" });
  });

  it("appends a 'failed' pill without duration when not provided", () => {
    inject("delegate.complete", {
      delegate_name: "writer",
      status: "failed",
    });

    const msg = useProjectStore.getState().messages[0];
    expect(msg.content).toBe("Delegate 'writer' failed");
    expect(msg.metadata?.status).toBe("failed");
  });

  it("appends a 'cancelled' pill", () => {
    inject("delegate.complete", {
      delegate_name: "planner",
      status: "cancelled",
      duration_ms: 1000,
    });

    expect(useProjectStore.getState().messages[0].content).toBe(
      "Delegate 'planner' cancelled · 1.0s",
    );
  });

  it("is a no-op when delegate_name is missing", () => {
    inject("delegate.complete", { status: "completed" });
    expect(useProjectStore.getState().messages).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// memory_saved
// ---------------------------------------------------------------------------

describe("memory_saved → project store pill", () => {
  it("appends an agent-scope pill", () => {
    inject("memory_saved", {
      content: "User prefers concise responses",
      scope: "agent",
    });

    const msgs = useProjectStore.getState().messages;
    expect(msgs).toHaveLength(1);
    expect(msgs[0].event_type).toBe("memory_saved");
    expect(msgs[0].content).toBe("Memory saved (agent): User prefers concise responses");
  });

  it("appends a global-scope pill", () => {
    inject("memory_saved", {
      content: "Always use TypeScript strict mode",
      scope: "Global",
    });

    expect(useProjectStore.getState().messages[0].content).toBe(
      "Memory saved (global): Always use TypeScript strict mode",
    );
  });

  it("truncates content longer than 80 characters", () => {
    const long = "a".repeat(100);
    inject("memory_saved", { content: long, scope: "agent" });

    const content = useProjectStore.getState().messages[0].content;
    expect(content).toBe(`Memory saved (agent): ${"a".repeat(80)}…`);
  });

  it("is a no-op when content is missing", () => {
    inject("memory_saved", { scope: "agent" });
    expect(useProjectStore.getState().messages).toHaveLength(0);
  });

  it("appends to allMessages as well as messages", () => {
    inject("memory_saved", { content: "test memory", scope: "agent" });
    const { messages, allMessages } = useProjectStore.getState();
    expect(messages).toHaveLength(1);
    expect(allMessages).toHaveLength(1);
    expect(messages[0]).toEqual(allMessages[0]);
  });
});
