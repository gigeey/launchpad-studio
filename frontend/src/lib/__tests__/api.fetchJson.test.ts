/**
 * Regression tests for the `fetchJson` shared wrapper in lib/api.ts (Fix B:
 * bound every request with a timeout + AbortController).
 *
 * Covers:
 *   a. A fetch that never settles rejects with the timeout error after the
 *      configured duration (fake timers — no real 15s wait).
 *   b. A fast successful response resolves normally, doesn't throw, and
 *      leaves no stray pending timer.
 *   c. A caller-supplied `init.signal` that aborts before the timeout
 *      rejects with a caller-abort error, distinguishable from a timeout.
 *   d. A non-ok HTTP response still throws the existing
 *      `API <status>: <body>` shape (regression guard on the pre-existing
 *      contract).
 *   e/f. A call site on one of the longer tiers (BULK / EXTERNAL_WORK) is
 *      actually bound by that tier and NOT by the 15s default — asserted by
 *      advancing fake timers past the default and requiring the request to
 *      still be in flight, then on to the tier's own deadline. These guard
 *      against the third `fetchJson` argument being dropped as "redundant"
 *      by a future cleanup.
 *
 * The bug this closes: an unbounded `fetchJson` left `loading` flags stuck
 * forever on a hung request.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  fetchJson,
  DEFAULT_FETCH_TIMEOUT_MS,
  BULK_FETCH_TIMEOUT_MS,
  EXTERNAL_WORK_TIMEOUT_MS,
  FetchTimeoutError,
  getStorageInfo,
  promoteSkillObservation,
} from "../api";

/** Mock `Response`-shaped object matching the pattern used by the other
 *  `api.*.test.ts` files in this directory (jsdom's real `Response` isn't
 *  needed — only `.ok`/`.status`/`.json`/`.text` are read by `fetchJson`). */
function mockOk(body: unknown, status = 200): Response {
  return {
    ok: true,
    status,
    json: () => Promise.resolve(body),
    text: () => Promise.resolve(JSON.stringify(body)),
  } as unknown as Response;
}

function mockError(status: number, body: string): Response {
  return {
    ok: false,
    status,
    json: () => Promise.reject(new Error("not json")),
    text: () => Promise.resolve(body),
  } as unknown as Response;
}

/** A `fetch` mock that behaves like the real thing with respect to abort:
 *  it never resolves on its own, but rejects the moment the `signal` passed
 *  in `init` fires — exactly what a hung request looks like once our
 *  AbortController-based timeout (or a caller's own signal) kicks in. */
