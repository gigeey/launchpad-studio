/**
 * Regression suite: per-thread streaming isolation.
 *
 * Bug: the "streaming bubble" (typing indicator / text buffer / tool-call
 * chips / thinking pill) was keyed purely by agentId in `inFlightByAgent`.
 * Once an agent could have multiple threads (Main thread + operator-created
 * threads), a run on one thread wrote into the SAME bucket a run on another
 * thread of the same agent would read from — so switching threads mid-stream
 * showed the wrong thread's content, and a reply on a fresh thread appeared
 * to "leak" into Main thread (and vice versa).
 *
 * Fix: `inFlightKey(agentId, threadId)` composes a thread-scoped bucket key
 * for any non-default thread (mirroring the backend, which only tags
 * `AgentEvent.thread_id` for non-default threads — see
 * `resolve_non_default_thread` in `ao-server/src/routes/messages.rs` and the
 * `bg_thread_id` threading in `ao-engine/src/agent_runner/cli.rs`).
 * `resolveStreamingThreadId` mirrors that same collapse rule on the read
 * side so selecting "Main thread" by its real backend id still resolves to
 * the plain per-agent bucket instead of a bucket the backend never tags.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  useChatStore,
  inFlightKey,
  agentIdFromInFlightKey,
  threadIdFromInFlightKey,
  resolveStreamingThreadId,
  isEventForActiveThread,
  type InFlightAgentMessage,
} from "../chatStore";
import { resolveThreadActivity, threadActivityKey } from "../../components/shared/ThreadActivityBadge";
import type { PendingForm, Thread } from "../../types/api";
import type { FormRequestPayload } from "../../types/form";

// finalizeInFlightText (exercised below) fires a fire-and-forget
// `fetchAgents()` sidebar refresh that isn't under test here — without a
// mock it falls through to a real `fetch("/agents")` and trips the global
// unmocked-fetch guard (`src/test/setupFetchGuard.ts`) as an unhandled
// rejection.
vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return { ...actual, getAgents: async () => [] };
});

function chatStore() {
  return useChatStore.getState();
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

function makeSyncForm(overrides: Partial<FormRequestPayload> = {}): FormRequestPayload {
  return {
    form_id: "form-1",
    agent_id: "agent-1",
    session_id: "session-1",
    title: "Pick one",
    fields: [],
    ...overrides,
  };
}

function makeAsyncPendingForm(overrides: Partial<PendingForm> = {}): PendingForm {
  return {
    thread_id: null,
    form_id: "async-form-1",
    spec: {
      form_id: "async-form-1",
      mode: "async",
      spec: { form_id: "async-form-1", title: "Pick one", fields: [] },
    },
    ...overrides,
  };
}

beforeEach(() => {
  useChatStore.getState().reset();
});

describe("inFlightKey / agentIdFromInFlightKey", () => {
  it("returns the plain agent id when no thread is given (default-thread back-compat)", () => {
    expect(inFlightKey("agent-1")).toBe("agent-1");
    expect(inFlightKey("agent-1", undefined)).toBe("agent-1");
  });

  it("composes a distinct key per (agent, thread) pair", () => {
    const keyA = inFlightKey("agent-1", "thread-a");
    const keyB = inFlightKey("agent-1", "thread-b");
    expect(keyA).not.toBe(keyB);
    expect(keyA).not.toBe("agent-1");
  });

  it("agentIdFromInFlightKey recovers the agent id from either key shape", () => {
    expect(agentIdFromInFlightKey("agent-1")).toBe("agent-1");
    expect(agentIdFromInFlightKey(inFlightKey("agent-1", "thread-a"))).toBe("agent-1");
  });

  it("threadIdFromInFlightKey recovers the thread id, or undefined for a plain key", () => {
    expect(threadIdFromInFlightKey("agent-1")).toBeUndefined();
    expect(threadIdFromInFlightKey(inFlightKey("agent-1", "thread-a"))).toBe("thread-a");
  });
});

describe("resolveStreamingThreadId", () => {
  const AGENT = "agent-1";
  const DEFAULT_ID = "default-thread-uuid";
  const FRESH_ID = "fresh-thread-uuid";
  const threads: Thread[] = [makeThread(DEFAULT_ID, AGENT, "default"), makeThread(FRESH_ID, AGENT, "fresh")];

  it("collapses the default thread's REAL backend id to undefined", () => {
    // This is the case that broke a naive implementation: selectedThreadIdByAgent
    // holds the default thread's real UUID (not a sentinel) once loadThreads
    // resolves — see chatStore.loadThreads.
    expect(resolveStreamingThreadId(AGENT, DEFAULT_ID, threads)).toBeUndefined();
  });

  it("collapses the default-{agentId} sentinel (pre-load state) to undefined", () => {
    expect(resolveStreamingThreadId(AGENT, `default-${AGENT}`, threads)).toBeUndefined();
    expect(resolveStreamingThreadId(AGENT, `default-${AGENT}`, undefined)).toBeUndefined();
  });

  it("returns the real id for a non-default thread", () => {
    expect(resolveStreamingThreadId(AGENT, FRESH_ID, threads)).toBe(FRESH_ID);
  });

  it("returns undefined when no thread is selected or no agent given", () => {
    expect(resolveStreamingThreadId(AGENT, undefined, threads)).toBeUndefined();
    expect(resolveStreamingThreadId(null, FRESH_ID, threads)).toBeUndefined();
  });
});

describe("streaming state isolation across threads of the same agent", () => {
  const AGENT = "agent-multi-thread";
  const THREAD_A = "thread-aaa";
  const THREAD_B = "thread-bbb";
  const KEY_MAIN = inFlightKey(AGENT); // Main thread reuses the plain agent key
  const KEY_A = inFlightKey(AGENT, THREAD_A);
  const KEY_B = inFlightKey(AGENT, THREAD_B);

  it("a run on a fresh thread does not write into Main thread's bucket", () => {
    // Simulates useSSE: run_started + text_delta tagged with THREAD_A's id
    // (mirrors keyFor(id, data) when data.thread_id === THREAD_A)
    chatStore().ensureInFlight(KEY_A);
    chatStore().appendInFlightDelta(KEY_A, "Hey! What are we working on today?");

    // Main thread's bucket (the bug: this used to be where the reply landed)
    expect(chatStore().inFlightByAgent.has(KEY_MAIN)).toBe(false);
    expect(chatStore().inFlightByAgent.get(KEY_MAIN)?.textBuffer ?? "").toBe("");

    // The fresh thread's own bucket has the content
    expect(chatStore().inFlightByAgent.get(KEY_A)?.textBuffer).toBe("Hey! What are we working on today?");
  });

  it("two non-default threads of the same agent stream independently", () => {
    chatStore().ensureInFlight(KEY_A);
    chatStore().ensureInFlight(KEY_B);
    chatStore().appendInFlightDelta(KEY_A, "Thread A content");
    chatStore().appendInFlightDelta(KEY_B, "Thread B content");

    expect(chatStore().inFlightByAgent.get(KEY_A)?.textBuffer).toBe("Thread A content");
    expect(chatStore().inFlightByAgent.get(KEY_B)?.textBuffer).toBe("Thread B content");
  });

  it("finalizing/tearing down one thread's run leaves the other thread's bucket untouched", () => {
    chatStore().ensureInFlight(KEY_A);
    chatStore().ensureInFlight(KEY_B);
    chatStore().appendInFlightDelta(KEY_A, "still streaming on A");
    chatStore().appendInFlightDelta(KEY_B, "finished on B");

    chatStore().finalizeInFlightText(KEY_B, "finished on B");
    chatStore().deleteInFlight(KEY_B);

    expect(chatStore().inFlightByAgent.has(KEY_B)).toBe(false);
    // Thread A's in-flight run is unaffected by thread B's teardown
    expect(chatStore().inFlightByAgent.get(KEY_A)?.textBuffer).toBe("still streaming on A");
  });

  it("Main thread's own streaming still works exactly as before (plain key, no thread tag)", () => {
    chatStore().ensureInFlight(KEY_MAIN);
    chatStore().appendInFlightDelta(KEY_MAIN, "plain agent-keyed reply");
    expect(chatStore().inFlightByAgent.get(AGENT)?.textBuffer).toBe("plain agent-keyed reply");
  });
});

describe("isEventForActiveThread", () => {
  const AGENT = "agent-1";
  const DEFAULT_ID = "default-thread-uuid";
  const FRESH_ID = "fresh-thread-uuid";
  const threads: Thread[] = [makeThread(DEFAULT_ID, AGENT, "default"), makeThread(FRESH_ID, AGENT, "fresh")];
  const threadsByAgent = new Map([[AGENT, threads]]);

  it("matches an untagged (default-thread) event when the default thread is active", () => {
    const selectedThreadIdByAgent = new Map([[AGENT, DEFAULT_ID]]);
    expect(isEventForActiveThread(AGENT, undefined, threadsByAgent, selectedThreadIdByAgent)).toBe(true);
  });

  it("does not match an untagged event when a non-default thread is active", () => {
    const selectedThreadIdByAgent = new Map([[AGENT, FRESH_ID]]);
    expect(isEventForActiveThread(AGENT, undefined, threadsByAgent, selectedThreadIdByAgent)).toBe(false);
  });

  it("matches a tagged event only when its thread id is the one active", () => {
    const selectedThreadIdByAgent = new Map([[AGENT, FRESH_ID]]);
    expect(isEventForActiveThread(AGENT, FRESH_ID, threadsByAgent, selectedThreadIdByAgent)).toBe(true);
    expect(isEventForActiveThread(AGENT, "some-other-thread", threadsByAgent, selectedThreadIdByAgent)).toBe(false);
  });
});

/**
 * Regression: `finalizeInFlightText` used to receive the composite
 * `inFlightKey` (plain agent id, or `agent::thread:xyz` for a non-default
 * thread — see `keyFor` in `useSSE.ts`) but compared it directly against
 * `selectedAgentId`, which is always a plain agent id. For any non-default
 * thread that comparison could never be true, so the finalized reply never
 * made it into `messages`/`allMessages` (and the buffer had already been
 * cleared to "" as part of finalizing) — the reply visibly vanished until a
 * navigate-away/navigate-back forced a refetch from disk. Fixed by unwrapping
 * the key into its agent + thread parts and gating on both.
 */
