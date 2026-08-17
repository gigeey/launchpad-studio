import type { Assignment, Thread } from "../types/api";
import { isThreadStreaming, threadActivityKey } from "../components/shared/ThreadActivityBadge";
import type { InFlightAgentMessage, RunningDelegateInfo } from "../stores/chatStore";

/** One assignment-originated thread, pre-computed for rendering: the group
 *  it belongs to, the human label, and whether it's unread. Mirrors
 *  `ChannelThreadInfo` (lib/channelThreads.ts) — the same shape, just keyed
 *  by `assignment_id` instead of a channel `kind`. */
export interface AssignmentThreadInfo {
  thread: Thread;
  assignmentId: string;
  label: string;
  unread: boolean;
}

/** All of one assignment's threads, most-recent-first, plus that group's own
 *  unread subtotal and its display label. Unlike `ChannelThreadGroup` (whose
 *  `kind` resolves to a label via the static `CHANNEL_KIND_LABELS` constant
 *  any caller can import), an assignment's name lives in already-loaded
 *  application data, not a constant — so `label` is resolved once here, at
 *  partition time, from the caller-supplied `assignmentLookup`. */
export interface AssignmentThreadGroup {
  assignmentId: string;
  label: string;
  threads: AssignmentThreadInfo[];
  unreadCount: number;
}

/** Return shape of `resolveAssignmentThreadPartition` — the one data layer
 *  both the Home "Assignments" section and the Chat pinned column derive
 *  their rendering from, so grouping/sorting/unread logic lives in exactly
 *  one place. Mirrors `ThreadChannelPartition`. */
export interface ThreadAssignmentPartition {
  /** Threads with no `assignment_origin` — the normal thread list, unchanged
   *  from today. */
  workingThreads: Thread[];
  /** Every assignment-originated thread, flattened in the same order as
   *  `assignmentGroups` (group order, then most-recent-first within a
   *  group). */
  assignmentThreads: AssignmentThreadInfo[];
  /** Assignment threads grouped by `assignment_origin.assignment_id`, groups
   *  ordered by their own most-recently-created thread — so whichever
   *  assignment has the freshest activity floats to the top, same
   *  "most recent first" feel as the per-group thread ordering. */
  assignmentGroups: AssignmentThreadGroup[];
  /** Sum of every group's `unreadCount` — feeds a collapsed tile's single
   *  aggregate badge. */
  totalUnreadCount: number;
}

/** Group display label for one assignment: its name if the caller supplied
 *  a lookup entry for it, else a truncated id — same truncation convention
 *  used elsewhere in the app for an unresolved id (see
 *  `AskUserQuestionForm`'s `id.slice(0, 8)`). */
function assignmentGroupLabel(
  assignmentId: string,
  assignmentLookup?: Map<string, Pick<Assignment, "name">>,
): string {
  const name = assignmentLookup?.get(assignmentId)?.name;
  if (name && name.trim().length > 0) return name.trim();
  return assignmentId.slice(0, 8);
}

/** Human label for an assignment thread: the operator/system title if one's
 *  set, else the owning assignment's group label — mirrors
 *  `channelThreadLabel` in lib/channelThreads.ts. */
function assignmentThreadLabel(thread: Thread, groupLabel: string): string {
  if (thread.title && thread.title.trim().length > 0) return thread.title.trim();
  if (thread.auto_title && thread.auto_title.trim().length > 0) return thread.auto_title.trim();
  return groupLabel;
}

/** Most-recent-created-first — same recency rule `channelThreads.ts` uses,
 *  duplicated here rather than shared since the two `*ThreadInfo` types
 *  differ. Exported so `homeAssignmentGrouping.ts`'s cross-agent merge sorts
 *  by the exact same rule instead of a second, possibly-drifting copy. */
export function byMostRecentlyCreated(a: AssignmentThreadInfo, b: AssignmentThreadInfo): number {
  return b.thread.created_at.localeCompare(a.thread.created_at);
}

