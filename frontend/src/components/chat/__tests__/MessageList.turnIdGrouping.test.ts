/**
 * Pins the turn-id-based bubble grouping behaviour in `buildMessageItems`.
 *
 * The continuation runner can fire multiple `response` events under one
 * logical model turn (e.g. the model emits XML + speculative prose →
 * dispatch → respawn delivers real outcome prose). Transcript persistence
 * stamps a shared `metadata.turn_id` on every event of the turn. Without
 * grouping the user saw two bubbles per turn —
 * benign-looking in the success case (prose flows), confusing in the
 * failure case (speculation contradicts the correction).
 *
 * These tests pin that:
 *   1. Consecutive agent-role entries sharing `metadata.turn_id` fold into
 *      a single bubble's `coalescedSegments[]` (text, text).
 *   2. `tool_use` / `tool_result` event_types are filtered from the
 *      visible flow so they don't break the prev-bubble chain or reserve
 *      empty virtualizer slots.
 *   3. Mixed turn_id values still render as separate bubbles.
 *   4. Skill-chip coalescing still works when turn_id is also present
 *      (chip path runs first and wins; segments include the chip).
 *   5. The user message between two distinct model turns breaks the
 *      coalesce chain even if some later turn_id is shared (no false
 *      bridging through unrelated user turns).
 */

import { describe, it, expect } from "vitest";
import { buildMessageItems } from "../MessageList";
import type { TranscriptEntry } from "../../../types/api";

const AGENT_A = "agent-a";

function mkAgentEntry(opts: {
  ts: string;
  content: string;
  turnId?: string;
  eventType?: string;
  agentId?: string;
}): TranscriptEntry {
  return {
    ts: opts.ts,
    role: { agent: opts.agentId ?? AGENT_A },
    content: opts.content,
    event_type: opts.eventType ?? "response",
    metadata: opts.turnId ? { turn_id: opts.turnId } : null,
  };
}

function mkToolResultEntry(opts: { ts: string; turnId: string }): TranscriptEntry {
  return {
    ts: opts.ts,
    role: "tool",
    content: "",
    event_type: "tool_result",
    metadata: { turn_id: opts.turnId, output: "<tool_result>…</tool_result>", is_error: false },
  };
}

function mkUserEntry(opts: { ts: string; content: string }): TranscriptEntry {
  return {
    ts: opts.ts,
    role: "user",
    content: opts.content,
    event_type: "message",
  };
}

