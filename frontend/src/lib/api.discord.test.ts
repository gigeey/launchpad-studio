/**
 * Tests for the per-agent Discord channel API client functions in lib/api.ts.
 *
 * Covers:
 * - upsertDiscordChannel: PUTs the right URL/body, returns the updated ChannelStatus, throws
 *   ApiError with the backend's {"error": ...} message on 400 (e.g. blank dm_role_auth_guild).
 * - setDiscordChannelSecret: PUTs the right URL/body ({ bot_token }), returns ChannelStatus with
 *   secret_stored: true, never echoes the bot token, throws on 400 (empty token).
 * - deleteDiscordChannel: DELETEs the right URL, resolves on 204, throws on non-2xx.
 *
 * getAgentChannels is shared with Email and already covered in api.email.test.ts.
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
  upsertDiscordChannel,
  setDiscordChannelSecret,
  deleteDiscordChannel,
  ApiError,
  type DiscordChannelConfig,
} from "./api";

const VALID_CONFIG: DiscordChannelConfig = {
  allowed_users: ["12345"],
  allowed_roles: ["67890"],
  allowed_channels: ["11111"],
  dm_role_auth_guild: "22222",
  require_mention: true,
  thread_follow: "sticky_decay",
  thread_idle_timeout_minutes: 15,
  thread_message_budget: 10,
  backfill_limit: 20,
  enabled: false,
};

// ---------------------------------------------------------------------------
// upsertDiscordChannel
// ---------------------------------------------------------------------------

describe("upsertDiscordChannel", () => {
  it("PUTs /agents/{agentId}/channels/discord with the config body and returns the updated status", async () => {
    const responseBody = {
      binding_id: "discord",
      kind: "discord",
      enabled: false,
      bridge_thread_provisioned: false,
      allowed_senders: [],
      secret_stored: false,
      kind_config: { ...VALID_CONFIG },
    };
    mockFetch.mockResolvedValue(mockOk(responseBody));

    const result = await upsertDiscordChannel(AGENT_ID, VALID_CONFIG);

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/channels\/discord$/);
    expect(init.method).toBe("PUT");
    expect(JSON.parse(init.body)).toEqual(VALID_CONFIG);
    expect(result).toEqual(responseBody);
  });

  it("sends a null dm_role_auth_guild as-is", async () => {
    const responseBody = {
      binding_id: "discord",
      kind: "discord",
      enabled: false,
      bridge_thread_provisioned: false,
      allowed_senders: [],
      secret_stored: false,
      kind_config: { ...VALID_CONFIG, dm_role_auth_guild: null },
    };
    mockFetch.mockResolvedValue(mockOk(responseBody));

    await upsertDiscordChannel(AGENT_ID, { ...VALID_CONFIG, dm_role_auth_guild: null });

    const [, init] = mockFetch.mock.calls[0];
    expect(JSON.parse(init.body).dm_role_auth_guild).toBeNull();
  });

  it("throws ApiError with the backend message on invalid config (400)", async () => {
    mockFetch.mockResolvedValue(mockError(400, "dm_role_auth_guild must not be blank"));

    await expect(upsertDiscordChannel(AGENT_ID, VALID_CONFIG)).rejects.toMatchObject({
      status: 400,
      message: "dm_role_auth_guild must not be blank",
    });
  });

  it("throws ApiError with the agent id for an unknown agent (404)", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(upsertDiscordChannel(AGENT_ID, VALID_CONFIG)).rejects.toBeInstanceOf(ApiError);
    await expect(upsertDiscordChannel(AGENT_ID, VALID_CONFIG)).rejects.toMatchObject({ status: 404 });
  });
});

// ---------------------------------------------------------------------------
// setDiscordChannelSecret
// ---------------------------------------------------------------------------

describe("setDiscordChannelSecret", () => {
  it("PUTs /agents/{agentId}/channels/discord/secret with the bot_token body and returns secret_stored: true", async () => {
    const responseBody = {
      binding_id: "discord",
      kind: "discord",
      enabled: false,
      bridge_thread_provisioned: false,
      allowed_senders: [],
      secret_stored: true,
      kind_config: { ...VALID_CONFIG },
    };
    mockFetch.mockResolvedValue(mockOk(responseBody));

    const result = await setDiscordChannelSecret(AGENT_ID, "abc123.def456.ghi789");

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/channels\/discord\/secret$/);
    expect(init.method).toBe("PUT");
    expect(JSON.parse(init.body)).toEqual({ bot_token: "abc123.def456.ghi789" });
    expect(result).toEqual(responseBody);
    expect(result.secret_stored).toBe(true);
    // Never echoes the bot token back onto the returned status.
    expect(JSON.stringify(result)).not.toContain("abc123.def456.ghi789");
  });

  it("throws ApiError with the backend message on an empty token (400)", async () => {
    mockFetch.mockResolvedValue(mockError(400, "bot_token must not be empty"));

    await expect(setDiscordChannelSecret(AGENT_ID, "   ")).rejects.toMatchObject({
      status: 400,
      message: "bot_token must not be empty",
    });
  });

  it("throws ApiError on unknown agent (404)", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(setDiscordChannelSecret(AGENT_ID, "abc123.def456.ghi789")).rejects.toMatchObject({ status: 404 });
  });
});

// ---------------------------------------------------------------------------
// deleteDiscordChannel
// ---------------------------------------------------------------------------

describe("deleteDiscordChannel", () => {
  it("DELETEs /agents/{agentId}/channels/discord and resolves on 204", async () => {
    mockFetch.mockResolvedValue(mockNoContent());

    await expect(deleteDiscordChannel(AGENT_ID)).resolves.toBeUndefined();

    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/channels\/discord$/);
    expect(init.method).toBe("DELETE");
  });

  it("throws ApiError on non-2xx response", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(deleteDiscordChannel(AGENT_ID)).rejects.toMatchObject({ status: 404 });
  });
});
