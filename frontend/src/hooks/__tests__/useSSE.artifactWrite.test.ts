// @vitest-environment jsdom
//
// Live half of inline artifact-card rendering: an `ArtifactWrite` tool call
// emits `tool_call_completed` with `{tool_name: "ArtifactWrite", output}`,
// where `output` is the same JSON string (`{id, renderer, refresh_intent,
// title}`) the persisted transcript's tool_result entry carries. The handler
// must parse it, register the card, and reactively append the id to the
// current turn's in-flight entry — no waiting for run_ended.
//
// Events are injected through the real `useSSE` hook via the SSE hub's
// `__dispatchForTest` seam, and a second harness component subscribes to
// the store the same way `StreamingMessage` does, so this exercises the
// production listener body end-to-end (including that the update is
// reactive — a prior attempt failed silently because the store mutation
// didn't trigger a re-render).

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { useChatStore, useInFlightArtifactIds, inFlightKey } from "../../stores/chatStore";
import { useArtifactStore } from "../../stores/artifactStore";
import { useSSE } from "../useSSE";
import { __dispatchForTest } from "../../lib/sseHub";

vi.mock("../sseUtils", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../sseUtils")>();
  return {
    ...actual,
    createManagedEventSource: vi.fn(() => ({ close: vi.fn() })),
  };
});

const AGENT_ID = "artifact-live-agent";

let mountedRoots: Array<{ root: Root; container: HTMLDivElement }> = [];
let renderCount = 0;
let lastRenderedIds: string[] = [];

function mountHarness(agentId: string): void {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  function Harness() {
    useSSE(agentId);
    // The exact hook `StreamingMessage` uses to read live artifact ids — a
    // real subscribing consumer, so a non-reactive store mutation would
    // leave this stale instead of just "the raw Map happened to have the
    // value".
    const ids = useInFlightArtifactIds(agentId);
    renderCount++;
    lastRenderedIds = ids;
    return React.createElement("div", { "data-testid": "ids" }, ids.join(","));
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

function injectToolCallCompleted(agentId: string, data: Record<string, unknown>): void {
  act(() => {
    __dispatchForTest({
      agent_id: agentId,
      run_id: "run-1",
      thread_id: null,
      eventName: "tool_call_completed",
      raw: JSON.stringify({
        agent_id: agentId,
        run_id: "run-1",
        payload: { type: "tool_call_completed", data },
      }),
    });
  });
}

beforeEach(() => {
  useChatStore.getState().reset();
  useArtifactStore.setState({ byAgent: new Map(), cardsById: new Map() });
  renderCount = 0;
  lastRenderedIds = [];
});

afterEach(() => {
  unmountAllHooks();
});

describe("tool_call_completed → live inline artifact card", () => {
  it("appends the artifact id to the in-flight entry and registers the card, reactively (no run_ended wait)", () => {
    useChatStore.getState().ensureInFlight(AGENT_ID);
    mountHarness(AGENT_ID);
    const renderCountBeforeDispatch = renderCount;

    injectToolCallCompleted(AGENT_ID, {
      tool_name: "ArtifactWrite",
      output: JSON.stringify({ id: "artifact-live-1", renderer: "cards", refresh_intent: "none", title: "Live board" }),
    });

    // Store mutated...
    expect(useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.artifactIds).toEqual(["artifact-live-1"]);
    // ...and the card is resolvable without a network fetch.
    expect(useArtifactStore.getState().getCard("artifact-live-1")).toEqual({
      id: "artifact-live-1",
      title: "Live board",
      kind: "cards",
      refresh_intent: "none",
    });
    // ...and a subscribing component actually re-rendered with the new value
    // (the reactivity regression this test guards against).
    expect(renderCount).toBeGreaterThan(renderCountBeforeDispatch);
    expect(lastRenderedIds).toEqual(["artifact-live-1"]);
  });

  it("ignores tool_call_completed for tools other than ArtifactWrite", () => {
    useChatStore.getState().ensureInFlight(AGENT_ID);
    mountHarness(AGENT_ID);

    injectToolCallCompleted(AGENT_ID, { tool_name: "Bash", output: "some shell output" });

    expect(useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.artifactIds).toEqual([]);
  });

  it("ignores a validation-error ArtifactWrite result (non-JSON output)", () => {
    useChatStore.getState().ensureInFlight(AGENT_ID);
    mountHarness(AGENT_ID);

    injectToolCallCompleted(AGENT_ID, {
      tool_name: "ArtifactWrite",
      output: "error: Missing required field: title",
    });

    expect(useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.artifactIds).toEqual([]);
  });

  it("marks the in-flight tool-call chip done rather than removing it (jumpy-indicator fix)", () => {
    useChatStore.getState().ensureInFlight(AGENT_ID);
    useChatStore.getState().addInFlightToolCall(AGENT_ID, { tool: "ArtifactWrite" });
    mountHarness(AGENT_ID);
    expect(useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.activeToolCalls).toHaveLength(1);

    injectToolCallCompleted(AGENT_ID, {
      tool_name: "ArtifactWrite",
      output: JSON.stringify({ id: "artifact-live-2", renderer: "table", refresh_intent: "none", title: "T" }),
    });

    // The chip stays stacked (marked done) — see markInFlightToolCallDone —
    // rather than being popped, so the bubble doesn't shrink until text_delta
    // /finalize/run_ended actually flushes the classic-chip stack.
    const calls = useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.activeToolCalls;
    expect(calls).toHaveLength(1);
    expect(calls?.[0].done).toBe(true);
  });

  it("scopes the appended id to the thread-composite in-flight key when the event carries a thread_id", () => {
    const threadId = "thread-42";
    useChatStore.getState().ensureInFlight(inFlightKey(AGENT_ID, threadId));
    mountHarness(AGENT_ID);

    act(() => {
      __dispatchForTest({
        agent_id: AGENT_ID,
        run_id: "run-1",
        thread_id: threadId,
        eventName: "tool_call_completed",
        raw: JSON.stringify({
          agent_id: AGENT_ID,
          run_id: "run-1",
          thread_id: threadId,
          payload: {
            type: "tool_call_completed",
            data: {
              tool_name: "ArtifactWrite",
              output: JSON.stringify({ id: "artifact-thread-1", renderer: "list", refresh_intent: "none", title: "T" }),
            },
          },
        }),
      });
    });

    expect(
      useChatStore.getState().inFlightByAgent.get(inFlightKey(AGENT_ID, threadId))?.artifactIds,
    ).toEqual(["artifact-thread-1"]);
    // Default-thread bucket (what the harness renders) was never created —
    // the event only ever touched the thread-scoped composite key.
    expect(useChatStore.getState().inFlightByAgent.has(AGENT_ID)).toBe(false);
  });
});
