/**
 * Tests for getProviderModels (`GET /providers/{name}/models`) in lib/api.ts.
 *
 * Covers:
 * - Success: parses the bare model-ID array and passes an AbortSignal deadline.
 * - Non-2xx: wraps the body's `error`/`code` into a ProviderModelDiscoveryError.
 * - Client-side timeout: fetch rejecting with the `AbortSignal.timeout()`
 *   TimeoutError DOMException maps to a ProviderModelDiscoveryError with
 *   `code: "network_failure"` and a usable, non-technical message — the
 *   frontend half of the server+client timeout bound on this endpoint.
 * - Any other rejection (e.g. a plain network error) passes through as-is,
 *   rather than being misreported as a timeout.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";

function mockOk(body: unknown): Response {
  return {
    ok: true,
    status: 200,
    json: () => Promise.resolve(body),
    text: () => Promise.resolve(JSON.stringify(body)),
  } as unknown as Response;
}

function mockError(status: number, body: { error?: string; code?: string }): Response {
  return {
    ok: false,
    status,
    json: () => Promise.resolve(body),
    text: () => Promise.resolve(JSON.stringify(body)),
  } as unknown as Response;
}

const mockFetch = vi.fn();

beforeEach(() => {
  vi.stubGlobal("fetch", mockFetch);
  mockFetch.mockClear();
});

import { getProviderModels, ProviderModelDiscoveryError } from "./api";

describe("getProviderModels", () => {
  it("GETs /providers/{name}/models with an AbortSignal deadline and returns the parsed model list", async () => {
    mockFetch.mockResolvedValue(mockOk(["gpt-4o", "gpt-4o-mini"]));

    const result = await getProviderModels("openai");

    expect(mockFetch).toHaveBeenCalledOnce();
    const [url, init] = mockFetch.mock.calls[0];
    expect(url).toMatch(/\/providers\/openai\/models$/);
    expect(init?.signal).toBeInstanceOf(AbortSignal);
    expect(result).toEqual(["gpt-4o", "gpt-4o-mini"]);
  });

  it("wraps a non-2xx response's error/code into a ProviderModelDiscoveryError", async () => {
    mockFetch.mockResolvedValue(mockError(401, { error: "invalid api key", code: "auth_failure" }));

    await expect(getProviderModels("openai")).rejects.toMatchObject(
      new ProviderModelDiscoveryError("invalid api key", "auth_failure"),
    );
  });

  it("maps the AbortSignal.timeout() rejection onto a network_failure ProviderModelDiscoveryError with a usable message", async () => {
    // What `fetch` actually rejects with when the signal passed in is one
    // created by `AbortSignal.timeout(...)` and it fires before the request
    // completes — a `TimeoutError` DOMException, not a generic AbortError.
    mockFetch.mockRejectedValue(new DOMException("The operation timed out.", "TimeoutError"));

    const err = await getProviderModels("openai").catch((e) => e);

    expect(err).toBeInstanceOf(ProviderModelDiscoveryError);
    expect((err as ProviderModelDiscoveryError).code).toBe("network_failure");
    // Must not spin or die silently — this is the string a caller like
    // CoordinatorConfigFields actually has to show the user.
    expect((err as ProviderModelDiscoveryError).message).toMatch(/timed out/i);
  });

  it("passes through a non-timeout rejection unchanged", async () => {
    const networkError = new TypeError("Failed to fetch");
    mockFetch.mockRejectedValue(networkError);

    await expect(getProviderModels("openai")).rejects.toBe(networkError);
  });
});
