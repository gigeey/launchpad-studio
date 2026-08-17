/**
 * Tests for the thread dimension of chatStore.
 *
 * Covers:
 * - loadThreads: populates thread list, initializes default selection
 * - loadThreads: respects existing (sticky) selection
 * - loadThreads: fallback id when server returns empty list
 * - selectThread: updates selection by scanning loaded threads
 * - selectThread: falls back to selectedAgentId when thread not in store
 * - createFreshThread: adds thread to list and selects it
 * - branchThread: adds branch thread to list and selects it
 * - renameThread: updates thread in list
 * - archiveThread / unarchiveThread: persist archived_at via the API and patch in place
 * - deleteThread: removes thread and reverts selection to default
 * - deleteThread: does not change selection when a different thread was selected
 * - addThreadLive: appends a server-created thread without a network call
 * - addThreadLive: is a no-op when the thread id is already known
 * - effectiveThreadId resolution: default thread omits thread_id from sendMessage
 * - effectiveThreadId resolution: non-default thread passes thread_id to sendMessage
 * - reset: clears thread state
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import type { Thread } from "../../types/api";

const AGENT_ID = "agent-abc";
const DEFAULT_THREAD_ID = `default-${AGENT_ID}`;

const defaultThread: Thread = {
  id: DEFAULT_THREAD_ID,
  title: null,
  scope: { type: "AgentChat", agent_id: AGENT_ID },
  transcript_path: `/data/messages/${AGENT_ID}.jsonl`,
  kind: "default",
  created_at: "2026-06-30T00:00:00Z",
  updated_at: "2026-06-30T00:00:00Z",
};

const freshThread: Thread = {
  id: "fresh-thread-1",
  title: "Exploration",
  scope: { type: "AgentChat", agent_id: AGENT_ID },
  transcript_path: `/data/messages/threads/fresh-thread-1.jsonl`,
  kind: "fresh",
  created_at: "2026-06-30T01:00:00Z",
  updated_at: "2026-06-30T01:00:00Z",
};

const branchThread: Thread = {
  id: "branch-thread-1",
  title: "Branch from main",
  scope: { type: "AgentChat", agent_id: AGENT_ID },
  transcript_path: `/data/messages/threads/branch-thread-1.jsonl`,
  kind: "branch",
  branch_source: {
    source_thread_id: DEFAULT_THREAD_ID,
    branch_at: "2026-06-30T00:30:00Z",
    source_message_id: null,
  },
  history_floor_ts: "2026-06-30T00:30:00Z",
  created_at: "2026-06-30T02:00:00Z",
  updated_at: "2026-06-30T02:00:00Z",
};

const mockListThreads = vi.fn();
const mockCreateThread = vi.fn();
const mockRenameThread = vi.fn();
const mockDeleteThread = vi.fn();
const mockArchiveThread = vi.fn();
const mockUnarchiveThread = vi.fn();
const mockSendMessage = vi.fn();

vi.mock("../../lib/api", () => ({
  getAgents: vi.fn().mockResolvedValue([]),
  getAgent: vi.fn().mockResolvedValue({
    id: "agent-abc",
    name: "Test",
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
  }),
  getMessages: vi.fn().mockResolvedValue({ messages: [], cursor: null }),
  sendMessage: (...args: unknown[]) => mockSendMessage(...args),
  listThreads: (...args: unknown[]) => mockListThreads(...args),
  createThread: (...args: unknown[]) => mockCreateThread(...args),
  renameThread: (...args: unknown[]) => mockRenameThread(...args),
  deleteThread: (...args: unknown[]) => mockDeleteThread(...args),
  archiveThread: (...args: unknown[]) => mockArchiveThread(...args),
  unarchiveThread: (...args: unknown[]) => mockUnarchiveThread(...args),
}));

import { useChatStore } from "../chatStore";

function store() {
  return useChatStore.getState();
}

beforeEach(() => {
  useChatStore.getState().reset();
  vi.clearAllMocks();
  mockSendMessage.mockResolvedValue({ message_id: "msg-1", status: "queued" });
});

// ---------------------------------------------------------------------------
// loadThreads
// ---------------------------------------------------------------------------

describe("loadThreads", () => {
  it("populates threadsByAgent with the returned list", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);
    expect(store().threadsByAgent.get(AGENT_ID)).toEqual([defaultThread, freshThread]);
  });

  it("selects the default thread when no thread was previously selected", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe(DEFAULT_THREAD_ID);
  });

  it("does not override an already-sticky thread selection", async () => {
    // Pre-select a fresh thread before loading
    useChatStore.setState((s) => {
      const next = new Map(s.selectedThreadIdByAgent);
      next.set(AGENT_ID, freshThread.id);
      return { selectedThreadIdByAgent: next };
    });
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);
    // Selection must remain on the fresh thread, not be reset to default
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe(freshThread.id);
  });

  it("falls back to deterministic default-thread id when list is empty", async () => {
    mockListThreads.mockResolvedValue([]);
    await store().loadThreads(AGENT_ID);
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe(DEFAULT_THREAD_ID);
  });
});

// ---------------------------------------------------------------------------
// selectThread
// ---------------------------------------------------------------------------

describe("selectThread", () => {
  it("updates the selection for the owning agent when thread is in store", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);
    store().selectThread(freshThread.id);
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe(freshThread.id);
  });

  it("falls back to selectedAgentId when thread is not yet in store", () => {
    useChatStore.setState({ selectedAgentId: AGENT_ID });
    store().selectThread("unknown-thread-id");
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe("unknown-thread-id");
  });

  it("misattributes an unloaded thread to the WRONG agent when it isn't the owner — the bug selectThreadForAgent exists to avoid", () => {
    const OTHER_AGENT_ID = "agent-other";
    useChatStore.setState({ selectedAgentId: AGENT_ID }); // viewing AGENT_ID, but the click targets OTHER_AGENT_ID
    store().selectThread("other-agents-thread"); // OTHER_AGENT_ID's threads were never loaded
    // The fallback attributes the thread to the currently-viewed agent, not its real owner.
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe("other-agents-thread");
    expect(store().selectedThreadIdByAgent.get(OTHER_AGENT_ID)).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// selectThreadForAgent
// ---------------------------------------------------------------------------

describe("selectThreadForAgent", () => {
  it("sets the selection for the given agent directly, with no reverse-lookup or fallback", () => {
    const OTHER_AGENT_ID = "agent-other";
    // Simulate the exact scenario that breaks plain `selectThread`: some
    // other agent is currently selected, and the target agent's threads
    // were never loaded (e.g. a ChatSidebar row for an agent whose page
    // isn't open).
    useChatStore.setState({ selectedAgentId: AGENT_ID });
    store().selectThreadForAgent(OTHER_AGENT_ID, "other-agents-thread");
    expect(store().selectedThreadIdByAgent.get(OTHER_AGENT_ID)).toBe("other-agents-thread");
    // And critically, the currently-selected agent's own selection is untouched.
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBeUndefined();
  });

  it("overwrites an existing selection for that agent", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);
    store().selectThreadForAgent(AGENT_ID, "another-thread-id");
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe("another-thread-id");
  });
});

// ---------------------------------------------------------------------------
// archiveThread / unarchiveThread
// ---------------------------------------------------------------------------

describe("archiveThread", () => {
  it("persists archived_at via the API and patches the thread in place", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);

    const archived: Thread = { ...freshThread, archived_at: "2026-07-05T00:00:00Z" };
    mockArchiveThread.mockResolvedValue(archived);

    const result = await store().archiveThread(freshThread.id);

    expect(result).toEqual(archived);
    expect(mockArchiveThread).toHaveBeenCalledWith(freshThread.id);
    const threads = store().threadsByAgent.get(AGENT_ID) ?? [];
    expect(threads.find((t) => t.id === freshThread.id)?.archived_at).toBe("2026-07-05T00:00:00Z");
    // Never deletes anything server-side or locally.
    expect(mockDeleteThread).not.toHaveBeenCalled();
    expect(threads.find((t) => t.id === freshThread.id)).toBeTruthy();
  });
});

describe("unarchiveThread", () => {
  it("clears archived_at via the API and patches the thread in place", async () => {
    const archivedThread: Thread = { ...freshThread, archived_at: "2026-07-05T00:00:00Z" };
    mockListThreads.mockResolvedValue([defaultThread, archivedThread]);
    await store().loadThreads(AGENT_ID);

    const restored: Thread = { ...freshThread, archived_at: null };
    mockUnarchiveThread.mockResolvedValue(restored);

    const result = await store().unarchiveThread(freshThread.id);

    expect(result).toEqual(restored);
    expect(mockUnarchiveThread).toHaveBeenCalledWith(freshThread.id);
    const threads = store().threadsByAgent.get(AGENT_ID) ?? [];
    expect(threads.find((t) => t.id === freshThread.id)?.archived_at).toBeFalsy();
  });
});

// ---------------------------------------------------------------------------
// createFreshThread
// ---------------------------------------------------------------------------

describe("createFreshThread", () => {
  it("adds the new thread to threadsByAgent and selects it", async () => {
    mockListThreads.mockResolvedValue([defaultThread]);
    await store().loadThreads(AGENT_ID);
    mockCreateThread.mockResolvedValue(freshThread);

    const result = await store().createFreshThread(AGENT_ID, "Exploration");

    expect(result).toEqual(freshThread);
    const threads = store().threadsByAgent.get(AGENT_ID) ?? [];
    expect(threads).toContainEqual(freshThread);
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe(freshThread.id);
  });

  it("calls createThread with kind=fresh and the given title", async () => {
    mockListThreads.mockResolvedValue([defaultThread]);
    await store().loadThreads(AGENT_ID);
    mockCreateThread.mockResolvedValue(freshThread);

    await store().createFreshThread(AGENT_ID, "My title");

    expect(mockCreateThread).toHaveBeenCalledWith(AGENT_ID, { kind: "fresh", title: "My title" });
  });

  it("passes null title when none provided", async () => {
    mockListThreads.mockResolvedValue([defaultThread]);
    await store().loadThreads(AGENT_ID);
    mockCreateThread.mockResolvedValue(freshThread);

    await store().createFreshThread(AGENT_ID);

    expect(mockCreateThread).toHaveBeenCalledWith(AGENT_ID, { kind: "fresh", title: null });
  });
});

// ---------------------------------------------------------------------------
// branchThread
// ---------------------------------------------------------------------------

describe("branchThread", () => {
  it("adds the branch thread to threadsByAgent and selects it", async () => {
    mockListThreads.mockResolvedValue([defaultThread]);
    await store().loadThreads(AGENT_ID);
    mockCreateThread.mockResolvedValue(branchThread);

    const params = {
      source_thread_id: DEFAULT_THREAD_ID,
      branch_at: "2026-06-30T00:30:00Z",
    };
    const result = await store().branchThread(AGENT_ID, params, "Branch from main");

    expect(result).toEqual(branchThread);
    const threads = store().threadsByAgent.get(AGENT_ID) ?? [];
    expect(threads).toContainEqual(branchThread);
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe(branchThread.id);
  });

  it("calls createThread with kind=branch and the branch_source", async () => {
    mockListThreads.mockResolvedValue([defaultThread]);
    await store().loadThreads(AGENT_ID);
    mockCreateThread.mockResolvedValue(branchThread);

    const params = {
      source_thread_id: DEFAULT_THREAD_ID,
      branch_at: "2026-06-30T00:30:00Z",
      source_message_id: "msg-abc",
    };
    await store().branchThread(AGENT_ID, params, "Branch");

    expect(mockCreateThread).toHaveBeenCalledWith(AGENT_ID, {
      kind: "branch",
      title: "Branch",
      branch_source: {
        source_thread_id: DEFAULT_THREAD_ID,
        branch_at: "2026-06-30T00:30:00Z",
        source_message_id: "msg-abc",
      },
    });
  });
});

// ---------------------------------------------------------------------------
// renameThread
// ---------------------------------------------------------------------------

describe("renameThread", () => {
  it("updates the thread in threadsByAgent with the returned value", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);

    const renamed: Thread = { ...freshThread, title: "Renamed thread" };
    mockRenameThread.mockResolvedValue(renamed);

    const result = await store().renameThread(freshThread.id, "Renamed thread");

    expect(result).toEqual(renamed);
    const threads = store().threadsByAgent.get(AGENT_ID) ?? [];
    const updated = threads.find((t) => t.id === freshThread.id);
    expect(updated?.title).toBe("Renamed thread");
  });

  it("calls the API with the thread id and new title", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);
    mockRenameThread.mockResolvedValue({ ...freshThread, title: "New name" });

    await store().renameThread(freshThread.id, "New name");

    expect(mockRenameThread).toHaveBeenCalledWith(freshThread.id, "New name");
  });
});

// ---------------------------------------------------------------------------
// patchThreadLive (SSE-driven local patch, no network call)
// ---------------------------------------------------------------------------

describe("patchThreadLive", () => {
  it("patches title in place without calling the rename API", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);

    store().patchThreadLive(freshThread.id, { title: "Renamed via tool" });

    const threads = store().threadsByAgent.get(AGENT_ID) ?? [];
    const updated = threads.find((t) => t.id === freshThread.id);
    expect(updated?.title).toBe("Renamed via tool");
    expect(mockRenameThread).not.toHaveBeenCalled();
  });

  it("patches auto_title without touching title", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);

    store().patchThreadLive(freshThread.id, { auto_title: "Derived from first message" });

    const threads = store().threadsByAgent.get(AGENT_ID) ?? [];
    const updated = threads.find((t) => t.id === freshThread.id);
    expect(updated?.auto_title).toBe("Derived from first message");
    expect(updated?.title).toBe(freshThread.title);
  });

  it("is a no-op when the thread id is not in any agent's list", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);

    expect(() => store().patchThreadLive("unknown-thread", { title: "X" })).not.toThrow();
    const threads = store().threadsByAgent.get(AGENT_ID) ?? [];
    expect(threads.find((t) => t.id === "unknown-thread")).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// addThreadLive (SSE-driven local append, no network call — thread_created)
// ---------------------------------------------------------------------------

describe("addThreadLive", () => {
  it("appends a newly-created thread to the agent's list", async () => {
    mockListThreads.mockResolvedValue([defaultThread]);
    await store().loadThreads(AGENT_ID);

    store().addThreadLive(AGENT_ID, freshThread);

    const threads = store().threadsByAgent.get(AGENT_ID) ?? [];
    expect(threads.map((t) => t.id)).toEqual([defaultThread.id, freshThread.id]);
    expect(mockListThreads).toHaveBeenCalledTimes(1);
  });

  it("is a no-op when the thread id is already in the agent's list", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);

    store().addThreadLive(AGENT_ID, { ...freshThread, title: "Should not apply" });

    const threads = store().threadsByAgent.get(AGENT_ID) ?? [];
    expect(threads).toHaveLength(2);
    expect(threads.find((t) => t.id === freshThread.id)?.title).toBe(freshThread.title);
  });

  it("initializes the agent's list when nothing has been loaded yet", () => {
    store().addThreadLive(AGENT_ID, freshThread);

    const threads = store().threadsByAgent.get(AGENT_ID) ?? [];
    expect(threads.map((t) => t.id)).toEqual([freshThread.id]);
  });
});

// ---------------------------------------------------------------------------
// deleteThread
// ---------------------------------------------------------------------------

describe("deleteThread", () => {
  it("removes the thread from threadsByAgent", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);
    mockDeleteThread.mockResolvedValue(undefined);

    await store().deleteThread(freshThread.id);

    const threads = store().threadsByAgent.get(AGENT_ID) ?? [];
    expect(threads.find((t) => t.id === freshThread.id)).toBeUndefined();
  });

  it("reverts selection to default when the deleted thread was selected", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);
    store().selectThread(freshThread.id);
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe(freshThread.id);

    mockDeleteThread.mockResolvedValue(undefined);
    await store().deleteThread(freshThread.id);

    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe(DEFAULT_THREAD_ID);
  });

  it("does not change selection when a different thread was deleted", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);
    // Keep the default selected
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe(DEFAULT_THREAD_ID);

    mockDeleteThread.mockResolvedValue(undefined);
    await store().deleteThread(freshThread.id);

    // Still on default
    expect(store().selectedThreadIdByAgent.get(AGENT_ID)).toBe(DEFAULT_THREAD_ID);
  });
});

// ---------------------------------------------------------------------------
// Thread id resolution in sendMessage
// ---------------------------------------------------------------------------

describe("effective thread_id in sendMessage", () => {
  it("omits thread_id when the default thread is selected", async () => {
    // Select the default thread
    useChatStore.setState({ selectedAgentId: AGENT_ID });
    useChatStore.setState((s) => {
      const next = new Map(s.selectedThreadIdByAgent);
      next.set(AGENT_ID, DEFAULT_THREAD_ID);
      return { selectedThreadIdByAgent: next };
    });

    await store().sendMessage("Hello");

    // thread_id should not appear in the call (5th argument is undefined)
    expect(mockSendMessage).toHaveBeenCalledWith(
      AGENT_ID,
      "Hello",
      undefined,
      undefined,
      undefined,
    );
  });

  it("passes thread_id when a non-default thread is selected", async () => {
    useChatStore.setState({ selectedAgentId: AGENT_ID });
    useChatStore.setState((s) => {
      const next = new Map(s.selectedThreadIdByAgent);
      next.set(AGENT_ID, freshThread.id);
      return { selectedThreadIdByAgent: next };
    });

    await store().sendMessage("Hello from fresh thread");

    expect(mockSendMessage).toHaveBeenCalledWith(
      AGENT_ID,
      "Hello from fresh thread",
      undefined,
      undefined,
      freshThread.id,
    );
  });

  it("omits thread_id when no thread is selected (initial state)", async () => {
    useChatStore.setState({ selectedAgentId: AGENT_ID });
    // selectedThreadIdByAgent is empty for this agent

    await store().sendMessage("First message");

    expect(mockSendMessage).toHaveBeenCalledWith(
      AGENT_ID,
      "First message",
      undefined,
      undefined,
      undefined,
    );
  });
});

// ---------------------------------------------------------------------------
// reset
// ---------------------------------------------------------------------------

describe("reset", () => {
  it("clears threadsByAgent and selectedThreadIdByAgent", async () => {
    mockListThreads.mockResolvedValue([defaultThread, freshThread]);
    await store().loadThreads(AGENT_ID);
    expect(store().threadsByAgent.size).toBeGreaterThan(0);

    store().reset();

    expect(store().threadsByAgent.size).toBe(0);
    expect(store().selectedThreadIdByAgent.size).toBe(0);
  });
});