describe("finalizeInFlightText — cross-thread write isolation", () => {
  const AGENT = "agent-finalize";
  const THREAD_A = "thread-fin-a";
  const THREAD_B = "thread-fin-b";
  const DEFAULT_THREAD_ID = `default-${AGENT}`;
  const threads: Thread[] = [
    makeThread(DEFAULT_THREAD_ID, AGENT, "default"),
    makeThread(THREAD_A, AGENT, "fresh"),
    makeThread(THREAD_B, AGENT, "fresh"),
  ];

  it("commits the finalized reply into messages/allMessages when its thread matches the one currently loaded", () => {
    useChatStore.setState({
      selectedAgentId: AGENT,
      threadsByAgent: new Map([[AGENT, threads]]),
      selectedThreadIdByAgent: new Map([[AGENT, THREAD_A]]),
      messages: [],
      allMessages: [],
    });
    const key = inFlightKey(AGENT, THREAD_A);
    chatStore().ensureInFlight(key);
    chatStore().finalizeInFlightText(key, "reply on thread A");

    expect(chatStore().messages.map((m) => m.content)).toEqual(["reply on thread A"]);
    expect(chatStore().allMessages.map((m) => m.content)).toEqual(["reply on thread A"]);
  });

  it("does NOT leak the finalized reply into the currently-viewed thread when the run belongs to a different thread of the same agent", () => {
    useChatStore.setState({
      selectedAgentId: AGENT,
      // User is viewing thread A...
      threadsByAgent: new Map([[AGENT, threads]]),
      selectedThreadIdByAgent: new Map([[AGENT, THREAD_A]]),
      messages: [],
      allMessages: [],
    });
    // ...but the run that just finished was on thread B.
    const keyB = inFlightKey(AGENT, THREAD_B);
    chatStore().ensureInFlight(keyB);
    chatStore().finalizeInFlightText(keyB, "reply on thread B");

    expect(chatStore().messages).toEqual([]);
    expect(chatStore().allMessages).toEqual([]);
  });

  it("still tears down the correct thread-scoped in-flight bucket even when the message write is skipped", () => {
    useChatStore.setState({
      selectedAgentId: AGENT,
      threadsByAgent: new Map([[AGENT, threads]]),
      selectedThreadIdByAgent: new Map([[AGENT, THREAD_A]]),
      messages: [],
      allMessages: [],
    });
    const keyA = inFlightKey(AGENT, THREAD_A);
    const keyB = inFlightKey(AGENT, THREAD_B);
    chatStore().ensureInFlight(keyA);
    chatStore().ensureInFlight(keyB);
    chatStore().appendInFlightDelta(keyA, "still streaming on A");
    chatStore().finalizeInFlightText(keyB, "reply on thread B");

    // Thread B's own bucket is torn down (finalize always clears its own key)...
    expect(chatStore().inFlightByAgent.get(keyB)?.textBuffer).toBe("");
    // ...without touching thread A's still-streaming bucket.
    expect(chatStore().inFlightByAgent.get(keyA)?.textBuffer).toBe("still streaming on A");
  });

  it("updates the agent's per-thread messageCache entry only when the event's thread matches the currently active thread", () => {
    // `messageCache` is keyed per-thread (`inFlightKey(agentId, threadId)`,
    // same composite-key convention as `inFlightByAgent` — see chatStore.ts's
    // doc comment on `messageCache`), not per-agent — a background finalize
    // must land in exactly its own thread's cache slot and never bleed into
    // a sibling thread's.
    const cacheKeyA = inFlightKey(AGENT, THREAD_A);
    useChatStore.setState({
      selectedAgentId: null, // agent not selected at all — background run
      threadsByAgent: new Map([[AGENT, threads]]),
      selectedThreadIdByAgent: new Map([[AGENT, THREAD_A]]),
      messageCache: new Map([[cacheKeyA, { allMessages: [], displayCount: 0, lastAccessed: 0, cursor: null }]]),
    });
    const keyA = inFlightKey(AGENT, THREAD_A);
    chatStore().ensureInFlight(keyA);
    chatStore().finalizeInFlightText(keyA, "background reply on A");
    expect(chatStore().messageCache.get(cacheKeyA)?.allMessages.map((m) => m.content)).toEqual(["background reply on A"]);

    const keyB = inFlightKey(AGENT, THREAD_B);
    chatStore().ensureInFlight(keyB);
    chatStore().finalizeInFlightText(keyB, "background reply on B");
    // Thread A's cache entry is untouched by B's finalize — distinct
    // composite key, and B isn't the currently active thread either.
    expect(chatStore().messageCache.get(cacheKeyA)?.allMessages.map((m) => m.content)).toEqual(["background reply on A"]);
    // B never had a cache entry to begin with; finalize doesn't create one
    // out of thin air (it only ever refreshes an existing entry).
    expect(chatStore().messageCache.has(inFlightKey(AGENT, THREAD_B))).toBe(false);
  });

  it("Main thread (no thread tag) still commits normally — back-compat", () => {
    useChatStore.setState({
      selectedAgentId: AGENT,
      threadsByAgent: new Map([[AGENT, threads]]),
      selectedThreadIdByAgent: new Map(),
      messages: [],
      allMessages: [],
    });
    chatStore().ensureInFlight(AGENT);
    chatStore().finalizeInFlightText(AGENT, "plain agent reply");
    expect(chatStore().messages.map((m) => m.content)).toEqual(["plain agent reply"]);
  });
});

