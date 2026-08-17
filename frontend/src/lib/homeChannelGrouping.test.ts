/**
 * Unit coverage for `groupHomeChannelThreads` (lib/homeChannelGrouping.ts) —
 * the merge-across-agents layer HomeSidebar's "Channels" section builds on
 * top of
 * `resolveChannelThreadPartition`'s per-agent output. Verifies both group-by
 * modes (by channel kind, by owning agent), recency ordering within a group
 * and across groups, and the per-group unread rollup — all pure derivation
 * over plain fixtures, no store or React rendering involved.
 */
import { describe, it, expect } from "vitest";
import { groupHomeChannelThreads, type HomeChannelThreadInfo } from "./homeChannelGrouping";
import type { ChannelOriginKind } from "./channelThreads";
import type { Thread } from "../types/api";

interface MakeItemOptions {
  id: string;
  kind?: ChannelOriginKind;
  agentId?: string;
  agentName?: string;
  unread?: boolean;
  createdAt?: string;
}

function makeItem({ id, kind = "slack", agentId = "agent-1", agentName = "Agent One", unread = false, createdAt = "2026-01-01T00:00:00Z" }: MakeItemOptions): HomeChannelThreadInfo {
  const thread: Thread = {
    id,
    title: null,
    scope: { type: "AgentChat", agent_id: agentId },
    transcript_path: `/tmp/${id}.jsonl`,
    kind: "fresh",
    created_at: createdAt,
    updated_at: createdAt,
  };
  return { thread, kind, label: id, unread, agentId, agentName, agentEmoji: null };
}

describe("groupHomeChannelThreads", () => {
  it("groups by channel_origin.kind when groupBy is 'channel', merging threads from different agents into one kind group", () => {
    const slackFromAgentA = makeItem({ id: "thread-1", kind: "slack", agentId: "agent-a", agentName: "Agent A" });
    const slackFromAgentB = makeItem({ id: "thread-2", kind: "slack", agentId: "agent-b", agentName: "Agent B" });
    const discordFromAgentA = makeItem({ id: "thread-3", kind: "discord", agentId: "agent-a", agentName: "Agent A" });

    const groups = groupHomeChannelThreads([slackFromAgentA, slackFromAgentB, discordFromAgentA], "channel");

    expect(groups).toHaveLength(2);
    const slackGroup = groups.find((g) => g.key === "slack");
    const discordGroup = groups.find((g) => g.key === "discord");
    expect(slackGroup?.threads.map((t) => t.thread.id).sort()).toEqual(["thread-1", "thread-2"]);
    expect(discordGroup?.threads.map((t) => t.thread.id)).toEqual(["thread-3"]);
  });

  it("groups by owning agent when groupBy is 'agent', keeping different channel kinds from the same agent together", () => {
    const slackFromAgentA = makeItem({ id: "thread-1", kind: "slack", agentId: "agent-a", agentName: "Agent A" });
    const discordFromAgentA = makeItem({ id: "thread-2", kind: "discord", agentId: "agent-a", agentName: "Agent A" });
    const slackFromAgentB = makeItem({ id: "thread-3", kind: "slack", agentId: "agent-b", agentName: "Agent B" });

    const groups = groupHomeChannelThreads([slackFromAgentA, discordFromAgentA, slackFromAgentB], "agent");

    expect(groups).toHaveLength(2);
    const agentAGroup = groups.find((g) => g.key === "agent-a");
    const agentBGroup = groups.find((g) => g.key === "agent-b");
    expect(agentAGroup?.threads.map((t) => t.thread.id).sort()).toEqual(["thread-1", "thread-2"]);
    expect(agentBGroup?.threads.map((t) => t.thread.id)).toEqual(["thread-3"]);
  });

  it("orders threads within a group by most-recent created_at first, same recency rule as resolveChannelThreadPartition", () => {
    const older = makeItem({ id: "thread-older", kind: "slack", createdAt: "2026-01-01T00:00:00Z" });
    const newer = makeItem({ id: "thread-newer", kind: "slack", createdAt: "2026-06-01T00:00:00Z" });

    const groups = groupHomeChannelThreads([older, newer], "channel");

    expect(groups[0].threads.map((t) => t.thread.id)).toEqual(["thread-newer", "thread-older"]);
  });

  it("orders groups themselves by their own freshest thread, freshest group first", () => {
    const staleSlack = makeItem({ id: "thread-slack-stale", kind: "slack", createdAt: "2026-01-01T00:00:00Z" });
    const freshDiscord = makeItem({ id: "thread-discord-fresh", kind: "discord", createdAt: "2026-06-01T00:00:00Z" });

    const groups = groupHomeChannelThreads([staleSlack, freshDiscord], "channel");

    expect(groups.map((g) => g.key)).toEqual(["discord", "slack"]);
  });

  it("rolls up each group's unread count from its member threads' `unread` flag", () => {
    const unreadSlack = makeItem({ id: "thread-unread", kind: "slack", unread: true });
    const readSlack = makeItem({ id: "thread-read", kind: "slack", unread: false });
    const unreadDiscord = makeItem({ id: "thread-discord-unread", kind: "discord", unread: true });

    const groups = groupHomeChannelThreads([unreadSlack, readSlack, unreadDiscord], "channel");

    expect(groups.find((g) => g.key === "slack")?.unreadCount).toBe(1);
    expect(groups.find((g) => g.key === "discord")?.unreadCount).toBe(1);
  });

  it("returns no groups for an empty input list, in either mode", () => {
    expect(groupHomeChannelThreads([], "channel")).toEqual([]);
    expect(groupHomeChannelThreads([], "agent")).toEqual([]);
  });
});
