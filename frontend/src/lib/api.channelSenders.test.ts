/**
 * Tests for the generic per-binding sender allow-list API client functions
 * in lib/api.ts.
 *
 * Covers:
 * - getChannelSenders: GETs the right URL, returns the parsed sender list, throws on 404.
 * - setChannelSenders: PUTs the right URL/body ({ senders }), returns the updated list,
 *   throws ApiError with the backend's {"error": ...} message on non-2xx.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";

const AGENT_ID = "agent-abc";
const BINDING_ID = "email";

function mockOk(body: unknown, status = 200): Response {
  return {
    ok: true,
    status,
    json: () => Promise.resolve(body),
    text: () => Promise.resolve(JSON.stringify(body)),
  } as unknown as Response;
}

function mockError(status: number, error: string): Response {
  return {
    ok: false,
    status,
    json: () => Promise.resolve({ error }),
    text: () => Promise.resolve(JSON.stringify({ error })),
  } as unknown as Response;
}

const mockFetch = vi.fn();

beforeEach(() => {
  vi.stubGlobal("fetch", mockFetch);
  mockFetch.mockClear();
});

import { getChannelSenders, setChannelSenders, ApiError } from "./api";

describe("getChannelSenders", () => {
  it("GETs /agents/{agentId}/channels/{bindingId}/senders and returns the parsed list", async () => {
    mockFetch.mockResolvedValue(mockOk({ senders: ["boss@example.com"] }));

    const result = await getChannelSenders(AGENT_ID, BINDING_ID);

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/channels\/email\/senders$/);
    expect(init).toBeUndefined();
    expect(result).toEqual({ senders: ["boss@example.com"] });
  });

  it("returns an empty list for a binding with no allow-list configured", async () => {
    mockFetch.mockResolvedValue(mockOk({ senders: [] }));

    const result = await getChannelSenders(AGENT_ID, BINDING_ID);

    expect(result).toEqual({ senders: [] });
  });

  it("throws ApiError on unknown agent (404)", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(getChannelSenders(AGENT_ID, BINDING_ID)).rejects.toMatchObject({ status: 404 });
  });
});

describe("setChannelSenders", () => {
  it("PUTs /agents/{agentId}/channels/{bindingId}/senders with { senders } and returns the updated list", async () => {
    mockFetch.mockResolvedValue(mockOk({ senders: ["boss@example.com", "@example.org"] }));

    const result = await setChannelSenders(AGENT_ID, BINDING_ID, ["boss@example.com", "@example.org"]);

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/channels\/email\/senders$/);
    expect(init.method).toBe("PUT");
    expect(JSON.parse(init.body)).toEqual({ senders: ["boss@example.com", "@example.org"] });
    expect(result).toEqual({ senders: ["boss@example.com", "@example.org"] });
  });

  it("never touches the rest of the binding's config — only the senders body is sent", async () => {
    mockFetch.mockResolvedValue(mockOk({ senders: [] }));

    await setChannelSenders(AGENT_ID, BINDING_ID, []);

    const [, init] = mockFetch.mock.calls[0];
    expect(Object.keys(JSON.parse(init.body))).toEqual(["senders"]);
  });

  it("throws ApiError with the backend message on non-2xx", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(setChannelSenders(AGENT_ID, BINDING_ID, [])).rejects.toBeInstanceOf(ApiError);
    await expect(setChannelSenders(AGENT_ID, BINDING_ID, [])).rejects.toMatchObject({ status: 404 });
  });
});
