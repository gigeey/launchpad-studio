/**
 * Tests for token-usage accumulation across a single user turn.
 *
 * Covers:
 * - First `usage` event populates the per-agent entry as-is
 * - Subsequent events sum into the running totals (key invariant for the
 *   CLI runner's tool-use continuation respawn pattern, where each respawn
 *   emits its own `usage` event but the user-facing "turn" spans all of them)
 * - finalizeInFlightText does NOT clear usage (would prematurely zero between
 *   continuation loops — the user's explicit guard: reset only on the
 *   completion marker, not on intermediate text_complete events)
 * - deleteInFlight clears usage (the actual turn-end teardown)
 * - reset() clears usage globally
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { useChatStore, type TurnUsage } from "../chatStore";

// finalizeInFlightText (exercised below) fires a fire-and-forget
// `fetchAgents()` sidebar refresh that isn't under test here — without a
// mock it falls through to a real `fetch("/agents")` and trips the global
// unmocked-fetch guard (`src/test/setupFetchGuard.ts`) as an unhandled
// rejection.
vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return { ...actual, getAgents: async () => [] };
});

const AGENT_ID = "test-agent-usage";

function store() {
  return useChatStore.getState();
}

function getUsage(): TurnUsage | undefined {
  return useChatStore.getState().usageByAgent.get(AGENT_ID);
}

beforeEach(() => {
  useChatStore.getState().reset();
});

describe("accumulateUsage", () => {
  it("populates the entry as-is on first event", () => {
    store().accumulateUsage(AGENT_ID, {
      input: 1,
      output: 162,
      cacheRead: 145467,
      cacheCreation: 2157,
      total: 147625,
    });
    expect(getUsage()).toEqual({
      input: 1,
      output: 162,
      cacheRead: 145467,
      cacheCreation: 2157,
      total: 147625,
    });
  });

  it("sums each new event into the running totals", () => {
    // Loop 1 — initial provider call within the turn
    store().accumulateUsage(AGENT_ID, {
      input: 10,
      output: 50,
      cacheRead: 100,
      cacheCreation: 200,
      total: 360,
    });
    // Loop 2 — CLI continuation respawn after a tool dispatch
    store().accumulateUsage(AGENT_ID, {
      input: 5,
      output: 30,
      cacheRead: 150,
      cacheCreation: 0,
      total: 185,
    });
    expect(getUsage()).toEqual({
      input: 15,
      output: 80,
      cacheRead: 250,
      cacheCreation: 200,
      total: 545,
    });
  });

  it("handles a long sequence of small accumulations", () => {
    for (let i = 0; i < 10; i++) {
      store().accumulateUsage(AGENT_ID, {
        input: 1,
        output: 1,
        cacheRead: 1,
        cacheCreation: 1,
        total: 4,
      });
    }
    expect(getUsage()).toEqual({
      input: 10,
      output: 10,
      cacheRead: 10,
      cacheCreation: 10,
      total: 40,
    });
  });
});

describe("usage lifecycle vs. inflight teardown", () => {
  it("finalizeInFlightText does NOT clear usage — preserves between continuation loops", () => {
    store().ensureInFlight(AGENT_ID);
    store().accumulateUsage(AGENT_ID, {
      input: 10,
      output: 50,
      cacheRead: 100,
      cacheCreation: 0,
      total: 160,
    });
    // text_complete fires between loops in the continuation pattern
    store().finalizeInFlightText(AGENT_ID, "intermediate text");
    expect(getUsage()).toEqual({
      input: 10,
      output: 50,
      cacheRead: 100,
      cacheCreation: 0,
      total: 160,
    });
    // Loop 2 usage accumulates onto loop 1's running totals
    store().accumulateUsage(AGENT_ID, {
      input: 5,
      output: 25,
      cacheRead: 80,
      cacheCreation: 0,
      total: 110,
    });
    expect(getUsage()).toEqual({
      input: 15,
      output: 75,
      cacheRead: 180,
      cacheCreation: 0,
      total: 270,
    });
  });

  it("deleteInFlight clears usage — the true completion marker", () => {
    store().ensureInFlight(AGENT_ID);
    store().accumulateUsage(AGENT_ID, {
      input: 10,
      output: 50,
      cacheRead: 100,
      cacheCreation: 0,
      total: 160,
    });
    expect(getUsage()).toBeDefined();
    store().deleteInFlight(AGENT_ID);
    expect(getUsage()).toBeUndefined();
  });

  it("deleteInFlight on an agent with no inflight entry still clears stale usage", () => {
    // Edge: usage arrives but the inflight entry was already torn down for
    // some reason. The next deleteInFlight call should sweep the orphan.
    store().accumulateUsage(AGENT_ID, {
      input: 1,
      output: 1,
      cacheRead: 1,
      cacheCreation: 1,
      total: 4,
    });
    expect(getUsage()).toBeDefined();
    store().deleteInFlight(AGENT_ID);
    expect(getUsage()).toBeUndefined();
  });
});

describe("reset clears usage globally", () => {
  it("wipes all per-agent usage entries", () => {
    store().accumulateUsage("agent-a", { input: 1, output: 1, cacheRead: 1, cacheCreation: 1, total: 4 });
    store().accumulateUsage("agent-b", { input: 1, output: 1, cacheRead: 1, cacheCreation: 1, total: 4 });
    expect(useChatStore.getState().usageByAgent.size).toBe(2);
    store().reset();
    expect(useChatStore.getState().usageByAgent.size).toBe(0);
  });
});
