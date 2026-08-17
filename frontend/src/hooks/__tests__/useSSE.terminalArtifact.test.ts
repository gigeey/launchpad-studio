// @vitest-environment jsdom
//
// Regression coverage for the terminal-ArtifactWrite disappearing-card bug.
//
// When `ArtifactWrite` is the LAST action of an agent turn — the model emits
// some text, calls `ArtifactWrite`, and the turn ends with no further text —
// the wire sequence is: `text_complete` (pre-tool text, flushed BEFORE the
// tool runs) -> `tool_call_completed` (ArtifactWrite, carries the id) ->
// `run_ended`, with no second `text_complete` to snapshot the id onto a
// finalized transcript entry. Before the `run_ended` guard in `useSSE.ts`
// also checked for a pending `artifactIds` list (not just a non-empty
// `textBuffer`), `scheduleInFlightTeardown` / `deleteInFlight` would drop the
// in-flight entry — id and all — so the card vanished at turn end and only
// came back after a full transcript reload from disk.
//
// Driven through the real `useSSE` hook + the SSE hub's `__dispatchForTest`
// seam (see `useSSE.artifactWrite.test.ts` / `streaming-navigation.test.ts`)
// so this exercises the production listener body and the real 400ms
// `scheduleInFlightTeardown` timer, not a hand-written reimplementation.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { useChatStore } from "../../stores/chatStore";
import { useArtifactStore } from "../../stores/artifactStore";
import { useSSE } from "../useSSE";
import { __dispatchForTest } from "../../lib/sseHub";
import type { TranscriptEntry } from "../../types/api";

// finalizeInFlightText (reached via text_complete below) fires a
// fire-and-forget `fetchAgents()` sidebar refresh that isn't under test
// here — without a mock it falls through to a real `fetch("/agents")` and
// trips the global unmocked-fetch guard (`src/test/setupFetchGuard.ts`) as
// an unhandled rejection.
vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return { ...actual, getAgents: async () => [] };
});

vi.mock("../sseUtils", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../sseUtils")>();
  return {
    ...actual,
    createManagedEventSource: vi.fn(() => ({ close: vi.fn() })),
  };
});

const AGENT_ID = "terminal-artifact-agent";
const TEARDOWN_DELAY_MS = 400; // mirrors IN_FLIGHT_TEARDOWN_DELAY_MS in chatStore.ts

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

function rawEvent(agentId: string, eventName: string, data: Record<string, unknown> = {}): string {
  return JSON.stringify({
    agent_id: agentId,
    run_id: "run-1",
    payload: { type: eventName, data },
  });
}

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

function artifactIdsOf(entry: TranscriptEntry): string[] {
  const md = entry.metadata as Record<string, unknown> | null | undefined;
  const ids = md?.artifact_ids;
  return Array.isArray(ids) ? ids.filter((x): x is string => typeof x === "string") : [];
}

beforeEach(() => {
  useChatStore.getState().reset();
  useArtifactStore.setState({ byAgent: new Map(), cardsById: new Map() });
  useChatStore.setState({ selectedAgentId: AGENT_ID });
  useChatStore.getState().ensureInFlight(AGENT_ID);
});

afterEach(() => {
  unmountAllHooks();
  vi.useRealTimers();
});

describe("terminal ArtifactWrite (no trailing text) survives run_ended teardown", () => {
  it("keeps the artifact card in the finalized transcript after the 400ms teardown fires — no reload needed", async () => {
    vi.useFakeTimers();

    mountHook(() => useSSE(AGENT_ID));

    inject(AGENT_ID, "run_started");
    inject(AGENT_ID, "text_delta", { text: "Let me build that:" });
    // Backend flushes the buffered text into text_complete the instant the
    // ToolUse arrives — BEFORE the tool executes, so artifactIds is still
    // empty at this point.
    inject(AGENT_ID, "text_complete", { text: "Let me build that:" });

    inject(AGENT_ID, "tool_call_completed", {
      tool_name: "ArtifactWrite",
      output: JSON.stringify({ id: "artifact-terminal-1", renderer: "cards", refresh_intent: "none", title: "Board" }),
    });

    // Live card already showing mid-stream, on the in-flight entry only.
    expect(useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.artifactIds).toEqual(["artifact-terminal-1"]);

    // Turn ends with NO further text_complete.
    inject(AGENT_ID, "run_ended", { reason: "Completed" });

    // Let scheduleInFlightTeardown's real 400ms timer fire deleteInFlight.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(TEARDOWN_DELAY_MS);
    });

    // In-flight entry (and its artifactIds) is gone...
    expect(useChatStore.getState().inFlightByAgent.has(AGENT_ID)).toBe(false);

    // ...but the id survived onto a finalized transcript entry.
    const messages = useChatStore.getState().messages;
    const cardEntry = messages.find((m) => artifactIdsOf(m).includes("artifact-terminal-1"));
    expect(cardEntry).toBeDefined();
    // Card-only bubble — the pre-tool text was already finalized as its own
    // entry by text_complete, so this one carries no duplicate text.
    expect(cardEntry?.content).toBe("");

    // No duplicate text bubble: the pre-tool text appears exactly once.
    const textBubbles = messages.filter((m) => m.content === "Let me build that:");
    expect(textBubbles).toHaveLength(1);

    // Exactly two finalized entries for this turn: the text bubble and the
    // card-only bubble.
    expect(messages).toHaveLength(2);
  });

  it("still finalizes correctly when text follows the artifact (existing working shape, unchanged)", async () => {
    vi.useFakeTimers();

    mountHook(() => useSSE(AGENT_ID));

    inject(AGENT_ID, "run_started");

    inject(AGENT_ID, "tool_call_completed", {
      tool_name: "ArtifactWrite",
      output: JSON.stringify({ id: "artifact-trailing-1", renderer: "table", refresh_intent: "none", title: "T" }),
    });

    inject(AGENT_ID, "text_delta", { text: "Done — here's your board." });
    inject(AGENT_ID, "text_complete", { text: "Done — here's your board." });

    inject(AGENT_ID, "run_ended", { reason: "Completed" });

    await act(async () => {
      await vi.advanceTimersByTimeAsync(TEARDOWN_DELAY_MS);
    });

    expect(useChatStore.getState().inFlightByAgent.has(AGENT_ID)).toBe(false);

    const messages = useChatStore.getState().messages;
    expect(messages).toHaveLength(1);
    expect(messages[0].content).toBe("Done — here's your board.");
    expect(artifactIdsOf(messages[0])).toEqual(["artifact-trailing-1"]);
  });
});
