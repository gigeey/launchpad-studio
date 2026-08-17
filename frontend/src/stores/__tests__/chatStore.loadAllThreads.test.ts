/**
 * Regression suite: bulk-hydration `loadAllThreads` (chatStore.ts).
 *
 * Three fixes covered, one describe block each:
 *
 * (a) Error state: a failed `api.listAllThreads` call must be recorded in
 *     `threadsHydrationError`, and — critically — must NOT seed empty arrays
 *     into `threadsByAgent` for the caller's `knownAgentIds`. That seeding is
 *     correct on the success path (it's what makes a legitimately-empty
 *     agent distinguishable from a not-yet-hydrated one) but on the failure
 *     path it's exactly what makes a failed fetch indistinguishable from a
 *     genuinely-empty result — see the DOM-level version of this same
 *     assertion in HomeSidebar.test.tsx's "bulk thread hydration" block.
 *
 * (b) AbortSignal: `loadAllThreads` must forward its `signal` param through
 *     to `api.listAllThreads` unchanged, and an abort (surfaced as an
 *     `AbortError`) must be treated as normal control flow — never recorded
 *     as a hydration error.
 *
 * (c) Stale-response guard: two overlapping `loadAllThreads` calls can
 *     resolve out of order (a slow first call settling after a fast second
 *     one). The internal generation counter must ensure only the LATEST
 *     dispatched call's result is ever applied — on both the success and the
 *     error path.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import type { Thread } from "../../types/api";

const mockListAllThreads = vi.fn();

vi.mock("../../lib/api", () => ({
  getAgents: vi.fn().mockResolvedValue([]),
  listAllThreads: (...args: unknown[]) => mockListAllThreads(...args),
}));

import { useChatStore } from "../chatStore";

function store() {
  return useChatStore.getState();
}

function makeThread(id: string, agentId: string): Thread {
  return {
    id,
    title: null,
    scope: { type: "AgentChat", agent_id: agentId },
    transcript_path: `/data/messages/threads/${id}.jsonl`,
    kind: "default",
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
  };
}

beforeEach(() => {
  useChatStore.getState().reset();
  mockListAllThreads.mockReset();
});

// ---------------------------------------------------------------------------
// (a) Error state — must not be confused with "genuinely empty"
// ---------------------------------------------------------------------------

describe("loadAllThreads — error state (Fix a)", () => {
  it("records threadsHydrationError on failure and does NOT seed empty arrays for knownAgentIds", async () => {
    mockListAllThreads.mockRejectedValue(new Error("network down"));

    await store().loadAllThreads(["agent-1", "agent-2"]);

    expect(store().threadsHydrationError).toBe("network down");
    // CRITICAL (see chatStore.ts loadAllThreads doc comment): the
    // empty-array seed loop must run on the SUCCESS path only. A dropped
    // key here would be exactly the bug — failure silently reading as
    // "these agents have zero threads".
    expect(store().threadsByAgent.has("agent-1")).toBe(false);
    expect(store().threadsByAgent.has("agent-2")).toBe(false);
  });

  it("clears a prior threadsHydrationError once a later call succeeds", async () => {
    mockListAllThreads.mockRejectedValueOnce(new Error("network down"));
    await store().loadAllThreads(["agent-1"]);
    expect(store().threadsHydrationError).toBe("network down");

    const thread = makeThread("t1", "agent-1");
    mockListAllThreads.mockResolvedValueOnce({ "agent-1": [thread] });
    await store().loadAllThreads(["agent-1"]);

    expect(store().threadsHydrationError).toBeNull();
    expect(store().threadsByAgent.get("agent-1")).toEqual([thread]);
  });
});

// ---------------------------------------------------------------------------
// (b) AbortSignal threaded end to end
// ---------------------------------------------------------------------------

describe("loadAllThreads — AbortSignal (Fix b)", () => {
  it("forwards the signal argument through to api.listAllThreads unchanged", async () => {
    mockListAllThreads.mockResolvedValue({});
    const controller = new AbortController();

    await store().loadAllThreads(["agent-1"], controller.signal);

    expect(mockListAllThreads).toHaveBeenCalledWith(controller.signal);
  });

  it("does not record a hydration error when the request is aborted", async () => {
    const abortError = new DOMException("The operation was aborted.", "AbortError");
    mockListAllThreads.mockRejectedValue(abortError);
    const controller = new AbortController();

    await store().loadAllThreads(["agent-1"], controller.signal);

    // An abort is normal control flow (unmount / dependency change), not a
    // failure — must not paint the error state.
    expect(store().threadsHydrationError).toBeNull();
    // Matches the error path's own contract: no empty-array seeding either.
    expect(store().threadsByAgent.has("agent-1")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// (c) Stale-response guard
// ---------------------------------------------------------------------------

describe("loadAllThreads — stale-response guard (Fix c)", () => {
  it("an older call's out-of-order SUCCESS does not overwrite a newer call's already-applied result", async () => {
    let resolveFirst!: (v: Record<string, Thread[]>) => void;
    mockListAllThreads.mockImplementationOnce(
      () =>
        new Promise<Record<string, Thread[]>>((resolve) => {
          resolveFirst = resolve;
        }),
    );
    const first = store().loadAllThreads(["agent-1"]);

    const newerThread = makeThread("newer", "agent-1");
    mockListAllThreads.mockResolvedValueOnce({ "agent-1": [newerThread] });
    const second = store().loadAllThreads(["agent-1"]);
    await second;

    expect(store().threadsByAgent.get("agent-1")).toEqual([newerThread]);

    // The first (older) call now resolves, out of order, with DIFFERENT data.
    resolveFirst({ "agent-1": [makeThread("older", "agent-1")] });
    await first;

    // Must still reflect the newer call's result — the older, later-arriving
    // response was dropped rather than clobbering it.
    expect(store().threadsByAgent.get("agent-1")).toEqual([newerThread]);
  });

  it("an older call's out-of-order FAILURE does not paint an error over a newer call's already-applied success", async () => {
    let rejectFirst!: (err: Error) => void;
    mockListAllThreads.mockImplementationOnce(
      () =>
        new Promise<Record<string, Thread[]>>((_resolve, reject) => {
          rejectFirst = reject;
        }),
    );
    const first = store().loadAllThreads(["agent-1"]);

    const newerThread = makeThread("newer", "agent-1");
    mockListAllThreads.mockResolvedValueOnce({ "agent-1": [newerThread] });
    const second = store().loadAllThreads(["agent-1"]);
    await second;

    expect(store().threadsHydrationError).toBeNull();

    // The first (older) call now rejects, out of order.
    rejectFirst(new Error("stale failure"));
    await first;

    expect(store().threadsHydrationError).toBeNull();
    expect(store().threadsByAgent.get("agent-1")).toEqual([newerThread]);
  });
});
