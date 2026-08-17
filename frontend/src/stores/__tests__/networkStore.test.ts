/**
 * Tests for the `isServerOnline` debounce in networkStore.ts: a single failed
 * `/health` ping must not trip the banner, two consecutive failures must,
 * and a success in between must reset the failure count back to zero.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../../lib/sseHub", () => ({
  isHubRecentlyAlive: () => false,
}));

import {
  __resetServerFailureCountForTest,
  checkServer,
  useNetworkStore,
} from "../networkStore";

function mockOk(): Response {
  return { ok: true, status: 200 } as unknown as Response;
}

function mockFailure(): Promise<Response> {
  return Promise.reject(new Error("network error"));
}

const mockFetch = vi.fn();

beforeEach(() => {
  vi.stubGlobal("fetch", mockFetch);
  mockFetch.mockClear();
  useNetworkStore.setState({ isServerOnline: true });
  __resetServerFailureCountForTest();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("networkStore isServerOnline debounce", () => {
  it("stays online after a single failed ping", async () => {
    mockFetch.mockReturnValueOnce(mockFailure());

    await checkServer();

    expect(useNetworkStore.getState().isServerOnline).toBe(true);
  });

  it("flips offline only after two consecutive failed pings", async () => {
    mockFetch.mockReturnValueOnce(mockFailure());
    await checkServer();
    expect(useNetworkStore.getState().isServerOnline).toBe(true);

    mockFetch.mockReturnValueOnce(mockFailure());
    await checkServer();

    expect(useNetworkStore.getState().isServerOnline).toBe(false);
  });

  it("resets the failure count when a success lands mid-way", async () => {
    // First failure — still online.
    mockFetch.mockReturnValueOnce(mockFailure());
    await checkServer();
    expect(useNetworkStore.getState().isServerOnline).toBe(true);

    // A success mid-way should reset the counter rather than letting it
    // carry over into the next failure streak.
    mockFetch.mockReturnValueOnce(Promise.resolve(mockOk()));
    await checkServer();
    expect(useNetworkStore.getState().isServerOnline).toBe(true);

    // A single subsequent failure must not trip the banner — proves the
    // counter was actually reset, not just left below threshold.
    mockFetch.mockReturnValueOnce(mockFailure());
    await checkServer();
    expect(useNetworkStore.getState().isServerOnline).toBe(true);

    // But a second consecutive failure after the reset still trips it.
    mockFetch.mockReturnValueOnce(mockFailure());
    await checkServer();
    expect(useNetworkStore.getState().isServerOnline).toBe(false);
  });

  it("goes back online immediately after a single success while offline", async () => {
    mockFetch.mockReturnValueOnce(mockFailure());
    await checkServer();
    mockFetch.mockReturnValueOnce(mockFailure());
    await checkServer();
    expect(useNetworkStore.getState().isServerOnline).toBe(false);

    mockFetch.mockReturnValueOnce(Promise.resolve(mockOk()));
    await checkServer();

    expect(useNetworkStore.getState().isServerOnline).toBe(true);
  });
});
