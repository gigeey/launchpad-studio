import { describe, it, expect, beforeEach } from "vitest";
import { useQueuedSendStore } from "../queuedSendStore";

function store() {
  return useQueuedSendStore.getState();
}

beforeEach(() => {
  useQueuedSendStore.setState({ queues: {} });
});

describe("queuedSendStore", () => {
  it("has no bucket for a key nothing was ever queued under", () => {
    expect(store().queues["a:t"]).toBeUndefined();
  });

  it("stores a queue under its key and leaves other keys untouched", () => {
    store().setQueue("a:t", [{ content: "one" }]);
    store().setQueue("b:t", [{ content: "other" }]);
    expect(store().queues["a:t"]).toEqual([{ content: "one" }]);
    expect(store().queues["b:t"]).toEqual([{ content: "other" }]);
  });

  it("drops the bucket entirely when set back to empty, rather than leaving a stale []", () => {
    store().setQueue("a:t", [{ content: "one" }]);
    expect("a:t" in store().queues).toBe(true);
    store().setQueue("a:t", []);
    expect("a:t" in store().queues).toBe(false);
  });

  it("survives being read again later, unlike component-local state that resets on remount", () => {
    // This is the property the fix relies on: `useQueuedMessageSend` used to
    // keep its queue in a component-scoped ref/state, which reset to empty
    // every time ChatView unmounted (any navigation away from Chat/Home).
    // Reading the store from an unrelated call site, simulating a fresh
    // hook instance mounting later, must still see what was queued before.
    store().setQueue("a:t", [{ content: "queued before navigating away" }]);
    const seenByANewMount = useQueuedSendStore.getState().queues["a:t"];
    expect(seenByANewMount).toEqual([{ content: "queued before navigating away" }]);
  });
});
