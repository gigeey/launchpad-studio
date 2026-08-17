import { byMostRecentlyCreated, type AssignmentThreadInfo } from "./assignmentThreads";

/** One assignment thread plus the agent it belongs to —
 *  `resolveAssignmentThreadPartition` (assignmentThreads.ts) only ever sees
 *  a single agent's threads, so HomeSidebar tags each of its per-agent
 *  results with the owning agent before merging across every agent it has
 *  thread data for. Mirrors `HomeChannelThreadInfo`. */
export interface HomeAssignmentThreadInfo extends AssignmentThreadInfo {
  agentId: string;
  agentName: string;
  agentEmoji: string | null;
}

export type HomeAssignmentsGroupBy = "assignment" | "agent";

/** One group in the Home "Assignments" section: either every conversation of
 *  a given assignment (groupBy "assignment") or of a given agent (groupBy
 *  "agent"). `key` is the assignment id or the agent id, matching whichever
 *  grouping produced this group. Mirrors `HomeChannelGroup` — the caller
 *  reads a member thread's own `label`/`agentName` for the group header
 *  rather than this layer resolving one, same deferred-rendering split
 *  `HomeChannelGroup` uses. */
export interface HomeAssignmentGroup {
  key: string;
  threads: HomeAssignmentThreadInfo[];
  unreadCount: number;
}

/** Groups already-partitioned, agent-tagged assignment threads either by
 *  assignment id or by owning agent, most-recent-first within a group and
 *  groups ordered by their own freshest thread — same recency convention
 *  `resolveAssignmentThreadPartition` uses for its per-agent
 *  `assignmentGroups`, just applied across however many agents HomeSidebar
 *  has merged in. Per-thread info (label/unread/assignmentId) is never
 *  recomputed here; it's whatever `resolveAssignmentThreadPartition` already
 *  produced. Mirrors `groupHomeChannelThreads`. */
export function groupHomeAssignmentThreads(
  items: HomeAssignmentThreadInfo[],
  groupBy: HomeAssignmentsGroupBy,
): HomeAssignmentGroup[] {
  const buckets = new Map<string, HomeAssignmentThreadInfo[]>();
  for (const item of items) {
    const key = groupBy === "assignment" ? item.assignmentId : item.agentId;
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
