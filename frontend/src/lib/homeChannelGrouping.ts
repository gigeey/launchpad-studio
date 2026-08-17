import { byMostRecentlyCreated, type ChannelThreadInfo } from "./channelThreads";

/** One channel thread plus the agent it belongs to — `resolveChannelThreadPartition`
 *  (channelThreads.ts) only ever sees a single agent's threads, so HomeSidebar
 *  tags each of its per-agent results with the owning agent before merging
 *  across every agent it has thread data for. */
export interface HomeChannelThreadInfo extends ChannelThreadInfo {
  agentId: string;
  agentName: string;
  agentEmoji: string | null;
}

export type HomeChannelsGroupBy = "channel" | "agent";

/** One group in the Home "Channels" section: either every conversation of a
 *  given `channel_origin.kind` (groupBy "channel") or of a given agent
 *  (groupBy "agent"). `key` is the `ChannelOriginKind` or the agent id,
 *  matching whichever grouping produced this group — HomeSidebar resolves it
 *  back to an icon/label (CHANNEL_KIND_ICON/LABELS, or the agent snapshot)
 *  since that rendering choice doesn't belong in this pure data layer. */
export interface HomeChannelGroup {
  key: string;
  threads: HomeChannelThreadInfo[];
  unreadCount: number;
}

/** Groups already-partitioned, agent-tagged channel threads either by
 *  `channel_origin.kind` or by owning agent, most-recent-first within a
 *  group and groups ordered by their own freshest thread — same recency
 *  convention `resolveChannelThreadPartition` uses for its per-agent
 *  `channelGroups`, just applied across however many agents HomeSidebar has
 *  merged in. Per-thread info (label/unread/kind) is never recomputed here;
 *  it's whatever `resolveChannelThreadPartition` already produced. */
export function groupHomeChannelThreads(
  items: HomeChannelThreadInfo[],
  groupBy: HomeChannelsGroupBy,
): HomeChannelGroup[] {
  const buckets = new Map<string, HomeChannelThreadInfo[]>();
  for (const item of items) {
    const key = groupBy === "channel" ? item.kind : item.agentId;
    const bucket = buckets.get(key);
    if (bucket) bucket.push(item);
    else buckets.set(key, [item]);
  }

  return Array.from(buckets.entries())
    .map(([key, groupItems]) => {
      const sorted = [...groupItems].sort(byMostRecentlyCreated);
      return { key, threads: sorted, unreadCount: sorted.filter((t) => t.unread).length };
    })
    .sort((a, b) => byMostRecentlyCreated(a.threads[0], b.threads[0]));
}
