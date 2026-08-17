// @vitest-environment jsdom
//
// Piece C of the assignment-thread streaming fix: a background/cron
// assignment fire finalizes on a thread the operator isn't currently
// viewing. `finalizeInFlightText` (chatStore.ts) deliberately does NOT
// splice the finalized reply into `messages`/`allMessages` for a
// non-selected thread — those fields hold whichever one thread's transcript
// is currently loaded (see that function's own doc comment). Instead, the
// guarantee that opening the thread afterward shows the complete reply
// without a full app reload comes from `messageCache` being keyed per-THREAD
// (`inFlightKey(agentId, threadId)` — see its doc comment in chatStore.ts):
// `switchToThread` (lib/threadNavigation.ts) selects the target thread and
// calls `selectAgent`, whose cache lookup naturally misses for a thread never
// visited this session, falling through to `selectAgent`'s non-cached branch
// to fetch straight from the backend (`api.getMessages`) — which already has
// the finalized message on disk, since the backend persists a run's
// transcript before emitting the completion event the client reacts to (see
// `ao-engine/src/agent_runner/native.rs`'s `persist_pending().await` running
// before `event_bus.emit(RunEnded...)`). No explicit cache invalidation is
// needed (or performed) for this to work.
//
// This test locks in that guarantee end-to-end: it seeds a cached transcript
// for a DIFFERENT thread of the same agent (correctly composite-keyed),
// finalizes a reply on a background thread while it's not selected, then
// drives the exact `switchToThread` call a tab click makes and asserts the
// rendered `messages` come from a fresh backend fetch of the target thread —
// not the other thread's cache, and not anything `finalizeInFlightText`
// wrote in-memory. It also confirms the other thread's own cache entry is
// left completely untouched by any of this.
import { describe, it, expect, beforeEach, vi } from "vitest";
import { useChatStore, inFlightKey } from "../stores/chatStore";
import { switchToThread } from "./threadNavigation";
import type { Thread } from "../types/api";

const getMessages = vi.fn();
const getAgent = vi.fn();
const getAgents = vi.fn();

vi.mock("./api", () => ({
  getAgent: (...args: unknown[]) => getAgent(...args),
  getAgents: (...args: unknown[]) => getAgents(...args),
  getMessages: (...args: unknown[]) => getMessages(...args),
}));

const AGENT = "agent-bg-finalize";
const THREAD_Z = "thread-z-previously-viewed";
const THREAD_Y = "thread-y-background-run";

function makeThread(id: string, kind: Thread["kind"]): Thread {
  return {
    id,
    title: null,
    scope: { type: "AgentChat", agent_id: AGENT },
    transcript_path: "",
    kind,
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
  };
}

beforeEach(() => {
  useChatStore.getState().reset();
  getMessages.mockReset();
  getAgent.mockReset();
  getAgents.mockReset();
  getAgent.mockResolvedValue({ id: AGENT, name: "Agent", emoji: "🤖" });
  getAgents.mockResolvedValue([]);
});

describe("opening a background-finalized thread never depends on a full app reload", () => {
  it("switchToThread cache-misses the never-visited target thread and re-fetches it straight from the backend, leaving the other thread's own cache entry untouched", async () => {
    const threads = [makeThread(THREAD_Z, "fresh"), makeThread(THREAD_Y, "fresh")];
    const cacheKeyZ = inFlightKey(AGENT, THREAD_Z);

    // Operator was viewing thread Z; its transcript is cached under Z's OWN
    // composite key (messageCache is keyed per-thread, not per-agent).
    useChatStore.setState({
      selectedAgentId: AGENT,
      threadsByAgent: new Map([[AGENT, threads]]),
      selectedThreadIdByAgent: new Map([[AGENT, THREAD_Z]]),
      messages: [{ ts: "t0", role: { agent: AGENT }, content: "Z's own reply", event_type: "message" }],
      allMessages: [{ ts: "t0", role: { agent: AGENT }, content: "Z's own reply", event_type: "message" }],
      messageCache: new Map([
        [
          cacheKeyZ,
          {
            allMessages: [{ ts: "t0", role: { agent: AGENT }, content: "Z's own reply", event_type: "message" }],
            displayCount: 1,
            lastAccessed: 0,
            cursor: null,
          },
        ],
      ]),
    });

    // A "Fresh" assignment thread policy mints Y and its run finalizes while
    // the operator is still looking at Z — the finalize path (chatStore.ts)
    // deliberately skips writing into messages/allMessages/messageCache for
    // this non-selected thread, only marking it unread.
    const keyY = inFlightKey(AGENT, THREAD_Y);
    useChatStore.getState().ensureInFlight(keyY);
    useChatStore.getState().finalizeInFlightText(keyY, "background reply on Y");

    expect(useChatStore.getState().unreadThreadIds.has(keyY)).toBe(true);
    // Confirms finalize did NOT touch Z's cache entry, and never created one
    // for Y (Y was never cached to begin with).
    expect(useChatStore.getState().messageCache.get(cacheKeyZ)?.allMessages.map((m) => m.content)).toEqual([
      "Z's own reply",
    ]);
    expect(useChatStore.getState().messageCache.has(inFlightKey(AGENT, THREAD_Y))).toBe(false);

    // The backend already has Y's finalized message on disk by this point
    // (persist-before-emit ordering) — the mocked fetch stands in for that.
    getMessages.mockResolvedValue({
      messages: [{ ts: "t1", role: { agent: AGENT }, content: "background reply on Y", event_type: "message" }],
      cursor: null,
    });

    // Exactly what clicking Y's tab does (ThreadTabStrip -> ChatView's
    // handleSelectThread -> switchToThread).
    await switchToThread(AGENT, THREAD_Y);

    // Fetched Y specifically, from the network — not a cache read (Y's own
    // composite key was never in the cache). `selectAgent` passes its
    // per-selection AbortController's signal through as the 3rd argument.
    expect(getMessages).toHaveBeenCalledWith(AGENT, THREAD_Y, expect.any(AbortSignal));
    // Rendered messages are Y's freshly-fetched, complete transcript — no
    // app reload involved.
    expect(useChatStore.getState().messages.map((m) => m.content)).toEqual(["background reply on Y"]);
    expect(useChatStore.getState().allMessages.map((m) => m.content)).toEqual(["background reply on Y"]);
    // Z's cache entry is completely untouched by the switch to Y.
    expect(useChatStore.getState().messageCache.get(cacheKeyZ)?.allMessages.map((m) => m.content)).toEqual([
      "Z's own reply",
    ]);
  });
});