/**
 * `unreadThreadIds` drives ThreadTabStrip's unread dot — a thread's pill
 * should read as unread exactly when its run finalized while the user was
 * looking somewhere else (a different thread, a different agent, or no
 * agent at all), and that marker should clear the instant `markThreadViewed`
 * reports the user navigated there (wired from ChatView's mount/thread-switch
 * effect, not exercised here).
 */
describe("finalizeInFlightText — unreadThreadIds", () => {
  const AGENT = "agent-unread";
  const THREAD_A = "thread-unread-a";
  const THREAD_B = "thread-unread-b";
  const DEFAULT_THREAD_ID = `default-${AGENT}`;
  const threads: Thread[] = [
    makeThread(DEFAULT_THREAD_ID, AGENT, "default"),
    makeThread(THREAD_A, AGENT, "fresh"),
    makeThread(THREAD_B, AGENT, "fresh"),
  ];

  it("does NOT mark a thread unread when its reply finalizes while it's the one currently viewed", () => {
    useChatStore.setState({
      selectedAgentId: AGENT,
      threadsByAgent: new Map([[AGENT, threads]]),
      selectedThreadIdByAgent: new Map([[AGENT, THREAD_A]]),
      messages: [],
      allMessages: [],
    });
    const keyA = inFlightKey(AGENT, THREAD_A);
    chatStore().ensureInFlight(keyA);
    chatStore().finalizeInFlightText(keyA, "reply on the viewed thread");
    expect(chatStore().unreadThreadIds.has(keyA)).toBe(false);
  });

  it("marks a thread unread when its reply finalizes on a different thread of the same agent", () => {
    useChatStore.setState({
      selectedAgentId: AGENT,
      threadsByAgent: new Map([[AGENT, threads]]),
      selectedThreadIdByAgent: new Map([[AGENT, THREAD_A]]),
      messages: [],
      allMessages: [],
    });
    const keyB = inFlightKey(AGENT, THREAD_B);
    chatStore().ensureInFlight(keyB);
    chatStore().finalizeInFlightText(keyB, "reply on the unopened thread");
    expect(chatStore().unreadThreadIds.has(keyB)).toBe(true);
  });

  it("marks a thread unread when the agent itself isn't selected at all (background run)", () => {
    useChatStore.setState({
      selectedAgentId: null,
      threadsByAgent: new Map([[AGENT, threads]]),
      selectedThreadIdByAgent: new Map([[AGENT, THREAD_A]]),
      messages: [],
      allMessages: [],
    });
    chatStore().ensureInFlight(AGENT);
    chatStore().finalizeInFlightText(AGENT, "background Main-thread reply");
    expect(chatStore().unreadThreadIds.has(AGENT)).toBe(true);
  });

  it("markThreadViewed clears the marker for that exact (agent, thread) pair only", () => {
    const keyA = inFlightKey(AGENT, THREAD_A);
    const keyB = inFlightKey(AGENT, THREAD_B);
    useChatStore.setState({ unreadThreadIds: new Set([keyA, keyB]) });
    chatStore().markThreadViewed(AGENT, THREAD_A);
    expect(chatStore().unreadThreadIds.has(keyA)).toBe(false);
    expect(chatStore().unreadThreadIds.has(keyB)).toBe(true);
  });

  it("markThreadViewed(agentId, undefined) clears the default thread's marker", () => {
    useChatStore.setState({ unreadThreadIds: new Set([AGENT]) });
    chatStore().markThreadViewed(AGENT, undefined);
    expect(chatStore().unreadThreadIds.has(AGENT)).toBe(false);
  });
});

