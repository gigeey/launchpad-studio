import type { ChannelBinding, Thread } from "../types/api";
import { threadActivityKey } from "../components/shared/ThreadActivityBadge";
import { CHANNEL_KIND_LABELS } from "./threadNavigation";

/** Alias for `ChannelBridgeOrigin["kind"]` (== `ChannelBinding["kind"]`) —
 *  named for readability at every call site below instead of repeating the
 *  indexed-access type. */
export type ChannelOriginKind = ChannelBinding["kind"];

/** One channel-originated thread, pre-computed for rendering: the icon a
 *  render site maps `kind` to, the human label, and whether it's unread.
 *  `kind` doubles as both the group-by key (see `ChannelThreadGroup`) and
 *  the icon key — there's no separate "icon key" field since the two are
 *  always the same value. */
export interface ChannelThreadInfo {
  thread: Thread;
  kind: ChannelOriginKind;
  label: string;
  unread: boolean;
}

/** All of one channel kind's threads, most-recent-first, plus that group's
 *  own unread subtotal (for a sub-header badge next to the kind, distinct
 *  from the tile-level aggregate on `ThreadChannelPartition`). */
export interface ChannelThreadGroup {
  kind: ChannelOriginKind;
  threads: ChannelThreadInfo[];
  unreadCount: number;
}

/** Return shape of `resolveChannelThreadPartition` — the one data layer both
 *  the collapsed strip tile and the Home "Channels" section derive their
 *  rendering from, so grouping/sorting/unread logic lives in exactly one
 *  place. */
export interface ThreadChannelPartition {
  /** Threads with no `channel_origin` — the normal strip/Home thread list,
   *  unchanged from today. */
  workingThreads: Thread[];
  /** Every channel-originated thread, flattened in the same order as
   *  `channelGroups` (group order, then most-recent-first within a group). */
  channelThreads: ChannelThreadInfo[];
  /** Channel threads grouped by `channel_origin.kind`, groups ordered by
   *  their own most-recently-created thread — so whichever channel has the
   *  freshest activity floats to the top, same "most recent first" feel as
   *  the per-group thread ordering. */
  channelGroups: ChannelThreadGroup[];
  /** Sum of every group's `unreadCount` — feeds the collapsed tile's single
   *  aggregate badge. */
  totalUnreadCount: number;
}

/** Human label for a channel thread: the operator/system title if one's set,
 *  else the channel's own display name (`CHANNEL_KIND_LABELS`) — more useful
 *  on an unlabeled channel thread than the generic "New thread" placeholder
 *  ThreadTabStrip/HomeSidebar fall back to for ordinary threads, since it
 *  still names which channel the conversation came from. */
function channelThreadLabel(thread: Thread, kind: ChannelOriginKind): string {
  if (thread.title && thread.title.trim().length > 0) return thread.title.trim();
  if (thread.auto_title && thread.auto_title.trim().length > 0) return thread.auto_title.trim();
  return CHANNEL_KIND_LABELS[kind];
}

/** Most-recent-created-first — the same recency field ThreadTabStrip's
 *  `otherThreads` and HomeSidebar's `orderThreads` already sort non-default
 *  threads by (`created_at`, descending). Reused here rather than invented
 *  anew so a channel thread's position agrees with how every other thread
 *  surface already orders things. Exported so HomeSidebar's cross-agent
 *  merge (lib/homeChannelGrouping.ts) sorts by the exact same rule instead
 *  of a second, possibly-drifting copy. */
export function byMostRecentlyCreated(a: ChannelThreadInfo, b: ChannelThreadInfo): number {
  return b.thread.created_at.localeCompare(a.thread.created_at);
}

/** Partitions one agent's already-loaded threads into working threads and
 *  channel threads, then groups the latter by `channel_origin.kind` for the
 *  collapsed strip tile and the Home "Channels" section.
 *  Pure derivation over data the caller already has on hand — `threads` is
 *  whatever `list_for_agent` already populated (`threadsByAgent` in
 *  chatStore), and `unreadThreadIds` is the exact same store set
 *  `resolveThreadActivity`'s `"unread"` branch reads, via the shared
 *  `threadActivityKey` helper both use to compose the same composite key.
 *  No new fetch, no new unread source.
 *
 *  An archived channel thread is dropped entirely here, not just filtered
 *  downstream — unlike `workingThreads` (whose callers each filter
 *  `archived_at` out themselves, since `ThreadOverflowPanel`'s "Archived" tab
 *  still needs to list them), a channel thread never appears loose anywhere
 *  outside this tile/section, so there is no equivalent surface an
 *  archived one needs to keep showing up on. Archiving a channel thread (the
 *  close button on its row in `ChannelsTilePanel`/`HomeSidebar`) is
 *  effectively permanent from the UI's perspective today — there is no
 *  "Archived channels" recovery view yet, only ordinary threads have one.
 *  That is intentional for now (this is the noise-reduction half of the
 *  feature; a recovery/delete view is a follow-up), but means calling
 *  `archiveThread` on a channel thread should not be done lightly. */
export function resolveChannelThreadPartition(
  agentId: string,
  threads: Thread[],
  unreadThreadIds: Set<string>,
): ThreadChannelPartition {
  const workingThreads: Thread[] = [];
  const byKind = new Map<ChannelOriginKind, ChannelThreadInfo[]>();

  for (const thread of threads) {
    const origin = thread.channel_origin;
    if (!origin) {
      workingThreads.push(thread);
      continue;
    }
    if (thread.archived_at) continue;
    const info: ChannelThreadInfo = {
      thread,
      kind: origin.kind,
      label: channelThreadLabel(thread, origin.kind),
      unread: unreadThreadIds.has(threadActivityKey(agentId, thread)),
    };
    const bucket = byKind.get(origin.kind);
    if (bucket) bucket.push(info);
    else byKind.set(origin.kind, [info]);
  }

  const channelGroups: ChannelThreadGroup[] = Array.from(byKind.entries())
    .map(([kind, infos]) => {
      const sorted = [...infos].sort(byMostRecentlyCreated);
      return { kind, threads: sorted, unreadCount: sorted.filter((t) => t.unread).length };
    })
    .sort((a, b) => byMostRecentlyCreated(a.threads[0], b.threads[0]));

  const channelThreads = channelGroups.flatMap((group) => group.threads);
  const totalUnreadCount = channelGroups.reduce((sum, group) => sum + group.unreadCount, 0);

  return { workingThreads, channelThreads, channelGroups, totalUnreadCount };
}
