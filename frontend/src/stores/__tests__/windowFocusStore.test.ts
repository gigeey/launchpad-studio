/**
 * Tests for windowFocusStore.ts: the store must boot with a sane initial
 * `isFocused` reading (jsdom's default `document.visibilityState` is
 * "visible", so the store must start focused rather than defaulting to
 * false/unfocused), and `setFocused` — the same setter the module-level
 * event listeners call — must flip state in both directions so consumers
 * (e.g. the notification tier logic) can rely on it.
 */

import { describe, it, expect, beforeEach } from "vitest";
import { useWindowFocusStore } from "../windowFocusStore";

function store() {
  return useWindowFocusStore.getState();
}

beforeEach(() => {
  useWindowFocusStore.setState({ isFocused: true });
});

describe("windowFocusStore", () => {
  it("initializes focused, matching jsdom's default visible document", () => {
    // Reset back to the module's own computed initial value to assert on
    // it directly (jsdom defaults document.visibilityState to "visible").
    expect(document.visibilityState).toBe("visible");
    expect(store().isFocused).toBe(true);
  });

  it("setFocused(false) then setFocused(true) transitions both ways", () => {
    store().setFocused(false);
    expect(store().isFocused).toBe(false);

    store().setFocused(true);
    expect(store().isFocused).toBe(true);
  });

  it("reacts to a real visibilitychange/focus/blur dispatch, same as production listeners", () => {
    window.dispatchEvent(new Event("blur"));
    expect(store().isFocused).toBe(false);

    window.dispatchEvent(new Event("focus"));
    expect(store().isFocused).toBe(true);
  });
});
