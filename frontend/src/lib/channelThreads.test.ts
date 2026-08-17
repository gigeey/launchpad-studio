/**
 * Unit coverage for `resolveChannelThreadPartition` (lib/channelThreads.ts) —
 * the shared data layer both the collapsed "Channels" strip tile and Home's
 * "Channels" section
 * derive their rendering from. Verifies the partition (working vs. channel
 * threads), the per-kind grouping + recency ordering, the label/icon-key
 * derivation (including the title-empty fallback), and the aggregate/
 * per-kind unread counts — all pure derivation over plain `Thread` fixtures,
 * no store or React rendering involved.
 */
import { describe, it, expect } from "vitest";
import { resolveChannelThreadPartition } from "./channelThreads";
import { threadActivityKey } from "../components/shared/ThreadActivityBadge";
import type { ChannelBridgeOrigin, Thread } from "../types/api";

const AGENT_ID = "agent-1";

function makeThread(overrides: Partial<Thread> & { id: string }): Thread {
  return {
    title: null,
    scope: { type: "AgentChat", agent_id: AGENT_ID },
    transcript_path: `/tmp/${overrides.id}.jsonl`,
    kind: "fresh",
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function slackOrigin(bindingId = "slack-binding-1"): ChannelBridgeOrigin {
  return { kind: "slack", binding_id: bindingId };
}

describe("resolveChannelThreadPartition", () => {
  it("puts a null-channel_origin thread in the working bucket, not the channel bucket", () => {
    const working = makeThread({ id: "thread-working-1", channel_origin: null });
    const result = resolveChannelThreadPartition(AGENT_ID, [working], new Set());

    expect(result.workingThreads).toEqual([working]);
    expect(result.channelThreads).toEqual([]);
    expect(result.channelGroups).toEqual([]);
  });

  it("puts an undefined channel_origin thread in the working bucket too", () => {
    const working = makeThread({ id: "thread-working-2" });
    const result = resolveChannelThreadPartition(AGENT_ID, [working], new Set());

    expect(result.workingThreads).toEqual([working]);
    expect(result.channelThreads).toEqual([]);
  });

  it("puts a non-null channel_origin thread in the channel bucket, not the working bucket", () => {
    const slackThread = makeThread({
      id: "thread-slack-1",
      title: "💬 Slack — general",
      channel_origin: slackOrigin(),
    });
    const result = resolveChannelThreadPartition(AGENT_ID, [slackThread], new Set());

    expect(result.workingThreads).toEqual([]);
    expect(result.channelThreads).toHaveLength(1);
    expect(result.channelThreads[0].thread).toBe(slackThread);
  });

  it("groups channel threads by channel_origin.kind, one group per kind", () => {
    const slack1 = makeThread({ id: "thread-slack-1", channel_origin: slackOrigin() });
    const slack2 = makeThread({ id: "thread-slack-2", channel_origin: slackOrigin() });
    const discord1 = makeThread({
      id: "thread-discord-1",
      channel_origin: { kind: "discord", binding_id: "discord-binding-1" },
    });
    const working = makeThread({ id: "thread-working-1" });

    const result = resolveChannelThreadPartition(AGENT_ID, [slack1, slack2, discord1, working], new Set());

    expect(result.workingThreads).toEqual([working]);
    expect(result.channelGroups).toHaveLength(2);
    const slackGroup = result.channelGroups.find((g) => g.kind === "slack");
    const discordGroup = result.channelGroups.find((g) => g.kind === "discord");
    expect(slackGroup?.threads.map((t) => t.thread.id).sort()).toEqual(["thread-slack-1", "thread-slack-2"]);
    expect(discordGroup?.threads.map((t) => t.thread.id)).toEqual(["thread-discord-1"]);
  });

  it("orders threads within a kind by most-recent created_at first (same recency field ThreadTabStrip/HomeSidebar sort by)", () => {
    const older = makeThread({
      id: "thread-slack-older",
      created_at: "2026-01-01T00:00:00Z",
      channel_origin: slackOrigin(),
    });
    const newer = makeThread({
      id: "thread-slack-newer",
      created_at: "2026-06-01T00:00:00Z",
      channel_origin: slackOrigin(),
    });

    const result = resolveChannelThreadPartition(AGENT_ID, [older, newer], new Set());
    const slackGroup = result.channelGroups.find((g) => g.kind === "slack");

    expect(slackGroup?.threads.map((t) => t.thread.id)).toEqual(["thread-slack-newer", "thread-slack-older"]);
  });

  it("derives the icon key from channel_origin.kind and the label from thread.title", () => {
    const slackThread = makeThread({
      id: "thread-slack-1",
      title: "💬 Slack — general",
      channel_origin: slackOrigin(),
    });

    const result = resolveChannelThreadPartition(AGENT_ID, [slackThread], new Set());

    expect(result.channelThreads[0].kind).toBe("slack");
    expect(result.channelThreads[0].label).toBe("💬 Slack — general");
  });

  it("falls back to auto_title, then the channel's display name, when title is empty", () => {
    const withAutoTitle = makeThread({
      id: "thread-slack-auto",
      title: null,
      auto_title: "Auto-derived title",
      channel_origin: slackOrigin(),
    });
    const withNeither = makeThread({
      id: "thread-slack-blank",
      title: "   ",
      auto_title: null,
      channel_origin: slackOrigin(),
    });

    const result = resolveChannelThreadPartition(AGENT_ID, [withAutoTitle, withNeither], new Set());
    const byId = new Map(result.channelThreads.map((t) => [t.thread.id, t]));

    expect(byId.get("thread-slack-auto")?.label).toBe("Auto-derived title");
    expect(byId.get("thread-slack-blank")?.label).toBe("Slack");
  });

  it("marks a channel thread unread via the same unreadThreadIds source ThreadTabStrip reads, and rolls it up into the aggregate + per-kind counts", () => {
    const unreadSlack = makeThread({ id: "thread-slack-unread", channel_origin: slackOrigin() });
    const readSlack = makeThread({ id: "thread-slack-read", channel_origin: slackOrigin() });
    const unreadDiscord = makeThread({
      id: "thread-discord-unread",
      channel_origin: { kind: "discord", binding_id: "discord-binding-1" },
    });

    const unreadThreadIds = new Set<string>([
      threadActivityKey(AGENT_ID, unreadSlack),
      threadActivityKey(AGENT_ID, unreadDiscord),
    ]);

    const result = resolveChannelThreadPartition(
      AGENT_ID,
      [unreadSlack, readSlack, unreadDiscord],
      unreadThreadIds,
    );
    const byId = new Map(result.channelThreads.map((t) => [t.thread.id, t]));

    expect(byId.get("thread-slack-unread")?.unread).toBe(true);
    expect(byId.get("thread-slack-read")?.unread).toBe(false);
    expect(byId.get("thread-discord-unread")?.unread).toBe(true);

    const slackGroup = result.channelGroups.find((g) => g.kind === "slack");
    const discordGroup = result.channelGroups.find((g) => g.kind === "discord");
    expect(slackGroup?.unreadCount).toBe(1);
    expect(discordGroup?.unreadCount).toBe(1);
    expect(result.totalUnreadCount).toBe(2);
  });

  it("returns empty groups and zero unread when there are no channel threads at all", () => {
    const working1 = makeThread({ id: "thread-working-1" });
    const working2 = makeThread({ id: "thread-working-2", channel_origin: null });

    const result = resolveChannelThreadPartition(AGENT_ID, [working1, working2], new Set());

    expect(result.workingThreads).toEqual([working1, working2]);
    expect(result.channelThreads).toEqual([]);
    expect(result.channelGroups).toEqual([]);
    expect(result.totalUnreadCount).toBe(0);
  });

  it("returns empty everything for an empty thread list", () => {
    const result = resolveChannelThreadPartition(AGENT_ID, [], new Set());

    expect(result.workingThreads).toEqual([]);
    expect(result.channelThreads).toEqual([]);
    expect(result.channelGroups).toEqual([]);
    expect(result.totalUnreadCount).toBe(0);
  });

  // Coverage for the archive/close button added to channel threads
  // (ChannelsTilePanel's and HomeSidebar's row `X`) — an archived channel
  // thread must disappear from both the collapsed tile and the Home
  // "Channels" section, since both derive their rendering from this one
  // selector. Unlike a working thread's `archived_at` (which callers filter
  // out of `workingThreads` themselves, since `ThreadOverflowPanel`'s
  // "Archived" tab still needs to list them), a channel thread never
  // surfaces anywhere once archived — there's no channel equivalent of that
  // recovery tab yet.
  it("drops an archived channel thread from channelThreads/channelGroups entirely", () => {
    const archivedSlack = makeThread({
      id: "thread-slack-archived",
      channel_origin: slackOrigin(),
      archived_at: "2026-02-01T00:00:00Z",
    });
    const liveSlack = makeThread({ id: "thread-slack-live", channel_origin: slackOrigin() });

    const result = resolveChannelThreadPartition(AGENT_ID, [archivedSlack, liveSlack], new Set());

    expect(result.channelThreads.map((t) => t.thread.id)).toEqual(["thread-slack-live"]);
    const slackGroup = result.channelGroups.find((g) => g.kind === "slack");
    expect(slackGroup?.threads.map((t) => t.thread.id)).toEqual(["thread-slack-live"]);
  });

  it("does not leak an archived channel thread into workingThreads either", () => {
    const archivedSlack = makeThread({
      id: "thread-slack-archived",
      channel_origin: slackOrigin(),
      archived_at: "2026-02-01T00:00:00Z",
    });

    const result = resolveChannelThreadPartition(AGENT_ID, [archivedSlack], new Set());

    expect(result.workingThreads).toEqual([]);
  });

  it("removes an entire kind's group once its only channel thread is archived", () => {
    const archivedDiscord = makeThread({
      id: "thread-discord-archived",
      channel_origin: { kind: "discord", binding_id: "discord-binding-1" },
      archived_at: "2026-02-01T00:00:00Z",
    });

    const result = resolveChannelThreadPartition(AGENT_ID, [archivedDiscord], new Set());

    expect(result.channelGroups).toEqual([]);
  });

  it("still includes an archived non-channel (working) thread — only channel-origin archiving is filtered here", () => {
    const archivedWorking = makeThread({ id: "thread-working-archived", archived_at: "2026-02-01T00:00:00Z" });

    const result = resolveChannelThreadPartition(AGENT_ID, [archivedWorking], new Set());

    // Working-thread archive filtering stays the caller's job (ThreadTabStrip/
    // HomeSidebar each filter `!t.archived_at` themselves downstream) — this
    // selector must not silently start dropping working threads too, or
    // `ThreadOverflowPanel`'s "Archived" tab (sourced from `workingThreads`)
    // would lose entries.
    expect(result.workingThreads).toEqual([archivedWorking]);
  });
});
