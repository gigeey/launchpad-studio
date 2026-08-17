/**
 * Regression suite: Fix A — thread-scoped stale-response guard in
 * `selectAgent` + per-thread `messageCache`.
 *
 * Bug: `selectAgent`'s only staleness guard was
 * `get().selectedAgentId !== agentId` — AGENT-scoped, not THREAD-scoped.
 * Switching between two threads of the SAME agent passed that check
 * trivially, so a slow in-flight response for a thread the user had already
 * navigated away from could land and overwrite whatever thread they were
 * actually looking at (or, on a 404/500, wipe a perfectly valid current
 * selection). Because `messageCache` was also keyed by bare `agentId`, a
 * thread switch either flashed the wrong thread's cached content or forced a
 * full network refetch every time.
 *
 * Fix: a monotonically increasing `selectionGeneration` counter (bumped at
 * the top of every `selectAgent` call) plus a per-selection
 * `AbortController`, both module-level in chatStore.ts — see the doc
 * comments there — and a `messageCache` re-keyed per-thread via
 * `inFlightKey(agentId, threadId)`.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import type { TranscriptEntry } from "../../types/api";

const mockGetAgent = vi.fn();
const mockGetMessages = vi.fn();

vi.mock("../../lib/api", () => ({
  getAgents: vi.fn().mockResolvedValue([]),
  getAgent: (...args: unknown[]) => mockGetAgent(...args),
  getMessages: (...args: unknown[]) => mockGetMessages(...args),
}));

import { useChatStore, inFlightKey } from "../chatStore";

function store() {
  return useChatStore.getState();
}

function profileFor(agentId: string) {
  return {
    id: agentId,
    name: agentId,
    description: "",
    provider: { type: "", command: "", args: [], output_format: "", input_mode: "", model_aliases: {}, resume_args: [], session_id_fields: [], clear_env: false, no_output_timeout_ms: 0 },
    model: null,
    skills: [],
    system_prompt: null,
    tools: null,
    env: {},
    max_instances: 1,
    timeout_seconds: 0,
    working_dir: null,
    home_dir: null,
    serialize: false,
  };
}

function entry(content: string, ts = "2026-01-01T00:00:00Z"): TranscriptEntry {
  return { ts, role: { agent: "agent" }, content, event_type: "message" };
}

/** A promise plus its own resolve/reject, so a test can control exactly when
 *  a mocked fetch settles relative to other mocked fetches. */
function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

beforeEach(() => {
  useChatStore.getState().reset();
  mockGetAgent.mockReset();
  mockGetMessages.mockReset();
  mockGetAgent.mockImplementation((id: string) => Promise.resolve(profileFor(id)));
});

// ---------------------------------------------------------------------------
// (a) RACE, SAME AGENT
// ---------------------------------------------------------------------------

describe("selectAgent — thread-scoped race, same agent", () => {
  const AGENT = "agent-race";
  const THREAD_A = "thread-race-a";
  const THREAD_B = "thread-race-b";

  it("a slow response for an abandoned thread never overwrites the thread the user switched to", async () => {
    const defA = deferred<{ messages: TranscriptEntry[]; cursor: null }>();

    mockGetMessages.mockImplementation((_agentId: string, threadId: string | undefined) => {
      if (threadId === THREAD_A) return defA.promise;
      if (threadId === THREAD_B) return Promise.resolve({ messages: [entry("B's message")], cursor: null });
      throw new Error(`unexpected threadId ${threadId}`);
    });

    // Start selecting thread A — its fetch hangs on defA.
    store().selectThreadForAgent(AGENT, THREAD_A);
    const pA = store().selectAgent(AGENT);

    // Switch to thread B before A resolves — B's fetch resolves immediately.
    store().selectThreadForAgent(AGENT, THREAD_B);
    const pB = store().selectAgent(AGENT);
    await pB;

    expect(store().messages.map((m) => m.content)).toEqual(["B's message"]);

    // NOW resolve the abandoned thread A response.
    defA.resolve({ messages: [entry("A's message")], cursor: null });
    await pA;

    // Still B's content — A's stale response must never have landed.
    expect(store().messages.map((m) => m.content)).toEqual(["B's message"]);
    expect(store().allMessages.map((m) => m.content)).toEqual(["B's message"]);
  });
});

// ---------------------------------------------------------------------------
// (b) STALE ERROR
// ---------------------------------------------------------------------------

