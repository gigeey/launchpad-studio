// @vitest-environment jsdom
//
// Sidebar delegate-activity indicator: `runningDelegatesByThread` (chatStore)
// tracks async `Delegate` calls that keep running in the background well
// after the `Delegate` tool call itself returns — see the field's doc
// comment in `chatStore.ts`. It's written from two `useSSE.ts` handlers:
//
//   - `delegate.started`   (fired once, on background-handle registration) → beginDelegateRun
//   - `delegate.complete`  (terminal: completed/failed/cancelled)          → endDelegateRun
//
// Both events are thread-tagged server-side (see
// `ao-engine/src/delegate_completion.rs`'s `QueueDelegateCompletionSink`), so
// they route to the thread that actually launched the delegate rather than
// always the agent's default thread.
//
// Events are injected through the real `useSSE` hook via the SSE hub's
// `__dispatchForTest`/`__triggerConnectionOpenForTest` seams (same approach
// as `useSSE.artifactWrite.test.ts`), so this exercises the production
// listener bodies end-to-end rather than re-implementing their logic in the
// test.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { useChatStore, inFlightKey } from "../../stores/chatStore";
import { useSSE } from "../useSSE";
import { __dispatchForTest, __triggerConnectionOpenForTest } from "../../lib/sseHub";

vi.mock("../sseUtils", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../sseUtils")>();
  return {
    ...actual,
    createManagedEventSource: vi.fn(() => ({ close: vi.fn() })),
  };
});

const AGENT_ID = "delegate-activity-agent";
// Fixed, non-"now" spawn timestamp for `delegate.started` payloads — the
// production `spawned_at` guard requires this field, and using a fixed past
// value (rather than an actual current timestamp) makes it obvious in
// assertions that `startedAt` comes from the payload, not the receive time.
const SPAWNED_AT_ISO = "2026-01-01T00:00:00.000Z";
const SPAWNED_AT_MS = Date.parse(SPAWNED_AT_ISO);

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

function dispatch(eventName: string, threadId: string | null, data: Record<string, unknown>): void {
  act(() => {
    __dispatchForTest({
      agent_id: AGENT_ID,
      run_id: "run-1",
      thread_id: threadId,
      eventName,
      raw: JSON.stringify({
        agent_id: AGENT_ID,
        run_id: "run-1",
        thread_id: threadId,
        payload: { type: eventName, data },
      }),
    });
  });
}

function runningDelegates(key: string): Map<string, { delegateName: string; startedAt: number }> {
  return useChatStore.getState().runningDelegatesByThread.get(key) ?? new Map();
}

function runningIds(key: string): Set<string> {
  return new Set(runningDelegates(key).keys());
}

beforeEach(() => {
  useChatStore.getState().reset();
});

afterEach(() => {
  unmountAllHooks();
  vi.useRealTimers();
});