/**
 * `runningDelegatesByThread` tracks async `Delegate` runs that outlive the
 * parent's own turn — see the doc comment on the field in chatStore.ts.
 * `beginDelegateRun`/`endDelegateRun` are the only writers (from
 * `useSSE.ts`'s `delegate.started`/`delegate.complete` handlers); these tests
 * exercise the map semantics directly, without needing a live SSE connection.
 */
describe("runningDelegatesByThread — beginDelegateRun / endDelegateRun", () => {
  const AGENT = "agent-delegate";
  const THREAD_A = "thread-delegate-a";
  const KEY_MAIN = inFlightKey(AGENT);
  const KEY_A = inFlightKey(AGENT, THREAD_A);

  it("adds on begin and clears the entry on a matching end", () => {
    chatStore().beginDelegateRun(KEY_A, "del-1", "Researcher", 1000);
    expect(chatStore().runningDelegatesByThread.get(KEY_A)).toEqual(
      new Map([["del-1", { delegateName: "Researcher", startedAt: 1000 }]]),
    );

    chatStore().endDelegateRun(KEY_A, "del-1");
    // Dropped entirely, not left as an empty map — keeps the map from
    // growing unbounded across a long session's worth of one-off delegates.
    expect(chatStore().runningDelegatesByThread.has(KEY_A)).toBe(false);
  });

  it("supports multiple concurrent delegates on the same thread — entry only clears once every id is gone", () => {
    chatStore().beginDelegateRun(KEY_A, "del-1", "Researcher", 1000);
    chatStore().beginDelegateRun(KEY_A, "del-2", "Writer", 2000);
    expect(chatStore().runningDelegatesByThread.get(KEY_A)).toEqual(
      new Map([
        ["del-1", { delegateName: "Researcher", startedAt: 1000 }],
        ["del-2", { delegateName: "Writer", startedAt: 2000 }],
      ]),
    );

    chatStore().endDelegateRun(KEY_A, "del-1");
    expect(chatStore().runningDelegatesByThread.get(KEY_A)).toEqual(
      new Map([["del-2", { delegateName: "Writer", startedAt: 2000 }]]),
    );

    chatStore().endDelegateRun(KEY_A, "del-2");
    expect(chatStore().runningDelegatesByThread.has(KEY_A)).toBe(false);
  });

  it("reconfirming an already-tracked id (e.g. a connect-time replay) is a harmless no-op, not a double-add", () => {
    chatStore().beginDelegateRun(KEY_A, "del-1", "Researcher", 1000);
    chatStore().beginDelegateRun(KEY_A, "del-1", "Researcher", 1000);
    expect(chatStore().runningDelegatesByThread.get(KEY_A)).toEqual(
      new Map([["del-1", { delegateName: "Researcher", startedAt: 1000 }]]),
    );

    // A single end fully clears it — proves the repeat begin never inflated
    // an internal count that would need a second end to unwind.
    chatStore().endDelegateRun(KEY_A, "del-1");
    expect(chatStore().runningDelegatesByThread.has(KEY_A)).toBe(false);
  });

  it("an end with no matching begin is a no-op", () => {
    chatStore().endDelegateRun(KEY_A, "del-ghost");
    expect(chatStore().runningDelegatesByThread.has(KEY_A)).toBe(false);
  });

  it("an end for an untracked id on an otherwise-active thread leaves the other id alone", () => {
    chatStore().beginDelegateRun(KEY_A, "del-1", "Researcher", 1000);
    chatStore().endDelegateRun(KEY_A, "del-other");
    expect(chatStore().runningDelegatesByThread.get(KEY_A)).toEqual(
      new Map([["del-1", { delegateName: "Researcher", startedAt: 1000 }]]),
    );
  });

  it("keeps two threads of the same agent independent", () => {
    const keyB = inFlightKey(AGENT, "thread-delegate-b");
    chatStore().beginDelegateRun(KEY_A, "del-1", "Researcher", 1000);
    expect(chatStore().runningDelegatesByThread.has(keyB)).toBe(false);
    chatStore().endDelegateRun(KEY_A, "del-1");
    expect(chatStore().runningDelegatesByThread.has(KEY_A)).toBe(false);
  });

  it("Main thread uses the plain agent key, same convention as inFlightByAgent", () => {
    chatStore().beginDelegateRun(KEY_MAIN, "del-1", "Researcher", 1000);
    expect(chatStore().runningDelegatesByThread.get(AGENT)).toEqual(
      new Map([["del-1", { delegateName: "Researcher", startedAt: 1000 }]]),
    );
  });

  describe("clearDelegateRunsForKey — reconnect zombie-guard escape hatch", () => {
    it("drops the whole entry regardless of how many ids it holds", () => {
      chatStore().beginDelegateRun(KEY_A, "del-1", "Researcher", 1000);
      chatStore().beginDelegateRun(KEY_A, "del-2", "Writer", 2000);

      chatStore().clearDelegateRunsForKey(KEY_A);

      expect(chatStore().runningDelegatesByThread.has(KEY_A)).toBe(false);
    });

    it("is a no-op for a key with no entry", () => {
      chatStore().clearDelegateRunsForKey(KEY_A);
      expect(chatStore().runningDelegatesByThread.has(KEY_A)).toBe(false);
    });

    it("leaves other threads untouched", () => {
      const keyB = inFlightKey(AGENT, "thread-delegate-b");
      chatStore().beginDelegateRun(KEY_A, "del-1", "Researcher", 1000);
      chatStore().beginDelegateRun(keyB, "del-2", "Writer", 2000);

      chatStore().clearDelegateRunsForKey(KEY_A);

      expect(chatStore().runningDelegatesByThread.has(KEY_A)).toBe(false);
      expect(chatStore().runningDelegatesByThread.get(keyB)).toEqual(
        new Map([["del-2", { delegateName: "Writer", startedAt: 2000 }]]),
      );
    });
  });

  it("holds the delegate's name and start time (not just its id) — and startedAt is caller-supplied, not stamped at receive time", () => {
    // A start time far in the past proves the store persists whatever the
    // caller passes rather than substituting its own receive-time clock
    // (e.g. `Date.now()`) — see `useSSE.ts`'s `delegate.started` handler,
    // which derives this from the backend's `spawned_at`.
    const pastStartedAt = 1_000;
    chatStore().beginDelegateRun(KEY_A, "del-1", "Researcher", pastStartedAt);

    const entry = chatStore().runningDelegatesByThread.get(KEY_A)?.get("del-1");
    expect(entry).toEqual({ delegateName: "Researcher", startedAt: pastStartedAt });
    expect(entry?.startedAt).toBe(pastStartedAt);
    expect(entry?.startedAt).toBeLessThan(Date.now() - 1_000 * 60 * 60 * 24 * 365);
  });
});