describe("selectAgent — a superseded request's error is swallowed", () => {
  const AGENT = "agent-stale-error";
  const THREAD_A = "thread-err-a";
  const THREAD_B = "thread-err-b";

  it("a stale 404 does not clear the current (valid) thread selection or surface an error", async () => {
    const defA = deferred<{ messages: TranscriptEntry[]; cursor: null }>();
    mockGetMessages.mockImplementation((_agentId: string, threadId: string | undefined) => {
      if (threadId === THREAD_A) return defA.promise;
      if (threadId === THREAD_B) return Promise.resolve({ messages: [entry("B's message")], cursor: null });
      throw new Error(`unexpected threadId ${threadId}`);
    });
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    store().selectThreadForAgent(AGENT, THREAD_A);
    const pA = store().selectAgent(AGENT);

    store().selectThreadForAgent(AGENT, THREAD_B);
    const pB = store().selectAgent(AGENT);
    await pB;
    expect(store().selectedThreadIdByAgent.get(AGENT)).toBe(THREAD_B);

    // The abandoned thread A request 404s (as if its thread id no longer exists).
    defA.reject(new Error("API 404: thread not found"));
    await expect(pA).resolves.toBeUndefined();

    // The 404 handler's "drop the stale selection" path must NOT have fired
    // against the CURRENT (B) selection — this is the exact failure mode
    // described in the bug report: a stale 404 wiping a valid selection.
    expect(store().selectedThreadIdByAgent.get(AGENT)).toBe(THREAD_B);
    expect(store().messages.map((m) => m.content)).toEqual(["B's message"]);
    expect(errorSpy).not.toHaveBeenCalled();

    errorSpy.mockRestore();
  });

  it("a stale 500 does not clear the current selection or surface an error, and does not rethrow", async () => {
    const defA = deferred<{ messages: TranscriptEntry[]; cursor: null }>();
    mockGetMessages.mockImplementation((_agentId: string, threadId: string | undefined) => {
      if (threadId === THREAD_A) return defA.promise;
      if (threadId === THREAD_B) return Promise.resolve({ messages: [entry("B's message")], cursor: null });
      throw new Error(`unexpected threadId ${threadId}`);
    });
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    store().selectThreadForAgent(AGENT, THREAD_A);
    const pA = store().selectAgent(AGENT);

    store().selectThreadForAgent(AGENT, THREAD_B);
    const pB = store().selectAgent(AGENT);
    await pB;

    defA.reject(new Error("API 500: internal server error"));
    // Must resolve cleanly, not reject — a non-stale caller would still see
    // a 500 rethrown (see the "still propagates real errors" test below),
    // but a STALE one must not.
    await expect(pA).resolves.toBeUndefined();

    expect(store().selectedThreadIdByAgent.get(AGENT)).toBe(THREAD_B);
    expect(store().messages.map((m) => m.content)).toEqual(["B's message"]);
    expect(errorSpy).not.toHaveBeenCalled();

    errorSpy.mockRestore();
  });

  it("a NON-stale error (no newer selection) still propagates — regression guard so the swallow isn't overbroad", async () => {
    const AGENT_SOLO = "agent-solo-error";
    mockGetMessages.mockRejectedValue(new Error("API 500: boom"));

    await expect(store().selectAgent(AGENT_SOLO)).rejects.toThrow("API 500: boom");
  });
});

// ---------------------------------------------------------------------------
// (c) ABORT IS SILENT
// ---------------------------------------------------------------------------

describe("selectAgent — abort wiring", () => {
  const AGENT = "agent-abort";
  const THREAD_A = "thread-abort-a";
  const THREAD_B = "thread-abort-b";

  it("aborts the previous selection's in-flight request when a new selection begins, and the abort produces no error state", async () => {
    let capturedSignalA: AbortSignal | undefined;
    const defA = deferred<{ messages: TranscriptEntry[]; cursor: null }>();

    mockGetMessages.mockImplementation((_agentId: string, threadId: string | undefined, signal?: AbortSignal) => {
      if (threadId === THREAD_A) {
        capturedSignalA = signal;
        return defA.promise;
      }
      if (threadId === THREAD_B) return Promise.resolve({ messages: [entry("B's message")], cursor: null });
      throw new Error(`unexpected threadId ${threadId}`);
    });
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    store().selectThreadForAgent(AGENT, THREAD_A);
    const pA = store().selectAgent(AGENT);
    expect(capturedSignalA?.aborted).toBe(false);

    // A new selection begins — the previous controller must be aborted
    // synchronously, not merely superseded-and-ignored.
    store().selectThreadForAgent(AGENT, THREAD_B);
    const pB = store().selectAgent(AGENT);
    expect(capturedSignalA?.aborted).toBe(true);

    await pB;
    expect(store().messages.map((m) => m.content)).toEqual(["B's message"]);

    // Now the aborted fetch actually rejects, exactly as a real aborted
    // `fetch()` would.
    defA.reject(new DOMException("The operation was aborted.", "AbortError"));
    await expect(pA).resolves.toBeUndefined();

    // Silent: no error state, no console.error, current selection untouched.
    expect(store().selectedThreadIdByAgent.get(AGENT)).toBe(THREAD_B);
    expect(store().messages.map((m) => m.content)).toEqual(["B's message"]);
    expect(errorSpy).not.toHaveBeenCalled();

    errorSpy.mockRestore();
  });
});