describe("runningDelegatesByThread — SSE wiring", () => {
  it("adds on delegate.started, even for a background (non-selected) thread", () => {
    const threadId = "thread-bg-1";
    // Nothing about this agent/thread is "selected" — mirrors the sidebar
    // use case: a delegate can be running on a thread the user never opened.
    mountHarness(AGENT_ID);

    dispatch("delegate.started", threadId, { delegate_name: "Researcher", delegation_id: "del-1", spawned_at: SPAWNED_AT_ISO });

    expect(runningIds(inFlightKey(AGENT_ID, threadId))).toEqual(new Set(["del-1"]));
  });

  it("holds the delegate's name and start time, not just its id — and startedAt comes from the payload's spawned_at rather than being stamped at receive time", () => {
    const threadId = "thread-name-and-time";
    mountHarness(AGENT_ID);

    dispatch("delegate.started", threadId, {
      delegate_name: "Researcher",
      delegation_id: "del-1",
      spawned_at: SPAWNED_AT_ISO,
    });

    const entry = runningDelegates(inFlightKey(AGENT_ID, threadId)).get("del-1");
    expect(entry).toEqual({ delegateName: "Researcher", startedAt: SPAWNED_AT_MS });
    // SPAWNED_AT_ISO is a fixed date in the past, nowhere near "now" — if the
    // store (or the handler) stamped its own receive-time clock instead of
    // carrying the payload's `spawned_at` through, this would fail.
    expect(entry?.startedAt).toBeLessThan(Date.now() - 1000 * 60 * 60 * 24 * 30);
  });

  it("ignores a delegate.started event missing delegation_id", () => {
    const threadId = "thread-malformed";
    mountHarness(AGENT_ID);

    dispatch("delegate.started", threadId, { delegate_name: "Researcher" });

    expect(runningIds(inFlightKey(AGENT_ID, threadId)).size).toBe(0);
  });

  it("ignores a delegate.started event missing delegate_name", () => {
    const threadId = "thread-no-name";
    mountHarness(AGENT_ID);

    dispatch("delegate.started", threadId, { delegation_id: "del-1", spawned_at: SPAWNED_AT_ISO });

    expect(runningIds(inFlightKey(AGENT_ID, threadId)).size).toBe(0);
  });

  it("ignores a delegate.started event missing spawned_at", () => {
    const threadId = "thread-no-spawned-at";
    mountHarness(AGENT_ID);

    dispatch("delegate.started", threadId, { delegate_name: "Researcher", delegation_id: "del-1" });

    expect(runningIds(inFlightKey(AGENT_ID, threadId)).size).toBe(0);
  });

  it("clears on delegate.complete, even though the harness's agent/thread was never 'selected' — background-thread requirement", () => {
    const threadId = "thread-completes-later";
    mountHarness(AGENT_ID);

    dispatch("delegate.started", threadId, { delegate_name: "Researcher", delegation_id: "abc-123", spawned_at: SPAWNED_AT_ISO });
    expect(runningIds(inFlightKey(AGENT_ID, threadId))).toEqual(new Set(["abc-123"]));

    dispatch("delegate.complete", threadId, {
      delegate_name: "Researcher",
      delegation_id: "abc-123",
      status: "completed",
      duration_ms: 4200,
      transcript_path: "/tmp/abc-123.jsonl",
    });

    expect(runningIds(inFlightKey(AGENT_ID, threadId)).size).toBe(0);
  });

  it("clears on delegate.complete with status 'failed' too — a failed delegate is no longer running", () => {
    const threadId = "thread-fails-later";
    mountHarness(AGENT_ID);

    dispatch("delegate.started", threadId, { delegate_name: "Researcher", delegation_id: "abc-456", spawned_at: SPAWNED_AT_ISO });

    dispatch("delegate.complete", threadId, {
      delegate_name: "Researcher",
      delegation_id: "abc-456",
      status: "failed",
    });

    expect(runningIds(inFlightKey(AGENT_ID, threadId)).size).toBe(0);
  });

  it("supports multiple concurrent delegates on the same thread — clears one at a time", () => {
    const threadId = "thread-concurrent";
    mountHarness(AGENT_ID);

    dispatch("delegate.started", threadId, { delegate_name: "A", delegation_id: "del-a", spawned_at: SPAWNED_AT_ISO });
    dispatch("delegate.started", threadId, { delegate_name: "B", delegation_id: "del-b", spawned_at: SPAWNED_AT_ISO });
    expect(runningIds(inFlightKey(AGENT_ID, threadId))).toEqual(new Set(["del-a", "del-b"]));

    dispatch("delegate.complete", threadId, { delegate_name: "A", delegation_id: "del-a", status: "completed" });
    expect(runningIds(inFlightKey(AGENT_ID, threadId))).toEqual(new Set(["del-b"]));

    dispatch("delegate.complete", threadId, { delegate_name: "B", delegation_id: "del-b", status: "completed" });
    expect(runningIds(inFlightKey(AGENT_ID, threadId)).size).toBe(0);
  });

  it("keeps two threads of the same agent independent", () => {
    const threadA = "thread-independent-a";
    const threadB = "thread-independent-b";
    mountHarness(AGENT_ID);

    dispatch("delegate.started", threadA, { delegate_name: "X", delegation_id: "del-x", spawned_at: SPAWNED_AT_ISO });

    expect(runningIds(inFlightKey(AGENT_ID, threadA))).toEqual(new Set(["del-x"]));
    expect(runningIds(inFlightKey(AGENT_ID, threadB)).size).toBe(0);

    dispatch("delegate.complete", threadA, { delegate_name: "X", delegation_id: "del-x", status: "completed" });

    expect(runningIds(inFlightKey(AGENT_ID, threadA)).size).toBe(0);
  });

  it("Main thread (no thread_id tag) uses the plain agent key, same convention as inFlightByAgent", () => {
    mountHarness(AGENT_ID);

    dispatch("delegate.started", null, { delegate_name: "X", delegation_id: "del-main", spawned_at: SPAWNED_AT_ISO });

    expect(runningIds(AGENT_ID)).toEqual(new Set(["del-main"]));

    dispatch("delegate.complete", null, { delegate_name: "X", delegation_id: "del-main", status: "completed" });

    expect(runningIds(AGENT_ID).size).toBe(0);
  });
});