function neverSettlingAbortableFetch() {
  return vi.fn((_url: string, init?: RequestInit) => {
    return new Promise((_resolve, reject) => {
      const signal = init?.signal;
      if (!signal) return; // no signal wired up — truly never settles
      const abort = () => reject(new DOMException("The operation was aborted.", "AbortError"));
      if (signal.aborted) {
        abort();
        return;
      }
      signal.addEventListener("abort", abort, { once: true });
    });
  });
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("fetchJson", () => {
  it("(a) rejects with the timeout error after the configured duration, without waiting for it in real time", async () => {
    const mockFetch = neverSettlingAbortableFetch();
    vi.stubGlobal("fetch", mockFetch);

    const promise = fetchJson("/agents");
    // Attach the rejection assertion before advancing timers so the
    // rejection is observed rather than becoming an unhandled rejection.
    const assertion = expect(promise).rejects.toMatchObject({ name: "TimeoutError" });

    await vi.advanceTimersByTimeAsync(DEFAULT_FETCH_TIMEOUT_MS);

    await assertion;
    await expect(promise).rejects.toBeInstanceOf(FetchTimeoutError);
  });

  it("(b) resolves normally for a fast successful response and leaves no stray pending timer", async () => {
    const payload = [{ id: "a1" }];
    const mockFetch = vi.fn().mockResolvedValue(mockOk(payload));
    vi.stubGlobal("fetch", mockFetch);

    await expect(fetchJson("/agents")).resolves.toEqual(payload);

    // The success path resolves synchronously-ish (microtasks only) well
    // before the timeout fires, so its internal timer must already be
    // cleared — nothing should be left pending.
    expect(vi.getTimerCount()).toBe(0);
  });

  it("(c) rejects with a caller-abort error, distinguishable from a timeout, when the caller's own signal fires first", async () => {
    const mockFetch = neverSettlingAbortableFetch();
    vi.stubGlobal("fetch", mockFetch);

    const controller = new AbortController();
    const promise = fetchJson("/agents", { signal: controller.signal });
    const assertion = expect(promise).rejects.toMatchObject({ name: "AbortError" });

    controller.abort();
    await assertion;

    // Distinguishable from our timeout error on both class and name.
    await expect(promise).rejects.not.toBeInstanceOf(FetchTimeoutError);
    await expect(promise).rejects.not.toMatchObject({ name: "TimeoutError" });

    // And the timeout timer must not still fire/throw afterwards.
    await vi.advanceTimersByTimeAsync(DEFAULT_FETCH_TIMEOUT_MS);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("(d) still throws the existing `API <status>: <body>` shape for a non-ok HTTP response", async () => {
    const mockFetch = vi.fn().mockResolvedValue(mockError(404, "agent not found"));
    vi.stubGlobal("fetch", mockFetch);

    await expect(fetchJson("/agents/missing")).rejects.toThrow("API 404: agent not found");
    expect(vi.getTimerCount()).toBe(0);
  });
});

/** The 15s default is correct for the ~125 ordinary call sites, but a handful
 *  of handlers legitimately run longer (recursive copies, ripgrep sweeps, LLM
 *  round-trips) and pass an explicit tier as `fetchJson`'s third argument.
 *  These tests pin that the argument is actually honoured end-to-end from the
 *  public API function, so it can't be silently dropped. */
describe("tiered call-site timeouts", () => {
  /** Drives a never-settling request and reports when it finally rejects. */
  function inFlight<T>(promise: Promise<T>) {
    const state = { settled: false, error: undefined as unknown };
    promise.catch((err) => {
      state.settled = true;
      state.error = err;
    });
    return state;
  }

  it("(e) bounds a BULK call site by BULK_FETCH_TIMEOUT_MS, not the 15s default", async () => {
    vi.stubGlobal("fetch", neverSettlingAbortableFetch());

    // `getStorageInfo` walks the whole data root; it passes BULK explicitly.
    const promise = getStorageInfo();
    const state = inFlight(promise);

    // Past the default deadline the request must still be in flight — if the
    // third argument were dropped, this is where it would have aborted.
    await vi.advanceTimersByTimeAsync(DEFAULT_FETCH_TIMEOUT_MS);
    expect(state.settled).toBe(false);

    // ...and it aborts on its own tier instead.
    await vi.advanceTimersByTimeAsync(BULK_FETCH_TIMEOUT_MS - DEFAULT_FETCH_TIMEOUT_MS);
    expect(state.settled).toBe(true);
    expect(state.error).toBeInstanceOf(FetchTimeoutError);
    // The tier value itself is threaded into the error message, so this
    // asserts on the number actually used rather than merely on "it waited".
    expect((state.error as Error).message).toContain(`${BULK_FETCH_TIMEOUT_MS}ms`);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("(f) bounds an EXTERNAL_WORK call site by EXTERNAL_WORK_TIMEOUT_MS, outlasting the BULK tier too", async () => {
    vi.stubGlobal("fetch", neverSettlingAbortableFetch());

    // `promoteSkillObservation` blocks on a provider round-trip.
    const promise = promoteSkillObservation("agent-1", "cand-1");
    const state = inFlight(promise);

    await vi.advanceTimersByTimeAsync(BULK_FETCH_TIMEOUT_MS);
    expect(state.settled).toBe(false);

    await vi.advanceTimersByTimeAsync(EXTERNAL_WORK_TIMEOUT_MS - BULK_FETCH_TIMEOUT_MS);
    expect(state.settled).toBe(true);
    expect(state.error).toBeInstanceOf(FetchTimeoutError);
    expect((state.error as Error).message).toContain(`${EXTERNAL_WORK_TIMEOUT_MS}ms`);
    expect(vi.getTimerCount()).toBe(0);
  });
});