// ---------------------------------------------------------------------------
// (d) CACHE CORRECTNESS
// ---------------------------------------------------------------------------

describe("selectAgent — per-thread cache correctness", () => {
  const AGENT = "agent-cache";
  const THREAD_A = "thread-cache-a";
  const THREAD_B = "thread-cache-b";

  it("serves the correct thread's own cached messages when switching back — not the other thread's, and not a blank flash", async () => {
    mockGetMessages.mockImplementation((_agentId: string, threadId: string | undefined) => {
      if (threadId === THREAD_A) return Promise.resolve({ messages: [entry("A's message")], cursor: null });
      if (threadId === THREAD_B) return Promise.resolve({ messages: [entry("B's message")], cursor: null });
      throw new Error(`unexpected threadId ${threadId}`);
    });

    store().selectThreadForAgent(AGENT, THREAD_A);
    await store().selectAgent(AGENT);
    expect(store().messages.map((m) => m.content)).toEqual(["A's message"]);

    store().selectThreadForAgent(AGENT, THREAD_B);
    await store().selectAgent(AGENT);
    expect(store().messages.map((m) => m.content)).toEqual(["B's message"]);

    // Switch back to A. Both A and B are now cached under distinct
    // composite keys, so this is a cache HIT — the cache-hit branch commits
    // its `set()` synchronously, before any network round-trip, so the
    // assertion below (made immediately after the call, no await needed to
    // see the render) also proves there's no intermediate blank flash.
    store().selectThreadForAgent(AGENT, THREAD_A);
    const pReturn = store().selectAgent(AGENT);
    expect(store().messages.map((m) => m.content)).toEqual(["A's message"]);
    await pReturn;
    expect(store().messages.map((m) => m.content)).toEqual(["A's message"]);

    // And both threads have their own distinct cache entries.
    expect(store().messageCache.get(inFlightKey(AGENT, THREAD_A))?.allMessages.map((m) => m.content)).toEqual([
      "A's message",
    ]);
    expect(store().messageCache.get(inFlightKey(AGENT, THREAD_B))?.allMessages.map((m) => m.content)).toEqual([
      "B's message",
    ]);
  });
});

// ---------------------------------------------------------------------------
// (e) Existing agent-level switching behaviour still works
// ---------------------------------------------------------------------------

describe("selectAgent — agent-level switching regression guard", () => {
  const AGENT_OLD = "agent-old";
  const AGENT_NEW = "agent-new";

  it("still discards a stale response when switching to a DIFFERENT agent entirely — the original guard's purpose", async () => {
    const defOld = deferred<{ messages: TranscriptEntry[]; cursor: null }>();
    mockGetMessages.mockImplementation((agentId: string) => {
      if (agentId === AGENT_OLD) return defOld.promise;
      if (agentId === AGENT_NEW) return Promise.resolve({ messages: [entry("new agent's message")], cursor: null });
      throw new Error(`unexpected agentId ${agentId}`);
    });

    const pOld = store().selectAgent(AGENT_OLD);
    const pNew = store().selectAgent(AGENT_NEW);
    await pNew;

    expect(store().selectedAgentId).toBe(AGENT_NEW);
    expect(store().messages.map((m) => m.content)).toEqual(["new agent's message"]);

    defOld.resolve({ messages: [entry("old agent's message")], cursor: null });
    await pOld;

    // Still on the new agent, showing its messages — the old agent's late
    // response never landed, exactly as before this fix.
    expect(store().selectedAgentId).toBe(AGENT_NEW);
    expect(store().messages.map((m) => m.content)).toEqual(["new agent's message"]);
  });
});
