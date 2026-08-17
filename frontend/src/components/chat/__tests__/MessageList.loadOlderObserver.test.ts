/**
 * Pins the readiness gate on the top-of-history IntersectionObserver that
 * drives older-message pagination.
 *
 * Bug (3rd pass): after the not-scrollable backfill (Fix 1) and the
 * deterministic post-cooldown re-call (Fix 2), scrolling up to load older
 * messages still failed *intermittently*. Every remaining trigger was
 * edge-triggered — a `scroll` event landing inside a narrow near-top
 * threshold, or a measured-total-size change — so a user who flicked the list
 * up and stopped just short of the very top produced no trigger at all and the
 * older page never loaded.
 *
 * The durable fix makes the trigger level-triggered: an IntersectionObserver
 * watching a sentinel pinned at content-offset 0 fires `maybeLoadOlder` on
 * visibility. jsdom has no real IntersectionObserver, so this exercises the
 * extracted pure gate (`shouldObserverTriggerLoad`) — the "should we even
 * consult the loader" decision — rather than a full-DOM observer.
 */

import { describe, it, expect } from "vitest";
import { shouldObserverTriggerLoad } from "../MessageList";

describe("shouldObserverTriggerLoad — top-history observer gate", () => {
  it("consults the loader when the sentinel is visible and the observer is armed", () => {
    expect(
      shouldObserverTriggerLoad({ isIntersecting: true, observerReady: true })
    ).toBe(true);
  });

  it("stays quiet while the sentinel is out of view, even once armed", () => {
    expect(
      shouldObserverTriggerLoad({ isIntersecting: false, observerReady: true })
    ).toBe(false);
  });

  it("stays quiet on the pre-scroll frame — sentinel visible but not yet armed — so opening a thread never spuriously pages in history", () => {
    expect(
      shouldObserverTriggerLoad({ isIntersecting: true, observerReady: false })
    ).toBe(false);
  });

  it("stays quiet when neither condition holds", () => {
    expect(
      shouldObserverTriggerLoad({ isIntersecting: false, observerReady: false })
    ).toBe(false);
  });
});
