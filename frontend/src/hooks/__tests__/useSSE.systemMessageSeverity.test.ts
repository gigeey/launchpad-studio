// @vitest-environment jsdom
//
// Threads the `severity` tag agent-watch contract authoring's convergence/
// retry/freeze health events carry (`AgentEventPayload::SystemMessage`'s
// `severity` field) from the raw `system_message` SSE event into the synthetic
// chat-store entry's `metadata.severity` — the field `MessageList.tsx`'s
// `systemMessageToneClass` reads to pick a success/error/neutral tone instead
// of every system message rendering identically.
//
// Driven through the real `useSSE` hook + the SSE hub's `__dispatchForTest`
// seam (same approach as `useSSE.terminalArtifact.test.ts`), so this
// exercises the production `system_message` listener body directly.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { useChatStore } from "../../stores/chatStore";
import { useSSE } from "../useSSE";
import { __dispatchForTest } from "../../lib/sseHub";

vi.mock("../sseUtils", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../sseUtils")>();
  return {
    ...actual,
    createManagedEventSource: vi.fn(() => ({ close: vi.fn() })),
  };
});

const AGENT_ID = "watch-owner-agent";

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

function injectSystemMessage(data: Record<string, unknown>): void {
  act(() => {
    __dispatchForTest({
      agent_id: AGENT_ID,
      run_id: "assignment:watch-1",
      thread_id: null,
      eventName: "system_message",
      raw: JSON.stringify({
        agent_id: AGENT_ID,
        run_id: "assignment:watch-1",
        payload: { type: "SystemMessage", data },
      }),
    });
  });
}

beforeEach(() => {
  useChatStore.getState().reset();
  useChatStore.setState({ selectedAgentId: AGENT_ID });
});

afterEach(() => {
  unmountAllHooks();
  vi.useRealTimers();
});

describe("system_message severity — SSE wiring into chat store metadata", () => {
  it("carries a success severity into the synthetic system entry's metadata", () => {
    mountHook(() => useSSE(AGENT_ID));

    injectSystemMessage({ text: "Agent watch \"Invoices\" successfully authored its watch contract on attempt 3 of 5.", severity: "success" });

    const messages = useChatStore.getState().messages;
    expect(messages).toHaveLength(1);
    expect(messages[0].role).toBe("system");
    expect(messages[0].metadata).toEqual({ severity: "success" });
  });

  it("carries an error severity into the synthetic system entry's metadata", () => {
    mountHook(() => useSSE(AGENT_ID));

    injectSystemMessage({ text: "Agent watch \"Invoices\" could not author a working contract after 5 polls.", severity: "error" });

    const messages = useChatStore.getState().messages;
    expect(messages[0].metadata).toEqual({ severity: "error" });
  });

  it("omits severity from metadata for an ordinary system message, same as before this field existed", () => {
    mountHook(() => useSSE(AGENT_ID));

    injectSystemMessage({ text: "Assignment run started: run-42" });

    const messages = useChatStore.getState().messages;
    expect(messages[0].metadata).toBeNull();
  });
});