/**
 * `resolveThreadActivity` folds a thread's running-delegate set into the
 * same "streaming" flag its LLM-turn activity already drives, so every
 * thread-list surface (ThreadsPanel, ThreadTabStrip, Home's sidebar) shows a
 * background async Delegate identically to a normal in-progress reply —
 * without a caller needing to check two separate maps.
 */
describe("resolveThreadActivity — running delegate folds into 'streaming'", () => {
  const AGENT = "agent-delegate-activity";
  const THREAD_A = "thread-activity-a";
  const thread = makeThread(THREAD_A, AGENT, "fresh");
  const emptyInFlight = new Map<string, InFlightAgentMessage>();
  const emptyUnread = new Set<string>();

  it("reports 'streaming' for a thread with a running delegate and no other activity", () => {
    const key = threadActivityKey(AGENT, thread);
    const running = new Map([[key, new Map([["del-1", { delegateName: "Researcher", startedAt: 1000 }]])]]);
    expect(resolveThreadActivity(AGENT, thread, emptyInFlight, emptyUnread, running)).toBe("streaming");
  });

  it("reports 'none' once the delegate set drops back to absent/empty", () => {
    expect(resolveThreadActivity(AGENT, thread, emptyInFlight, emptyUnread, new Map())).toBe("none");
    expect(resolveThreadActivity(AGENT, thread, emptyInFlight, emptyUnread, undefined)).toBe("none");
  });
});