describe("runningDelegatesByThread — reconnect zombie-guard", () => {
  it("a reconnect that replays delegate.started for the same id cancels the pending clear — badge stays lit", () => {
    vi.useFakeTimers();
    const threadId = "thread-reconnect-alive";
    const key = inFlightKey(AGENT_ID, threadId);
    mountHarness(AGENT_ID);

    dispatch("delegate.started", threadId, { delegate_name: "Researcher", delegation_id: "del-1", spawned_at: SPAWNED_AT_ISO });
    expect(runningIds(key)).toEqual(new Set(["del-1"]));

    // Simulate a mere network blip: the shared connection reopens, and the
    // server's connect-time replay reconfirms the still-live delegation
    // before the grace window elapses.
    act(() => {
      __triggerConnectionOpenForTest();
    });
    dispatch("delegate.started", threadId, { delegate_name: "Researcher", delegation_id: "del-1", spawned_at: SPAWNED_AT_ISO });

    act(() => {
      vi.advanceTimersByTime(10_000);
    });

    // Reconfirmed delegation must survive the grace window.
    expect(runningIds(key)).toEqual(new Set(["del-1"]));
  });

  it("a reconnect with no replay (simulated server restart) clears the stale badge after the grace window", () => {
    vi.useFakeTimers();
    const threadId = "thread-reconnect-dead";
    const key = inFlightKey(AGENT_ID, threadId);
    mountHarness(AGENT_ID);

    dispatch("delegate.started", threadId, { delegate_name: "Researcher", delegation_id: "del-1", spawned_at: SPAWNED_AT_ISO });
    expect(runningIds(key)).toEqual(new Set(["del-1"]));

    // A server restart means `mcp_sessions` came back empty — nothing to
    // replay, so no `delegate.started` ever arrives after this reconnect.
    act(() => {
      __triggerConnectionOpenForTest();
    });

    act(() => {
      vi.advanceTimersByTime(10_000);
    });

    expect(runningIds(key).size).toBe(0);
  });

  it("does not clear before the grace window elapses", () => {
    vi.useFakeTimers();
    const threadId = "thread-reconnect-pending";
    const key = inFlightKey(AGENT_ID, threadId);
    mountHarness(AGENT_ID);

    dispatch("delegate.started", threadId, { delegate_name: "Researcher", delegation_id: "del-1", spawned_at: SPAWNED_AT_ISO });

    act(() => {
      __triggerConnectionOpenForTest();
    });
    act(() => {
      vi.advanceTimersByTime(100);
    });

    expect(runningIds(key)).toEqual(new Set(["del-1"]));
  });
});
