/**
 * Pins the "Forked here" divider behaviour in `buildMessageItems`.
 *
 * Backend fix: opening a branch thread now merges in inherited (pre-fork)
 * history from the SOURCE thread's transcript (see
 * `crates/ao-server/src/routes/messages.rs::merge_inherited_tail`), so the
 * merged message array can contain both inherited and own-thread entries.
 * `historyFloorTs` (the branch's fork-point timestamp) tells
 * `buildMessageItems` where that boundary sits so the UI can render a
 * one-time divider there instead of leaving inherited and own messages
 * visually indistinguishable.
 */

import { describe, it, expect } from "vitest";
import { buildMessageItems } from "../MessageList";
import type { TranscriptEntry } from "../../../types/api";

function mkUserEntry(ts: string, content: string): TranscriptEntry {
  return { ts, role: "user", content, event_type: "message" };
}

function mkAgentEntry(ts: string, content: string, agentId = "agent-a", turnId?: string): TranscriptEntry {
  return {
    ts,
    role: { agent: agentId },
    content,
    event_type: "response",
    metadata: turnId ? { turn_id: turnId } : null,
  };
}

describe("buildMessageItems — fork divider (history_floor_ts)", () => {
  it("marks the first post-floor entry when the floor sits mid-list", () => {
    const floor = "2026-06-01T00:00:02Z";
    const messages: TranscriptEntry[] = [
      mkUserEntry("2026-06-01T00:00:00Z", "inherited 0"),
      mkAgentEntry("2026-06-01T00:00:01Z", "inherited 1"),
      mkUserEntry("2026-06-01T00:00:02Z", "inherited 2 (== floor, still inherited)"),
      mkUserEntry("2026-06-01T00:00:03Z", "own 0"),
      mkAgentEntry("2026-06-01T00:00:04Z", "own 1"),
    ];

    const { items } = buildMessageItems(messages, floor);

    expect(items).toHaveLength(5);
    expect(items.map((i) => !!i.showForkDivider)).toEqual([false, false, false, true, false]);
    expect(items[3].entry.content).toBe("own 0");
  });

  it("does not mark anything when historyFloorTs is null (non-branch thread)", () => {
    const messages: TranscriptEntry[] = [
      mkUserEntry("2026-06-01T00:00:00Z", "a"),
      mkAgentEntry("2026-06-01T00:00:01Z", "b"),
    ];
    const { items } = buildMessageItems(messages, null);
    expect(items.every((i) => !i.showForkDivider)).toBe(true);
  });

  it("does not mark anything when historyFloorTs is omitted (back-compat call sites)", () => {
    const messages: TranscriptEntry[] = [mkUserEntry("2026-06-01T00:00:00Z", "a")];
    const { items } = buildMessageItems(messages);
    expect(items.every((i) => !i.showForkDivider)).toBe(true);
  });

  it("does not mark anything when every entry is inherited (floor not yet reached in this window)", () => {
    const floor = "2026-06-01T01:00:00Z";
    const messages: TranscriptEntry[] = [
      mkUserEntry("2026-06-01T00:00:00Z", "inherited 0"),
      mkAgentEntry("2026-06-01T00:00:01Z", "inherited 1"),
    ];
    const { items } = buildMessageItems(messages, floor);
    expect(items.every((i) => !i.showForkDivider)).toBe(true);
  });

  it("marks the very first item when even the earliest loaded entry is already post-floor", () => {
    // Simulates a page of "load older" that landed entirely within the
    // branch's own (post-fork) history, e.g. after inherited history was
    // already exhausted on an earlier page — the divider still needs to
    // fire the first time this window is built with entries past the floor
    // and no prior entry to compare against.
    const floor = "2026-05-01T00:00:00Z";
    const messages: TranscriptEntry[] = [
      mkUserEntry("2026-06-01T00:00:00Z", "own 0"),
      mkAgentEntry("2026-06-01T00:00:01Z", "own 1"),
    ];
    const { items } = buildMessageItems(messages, floor);
    expect(items[0].showForkDivider).toBe(true);
    expect(items[1].showForkDivider).toBeFalsy();
  });

  it("reattaches the divider to the coalesced row when the boundary entry folds via turn_id", () => {
    // The first post-floor entry is same-agent + same-turn_id as the last
    // inherited entry, so it folds into that bubble's row instead of
    // getting its own item (Pass 2b). The divider must land on that row,
    // not silently vanish.
    const floor = "2026-06-01T00:00:01Z";
    const messages: TranscriptEntry[] = [
      mkUserEntry("2026-06-01T00:00:00Z", "inherited user turn"),
      mkAgentEntry("2026-06-01T00:00:01Z", "inherited half of the turn", "agent-a", "t1"),
      mkAgentEntry("2026-06-01T00:00:02Z", "own half of the same turn", "agent-a", "t1"),
    ];
    const { items } = buildMessageItems(messages, floor);

    // Folds into one bubble for the agent turn — 2 items total.
    expect(items).toHaveLength(2);
    expect(items[1].coalescedSegments).toHaveLength(2);
    expect(items[1].showForkDivider).toBe(true);
  });
});