/**
 * `resolveThreadActivity` reports "question" — outranking both "streaming"
 * and "unread" — whenever a thread is blocked on an unanswered form, whether
 * that form arrived via the sync `AskUserQuestionWithForm` path
 * (`pendingFormByAgent`) or the async `pending_forms` path on the agent's own
 * snapshot. A thread awaiting an answer is the one state that needs the
 * operator to actually act, so it has to stay visible even while the same
 * thread is also (still) streaming or carries a stale unread dot.
 */
describe("resolveThreadActivity — unanswered forms report 'question'", () => {
  const AGENT = "agent-question-activity";
  const THREAD_A = "thread-question-a";
  const thread = makeThread(THREAD_A, AGENT, "fresh");
  const defaultThread = makeThread(`default-${AGENT}`, AGENT, "default");
  const emptyInFlight = new Map<string, InFlightAgentMessage>();
  const emptyUnread = new Set<string>();

  it("reports 'question' when a sync form is pending for this thread", () => {
    const pendingFormByAgent = { [inFlightKey(AGENT, THREAD_A)]: makeSyncForm({ thread_id: THREAD_A }) };
    expect(
      resolveThreadActivity(AGENT, thread, emptyInFlight, emptyUnread, undefined, pendingFormByAgent),
    ).toBe("question");
  });

  it("reports 'question' when an async pending_forms entry exists for this thread", () => {
    const pendingForms = [makeAsyncPendingForm({ thread_id: THREAD_A })];
    expect(
      resolveThreadActivity(AGENT, thread, emptyInFlight, emptyUnread, undefined, undefined, pendingForms),
    ).toBe("question");
  });

  it("collapses undefined/null thread_id to the default thread for both form sources", () => {
    const pendingFormByAgent = { [AGENT]: makeSyncForm() };
    expect(
      resolveThreadActivity(AGENT, defaultThread, emptyInFlight, emptyUnread, undefined, pendingFormByAgent),
    ).toBe("question");

    const pendingForms = [makeAsyncPendingForm({ thread_id: null })];
    expect(
      resolveThreadActivity(AGENT, defaultThread, emptyInFlight, emptyUnread, undefined, undefined, pendingForms),
    ).toBe("question");
  });

  it("does not leak a form pending on a different thread of the same agent", () => {
    const pendingFormByAgent = { [inFlightKey(AGENT, "some-other-thread")]: makeSyncForm() };
    const pendingForms = [makeAsyncPendingForm({ thread_id: "some-other-thread" })];
    expect(
      resolveThreadActivity(AGENT, thread, emptyInFlight, emptyUnread, undefined, pendingFormByAgent, pendingForms),
    ).toBe("none");
  });

  it("'question' outranks 'streaming' — a thread can be both actively running and blocked on a form", () => {
    const key = threadActivityKey(AGENT, thread);
    chatStore().ensureInFlight(key);
    const pendingFormByAgent = { [inFlightKey(AGENT, THREAD_A)]: makeSyncForm({ thread_id: THREAD_A }) };
    expect(
      resolveThreadActivity(
        AGENT,
        thread,
        chatStore().inFlightByAgent,
        emptyUnread,
        undefined,
        pendingFormByAgent,
      ),
    ).toBe("question");
  });

  it("'question' outranks 'unread'", () => {
    const key = threadActivityKey(AGENT, thread);
    const unread = new Set([key]);
    const pendingForms = [makeAsyncPendingForm({ thread_id: THREAD_A })];
    expect(
      resolveThreadActivity(AGENT, thread, emptyInFlight, unread, undefined, undefined, pendingForms),
    ).toBe("question");
  });

  it("still reports 'streaming'/'unread' when no form is pending", () => {
    const key = threadActivityKey(AGENT, thread);
    chatStore().ensureInFlight(key);
    expect(resolveThreadActivity(AGENT, thread, chatStore().inFlightByAgent, emptyUnread)).toBe("streaming");

    const unread = new Set([key]);
    expect(resolveThreadActivity(AGENT, thread, emptyInFlight, unread)).toBe("unread");
  });
});
