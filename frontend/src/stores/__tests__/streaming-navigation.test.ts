/**
 * Regression suite: streaming survives navigation.
 *
 * Drives events through the real `useProjectSSE` / `useSSE` hooks via the SSE
 * hub's `__dispatchForTest` seam (see `frontend/src/lib/sseHub.ts`) — the hooks
 * are mounted for the life of each test and receive injected envelopes exactly
 * as they would receive real `/system/stream` traffic, so this exercises the
 * production handler bodies rather than a hand-written duplicate of them.
 *
 * Verifies that:
 *
 * 1. Project chat — tokens written into the keyed chatStore buffer
 *    (inFlightByAgent["project:{id}"]) survive a "navigate away / navigate
 *    back" cycle (mountProjectChannel called again while streaming).
 *
 * 2. Agent chat — tokens written into inFlightByAgent[agentId] survive
 *    selectAgent being called (equivalent to the user visiting another agent
 *    and returning), because the hub subscription stays open across the
 *    simulated navigation and the buffer is not cleared on mount.
 *
 * 3. Both surfaces clear their buffers ONLY on the finalize points:
 *    text_complete → finalizeInFlightText, run_ended → scheduleInFlightTeardown.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { useChatStore } from "../chatStore";
import { useProjectStore } from "../projectStore";
import { useSSE } from "../../hooks/useSSE";
import { useProjectSSE } from "../../hooks/useProjectSSE";
import { __dispatchForTest } from "../../lib/sseHub";

// The hub lazily opens a real fetch-based connection on first subscription.
// Stub it out so mounting `useSSE`/`useProjectSSE` in jsdom never attempts a
// network call — events are injected directly via `__dispatchForTest`, which
// bypasses the connection entirely.
vi.mock("../../hooks/sseUtils", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../hooks/sseUtils")>();
  return {
    ...actual,
    createManagedEventSource: vi.fn(() => ({ close: vi.fn() })),
  };
});

// finalizeInFlightText (reached via text_complete below) fires a
// fire-and-forget `fetchAgents()` sidebar refresh that isn't under test
// here — without a mock it falls through to a real `fetch("/agents")` and
// trips the global unmocked-fetch guard (`src/test/setupFetchGuard.ts`) as
// an unhandled rejection.
vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return { ...actual, getAgents: async () => [] };
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function chatStore() {
  return useChatStore.getState();
}

function streamingText(key: string): string {
  return useChatStore.getState().inFlightByAgent.get(key)?.textBuffer ?? "";
}

function hasInFlight(key: string): boolean {
  return useChatStore.getState().inFlightByAgent.has(key);
}

// Mounts a hook for the duration of a test via a throwaway host component —
// there's no react-testing-library in this project, so this mirrors the
// createRoot()/act() pattern already used by ProjectDetailView.transition.test.tsx.
let mountedRoots: Array<{ root: Root; container: HTMLDivElement }> = [];

function mountHook(useHook: () => unknown): void {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  function Harness() {
    useHook();
    return null;
  }
  act(() => {
    root.render(React.createElement(Harness));
  });
  mountedRoots.push({ root, container });
}

function unmountAllHooks(): void {
  act(() => {
    for (const { root } of mountedRoots) root.unmount();
  });
  for (const { container } of mountedRoots) document.body.removeChild(container);
  mountedRoots = [];
}

// Builds the raw AgentEvent JSON exactly as the backend emits it — the
// listener bodies parse this themselves via `parsePayloadData`.
function rawEvent(agentId: string, eventName: string, data: Record<string, unknown> = {}): string {
  return JSON.stringify({
    agent_id: agentId,
    run_id: "run-1",
    payload: { type: eventName, data },
  });
}

// Injects an event on the given channel key (a project key like
// "project:{id}" or a plain agent id) through the hub's test seam.
function inject(channelAgentId: string, eventName: string, data: Record<string, unknown> = {}): void {
  act(() => {
    __dispatchForTest({
      agent_id: channelAgentId,
      run_id: "run-1",
      thread_id: null,
      eventName,
      raw: rawEvent(channelAgentId, eventName, data),
    });
  });
}

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

beforeEach(() => {
  useChatStore.getState().reset();
  useProjectStore.getState().reset();
});

afterEach(() => {
  unmountAllHooks();
});

// ---------------------------------------------------------------------------
// Project chat — navigation survival
// ---------------------------------------------------------------------------

describe("project chat streaming survives navigation", () => {
  const PROJECT_ID = "proj-nav-test";
  const PROJECT_KEY = `project:${PROJECT_ID}`;

  it("accumulates pre-navigation and during-navigation deltas in the keyed buffer", () => {
    // Open the project channel (simulates useProjectChatChannel mount) and
    // subscribe the real SSE hook, which stays mounted across the whole test
    // just like SSEManager keeps the hub subscription alive across navigation.
    chatStore().mountProjectChannel(PROJECT_ID, [], null, "TestAgent", "🤖");
    mountHook(() => useProjectSSE(PROJECT_ID));

    inject(PROJECT_KEY, "run_started");

    // Stream some tokens while on the project page
    inject(PROJECT_KEY, "text_delta", { text: "Hello " });
    inject(PROJECT_KEY, "text_delta", { text: "world" });

    expect(streamingText(PROJECT_KEY)).toBe("Hello world");

    // Simulate "navigate away": the hub subscription stays open (mirrors
    // SSEManager keeping the connection alive) but mountProjectChannel is NOT
    // called again, so nothing clears the buffer. The user's selectedAgentId
    // changes to something else.
    useChatStore.setState({ selectedAgentId: "other-agent" });

    // The hub keeps delivering tokens to the still-open subscription.
    inject(PROJECT_KEY, "text_delta", { text: " — " });
    inject(PROJECT_KEY, "text_delta", { text: "still streaming" });

    // Buffer accumulated during navigation
    expect(streamingText(PROJECT_KEY)).toBe("Hello world — still streaming");

    // Simulate "navigate back": mountProjectChannel is called again.
    // It should NOT wipe an active in-flight entry.
    chatStore().mountProjectChannel(PROJECT_ID, [], null, "TestAgent", "🤖");

    // Buffer still intact after remount
    expect(streamingText(PROJECT_KEY)).toBe("Hello world — still streaming");
    expect(hasInFlight(PROJECT_KEY)).toBe(true);
  });

  it("clears the buffer only after text_complete + run_ended", () => {
    chatStore().mountProjectChannel(PROJECT_ID, [], null, "TestAgent", "🤖");
    mountHook(() => useProjectSSE(PROJECT_ID));

    inject(PROJECT_KEY, "run_started");
    inject(PROJECT_KEY, "text_delta", { text: "Full response text" });

    expect(streamingText(PROJECT_KEY)).toBe("Full response text");

    // text_complete finalizes the message into the transcript
    inject(PROJECT_KEY, "text_complete", { text: "Full response text" });

    // Buffer cleared, message in transcript
    expect(streamingText(PROJECT_KEY)).toBe("");
    const msgs = useChatStore.getState().messages;
    expect(msgs[msgs.length - 1]?.content).toBe("Full response text");

    // run_ended triggers teardown scheduling (the real handler drains any
    // remaining buffer itself and schedules a 400ms teardown timer; we only
    // verify the buffer is already empty before that fires)
    inject(PROJECT_KEY, "run_ended", { reason: "Completed" });
    expect(streamingText(PROJECT_KEY)).toBe("");
  });

  it("mountProjectChannel preserves active streaming entry on remount", () => {
    chatStore().mountProjectChannel(PROJECT_ID, [], null, "TestAgent", "🤖");
    mountHook(() => useProjectSSE(PROJECT_ID));

    inject(PROJECT_KEY, "run_started");
    inject(PROJECT_KEY, "text_delta", { text: "in-progress text" });

    const entryBefore = useChatStore.getState().inFlightByAgent.get(PROJECT_KEY);
    expect(entryBefore?.textBuffer).toBe("in-progress text");
    expect(entryBefore?.isTyping).toBe(true);

    // Remount (e.g., navigate back to project page)
    chatStore().mountProjectChannel(PROJECT_ID, [], null, "TestAgent", "🤖");

    // Entry still alive with same text
    const entryAfter = useChatStore.getState().inFlightByAgent.get(PROJECT_KEY);
    expect(entryAfter?.textBuffer).toBe("in-progress text");
  });

  it("mountProjectChannel clears an idle stale entry", () => {
    // Stale entry with no active streaming (textBuffer empty, not typing, no tool calls)
    const staleMap = new Map(useChatStore.getState().inFlightByAgent);
    staleMap.set(PROJECT_KEY, {
      textBuffer: "",
      activeToolCalls: [],
      isTyping: false,
      startedAt: Date.now() - 60000,
      thinkingActive: false,
      thinkingBuffer: "",
      thinkingStartedAt: null,
      thinkingElapsedMs: null,
      thinkingShown: false,
      artifactIds: [],
    });
    useChatStore.setState({ inFlightByAgent: staleMap });

    // Remount should clear the stale entry
    chatStore().mountProjectChannel(PROJECT_ID, [], null, "TestAgent", "🤖");
    expect(hasInFlight(PROJECT_KEY)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// Agent chat — buffer persists across selectAgent calls
// ---------------------------------------------------------------------------

describe("agent chat streaming survives navigation", () => {
  const AGENT_ID = "agent-nav-test";

  beforeEach(() => {
    useChatStore.setState({ selectedAgentId: AGENT_ID });
    chatStore().ensureInFlight(AGENT_ID);
  });

  it("keeps the in-flight buffer while navigating to another agent and back", () => {
    mountHook(() => useSSE(AGENT_ID));

    inject(AGENT_ID, "run_started");
    inject(AGENT_ID, "text_delta", { text: "Before nav " });
    inject(AGENT_ID, "text_delta", { text: "content" });

    expect(streamingText(AGENT_ID)).toBe("Before nav content");

    // Simulate "navigate away" to another agent (selectedAgentId changes).
    // The hub subscription for AGENT_ID stays open — SSEManager keeps every
    // in-flight agent's subscription alive regardless of what's selected.
    useChatStore.setState({ selectedAgentId: "other-agent" });

    inject(AGENT_ID, "text_delta", { text: " — during nav" });

    // Buffer accumulated during navigation
    expect(streamingText(AGENT_ID)).toBe("Before nav content — during nav");

    // Simulate "navigate back" (selectAgent called, selectedAgentId restored)
    useChatStore.setState({ selectedAgentId: AGENT_ID });

    // Buffer still intact
    expect(streamingText(AGENT_ID)).toBe("Before nav content — during nav");
    expect(hasInFlight(AGENT_ID)).toBe(true);
  });

  it("clears buffer after text_complete / run_ended", () => {
    mountHook(() => useSSE(AGENT_ID));

    inject(AGENT_ID, "run_started");
    inject(AGENT_ID, "text_delta", { text: "Some streamed text" });

    inject(AGENT_ID, "text_complete", { text: "Some streamed text" });
    expect(streamingText(AGENT_ID)).toBe("");

    const msgs = useChatStore.getState().messages;
    expect(msgs[msgs.length - 1]?.content).toBe("Some streamed text");

    inject(AGENT_ID, "run_ended", { reason: "Completed" });
    expect(streamingText(AGENT_ID)).toBe("");
  });
});
