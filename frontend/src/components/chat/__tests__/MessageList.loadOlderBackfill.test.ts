/**
 * Pins the not-scrollable backfill fix in `shouldLoadOlderMessages`.
 *
 * Bug: older-message pagination armed off a scroll-position check, which can
 * only ever fire once the container overflows (`scrollHeight > clientHeight`).
 * A short thread, or one whose recent replies sit collapsed behind "Read
 * more" so their rendered height is small, never overflows — so the fetch
 * never armed and the user had to manually expand replies until the content
 * happened to grow tall enough to scroll. `shouldLoadOlderMessages` now
 * treats "container isn't scrollable" as its own trigger, independent of
 * `scrollTop`/index checks, with a defensive iteration cap.
 */

import { describe, it, expect } from "vitest";
import { shouldLoadOlderMessages, MAX_AUTO_BACKFILL_ITERATIONS } from "../MessageList";
import type { LoadOlderCheckParams } from "../MessageList";

function baseParams(overrides: Partial<LoadOlderCheckParams> = {}): LoadOlderCheckParams {
  return {
    hasMoreMessages: true,
    loadingMore: false,
    loadMoreInFlight: false,
    loadMoreCooldown: false,
    scrollTop: 0,
    scrollHeight: 400,
    clientHeight: 800, // content shorter than viewport — not scrollable
    nearTopByIndex: false,
    autoBackfillCount: 0,
    ...overrides,
  };
}

describe("shouldLoadOlderMessages — not-scrollable backfill", () => {
  it("fetches automatically when content doesn't overflow the container, with no scroll involved", () => {
    const result = shouldLoadOlderMessages(baseParams());
    expect(result.shouldFetch).toBe(true);
    expect(result.nextAutoBackfillCount).toBe(1);
  });

  it("keeps fetching across repeated calls as long as history remains, then stops once hasMoreMessages flips false", () => {
    let autoBackfillCount = 0;
    let hasMoreMessages = true;
    const fetchCalls: boolean[] = [];

    for (let page = 0; page < 5; page++) {
      const result = shouldLoadOlderMessages(baseParams({ hasMoreMessages, autoBackfillCount }));
      fetchCalls.push(result.shouldFetch);
      autoBackfillCount = result.nextAutoBackfillCount;
      if (page === 2) hasMoreMessages = false; // backend runs out of history after 3rd page
    }

    expect(fetchCalls).toEqual([true, true, true, false, false]);
  });

  it("does not fetch while a load is already in flight or cooling down", () => {
    expect(shouldLoadOlderMessages(baseParams({ loadMoreInFlight: true })).shouldFetch).toBe(false);
    expect(shouldLoadOlderMessages(baseParams({ loadMoreCooldown: true })).shouldFetch).toBe(false);
    expect(shouldLoadOlderMessages(baseParams({ loadingMore: true })).shouldFetch).toBe(false);
  });

  it("does not fetch once there is no more history, regardless of scrollability", () => {
    const result = shouldLoadOlderMessages(baseParams({ hasMoreMessages: false }));
    expect(result.shouldFetch).toBe(false);
  });

  it("caps consecutive auto-triggered fetches to guard against a runaway loop", () => {
    const result = shouldLoadOlderMessages(
      baseParams({ autoBackfillCount: MAX_AUTO_BACKFILL_ITERATIONS })
    );
    expect(result.shouldFetch).toBe(false);
    expect(result.nextAutoBackfillCount).toBe(MAX_AUTO_BACKFILL_ITERATIONS);
  });

  it("resets the auto-backfill counter once the container becomes scrollable", () => {
    const result = shouldLoadOlderMessages(
      baseParams({
        scrollHeight: 2000,
        clientHeight: 800,
        scrollTop: 0,
        nearTopByIndex: true,
        autoBackfillCount: 42,
      })
    );
    expect(result.shouldFetch).toBe(true);
    expect(result.nextAutoBackfillCount).toBe(0);
  });
});

describe("shouldLoadOlderMessages — scrollable container (existing scroll-driven behavior)", () => {
  it("fetches when scrolled near the top", () => {
    const result = shouldLoadOlderMessages(
      baseParams({ scrollHeight: 2000, clientHeight: 800, scrollTop: 10, nearTopByIndex: true })
    );
    expect(result.shouldFetch).toBe(true);
  });

  it("does not fetch when scrolled away from the top", () => {
    const result = shouldLoadOlderMessages(
      baseParams({ scrollHeight: 2000, clientHeight: 800, scrollTop: 500, nearTopByIndex: false })
    );
    expect(result.shouldFetch).toBe(false);
  });

  it("fetches when the pixel gap says not-near-top but the virtualizer index says otherwise", () => {
    const result = shouldLoadOlderMessages(
      baseParams({ scrollHeight: 2000, clientHeight: 800, scrollTop: 500, nearTopByIndex: true })
    );
    expect(result.shouldFetch).toBe(true);
  });
});
