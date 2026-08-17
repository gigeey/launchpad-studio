/**
 * Reload/historical path for inline artifact cards.
 *
 * `ArtifactWrite`'s tool result JSON (`{id, renderer, refresh_intent, title}`)
 * is already sitting in the persisted transcript's `tool_result` entry
 * (`metadata.output`), correlated to its `tool_use` entry (which carries
 * `tool_name` + `turn_id`) via `tool_use_id`. `extractArtifactWriteResults`
 * scans for that pair and `buildMessageItems` attaches the resolved id(s) to
 * whichever bubble the producing turn coalesces into — no fetch, no
 * source_message_id/turn_id cross-store matching required.
 */

import { describe, it, expect } from "vitest";
import { buildMessageItems, extractArtifactWriteResults } from "../MessageList";
import type { TranscriptEntry } from "../../../types/api";

const AGENT_A = "agent-a";

function mkAgentEntry(opts: { ts: string; content: string; turnId?: string; eventType?: string }): TranscriptEntry {
  return {
    ts: opts.ts,
    role: { agent: AGENT_A },
    content: opts.content,
    event_type: opts.eventType ?? "response",
    metadata: opts.turnId ? { turn_id: opts.turnId } : null,
  };
}

function mkToolUseEntry(opts: { ts: string; turnId: string; toolUseId: string; toolName: string }): TranscriptEntry {
  return {
    ts: opts.ts,
    role: { agent: AGENT_A },
    content: "",
    event_type: "tool_use",
    metadata: { turn_id: opts.turnId, tool_use_id: opts.toolUseId, tool_name: opts.toolName, input: {} },
  };
}

function mkToolResultEntry(opts: { ts: string; turnId: string; toolUseId: string; output: string; isError?: boolean }): TranscriptEntry {
  return {
    ts: opts.ts,
    role: "tool",
    content: "",
    event_type: "tool_result",
    metadata: { turn_id: opts.turnId, tool_use_id: opts.toolUseId, output: opts.output, is_error: opts.isError ?? false },
  };
}

function artifactWriteOutput(overrides: Partial<{ id: string; title: string; renderer: string; refresh_intent: string }> = {}): string {
  return JSON.stringify({
    id: "artifact-1",
    renderer: "table",
    refresh_intent: "none",
    title: "Weekly report",
    ...overrides,
  });
}

describe("extractArtifactWriteResults", () => {
  it("correlates an ArtifactWrite tool_use/tool_result pair by tool_use_id and groups by turn_id", () => {
    const messages: TranscriptEntry[] = [
      mkToolUseEntry({ ts: "t0", turnId: "turn-1", toolUseId: "tu-1", toolName: "ArtifactWrite" }),
      mkToolResultEntry({ ts: "t1", turnId: "turn-1", toolUseId: "tu-1", output: artifactWriteOutput() }),
    ];
    const { idsByTurnId, stubs } = extractArtifactWriteResults(messages);
    expect(idsByTurnId.get("turn-1")).toEqual(["artifact-1"]);
    expect(stubs).toEqual([{ id: "artifact-1", title: "Weekly report", kind: "table", refresh_intent: "none" }]);
  });

  it("ignores tool_use entries for other tools", () => {
    const messages: TranscriptEntry[] = [
      mkToolUseEntry({ ts: "t0", turnId: "turn-1", toolUseId: "tu-1", toolName: "Bash" }),
      mkToolResultEntry({ ts: "t1", turnId: "turn-1", toolUseId: "tu-1", output: "some shell output" }),
    ];
    const { idsByTurnId, stubs } = extractArtifactWriteResults(messages);
    expect(idsByTurnId.size).toBe(0);
    expect(stubs).toEqual([]);
  });

  it("skips an ArtifactWrite call whose result is a validation error (non-JSON output)", () => {
    const messages: TranscriptEntry[] = [
      mkToolUseEntry({ ts: "t0", turnId: "turn-1", toolUseId: "tu-1", toolName: "ArtifactWrite" }),
      mkToolResultEntry({ ts: "t1", turnId: "turn-1", toolUseId: "tu-1", output: "error: Missing required field: title", isError: true }),
    ];
    const { idsByTurnId, stubs } = extractArtifactWriteResults(messages);
    expect(idsByTurnId.size).toBe(0);
    expect(stubs).toEqual([]);
  });

  it("collects multiple artifacts produced in the same turn", () => {
    const messages: TranscriptEntry[] = [
      mkToolUseEntry({ ts: "t0", turnId: "turn-1", toolUseId: "tu-1", toolName: "ArtifactWrite" }),
      mkToolResultEntry({ ts: "t1", turnId: "turn-1", toolUseId: "tu-1", output: artifactWriteOutput({ id: "artifact-1" }) }),
      mkToolUseEntry({ ts: "t2", turnId: "turn-1", toolUseId: "tu-2", toolName: "ArtifactWrite" }),
      mkToolResultEntry({ ts: "t3", turnId: "turn-1", toolUseId: "tu-2", output: artifactWriteOutput({ id: "artifact-2", title: "Second" }) }),
    ];
    const { idsByTurnId } = extractArtifactWriteResults(messages);
    expect(idsByTurnId.get("turn-1")).toEqual(["artifact-1", "artifact-2"]);
  });
});