describe("buildMessageItems — turn_id coalescing", () => {
  it("folds two same-turn agent response entries into one bubble", () => {
    // Production-shape repro: a real transcript turn had one entry (XML +
    // speculation prose) followed by a later one (continuation prose) under
    // the same turn_id, with tool_use+tool_result in between.
    const turnId = "3661f8cf-a5d0-4eb6-8f01-3234845add4a";
    const messages: TranscriptEntry[] = [
      mkUserEntry({ ts: "2026-05-13T14:33:54Z", content: "umm create another one testing845" }),
      mkAgentEntry({
        ts: "2026-05-13T14:34:04Z",
        content: "Hmm — the structured WorkflowActionCreate call is failing.",
        turnId,
      }),
      // Filtered out of visible flow:
      { ts: "2026-05-13T14:34:04.1Z", role: { agent: AGENT_A }, content: "", event_type: "tool_use", metadata: { turn_id: turnId, tool_use_id: "x", tool_name: "WorkflowActionCreate", input: {} } },
      mkToolResultEntry({ ts: "2026-05-13T14:34:04.2Z", turnId }),
      mkAgentEntry({
        ts: "2026-05-13T14:34:20Z",
        content: "I'll read the interview prompt. What are we building?",
        turnId,
      }),
    ];

    const { items, orphanChips } = buildMessageItems(messages);
    expect(orphanChips).toEqual([]);

    // Two bubbles total: the user message and ONE merged agent bubble.
    expect(items).toHaveLength(2);
    expect(items[0].entry.role).toBe("user");

    const agentBubble = items[1];
    expect(agentBubble.entry.role).toEqual({ agent: AGENT_A });
    // Both response entries' text live in coalescedSegments.
    expect(agentBubble.coalescedSegments).toBeDefined();
    expect(agentBubble.coalescedSegments).toHaveLength(2);
    expect(agentBubble.coalescedSegments?.[0]).toMatchObject({
      kind: "text",
      content: expect.stringContaining("failing"),
    });
    expect(agentBubble.coalescedSegments?.[1]).toMatchObject({
      kind: "text",
      content: expect.stringContaining("interview prompt"),
    });
  });

  it("filters tool_use and tool_result entries out of the visible flow", () => {
    // Even when no turn_id coalesce applies, dropping these from `visible`
    // avoids reserving null-bubble virtualizer slots.
    const messages: TranscriptEntry[] = [
      mkAgentEntry({ ts: "2026-05-13T14:00:00Z", content: "Hello", turnId: "t1" }),
      { ts: "2026-05-13T14:00:01Z", role: { agent: AGENT_A }, content: "", event_type: "tool_use", metadata: { turn_id: "t1" } },
      mkToolResultEntry({ ts: "2026-05-13T14:00:02Z", turnId: "t1" }),
    ];
    const { items } = buildMessageItems(messages);
    expect(items).toHaveLength(1);
    expect(items[0].entry.event_type).toBe("response");
  });

  it("does NOT fold entries with different turn_ids", () => {
    // Two distinct model turns from the same agent — must remain two
    // separate bubbles even though same sender.
    const messages: TranscriptEntry[] = [
      mkAgentEntry({ ts: "2026-05-13T14:00:00Z", content: "Turn one prose.", turnId: "t1" }),
      mkAgentEntry({ ts: "2026-05-13T14:01:00Z", content: "Turn two prose.", turnId: "t2" }),
    ];
    const { items } = buildMessageItems(messages);
    expect(items).toHaveLength(2);
    expect(items[0].coalescedSegments).toBeUndefined();
    expect(items[1].coalescedSegments).toBeUndefined();
    expect(items[0].entry.content).toBe("Turn one prose.");
    expect(items[1].entry.content).toBe("Turn two prose.");
  });

  it("does NOT fold legacy entries that have no turn_id at all", () => {
    // Legacy transcripts have `metadata: null`. They must keep rendering
    // as separate bubbles regardless of sender continuity.
    const messages: TranscriptEntry[] = [
      mkAgentEntry({ ts: "2026-05-13T14:00:00Z", content: "First." }),
      mkAgentEntry({ ts: "2026-05-13T14:01:00Z", content: "Second." }),
    ];
    const { items } = buildMessageItems(messages);
    expect(items).toHaveLength(2);
    expect(items[0].coalescedSegments).toBeUndefined();
    expect(items[1].coalescedSegments).toBeUndefined();
  });

  it("does NOT bridge across a user message even when later turn_id repeats", () => {
    // User message in the middle breaks sender continuity. Even if a stale
    // turn_id resurfaces later, fold must not span the user turn.
    const messages: TranscriptEntry[] = [
      mkAgentEntry({ ts: "2026-05-13T14:00:00Z", content: "First agent prose.", turnId: "t1" }),
      mkUserEntry({ ts: "2026-05-13T14:00:30Z", content: "user reply" }),
      mkAgentEntry({ ts: "2026-05-13T14:01:00Z", content: "Second agent prose.", turnId: "t1" }),
    ];
    const { items } = buildMessageItems(messages);
    expect(items).toHaveLength(3);
    expect(items[0].entry.content).toBe("First agent prose.");
    expect(items[1].entry.role).toBe("user");
    expect(items[2].entry.content).toBe("Second agent prose.");
    expect(items[2].coalescedSegments).toBeUndefined();
  });

  it("skill-chip coalesce wins over turn_id when both could apply", () => {
    // Hidden skill-load between two same-turn_id agent messages from the
    // same agent. The existing chip-coalesce path runs first and merges
    // text + chip + text into one bubble; the turn_id branch is a no-op.
    const messages: TranscriptEntry[] = [
      mkAgentEntry({ ts: "2026-05-13T14:00:00Z", content: "Loading the helper.", turnId: "t1" }),
      {
        ts: "2026-05-13T14:00:01Z",
        role: "user",
        content: '[skill "verify-studio" loaded]\nbody-here',
        event_type: "message",
        hidden_from_user: true,
      },
      mkAgentEntry({ ts: "2026-05-13T14:00:02Z", content: "Done.", turnId: "t1" }),
    ];
    const { items } = buildMessageItems(messages);
    expect(items).toHaveLength(1);
    const segs = items[0].coalescedSegments;
    expect(segs).toBeDefined();
    expect(segs).toHaveLength(3);
    expect(segs?.[0]).toMatchObject({ kind: "text", content: "Loading the helper." });
    expect(segs?.[1]).toMatchObject({ kind: "chip", skillName: "verify-studio", success: true });
    expect(segs?.[2]).toMatchObject({ kind: "text", content: "Done." });
  });

  it("preserves the second entry's text when the first had empty content", () => {
    // Edge case: prior fold left prev with empty content + no segments;
    // turn_id coalesce must not lose the second entry's text.
    const messages: TranscriptEntry[] = [
      mkAgentEntry({ ts: "2026-05-13T14:00:00Z", content: "", turnId: "t1" }),
      mkAgentEntry({ ts: "2026-05-13T14:00:01Z", content: "real prose", turnId: "t1" }),
    ];
    const { items } = buildMessageItems(messages);
    expect(items).toHaveLength(1);
    expect(items[0].coalescedSegments).toEqual([{ kind: "text", content: "real prose" }]);
  });

  it("does NOT fold when agents differ even if turn_id matches", () => {
    // Defensive: two agents accidentally sharing a turn_id (shouldn't
    // happen in practice but the grouping check must hold).
    const messages: TranscriptEntry[] = [
      mkAgentEntry({ ts: "2026-05-13T14:00:00Z", content: "From A.", turnId: "t1", agentId: "a" }),
      mkAgentEntry({ ts: "2026-05-13T14:00:01Z", content: "From B.", turnId: "t1", agentId: "b" }),
    ];
    const { items } = buildMessageItems(messages);
    expect(items).toHaveLength(2);
    expect(items[0].coalescedSegments).toBeUndefined();
    expect(items[1].coalescedSegments).toBeUndefined();
  });
});
