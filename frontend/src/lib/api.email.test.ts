/**
 * Tests for the per-agent Email channel API client functions in lib/api.ts.
 *
 * Covers:
 * - getAgentChannels: GETs the right URL, returns the parsed binding list, throws on 404.
 * - upsertEmailChannel: PUTs the right URL/body, returns the updated ChannelStatus, throws
 *   ApiError with the backend's {"error": ...} message on 400 (invalid config).
 * - setEmailChannelSecret: PUTs the right URL/body, returns ChannelStatus with
 *   secret_stored: true, never echoes the password, throws on 400 (empty password).
 * - deleteEmailChannel: DELETEs the right URL, resolves on 204, throws on non-2xx.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";

const AGENT_ID = "agent-abc";

function mockOk(body: unknown, status = 200): Response {
  return {
    ok: true,
    status,
    json: () => Promise.resolve(body),
    text: () => Promise.resolve(JSON.stringify(body)),
  } as unknown as Response;
}

function mockNoContent(): Response {
  return {
    ok: true,
    status: 204,
    json: () => Promise.resolve(undefined),
    text: () => Promise.resolve(""),
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

import {
  getAgentChannels,
  upsertEmailChannel,
  setEmailChannelSecret,
  deleteEmailChannel,
  ApiError,
  type EmailChannelConfig,
} from "./api";

const VALID_CONFIG: EmailChannelConfig = {
  address: "agent@example.com",
  imap_host: "imap.example.com",
  imap_port: 993,
  smtp_host: "smtp.example.com",
  smtp_port: 587,
  poll_secs: 60,
  require_auth_results: true,
  allowed_senders: ["boss@example.com"],
  enabled: false,
};

// ---------------------------------------------------------------------------
// getAgentChannels
// ---------------------------------------------------------------------------

describe("getAgentChannels", () => {
  it("GETs /agents/{agentId}/channels and returns the parsed binding list", async () => {
    const channels = [
      {
        binding_id: "email",
        kind: "email",
        enabled: true,
        bridge_thread_provisioned: true,
        allowed_senders: ["boss@example.com"],
        secret_stored: true,
        kind_config: { ...VALID_CONFIG },
      },
    ];
    mockFetch.mockResolvedValue(mockOk(channels));

    const result = await getAgentChannels(AGENT_ID);

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/channels$/);
    expect(init).toBeUndefined();
    expect(result).toEqual(channels);
  });

  it("returns an empty list for an agent with no channels configured", async () => {
    mockFetch.mockResolvedValue(mockOk([]));

    const result = await getAgentChannels(AGENT_ID);

    expect(result).toEqual([]);
  });

  it("throws ApiError on unknown agent (404)", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(getAgentChannels(AGENT_ID)).rejects.toMatchObject({ status: 404 });
  });
});

// ---------------------------------------------------------------------------
// upsertEmailChannel
// ---------------------------------------------------------------------------

describe("upsertEmailChannel", () => {
  it("PUTs /agents/{agentId}/channels/email with the config body and returns the updated status", async () => {
    const responseBody = {
      binding_id: "email",
      kind: "email",
      enabled: false,
      bridge_thread_provisioned: false,
      allowed_senders: ["boss@example.com"],
      secret_stored: false,
      kind_config: { ...VALID_CONFIG },
    };
    mockFetch.mockResolvedValue(mockOk(responseBody));

    const result = await upsertEmailChannel(AGENT_ID, VALID_CONFIG);

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/channels\/email$/);
    expect(init.method).toBe("PUT");
    expect(JSON.parse(init.body)).toEqual(VALID_CONFIG);
    expect(result).toEqual(responseBody);
  });

  it("throws ApiError with the backend message on invalid config (400)", async () => {
    mockFetch.mockResolvedValue(mockError(400, "a valid email address is required"));

    await expect(upsertEmailChannel(AGENT_ID, { ...VALID_CONFIG, address: "not-an-email" })).rejects.toMatchObject({
      status: 400,
      message: "a valid email address is required",
    });
  });

  it("throws ApiError with the agent id for an unknown agent (404)", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(upsertEmailChannel(AGENT_ID, VALID_CONFIG)).rejects.toBeInstanceOf(ApiError);
    await expect(upsertEmailChannel(AGENT_ID, VALID_CONFIG)).rejects.toMatchObject({ status: 404 });
  });
});

// ---------------------------------------------------------------------------
// setEmailChannelSecret
// ---------------------------------------------------------------------------

describe("setEmailChannelSecret", () => {
  it("PUTs /agents/{agentId}/channels/email/secret with the password body and returns secret_stored: true", async () => {
    const responseBody = {
      binding_id: "email",
      kind: "email",
      enabled: false,
      bridge_thread_provisioned: false,
      allowed_senders: [],
      secret_stored: true,
      kind_config: { ...VALID_CONFIG },
    };
    mockFetch.mockResolvedValue(mockOk(responseBody));

    const result = await setEmailChannelSecret(AGENT_ID, "hunter2-app-password");

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/channels\/email\/secret$/);
    expect(init.method).toBe("PUT");
    expect(JSON.parse(init.body)).toEqual({ password: "hunter2-app-password" });
    expect(result).toEqual(responseBody);
    expect(result.secret_stored).toBe(true);
    // Never echoes the password back onto the returned status.
    expect(JSON.stringify(result)).not.toContain("hunter2");
  });

  it("throws ApiError with the backend message on an empty password (400)", async () => {
    mockFetch.mockResolvedValue(mockError(400, "password must not be empty"));

    await expect(setEmailChannelSecret(AGENT_ID, "   ")).rejects.toMatchObject({
      status: 400,
      message: "password must not be empty",
    });
  });

  it("throws ApiError on unknown agent (404)", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(setEmailChannelSecret(AGENT_ID, "app-password")).rejects.toMatchObject({ status: 404 });
  });
});

// ---------------------------------------------------------------------------
// deleteEmailChannel
// ---------------------------------------------------------------------------

describe("deleteEmailChannel", () => {
  it("DELETEs /agents/{agentId}/channels/email and resolves on 204", async () => {
    mockFetch.mockResolvedValue(mockNoContent());

    await expect(deleteEmailChannel(AGENT_ID)).resolves.toBeUndefined();

    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/channels\/email$/);
    expect(init.method).toBe("DELETE");
  });

  it("throws ApiError on non-2xx response", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(deleteEmailChannel(AGENT_ID)).rejects.toMatchObject({ status: 404 });
  });
});