/** Partitions one agent's already-loaded threads into working threads and
 *  assignment threads, then groups the latter by `assignment_origin.assignment_id`
 *  for the Home "Assignments" section and the Chat pinned column. Pure
 *  derivation over data the caller already has on hand — `threads` is
 *  whatever `list_for_agent` already populated, `unreadThreadIds` is the
 *  same store set `resolveThreadActivity`'s `"unread"` branch reads (via the
 *  shared `threadActivityKey` helper), and `assignmentLookup` is whichever
 *  already-loaded assignment definitions the calling surface has on hand
 *  (Home and Chat each load assignments independently, so this stays a
 *  parameter rather than a fetch owned by this selector). Mirrors
 *  `resolveChannelThreadPartition`.
 *
 *  An archived assignment thread is dropped entirely here, not just filtered
 *  downstream, same as an archived channel thread — see
 *  `resolveChannelThreadPartition`'s doc comment for the full rationale. */
export function resolveAssignmentThreadPartition(
  agentId: string,
  threads: Thread[],
  unreadThreadIds: Set<string>,
  assignmentLookup?: Map<string, Pick<Assignment, "name">>,
): ThreadAssignmentPartition {
  const workingThreads: Thread[] = [];
  const byAssignment = new Map<string, Thread[]>();

  for (const thread of threads) {
    const origin = thread.assignment_origin;
    if (!origin) {
      workingThreads.push(thread);
      continue;
    }
    if (thread.archived_at) continue;
    const bucket = byAssignment.get(origin.assignment_id);
    if (bucket) bucket.push(thread);
    else byAssignment.set(origin.assignment_id, [thread]);
  }

  const assignmentGroups: AssignmentThreadGroup[] = Array.from(byAssignment.entries())
    .map(([assignmentId, groupThreads]) => {
      const label = assignmentGroupLabel(assignmentId, assignmentLookup);
      const infos: AssignmentThreadInfo[] = groupThreads.map((thread) => ({
        thread,
        assignmentId,
        label: assignmentThreadLabel(thread, label),
        unread: unreadThreadIds.has(threadActivityKey(agentId, thread)),
      }));
      const sorted = infos.sort(byMostRecentlyCreated);
      return { assignmentId, label, threads: sorted, unreadCount: sorted.filter((t) => t.unread).length };
    })
    .sort((a, b) => byMostRecentlyCreated(a.threads[0], b.threads[0]));

  const assignmentThreads = assignmentGroups.flatMap((group) => group.threads);
  const totalUnreadCount = assignmentGroups.reduce((sum, group) => sum + group.unreadCount, 0);

  return { workingThreads, assignmentThreads, assignmentGroups, totalUnreadCount };
}

/** Per-thread "is this actively streaming right now" flag for every
 *  assignment-originated thread, keyed by thread id (plain object of
 *  primitives, not folded into `AssignmentThreadInfo` itself) — deliberately
 *  kept separate from `resolveAssignmentThreadPartition` so that function's
 *  `useMemo` (keyed on `threads`/`unreadThreadIds`, both low-frequency)
 *  doesn't recompute the whole grouped/sorted structure on every streaming
 *  token. Callers wrap this in `useShallow` (mirrors `ThreadTabStrip`'s own
 *  `activity` map for ordinary thread pills) so a component only re-renders
 *  when a thread's actual streaming flag flips, not on every raw
 *  `inFlightByAgent` mutation. Mirrors `isThreadStreaming`'s own
 *  "typing, buffered text, or an active tool call" definition, plus a
 *  running async Delegate — same union `resolveThreadActivity` uses for its
 *  `"streaming"` branch. */
export function resolveAssignmentStreamingByThreadId(
  agentId: string,
  assignmentThreads: AssignmentThreadInfo[],
  inFlightByAgent: Map<string, InFlightAgentMessage>,
  runningDelegatesByThread?: Map<string, Map<string, RunningDelegateInfo>>,
): Record<string, boolean> {
  const map: Record<string, boolean> = {};
  for (const info of assignmentThreads) {
    const key = threadActivityKey(agentId, info.thread);
    map[info.thread.id] =
      isThreadStreaming(inFlightByAgent.get(key)) || (runningDelegatesByThread?.get(key)?.size ?? 0) > 0;
  }
  return map;
}