describe("buildMessageItems — attaches artifactIds to the coalesced bubble (reload path)", () => {
  it("exposes the ArtifactWrite id on the assistant bubble that shares its turn_id", () => {
    const turnId = "turn-1";
    const messages: TranscriptEntry[] = [
      mkToolUseEntry({ ts: "2026-07-11T00:00:00Z", turnId, toolUseId: "tu-1", toolName: "ArtifactWrite" }),
      mkToolResultEntry({ ts: "2026-07-11T00:00:01Z", turnId, toolUseId: "tu-1", output: artifactWriteOutput() }),
      mkAgentEntry({ ts: "2026-07-11T00:00:02Z", content: "Here's your report.", turnId }),
    ];
    const { idsByTurnId } = extractArtifactWriteResults(messages);
    const { items } = buildMessageItems(messages, null, idsByTurnId);

    // tool_use/tool_result are filtered from the visible flow — one bubble.
    expect(items).toHaveLength(1);
    expect(items[0].entry.content).toBe("Here's your report.");
    expect(items[0].artifactIds).toEqual(["artifact-1"]);
  });

  it("unions ids across entries folded into the same turn-id-coalesced bubble", () => {
    const turnId = "turn-1";
    const messages: TranscriptEntry[] = [
      mkAgentEntry({ ts: "2026-07-11T00:00:00Z", content: "Building it now.", turnId }),
      mkToolUseEntry({ ts: "2026-07-11T00:00:01Z", turnId, toolUseId: "tu-1", toolName: "ArtifactWrite" }),
      mkToolResultEntry({ ts: "2026-07-11T00:00:02Z", turnId, toolUseId: "tu-1", output: artifactWriteOutput() }),
      mkAgentEntry({ ts: "2026-07-11T00:00:03Z", content: "Done — see above.", turnId }),
    ];
    const { idsByTurnId } = extractArtifactWriteResults(messages);
    const { items } = buildMessageItems(messages, null, idsByTurnId);

    expect(items).toHaveLength(1);
    expect(items[0].coalescedSegments).toHaveLength(2);
    expect(items[0].artifactIds).toEqual(["artifact-1"]);
  });

  it("does not attach artifact ids to an unrelated turn", () => {
    const messages: TranscriptEntry[] = [
      mkToolUseEntry({ ts: "2026-07-11T00:00:00Z", turnId: "turn-1", toolUseId: "tu-1", toolName: "ArtifactWrite" }),
      mkToolResultEntry({ ts: "2026-07-11T00:00:01Z", turnId: "turn-1", toolUseId: "tu-1", output: artifactWriteOutput() }),
      mkAgentEntry({ ts: "2026-07-11T00:00:02Z", content: "Reply for turn 1.", turnId: "turn-1" }),
      mkAgentEntry({ ts: "2026-07-11T00:01:00Z", content: "Unrelated later turn.", turnId: "turn-2" }),
    ];
    const { idsByTurnId } = extractArtifactWriteResults(messages);
    const { items } = buildMessageItems(messages, null, idsByTurnId);

    expect(items).toHaveLength(2);
    expect(items[0].artifactIds).toEqual(["artifact-1"]);
    expect(items[1].artifactIds).toBeUndefined();
  });

  it("also picks up the live-finalized metadata.artifact_ids field (no turn_id needed)", () => {
    // What `finalizeInFlightText` stamps client-side onto a just-finalized
    // live reply — no tool_use/tool_result entries exist for it yet.
    const messages: TranscriptEntry[] = [
      {
        ts: "2026-07-11T00:00:00Z",
        role: { agent: AGENT_A },
        content: "Here it is, live.",
        event_type: "message",
        metadata: { artifact_ids: ["artifact-live-1"] },
      },
    ];
    const { items } = buildMessageItems(messages);
    expect(items).toHaveLength(1);
    expect(items[0].artifactIds).toEqual(["artifact-live-1"]);
  });

  it("defaults artifactIds to undefined when there is nothing to attach", () => {
    const messages: TranscriptEntry[] = [
      mkAgentEntry({ ts: "2026-07-11T00:00:00Z", content: "Just text, no artifact." }),
    ];
    const { items } = buildMessageItems(messages);
    expect(items[0].artifactIds).toBeUndefined();
  });
});
