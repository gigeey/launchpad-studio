/**
 * Tests for thread API client functions in lib/api.ts.
 *
 * Covers:
 * - listThreads: correct URL, returns parsed Thread[]
 * - createThread: correct URL, method, body
 * - getThread: correct URL
 * - renameThread: correct URL, method, body
 * - deleteThread: correct URL, method; rejects on non-2xx
 * - getMessages: thread_id included as query param when provided
 * - getMessagesBefore: thread_id included in query params when provided
 * - sendMessage: thread_id included in body when provided; omitted otherwise
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import type { Thread } from "../types/api";

const AGENT_ID = "agent-abc";
const THREAD_ID = "thread-xyz";

const mockThread: Thread = {
  id: THREAD_ID,
  title: "Test thread",
  scope: { type: "AgentChat", agent_id: AGENT_ID },
  transcript_path: `/data/messages/threads/${THREAD_ID}.jsonl`,
  kind: "fresh",
  created_at: "2026-06-30T00:00:00Z",
  updated_at: "2026-06-30T00:00:00Z",
};

function mockOk(body: unknown, status = 200): Response {
  return {
    ok: true,
    status,
    json: () => Promise.resolve(body),
    text: () => Promise.resolve(JSON.stringify(body)),
  } as unknown as Response;
}

function mockError(status: number, body = "error"): Response {
  return {
    ok: false,
    status,
    json: () => Promise.resolve({ error: body }),
    text: () => Promise.resolve(body),
  } as unknown as Response;
}

const mockFetch = vi.fn();

beforeEach(() => {
  vi.stubGlobal("fetch", mockFetch);
  mockFetch.mockClear();
});

import {
  listThreads,
  createThread,
  getThread,
  renameThread,
  deleteThread,
  getMessages,
  getMessagesBefore,
  sendMessage,
} from "./api";

// ---------------------------------------------------------------------------
// listThreads
// ---------------------------------------------------------------------------

describe("listThreads", () => {
  it("GETs /agents/{agentId}/threads and returns the thread array", async () => {
    mockFetch.mockResolvedValue(mockOk([mockThread]));

    const result = await listThreads(AGENT_ID);

    expect(mockFetch).toHaveBeenCalledOnce();
    const url: string = mockFetch.mock.calls[0][0];
    expect(url).toMatch(/\/agents\/agent-abc\/threads$/);
    expect(result).toEqual([mockThread]);
  });
});

// ---------------------------------------------------------------------------
// createThread
// ---------------------------------------------------------------------------

describe("createThread", () => {
  it("POSTs to /agents/{agentId}/threads with the request body", async () => {
    mockFetch.mockResolvedValue(mockOk(mockThread));

    const req = { kind: "fresh" as const, title: "New thread" };
    const result = await createThread(AGENT_ID, req);

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/threads$/);
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body)).toEqual(req);
    expect(result).toEqual(mockThread);
  });

  it("includes branch_source for branch threads", async () => {
    mockFetch.mockResolvedValue(mockOk(mockThread));

    const branchSource = {
      source_thread_id: "parent-thread",
      branch_at: "2026-06-30T00:30:00Z",
      source_message_id: null,
    };
    await createThread(AGENT_ID, { kind: "branch", branch_source: branchSource });

    const body = JSON.parse(mockFetch.mock.calls[0][1].body);
    expect(body.branch_source).toEqual(branchSource);
  });
});

// ---------------------------------------------------------------------------
// getThread
// ---------------------------------------------------------------------------

describe("getThread", () => {
  it("GETs /threads/{threadId} and returns the thread", async () => {
    mockFetch.mockResolvedValue(mockOk(mockThread));

    const result = await getThread(THREAD_ID);

    const url: string = mockFetch.mock.calls[0][0];
    expect(url).toMatch(/\/threads\/thread-xyz$/);
    expect(result).toEqual(mockThread);
  });
});

// ---------------------------------------------------------------------------
// renameThread
// ---------------------------------------------------------------------------

describe("renameThread", () => {
  it("PATCHes /threads/{threadId} with the new title", async () => {
    const renamed = { ...mockThread, title: "Renamed" };
    mockFetch.mockResolvedValue(mockOk(renamed));

    const result = await renameThread(THREAD_ID, "Renamed");

    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/threads\/thread-xyz$/);
    expect(init.method).toBe("PATCH");
    expect(JSON.parse(init.body)).toEqual({ title: "Renamed" });
    expect(result).toEqual(renamed);
  });

  it("passes null to clear the title", async () => {
    mockFetch.mockResolvedValue(mockOk({ ...mockThread, title: null }));

    await renameThread(THREAD_ID, null);

    const body = JSON.parse(mockFetch.mock.calls[0][1].body);
    expect(body.title).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// deleteThread
// ---------------------------------------------------------------------------

describe("deleteThread", () => {
  it("DELETEs /threads/{threadId} on success (204)", async () => {
    mockFetch.mockResolvedValue({ ok: true, status: 204 });

    await deleteThread(THREAD_ID);

    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/threads\/thread-xyz$/);
    expect(init.method).toBe("DELETE");
  });

  it("throws on non-2xx response", async () => {
    mockFetch.mockResolvedValue(mockError(400, "cannot delete default thread"));

    await expect(deleteThread(THREAD_ID)).rejects.toThrow("400");
  });
});

// ---------------------------------------------------------------------------
// getMessages — thread_id query param
// ---------------------------------------------------------------------------

describe("getMessages thread_id", () => {
  it("omits thread_id from URL when not provided", async () => {
    mockFetch.mockResolvedValue(mockOk({ messages: [], cursor: null }));

    await getMessages(AGENT_ID);

    const url: string = mockFetch.mock.calls[0][0];
    expect(url).not.toContain("thread_id");
  });

  it("appends thread_id as query param when provided", async () => {
    mockFetch.mockResolvedValue(mockOk({ messages: [], cursor: null }));

    await getMessages(AGENT_ID, THREAD_ID);

    const url: string = mockFetch.mock.calls[0][0];
    expect(url).toContain(`thread_id=${encodeURIComponent(THREAD_ID)}`);
  });
});

// ---------------------------------------------------------------------------
// getMessagesBefore — thread_id in pagination params
// ---------------------------------------------------------------------------

describe("getMessagesBefore thread_id", () => {
  const cursor = { byte_offset: 100, last_message_id: "msg-1", timestamp: "2026-06-30T00:00:00Z" };

  it("omits thread_id when not provided", async () => {
    mockFetch.mockResolvedValue(mockOk({ messages: [], cursor: null }));

    await getMessagesBefore(AGENT_ID, cursor);

    const url: string = mockFetch.mock.calls[0][0];
    expect(url).not.toContain("thread_id");
  });

  it("includes thread_id in query string when provided", async () => {
    mockFetch.mockResolvedValue(mockOk({ messages: [], cursor: null }));

    await getMessagesBefore(AGENT_ID, cursor, 50, THREAD_ID);

    const url: string = mockFetch.mock.calls[0][0];
    expect(url).toContain(`thread_id=${encodeURIComponent(THREAD_ID)}`);
  });
});

// ---------------------------------------------------------------------------
// sendMessage — thread_id in body
// ---------------------------------------------------------------------------

describe("sendMessage thread_id", () => {
  it("omits thread_id from body when not provided", async () => {
    mockFetch.mockResolvedValue(mockOk({ message_id: "m1", status: "queued" }));

    await sendMessage(AGENT_ID, "Hello");

    const body = JSON.parse(mockFetch.mock.calls[0][1].body);
    expect(body.thread_id).toBeUndefined();
  });

  it("includes thread_id in body when provided", async () => {
    mockFetch.mockResolvedValue(mockOk({ message_id: "m1", status: "queued" }));

    await sendMessage(AGENT_ID, "Hello", undefined, null, THREAD_ID);

    const body = JSON.parse(mockFetch.mock.calls[0][1].body);
    expect(body.thread_id).toBe(THREAD_ID);
  });

  it("keeps other body fields intact when thread_id is added", async () => {
    mockFetch.mockResolvedValue(mockOk({ message_id: "m1", status: "queued" }));

    await sendMessage(AGENT_ID, "With attachments", ["att-1"], "/path/to/focus", THREAD_ID);

    const body = JSON.parse(mockFetch.mock.calls[0][1].body);
    expect(body.content).toBe("With attachments");
    expect(body.attachment_ids).toEqual(["att-1"]);
    expect(body.focus_path).toBe("/path/to/focus");
    expect(body.thread_id).toBe(THREAD_ID);
  });
});
