/**
 * Tests for the per-agent Telegram bridge API client functions in lib/api.ts.
 *
 * Covers:
 * - setTelegramToken: PUTs the right URL/body, returns { bot_username }, throws
 *   ApiError with the backend's {"error": ...} message on 400 (invalid/empty token).
 * - deleteTelegramToken: DELETEs the right URL, resolves on 204, throws on non-2xx.
 * - getTelegramStatus: GETs the right URL, returns the parsed status (including
 *   allowed_chat_ids + pending_pairing_code), throws on 404.
 * - createTelegramPairingCode: POSTs the right URL, returns { code, expires_at_unix }.
 * - unlinkTelegramChat: DELETEs the right URL (with numeric chatId), returns
 *   { allowed_chat_ids }.
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
  setTelegramToken,
  deleteTelegramToken,
  getTelegramStatus,
  createTelegramPairingCode,
  unlinkTelegramChat,
  ApiError,
} from "./api";

// ---------------------------------------------------------------------------
// setTelegramToken
// ---------------------------------------------------------------------------

describe("setTelegramToken", () => {
  it("PUTs /agents/{agentId}/telegram/token with the token body and returns bot_username", async () => {
    mockFetch.mockResolvedValue(mockOk({ bot_username: "axew_research_bot" }));

    const result = await setTelegramToken(AGENT_ID, "123456:ABC-DEF");

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/telegram\/token$/);
    expect(init.method).toBe("PUT");
    expect(JSON.parse(init.body)).toEqual({ token: "123456:ABC-DEF" });
    expect(result).toEqual({ bot_username: "axew_research_bot" });
  });

  it("throws ApiError with the backend message on an invalid token (400)", async () => {
    mockFetch.mockResolvedValue(mockError(400, "invalid Telegram bot token"));

    await expect(setTelegramToken(AGENT_ID, "bogus")).rejects.toMatchObject({
      status: 400,
      message: "invalid Telegram bot token",
    });
  });

  it("throws ApiError with the backend message on an empty/whitespace token (400)", async () => {
    mockFetch.mockResolvedValue(mockError(400, "token must not be empty"));

    await expect(setTelegramToken(AGENT_ID, "   ")).rejects.toMatchObject({
      status: 400,
      message: "token must not be empty",
    });
  });

  it("throws ApiError with the agent id for an unknown agent (404)", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(setTelegramToken(AGENT_ID, "123456:ABC-DEF")).rejects.toBeInstanceOf(ApiError);
    await expect(setTelegramToken(AGENT_ID, "123456:ABC-DEF")).rejects.toMatchObject({ status: 404 });
  });
});

// ---------------------------------------------------------------------------
// deleteTelegramToken
// ---------------------------------------------------------------------------

describe("deleteTelegramToken", () => {
  it("DELETEs /agents/{agentId}/telegram/token and resolves on 204", async () => {
    mockFetch.mockResolvedValue(mockNoContent());

    await expect(deleteTelegramToken(AGENT_ID)).resolves.toBeUndefined();

    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/telegram\/token$/);
    expect(init.method).toBe("DELETE");
  });

  it("throws ApiError on non-2xx response", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(deleteTelegramToken(AGENT_ID)).rejects.toMatchObject({ status: 404 });
  });
});

// ---------------------------------------------------------------------------
// getTelegramStatus
// ---------------------------------------------------------------------------

describe("getTelegramStatus", () => {
  it("GETs /agents/{agentId}/telegram/status and returns the parsed status", async () => {
    const statusBody = {
      has_token: true,
      bot_username: "axew_research_bot",
      enabled: true,
      linked: false,
      allowed_chat_ids: [111, 222],
      pending_pairing_code: { code: "ABC123", expires_at_unix: 1_700_000_600 },
    };
    mockFetch.mockResolvedValue(mockOk(statusBody));

    const result = await getTelegramStatus(AGENT_ID);

    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/telegram\/status$/);
    expect(init).toBeUndefined();
    expect(result).toEqual(statusBody);
  });

  it("returns the not-configured shape when no token is stored", async () => {
    const statusBody = {
      has_token: false,
      bot_username: null,
      enabled: false,
      linked: false,
      allowed_chat_ids: [],
      pending_pairing_code: null,
    };
    mockFetch.mockResolvedValue(mockOk(statusBody));

    const result = await getTelegramStatus(AGENT_ID);

    expect(result).toEqual(statusBody);
  });

  it("throws ApiError on unknown agent (404)", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(getTelegramStatus(AGENT_ID)).rejects.toMatchObject({ status: 404 });
  });
});

// ---------------------------------------------------------------------------
// createTelegramPairingCode
// ---------------------------------------------------------------------------

describe("createTelegramPairingCode", () => {
  it("POSTs /agents/{agentId}/telegram/pairing-code and returns the parsed code", async () => {
    const codeBody = { code: "ABC123", expires_at_unix: 1_700_000_600 };
    mockFetch.mockResolvedValue(mockOk(codeBody));

    const result = await createTelegramPairingCode(AGENT_ID);

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/telegram\/pairing-code$/);
    expect(init.method).toBe("POST");
    expect(result).toEqual(codeBody);
  });

  it("throws ApiError on unknown agent (404)", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(createTelegramPairingCode(AGENT_ID)).rejects.toMatchObject({ status: 404 });
  });
});

// ---------------------------------------------------------------------------
// unlinkTelegramChat
// ---------------------------------------------------------------------------

describe("unlinkTelegramChat", () => {
  it("DELETEs /agents/{agentId}/telegram/chats/{chatId} and returns the updated allow-list", async () => {
    const resultBody = { allowed_chat_ids: [222] };
    mockFetch.mockResolvedValue(mockOk(resultBody));

    const result = await unlinkTelegramChat(AGENT_ID, 111);

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/agents\/agent-abc\/telegram\/chats\/111$/);
    expect(init.method).toBe("DELETE");
    expect(result).toEqual(resultBody);
  });

  it("throws ApiError on non-2xx response", async () => {
    mockFetch.mockResolvedValue(mockError(404, AGENT_ID));

    await expect(unlinkTelegramChat(AGENT_ID, 111)).rejects.toMatchObject({ status: 404 });
  });
});
