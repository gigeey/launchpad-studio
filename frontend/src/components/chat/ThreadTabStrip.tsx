import { useMemo, useRef, useState } from "react";
import {
  AtSign,
  ClipboardList,
  GitBranch,
  Hash,
  Mail,
  MessageCircle,
  MoreHorizontal,
  Plus,
  Send,
  Slack,
  Webhook,
  X,
} from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { useChatStore } from "../../stores/chatStore";
import {
  isSyncQuestion,
  resolveThreadActivity,
  ThreadQuestionBadge,
  ThreadStreamingBadge,
  ThreadUnreadDot,
  type ThreadActivity,
} from "../shared/ThreadActivityBadge";
import { resolveChannelThreadPartition, type ChannelOriginKind } from "../../lib/channelThreads";
import { resolveAssignmentStreamingByThreadId, resolveAssignmentThreadPartition } from "../../lib/assignmentThreads";
import { CHANNEL_KIND_LABELS } from "../../lib/threadNavigation";
import { RenameThreadModal } from "./RenameThreadModal";
import { ThreadOverflowPanel } from "./ThreadOverflowPanel";
import { ChannelsTilePanel } from "./ChannelsTilePanel";
import { AssignmentsTilePanel } from "./AssignmentsTilePanel";
import { Tooltip } from "../ui/Tooltip";
import type { Assignment, Thread } from "../../types/api";

/** Stable empty array so the `assignmentsByAgent` selector below never
 *  allocates a fresh `[]` on every render for an agent with no assignments
 *  loaded yet — same convention `ChatView`'s `EMPTY_THREADS` uses. */
const EMPTY_ASSIGNMENTS: Assignment[] = [];

/** The default thread is rendered as "Main thread" on the strip — the agent's
 *  one conversation that always exists. Anything else is a thread the
 *  operator created explicitly (via the `+` button, or by branching off a
 *  message). */
function isDefault(thread: Thread): boolean {
  return thread.kind === "default";
}

/** Human label for a tab. Default → "Main thread"; otherwise the
 *  operator-assigned title, falling back to the system-derived `auto_title`
 *  (set from the thread's first message once there's no explicit title yet),
 *  then a kind-derived placeholder. This is the FULL label — used for the
 *  native tooltip and the rename modal's placeholder. The pill itself shows
 *  `shortTabLabel`, since a tab strip has nowhere near the ~48 chars this can
 *  run to. Exported so `ThreadOverflowPanel` renders the exact same label a
 *  pill would, without duplicating the fallback chain. */
export function tabLabel(thread: Thread): string {
  if (isDefault(thread)) return "Main thread";
  if (thread.title && thread.title.trim().length > 0) return thread.title;
  if (thread.auto_title && thread.auto_title.trim().length > 0) return thread.auto_title;
  // Channel-originated threads (the rename modal's placeholder for a
  // Channels-tile row, via the shared `renameTarget` below) fall back to the
  // channel's own display name instead of the generic "New thread" — same
  // precedence `channelThreadLabel` (lib/channelThreads.ts) uses for the
  // row label itself.
  if (thread.channel_origin) return CHANNEL_KIND_LABELS[thread.channel_origin.kind];
  // Assignment-originated threads get the same treatment, but this function
  // only ever sees the bare `Thread` (no assignment-name lookup available
  // here) — the Assignments tile's own rows show the resolved assignment
  // name via `AssignmentThreadInfo.label` (lib/assignmentThreads.ts); this
  // is just the rename modal's generic placeholder for one.
  if (thread.assignment_origin) return "Assignment";
  return thread.kind === "branch" ? "Branch" : "New thread";
}

/** Longest a tab pill's own text runs before eliding — a tab strip has to
 *  fit several pills side by side, so it gets a much tighter budget than the
 *  Threads panel list or the tooltip (both show the untruncated label from
 *  `tabLabel`). */
const TAB_PILL_MAX_CHARS = 22;

function shortTabLabel(fullLabel: string): string {
  if (fullLabel.length <= TAB_PILL_MAX_CHARS) return fullLabel;
  return `${fullLabel.slice(0, TAB_PILL_MAX_CHARS).trimEnd()}…`;
}

/** Floor width for a non-pinned (non-Main) pill once the strip runs out of
 *  room and starts compressing — enough for the icon, a couple characters
 *  of (now CSS-ellipsised, not just JS-truncated) label, and the close
 *  button, even with every other pill squeezed just as small. Below this,
 *  a pill stops shrinking further; if there still isn't room for everyone
 *  at this floor, the `overflow-hidden` wrapper around the group (see the
 *  tab row below) clips whichever ones don't fit instead of letting them
 *  wrap to a second line or push the fixed Main/More pills off-screen —
 *  they're still one click away via the "More" overflow panel, which
 *  always renders the full, unclipped thread list regardless of what's
 *  visible here. */
const MIN_PILL_WIDTH_CLASS = "min-w-[92px]";

/** Branch threads get a distinct icon so a forked conversation reads
 *  differently at a glance; Main and every other (plain, operator-created)
 *  thread share the hash icon — the render sites below additionally box
 *  Main's copy in a small rounded square so it still stands out from the
 *  rest despite sharing the glyph. Exported for the same reason as
 *  `tabLabel` above. */
export function tabIcon(thread: Thread) {
  return thread.kind === "branch" ? GitBranch : Hash;
}

/** Per-`channel_origin.kind` icon for a channel conversation row/header
 *  inside the collapsed Channels tile (see `ChannelsTilePanel`) — mirrors
 *  `AgentProfileModal`'s `CHANNEL_SUB_TABS` icon choices (`telegram`/
 *  `discord`/`email`/`slack`) so a channel reads with the same glyph
 *  wherever its kind is named, then extends the same map to `whatsapp`/
 *  `webhook`, which that settings-only map never needed to cover. Exported
 *  so `ChannelsTilePanel` shares this exact mapping instead of inventing its
 *  own. */
export const CHANNEL_KIND_ICON: Record<ChannelOriginKind, React.ComponentType<{ className?: string }>> = {
  telegram: Send,
  discord: Hash,
  email: Mail,
  slack: Slack,
  whatsapp: MessageCircle,
  webhook: Webhook,
};

interface ThreadTabStripProps {
  /** Owning agent — used to look up each thread's live streaming/unread
   *  activity in chatStore (see `resolveThreadActivity` in
   *  `ThreadActivityBadge.tsx`). */
  agentId: string;
  /** The agent's threads, default thread included. Rendered in order, with
   *  the default thread pinned first regardless of list order so "Main
   *  thread" always anchors the strip. */
  threads: Thread[];
  /** The thread currently shown in the conversation column. */
  activeThreadId: string;
  onSelectThread: (threadId: string) => void;
  /** Create a new thread and switch to it — the `+` pill. */
  onCreateThread: () => void;
  /** Archive a non-default thread — the tab strip's own `X`. This does NOT
   *  delete anything: the thread and its history stay exactly as they were.
   *  Unlike the old client-only "closed" concept, this persists server-side
   *  (see `archived_at` on `Thread` in chatStore) and hides the thread from
   *  every surface (this strip, the overflow panel, `ThreadsPanel`'s main
   *  list, Home) until it's unarchived from `ThreadsPanel`'s "Archived"
   *  section. Never called for the default thread, whose pill has no close
   *  control. Also passed straight through to `ChannelsTilePanel` for its
   *  own rows' close button — a channel thread archives via this exact same
   *  action, it just has no recovery view yet (see
   *  `resolveChannelThreadPartition`'s doc comment in lib/channelThreads.ts). */
  onArchiveThread: (threadId: string) => void;
  /** Permanently delete a non-default thread. Reached only through the
   *  overflow panel's bulk-delete (below) — there's no per-pill delete
   *  control, since a stray click on a tab shouldn't be able to destroy
   *  history. Awaited in sequence for a batch, so it's typed
   *  `void | Promise<void>` to admit either a fire-and-forget or awaited
   *  caller honestly. */
  onDeleteThread: (threadId: string) => void | Promise<void>;
  /** Rename a non-default thread — `title` is `null` to clear back to the
   *  kind-derived placeholder. Wired to the right-click context menu on a
   *  pill (see `RenameThreadModal`); never called for the default thread,
   *  which keeps its fixed "Main thread" label. */
  onRenameThread: (threadId: string, title: string | null) => Promise<unknown>;
  /** Restore an archived thread — reached only from the overflow panel's
   *  "Archived" tab (see `ThreadOverflowPanel`'s `onUnarchiveThread`). The
   *  implementation (ChatView's `handleUnarchiveThread`) also switches to the
   *  thread once restored, so it both reopens and focuses it — unlike
   *  `ThreadsPanel`'s own unarchive, which only brings it back into every
   *  surface's active list without switching. */
  onUnarchiveThread: (threadId: string) => void | Promise<void>;
}

/** Vertical fade for the frosted backdrop. The glass is solid only through the
 *  tab row (top ~30px) so it reads as a seamless continuation of the navbar
 *  above — no visible seam or gap where the strip begins. Below the tabs it
 *  eases off through progressively lower alpha stops (1 → 0.5 → 0.2 → 0) so the
 *  frost gets softer the further it sits from the navbar, feathering out at the
 *  bottom instead of ending in a hard rectangular block. The eased, non-linear
 *  stops concentrate the softness near the bottom edge — the gentlest part of
 *  the transition, where the glass melts into the scrolling conversation. */
const FROST_FADE =
  "linear-gradient(to bottom, #000 0, #000 30px, rgba(0,0,0,0.5) 50px, rgba(0,0,0,0.2) 66px, transparent 80px)";

/** Approximate line height (px) used to scale a DOM_DELTA_LINE wheel event —
 *  see `forwardWheelToMessages` below. */
const WHEEL_LINE_HEIGHT_PX = 16;

/** The strip floats as a `pointer-events-none` overlay over, not inside, the
 *  message scroll container (`[data-scroll-container]` in MessageList) — each
 *  pill/button opts back in with `pointer-events-auto` so it stays clickable,
 *  but that also means it has no scrollable ancestor of its own. A wheel/
 *  trackpad gesture with the cursor parked over a pill has nowhere to go, so
 *  the browser drops it: no native `scroll` event ever reaches the message
 *  list, so its "load older messages" check never runs. This forwards the
 *  gesture's vertical delta straight to the scroll container via `scrollBy`,
 *  normalizing `deltaMode` first since line/page-mode deltas (line-stepping
 *  mice vs. pixel-precise trackpads) are on a completely different scale than
 *  raw pixels. */
function forwardWheelToMessages(e: React.WheelEvent) {
  // React bubbles synthetic events along the JSX tree, not the DOM tree — so
  // a wheel gesture inside the "More" pill's popover (portaled to
  // `document.body` by ThreadOverflowPanel, but still nested under this pill
  // in JSX) reaches this handler too, even though it's nowhere near the pill
  // in the actual DOM. Bail unless the event's real DOM target lives inside
  // this handler's own element, so scrolling the popover's own list doesn't
  // also scroll the messages behind it.
  if (!e.currentTarget.contains(e.target as Node)) return;
  const scrollContainer = document.querySelector<HTMLElement>("[data-scroll-container]");
  if (!scrollContainer) return;
  let deltaY = e.deltaY;
  if (e.deltaMode === 1) {
    deltaY *= WHEEL_LINE_HEIGHT_PX;
  } else if (e.deltaMode === 2) {
    deltaY *= scrollContainer.clientHeight;
  }
  scrollContainer.scrollBy({ top: deltaY });
}

export function ThreadTabStrip({
  agentId,
  threads,
  activeThreadId,
  onSelectThread,
  onCreateThread,
  onArchiveThread,
  onDeleteThread,
  onRenameThread,
  onUnarchiveThread,
}: ThreadTabStripProps) {
  // The thread currently targeted by the rename modal — opened via
  // right-click on a (non-default) pill OR a row inside the collapsed
  // Channels tile (via `onOpenRename`, passed straight to `ChannelsTilePanel`
  // below), closed on submit/cancel/Escape. One shared instance covers both,
  // same as HomeSidebar's own `renameTarget`. Holding the whole `Thread`, not
  // just an id, means the modal always has the title/placeholder to pre-fill
  // even if `threads` changes underneath it while it's open.
  const [renameTarget, setRenameTarget] = useState<Thread | null>(null);

  // The "More" pill's own open/closed state, plus a ref to its wrapper (used
  // both to position the floating panel under it and, in the panel, to tell
  // "clicked the toggle button again" apart from a genuine outside click).
  const [overflowOpen, setOverflowOpen] = useState(false);
  const moreButtonRef = useRef<HTMLDivElement>(null);

  // The collapsed "Channels" tile's own open/closed state — same shape as
  // the "More" pill's above, one popover toggle plus an anchor ref for
  // ChannelsTilePanel to position itself under.
  const [channelsOpen, setChannelsOpen] = useState(false);
  const channelsButtonRef = useRef<HTMLDivElement>(null);

  // The collapsed "Assignments" tile's own open/closed state — mirrors
  // `channelsOpen`/`channelsButtonRef` above exactly, one popover toggle
  // plus an anchor ref for AssignmentsTilePanel to position itself under.
  const [assignmentsOpen, setAssignmentsOpen] = useState(false);
  const assignmentsButtonRef = useRef<HTMLDivElement>(null);

  // Same live `unreadThreadIds` source `activity`/`syncQuestionByThread`
  // below already read off the store — subscribed directly (not inside a
  // `useShallow` compute function) since `resolveChannelThreadPartition`
  // needs the raw `Set` itself, not a derived per-thread record. The store
  // only ever replaces this `Set` wholesale (never mutates it in place — see
  // chatStore.ts), so a plain reference-equality subscription re-renders
  // exactly when it actually changes.
  const unreadThreadIds = useChatStore((s) => s.unreadThreadIds);

  // Whether the persistent Channels column (`ChannelsColumn`, rendered by
  // ChatView alongside the chat area) is pinned open for this agent — set via
  // the "Pin" control inside the popover below, cleared from the column's own
  // header. See `channelsColumnPinnedByAgent`'s doc comment in chatStore.
  const channelsColumnPinned = useChatStore((s) => s.channelsColumnPinnedByAgent.get(agentId) ?? false);
  const setChannelsColumnPinned = useChatStore((s) => s.setChannelsColumnPinned);

  // Same pin-state pattern as the Channels column above, for the
  // Assignments column (`AssignmentsColumn`, rendered alongside it in
  // ChatView).
  const assignmentsColumnPinned = useChatStore((s) => s.assignmentsColumnPinnedByAgent.get(agentId) ?? false);
  const setAssignmentsColumnPinned = useChatStore((s) => s.setAssignmentsColumnPinned);

  // Partitions `threads` into working (normal strip pills) vs. channel
  // (collapsed Channels tile) threads, and groups the latter by
  // `channel_origin.kind` — see channelThreads.ts. Shared with Home's
  // "Channels" section so both surfaces agree on grouping/ordering/unread.
  const channelPartition = useMemo(
    () => resolveChannelThreadPartition(agentId, threads, unreadThreadIds),
    [agentId, threads, unreadThreadIds],
  );

  // `id -> name` lookup `resolveAssignmentThreadPartition` resolves each
  // assignment thread's group label from — same construction Home's own
  // sidebar uses (`assignmentLookup` in HomeSidebar.tsx), just scoped to
  // this one agent's already-loaded assignments (populated by ChatView's
  // `loadAssignments` call, the same fetch ChatHeader's assignment-count
  // badge reads).
  const assignmentsForAgent = useChatStore((s) => s.assignmentsByAgent.get(agentId) ?? EMPTY_ASSIGNMENTS);
  const assignmentLookup = useMemo(
    () => new Map(assignmentsForAgent.map((a) => [a.id, { name: a.name }])),
    [assignmentsForAgent],
  );

  // Same partitioning as `channelPartition` above, sourced from
  // assignmentThreads.ts instead — groups by `assignment_origin.assignment_id`
  // for the collapsed Assignments tile below.
  const assignmentPartition = useMemo(
    () => resolveAssignmentThreadPartition(agentId, threads, unreadThreadIds, assignmentLookup),
    [agentId, threads, unreadThreadIds, assignmentLookup],
  );

  // Per-assignment-thread "streaming right now" flag, kept separate from
  // `assignmentPartition` above (see `resolveAssignmentStreamingByThreadId`'s
  // doc comment) so a background run's token deltas don't force the whole
  // grouped/sorted assignment structure to recompute on every tick — only
  // this primitives-only map does, and `useShallow` skips the re-render
  // entirely unless a thread's actual flag flips.
  const assignmentStreamingByThreadId = useChatStore(
    useShallow((s) =>
      resolveAssignmentStreamingByThreadId(agentId, assignmentPartition.assignmentThreads, s.inFlightByAgent, s.runningDelegatesByThread),
    ),
  );

  // True when the thread currently shown in the conversation column is one
  // of the channel threads folded into the collapsed tile below — used to
  // give the tile itself a subtle "your open conversation lives in here"
  // hint without reproducing Main's full active-tab chrome (the tile isn't
  // literally the page being viewed; one of its child conversations is).
  const activeIsChannelThread = channelPartition.channelThreads.some(
    (info) => info.thread.id === activeThreadId,
  );

  // Same hint, mirrored for the Assignments tile.
  const activeIsAssignmentThread = assignmentPartition.assignmentThreads.some(
    (info) => info.thread.id === activeThreadId,
  );

  // Default thread always leads the strip, independent of fetch/creation
  // order. Sourced from `channelPartition.workingThreads` (channel threads
  // already stripped out) rather than the raw `threads` prop — a
  // channel-originated thread must never appear loose in the strip (or its
  // overflow panel) anymore, only behind the collapsed tile below.
  // Assignment-originated threads are excluded the same way (filtered
  // inline rather than via a second partition's `workingThreads`, since only
  // the exclusion is needed here) — they only ever appear behind the
  // collapsed Assignments tile, same reasoning HomeSidebar's own agent list
  // uses for the identical filter.
  const orderedThreads = [...channelPartition.workingThreads]
    .filter((t) => !t.assignment_origin)
    .sort((a, b) => Number(isDefault(b)) - Number(isDefault(a)));
  const defaultThread = orderedThreads.find(isDefault);
  // Newest-created first so a freshly-opened thread lands right next to the
  // `+` button (closest to Main) instead of at the tail of a growing row —
  // ties (e.g. fixtures sharing a timestamp) keep their original relative
  // order since `sort` is stable. Archived threads are excluded entirely —
  // archiving (the pill's `X`) hides a thread from every surface, this strip
  // included, until it's unarchived from `ThreadsPanel`'s "Archived" section.
  // The overflow panel reuses this exact same list — it's "every visible
  // thread besides Main", the same set the strip already renders pills for,
  // just reachable without hunting through a long row.
  const otherThreads = orderedThreads
    .filter((t) => !isDefault(t) && !t.archived_at)
    .sort((a, b) => b.created_at.localeCompare(a.created_at));

  // Archived threads, most-recently-archived-first (via `updated_at`, bumped
  // whenever `archived_at` is set) — same sort `ThreadsPanel`'s own
  // "Archived" section uses. Fed to the overflow panel's "Archived" tab so
  // there's a second, more discoverable path to recover or permanently
  // delete a closed thread besides the sidebar.
  const archivedThreads = orderedThreads
    .filter((t) => !isDefault(t) && t.archived_at)
    .sort((a, b) => b.updated_at.localeCompare(a.updated_at));

  // Deliberately NOT reordered by active state: an earlier version sorted
  // the active thread to the front of this group (right after Main) so
  // compression could never clip the one tab you're looking at — but that
  // made clicking any tab visibly yank it out of its slot to the front of
  // the row, which reads as far more broken than the clipping it was meant
  // to prevent. Tab order should only ever change when a thread is created
  // (newest-first) — selecting one never moves it. If it's compressed out
  // of view, the "More" panel (see below) always has the full list.
  const orderedVisibleOtherThreads = otherThreads;

  // Bulk-delete for the overflow panel — sequenced (not `Promise.all`) so two
  // deletes that both happen to target the currently-active thread can't race
  // on `switchToThread`'s fallback in ChatView's `onDeleteThread`. Wrapped in
  // `Promise.resolve` because the prop is typed `void | Promise<void>` (a
  // single delete never needs to be awaited by itself) — this is the one
  // caller that needs to.
  async function handleDeleteThreads(threadIds: string[]) {
    for (const id of threadIds) {
      await Promise.resolve(onDeleteThread(id));
    }
  }

  // Per-thread streaming/unread/question snapshot, recomputed only when one
  // of THESE threads' actual flags changes — `useShallow` shallow-compares
  // the returned record so an unrelated agent's token deltas elsewhere in
  // the app don't re-render this strip. Values must stay primitives:
  // `useShallow` only compares one level deep, so a record of freshly-
  // allocated nested objects would never compare equal across renders and
  // would spin forever (each render sees "new" values, forces another
  // re-render...). See `resolveThreadActivity`'s doc comment for the
  // question > streaming > unread priority order.
  const activity = useChatStore(
    useShallow((s) => {
      const pendingForms = s.agents.find((a) => a.agent_id === agentId)?.pending_forms;
      const map: Record<string, ThreadActivity> = {};
      for (const thread of channelPartition.workingThreads) {
        map[thread.id] = resolveThreadActivity(
          agentId,
          thread,
          s.inFlightByAgent,
          s.unreadThreadIds,
          s.runningDelegatesByThread,
          s.pendingFormByAgent,
          pendingForms,
        );
      }
      return map;
    }),
  );

  // Per-thread sync-vs-async flag for the "question" activity above — kept
  // as its own flat (primitive-valued) map, same `useShallow` reasoning as
  // `activity`, rather than nested inside it: a shallow compare only looks
  // one level deep, so folding this into `activity`'s record would compare
  // freshly-allocated nested objects by reference and never memoize. Only
  // consulted when the corresponding thread's `activity` is `"question"`.
  const syncQuestionByThread = useChatStore(
    useShallow((s) => {
      const map: Record<string, boolean> = {};
      for (const thread of channelPartition.workingThreads) {
        map[thread.id] = isSyncQuestion(agentId, thread, s.pendingFormByAgent);
      }
      return map;
    }),
  );

  /** Renders one pill. Split into a non-button `relative` wrapper (carrying
   *  the pill's chrome — background/border/rounded corners) plus two
   *  sibling buttons inside it — select and, for non-default threads,
   *  close — because a close `<button>` can't nest inside a select
   *  `<button>` (invalid HTML), and as siblings a click on one never
   *  bubbles into the other. The select button keeps `role="tab"` /
   *  `aria-selected` / the `thread-tab-*` testid, so existing selection
   *  semantics are unaffected by the split. */
  function renderTab(thread: Thread) {
    const active = thread.id === activeThreadId;
    const Icon = tabIcon(thread);
    const fullLabel = tabLabel(thread);
    const label = shortTabLabel(fullLabel);
    const closable = !isDefault(thread);
    // Right-click-to-rename has zero visual affordance on its own — nothing
    // about a pill hints that it's interactive beyond selecting/closing it.
    // So once a thread is still on its kind-derived placeholder (no operator
    // title, no system auto_title yet — the exact condition `tabLabel` falls
    // through to below), the hover tooltip grows a second line nudging the
    // operator toward the one discoverable path to name it. A thread that
    // already has a title or auto_title skips the hint — renaming is still
    // possible, but the tooltip's job there is just to reveal the un-elided
    // label, and stacking an unconditional hint under every real name would
    // be constant noise rather than a one-time nudge. Never shown for Main
    // (`!closable`), which can't be renamed at all.
    const nameNotSet =
      closable &&
      !(thread.title && thread.title.trim().length > 0) &&
      !(thread.auto_title && thread.auto_title.trim().length > 0);
    const tooltipLabel = nameNotSet ? `${fullLabel}\nRight-click to rename` : fullLabel;
    // Main's icon renders boxed (see the `isMainThread` branches below) since
    // it now shares the plain hash glyph with ordinary threads.
    const isMainThread = isDefault(thread);
    // Main's pill carries the `+` (new thread) control instead of a close
    // button — Main can't be closed, but it's always present, so it's the
    // one fixed anchor where "add a thread" reads as reachable no matter
    // how many pills pile up.
    const showCreateButton = isDefault(thread);
    // Main never compresses — it's one of the row's two fixed anchors
    // (alongside the "More" pill). Every other pill is allowed to shrink
    // down to MIN_PILL_WIDTH_CLASS as the strip runs out of room.
    const pinned = showCreateButton;
    const widthClass = pinned ? "shrink-0" : `shrink ${MIN_PILL_WIDTH_CLASS}`;
    // Every pill has exactly one trailing control (close for closable
    // pills, `+` for Main), so both reserve the same reduced right padding
    // to leave room for it.
    const selectPadding = closable || showCreateButton ? "pl-[14px] pr-[6px]" : "px-[14px]";
    // Only surfaced on background (inactive) pills — the active tab's own
    // content is already visible below, so a redundant badge there would
    // just be noise.
    const activityFlag = activity[thread.id] ?? "none";
    // Right-click opens the rename modal — only for threads that can
    // actually be renamed (non-default; Main keeps its fixed label). Bound
    // on the pill wrapper rather than just the select button so right-
    // clicking the close button's area also works, matching how the whole
    // pill already acts as one hit target for hover states.
    const handleContextMenu = closable
      ? (e: React.MouseEvent) => {
        e.preventDefault();
        setRenameTarget(thread);
      }
      : undefined;

    if (active) {
      return (
        <div
          key={thread.id}
          onContextMenu={handleContextMenu}
          onWheel={forwardWheelToMessages}
          // The Chrome-style feet are the two SVGs below, one tucked just
          // outside each top corner. A `border-radius` on a filled box can
          // only make a *convex* corner (the tab bulging outward), which is
          // the wrong direction — a Chrome foot is *concave*: the tab is
          // widest where it meets the bar and its side sweeps inward with a
          // scooped curve. So each foot is instead an SVG quadratic curve we
          // can shape exactly: filled with the tab colour and stroked with the
          // border, positioned so the outline flows straight down the side,
          // around the concave flare, and up into the navbar with no break.
          className={`pointer-events-auto relative flex items-stretch
                   bg-[var(--bg-secondary)]
                   border-l border-r border-b border-[var(--border-secondary)]
                   rounded-bl-[14px] rounded-br-[14px]
                   -mt-[1px] z-10
                   transition-colors
                   ${widthClass}`}
        >
          {/* Left foot. A concave Bezier "flare" that lets the tab's
            straight side border curve outward into open space at the top
            instead of butting into a hard corner — the Chrome-tab look.
            viewBox y grows downward; `top:-2px` places local y=2 exactly at
            the tab's own top edge (tab-relative y=0). The curve runs from
            the body side (8,10 — vertical tangent, continuing the left
            border) up to (0,2) — horizontal tangent, so it terminates flush
            and level rather than cutting off at an angle.

            The curve's own tangent point (x=8, tab-relative real x=0 via
            `left:-8px` below) only TOUCHES the CSS border's outer edge —
            it doesn't overlap the border's own 1px-wide paint band
            (tab-relative x∈[0,1)) at all. Two independently-rendered lines
            (an SVG stroke here, a CSS border on the tab's own box) that
            merely touch rather than overlap don't reliably fuse into one
            continuous edge: sub-pixel/antialiasing rounding differences
            between the two rendering paths can leave the straight CSS
            border partly un-masked right beside the curve, reading as a
            second, disjointed line floating apart from the flare instead
            of sitting flush on it (this bit us twice now — first as a
            stray fill notch above the tab's top edge, now as this). So the
            fill's own footprint (the `L12,2 L12,10` below) reaches 4px past
            the border — comfortably past its 1px band regardless of
            rounding — instead of stopping exactly at it; only the
            invisible mask grows, the visible stroke path is untouched.
            `left:-8px` (not -7px) keeps the curve's tangent flush with the
            border's outer edge instead of lapping 1px inside it — the old
            1px lap was itself relying on the same "just touching" masking
            this comment is moving away from. If you retune the flare keep
            these in sync: the tab-top alignment (top:-2px ↔ the `2` in the
            paths), the flare depth (the `10`), and the 8px horizontal
            offset (also referenced by the gap/safe-paint-area comments
            below, in the outer tab row). */}
          <svg
            aria-hidden
            width="12"
            height="10"
            viewBox="0 0 12 10"
            className="pointer-events-none absolute"
            style={{ top: "-2px", left: "-8px" }}
          >
            <path d="M8,10 Q8,2 0,2 L12,2 L12,10 Z" style={{ fill: "var(--bg-secondary)" }} />
            <path
              d="M8,10 Q8,2 0,2"
              fill="none"
              vectorEffect="non-scaling-stroke"
              style={{ stroke: "var(--border-secondary)", strokeWidth: 1 }}
            />
          </svg>
          {/* Right foot — mirror of the left. The curve keeps the exact
              same (0,10)/(8,2) coordinates as before; only the canvas grows
              (12 wide, viewBox shifted to `-4 0 12 10` so the extra 4px of
              mask room lands to the LEFT of the curve — the "further into
              the tab" direction on this side, mirroring the left foot's
              rightward growth) and `right:-8px` (was -7px) keeps the
              tangent flush with the border, same reasoning as the left
              foot above. */}
          <svg
            aria-hidden
            width="12"
            height="10"
            viewBox="-4 0 12 10"
            className="pointer-events-none absolute"
            style={{ top: "-2px", right: "-8px" }}
          >
            <path d="M0,10 Q0,2 8,2 L-4,2 L-4,10 Z" style={{ fill: "var(--bg-secondary)" }} />
            <path
              d="M0,10 Q0,2 8,2"
              fill="none"
              vectorEffect="non-scaling-stroke"
              style={{ stroke: "var(--border-secondary)", strokeWidth: 1 }}
            />
          </svg>
          <button
            type="button"
            role="tab"
            aria-selected={true}
            data-testid={`thread-tab-${thread.id}`}
            onClick={() => onSelectThread(thread.id)}
            className={`flex items-center gap-[6px] min-w-0 ${selectPadding} text-[13px] font-medium
                     text-[var(--text-primary)]
                     bg-transparent cursor-pointer transition-colors`}
          >
            {isMainThread ? (
              <span className="flex items-center justify-center w-[13px] h-[13px] rounded-[4px] border border-[var(--text-primary)] shrink-0">
                <Icon className="w-[9px] h-[9px]" />
              </span>
            ) : (
              <Icon className="w-[13px] h-[13px] shrink-0" />
            )}
            {/* Native `title` would double up with this, so it's dropped in
                favor of the shared tooltip — same fancy-pill look as the rest
                of the app, and (via `placement="top"`'s portal) not at risk
                of being clipped by the compressible group's `overflow-hidden`
                the way an absolutely-positioned tooltip anchored here would
                be. Only compressed (elided) labels truly need this, but
                showing it unconditionally costs nothing and stays correct as
                `TAB_PILL_MAX_CHARS`/pill widths change. */}
            <Tooltip label={tooltipLabel} placement="top" className="min-w-0 flex-1">
              <span className="block truncate">{label}</span>
            </Tooltip>
          </button>
          {closable && (
            <button
              type="button"
              onClick={(e) => {
                e.stopPropagation();
                onArchiveThread(thread.id);
              }}
              data-testid={`thread-archive-${thread.id}`}
              aria-label="Archive thread"
              className="group flex items-center self-stretch shrink-0 pl-[2px] pr-[10px]
                       bg-transparent cursor-pointer"
            >
              <span
                className="flex items-center justify-center w-[18px] h-[18px] rounded-full
                         text-[var(--text-tertiary)] group-hover:text-[var(--text-primary)]
                         group-hover:bg-[var(--border-secondary)]
                         transition-colors"
              >
                <X className="w-[12px] h-[12px]" />
              </span>
            </button>
          )}
          {showCreateButton && (
            <button
              type="button"
              onClick={(e) => {
                e.stopPropagation();
                onCreateThread();
              }}
              data-testid="thread-tab-new"
              title="New thread"
              aria-label="New thread"
              className="group flex items-center self-stretch shrink-0 pl-[2px] pr-[10px]
                       bg-transparent cursor-pointer"
            >
              {/* Accent-filled, not just on hover — Main's `+` is the one
                  fixed spot to add a thread, so it needs to read as an
                  affordance at a glance rather than blend into the pill
                  until hovered. */}
              <span
                className="flex items-center justify-center w-[18px] h-[18px] rounded-full
                         bg-[var(--accent)] text-white
                         group-hover:bg-[var(--accent-hover)]
                         transition-colors"
              >
                <Plus className="w-[12px] h-[12px]" />
              </span>
            </button>
          )}
        </div>
      );
    }

    return (
      <div
        key={thread.id}
        onContextMenu={handleContextMenu}
        onWheel={forwardWheelToMessages}
        // Same padding/gap/font-size/weight/icon-size as the active tab
        // above (and a matching transparent 1px l/r border to reserve the
        // same width the active tab's visible border occupies) — only
        // colour, border visibility, corner radius, and shadow differ
        // between the two states. If any of those metrics drift out of
        // sync, switching tabs reflows neighbouring pills (they visibly
        // resize/jump) instead of just recolouring the active one.
        className={`pointer-events-auto relative self-center flex items-stretch
                 bg-[var(--bg-hover)] hover:bg-[var(--border-secondary)]
                 border-l border-r border-transparent
                 rounded-full shadow-sm
                 transition-colors
                 ${widthClass}`}
      >
        <button
          type="button"
          role="tab"
          aria-selected={false}
          data-testid={`thread-tab-${thread.id}`}
          onClick={() => onSelectThread(thread.id)}
          className={`group flex items-center gap-[6px] min-w-0 ${selectPadding} py-[5px] text-[13px] font-medium
                   text-[var(--text-secondary)] hover:text-[var(--text-primary)]
                   bg-transparent cursor-pointer transition-colors`}
        >
          {/* Fixed-size slot matching the icon's own footprint (13x13) so
              swapping between icon/badge/dot never changes the pill's
              width — the badge/dot replaces the icon in place rather than
              sitting alongside it as an extra element. */}
          <span className="flex items-center justify-center w-[13px] h-[13px] shrink-0">
            {activityFlag === "question" ? (
              <ThreadQuestionBadge id={thread.id} sync={syncQuestionByThread[thread.id] ?? false} />
            ) : activityFlag === "streaming" ? (
              <ThreadStreamingBadge id={thread.id} />
            ) : activityFlag === "unread" ? (
              <ThreadUnreadDot id={thread.id} />
            ) : isMainThread ? (
              <span className="flex items-center justify-center w-[13px] h-[13px] rounded-[4px] border border-[var(--text-secondary)] group-hover:border-[var(--text-primary)] transition-colors">
                <Icon className="w-[9px] h-[9px]" />
              </span>
            ) : (
              <Icon className="w-[13px] h-[13px]" />
            )}
          </span>
          {/* See the active-tab render above for why this replaces a native
              `title` instead of adding to it. */}
          <Tooltip label={tooltipLabel} placement="top" className="min-w-0 flex-1">
            <span className="block truncate">{label}</span>
          </Tooltip>
        </button>
        {closable && (
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              onArchiveThread(thread.id);
            }}
            data-testid={`thread-archive-${thread.id}`}
            aria-label="Archive thread"
            className="group flex items-center self-stretch shrink-0 py-[5px] pl-[2px] pr-[10px]
                     bg-transparent cursor-pointer"
          >
            <span
              className="flex items-center justify-center w-[18px] h-[18px] rounded-full
                       text-[var(--text-tertiary)] group-hover:text-[var(--text-primary)]
                       group-hover:bg-[var(--border-secondary)]
                       transition-colors"
            >
              <X className="w-[12px] h-[12px]" />
            </span>
          </button>
        )}
        {showCreateButton && (
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              onCreateThread();
            }}
            data-testid="thread-tab-new"
            title="New thread"
            aria-label="New thread"
            className="group flex items-center self-stretch shrink-0 py-[5px] pl-[2px] pr-[10px]
                     bg-transparent cursor-pointer"
          >
            <span
              className="flex items-center justify-center w-[18px] h-[18px] rounded-full
                       bg-[var(--accent)] text-white
                       group-hover:bg-[var(--accent-hover)]
                       transition-colors"
            >
              <Plus className="w-[12px] h-[12px]" />
            </span>
          </button>
        )}
      </div>
    );
  }

  return (
    <div
      role="tablist"
      aria-label="Threads"
      // Floating frosted strip. The bar is click-through (`pointer-events-none`)
      // so wheel/scroll passes to the message list below; only the individual
      // tabs opt back in to pointer events. The frosted glass is a separate
      // layer behind the tabs (see below) so the tabs themselves stay solid and
      // sharp while only the glass fades.
      className="relative pointer-events-none"
    >
      {/* Frosted backdrop. Tinted with the CONTENT background (--bg-secondary),
          the same colour the conversation sits on, so where no bubble is behind
          it the tint is indistinguishable from the page and the strip reads as
          nothing; where a bubble scrolls underneath, backdrop-blur frosts it.
          The tint is fully opaque and the mask holds it solid through the tab
          row, so the top of the strip reads as a seamless extension of the
          navbar (no gap/seam). The layer runs well past the 44px tab row and its
          FROST_FADE mask eases the tint + blur down to nothing, so the glass
          gets progressively softer toward the bottom and feathers continuously
          into the conversation instead of ending in a hard rectangular block. */}
      <div
        aria-hidden
        className="absolute inset-x-0 top-0 h-[80px] backdrop-blur-md bg-[var(--bg-secondary)]"
        style={{ maskImage: FROST_FADE, WebkitMaskImage: FROST_FADE }}
      />

      {/* Tab row — solid pills painted above the frosted backdrop. The active
          tab keeps the inverted-chrome look, hanging from the header's bottom
          border; `items-stretch` lets it fill the row height so its rounded
          bottom edge reads as a hanging chrome tab. The active tab also grows
          two small SVG "feet" at its top corners (see below) that flare outward
          into the navbar — the concave-curved join that makes a Chrome tab read
          as part of the bar instead of a rectangle butted against it.
          `gap-[10px]` (not a tighter value) is load-bearing: the feet reach 8px
          past the active tab's own box on each side (see the SVG comments
          below), and this row's gap is shared uniformly by every pill — active
          or not — rather than added as extra margin on just the active one, so
          switching which tab is active never reflows its neighbours. Below
          ~8px, whichever pill sits next to the active tab is close enough that
          a foot's white (`--bg-secondary`) fill paints over the tail of that
          neighbour's own `--bg-hover` pill instead of the matching-white
          frosted backdrop behind the gap — a visible mismatched patch, not a
          smooth line. 10px keeps a couple of pixels of margin past the 8px
          reach for anti-aliasing. */}
      <div className="relative z-10 flex items-stretch h-[44px] gap-[10px] px-[10px]">
        {/* Main's pill carries its own trailing `+` control now (see
            `showCreateButton` in renderTab) so new threads always open
            right next to Main instead of behind a growing row of pills —
            no separate button needed here. */}
        {defaultThread && renderTab(defaultThread)}

        {/* Collapsed "Channels" tile — a second fixed/pinned tile right
            after Main: a channel-originated thread must never appear loose
            in the strip anymore. Hidden entirely when the agent has no channel
            threads at all, same as how the "More" pill would be pointless
            with nothing to browse — most agents never bind a channel, so an
            always-visible-but-disabled tile would just be permanent clutter
            for them. `shrink-0` keeps it pinned like Main, never absorbing
            the compressible group's shrink pressure below. */}
        {channelPartition.channelGroups.length > 0 && (
          <div
            ref={channelsButtonRef}
            // Stretches to the full row height (matching a real active tab's
            // `items-stretch` chrome below) only while a channel thread is
            // actually open; otherwise it stays a vertically-centered pill
            // like the "More" tile beside it.
            className={`relative shrink-0 pointer-events-auto ${activeIsChannelThread ? "" : "self-center"}`}
            onWheel={forwardWheelToMessages}
          >
            {activeIsChannelThread ? (
              // Same Chrome-tab chrome as an active thread pill in
              // `renderTab` above (hanging from the header, flared feet) —
              // the collapsed tile IS where the open conversation lives when
              // one of its channel threads is active, so it earns the exact
              // same "this is the open tab" treatment instead of just a
              // highlighted pill.
              <div
                // `h-full` is load-bearing: this div is a plain block child of
                // the `channelsButtonRef` wrapper above (not a direct flex item
                // of the outer tab row like a normal active pill in `renderTab`
                // is), so without it the box would size to its own content
                // height instead of filling the wrapper's stretched 44px —
                // reading as a squashed, misaligned tab instead of hanging
                // full-height from the header like Main's.
                className="pointer-events-auto relative flex items-stretch h-full
                         bg-[var(--bg-secondary)]
                         border-l border-r border-b border-[var(--border-secondary)]
                         rounded-bl-[14px] rounded-br-[14px]
                         -mt-[1px] z-10
                         transition-colors"
              >
                <svg
                  aria-hidden
                  width="12"
                  height="10"
                  viewBox="0 0 12 10"
                  className="pointer-events-none absolute"
                  style={{ top: "-2px", left: "-8px" }}
                >
                  <path d="M8,10 Q8,2 0,2 L12,2 L12,10 Z" style={{ fill: "var(--bg-secondary)" }} />
                  <path
                    d="M8,10 Q8,2 0,2"
                    fill="none"
                    vectorEffect="non-scaling-stroke"
                    style={{ stroke: "var(--border-secondary)", strokeWidth: 1 }}
                  />
                </svg>
                <svg
                  aria-hidden
                  width="12"
                  height="10"
                  viewBox="-4 0 12 10"
                  className="pointer-events-none absolute"
                  style={{ top: "-2px", right: "-8px" }}
                >
                  <path d="M0,10 Q0,2 8,2 L-4,2 L-4,10 Z" style={{ fill: "var(--bg-secondary)" }} />
                  <path
                    d="M0,10 Q0,2 8,2"
                    fill="none"
                    vectorEffect="non-scaling-stroke"
                    style={{ stroke: "var(--border-secondary)", strokeWidth: 1 }}
                  />
                </svg>
                <button
                  type="button"
                  onClick={() => setChannelsOpen((open) => !open)}
                  data-testid="thread-tab-channels"
                  aria-haspopup="true"
                  aria-expanded={channelsOpen}
                  className="flex items-center gap-[6px] px-[14px] text-[13px] font-medium
                           text-[var(--text-primary)] bg-transparent cursor-pointer transition-colors"
                >
                  <span className="flex items-center justify-center w-[13px] h-[13px] shrink-0">
                    <AtSign className="w-[13px] h-[13px]" />
                  </span>
                  <span>Channels</span>
                  {channelPartition.totalUnreadCount > 0 && (
                    <span
                      data-testid="channels-tile-unread-badge"
                      className="shrink-0 min-w-[16px] h-[16px] px-[4px] rounded-full
                               bg-[var(--unread-badge-bg,var(--accent))] text-white
                               text-[10px] font-semibold leading-none flex items-center justify-center"
                    >
                      {channelPartition.totalUnreadCount > 99 ? "99+" : channelPartition.totalUnreadCount}
                    </span>
                  )}
                </button>
              </div>
            ) : (
              <button
                type="button"
                onClick={() => setChannelsOpen((open) => !open)}
                data-testid="thread-tab-channels"
                aria-haspopup="true"
                aria-expanded={channelsOpen}
                className={`group flex items-center gap-[6px] pl-[10px] pr-[10px] py-[5px] rounded-full text-[13px] font-medium
                         transition-colors cursor-pointer ${channelsOpen || channelsColumnPinned
                    ? "bg-[var(--border-secondary)] text-[var(--text-primary)]"
                    : "bg-[var(--bg-hover)] text-[var(--text-secondary)] hover:bg-[var(--border-secondary)] hover:text-[var(--text-primary)]"
                  }`}
              >
                <span className="flex items-center justify-center w-[13px] h-[13px] shrink-0">
                  <AtSign className="w-[13px] h-[13px]" />
                </span>
                <span>Channels</span>
                {channelPartition.totalUnreadCount > 0 && (
                  <span
                    data-testid="channels-tile-unread-badge"
                    className="shrink-0 min-w-[16px] h-[16px] px-[4px] rounded-full
                             bg-[var(--unread-badge-bg,var(--accent))] text-white
                             text-[10px] font-semibold leading-none flex items-center justify-center"
                  >
                    {channelPartition.totalUnreadCount > 99 ? "99+" : channelPartition.totalUnreadCount}
                  </span>
                )}
              </button>
            )}

            <ChannelsTilePanel
              open={channelsOpen}
              onClose={() => setChannelsOpen(false)}
              anchorRef={channelsButtonRef}
              channelGroups={channelPartition.channelGroups}
              activeThreadId={activeThreadId}
              onSelectThread={onSelectThread}
              onOpenRename={setRenameTarget}
              onArchiveThread={onArchiveThread}
              pinned={channelsColumnPinned}
              onPin={() => {
                setChannelsColumnPinned(agentId, true);
                setChannelsOpen(false);
              }}
            />
          </div>
        )}

        {/* Collapsed "Assignments" tile — exact mirror of the Channels tile
            above, just sourced from `assignmentPartition` (assignmentThreads.ts)
            instead of `channelPartition`. Hidden entirely when the agent has
            no assignment-originated threads yet, same reasoning as the
            Channels tile's own visibility gate. */}
        {assignmentPartition.assignmentGroups.length > 0 && (
          <div
            ref={assignmentsButtonRef}
            className={`relative shrink-0 pointer-events-auto ${activeIsAssignmentThread ? "" : "self-center"}`}
            onWheel={forwardWheelToMessages}
          >
            {activeIsAssignmentThread ? (
              // Same Chrome-tab chrome as an active thread pill in
              // `renderTab` above (hanging from the header, flared feet) —
              // mirrors the Channels tile's own active-tab treatment.
              <div
                className="pointer-events-auto relative flex items-stretch h-full
                         bg-[var(--bg-secondary)]
                         border-l border-r border-b border-[var(--border-secondary)]
                         rounded-bl-[14px] rounded-br-[14px]
                         -mt-[1px] z-10
                         transition-colors"
              >
                <svg
                  aria-hidden
                  width="12"
                  height="10"
                  viewBox="0 0 12 10"
                  className="pointer-events-none absolute"
                  style={{ top: "-2px", left: "-8px" }}
                >
                  <path d="M8,10 Q8,2 0,2 L12,2 L12,10 Z" style={{ fill: "var(--bg-secondary)" }} />
                  <path
                    d="M8,10 Q8,2 0,2"
                    fill="none"
                    vectorEffect="non-scaling-stroke"
                    style={{ stroke: "var(--border-secondary)", strokeWidth: 1 }}
                  />
                </svg>
                <svg
                  aria-hidden
                  width="12"
                  height="10"
                  viewBox="-4 0 12 10"
                  className="pointer-events-none absolute"
                  style={{ top: "-2px", right: "-8px" }}
                >
                  <path d="M0,10 Q0,2 8,2 L-4,2 L-4,10 Z" style={{ fill: "var(--bg-secondary)" }} />
                  <path
                    d="M0,10 Q0,2 8,2"
                    fill="none"
                    vectorEffect="non-scaling-stroke"
                    style={{ stroke: "var(--border-secondary)", strokeWidth: 1 }}
                  />
                </svg>
                <button
                  type="button"
                  onClick={() => setAssignmentsOpen((open) => !open)}
                  data-testid="thread-tab-assignments"
                  aria-haspopup="true"
                  aria-expanded={assignmentsOpen}
                  className="flex items-center gap-[6px] px-[14px] text-[13px] font-medium
                           text-[var(--text-primary)] bg-transparent cursor-pointer transition-colors"
                >
                  <span className="flex items-center justify-center w-[13px] h-[13px] shrink-0">
                    <ClipboardList className="w-[13px] h-[13px]" />
                  </span>
                  <span>Assignments</span>
                  {assignmentPartition.totalUnreadCount > 0 && (
                    <span
                      data-testid="assignments-tile-unread-badge"
                      className="shrink-0 min-w-[16px] h-[16px] px-[4px] rounded-full
                               bg-[var(--unread-badge-bg,var(--accent))] text-white
                               text-[10px] font-semibold leading-none flex items-center justify-center"
                    >
                      {assignmentPartition.totalUnreadCount > 99 ? "99+" : assignmentPartition.totalUnreadCount}
                    </span>
                  )}
                </button>
              </div>
            ) : (
              <button
                type="button"
                onClick={() => setAssignmentsOpen((open) => !open)}
                data-testid="thread-tab-assignments"
                aria-haspopup="true"
                aria-expanded={assignmentsOpen}
                className={`group flex items-center gap-[6px] pl-[10px] pr-[10px] py-[5px] rounded-full text-[13px] font-medium
                         transition-colors cursor-pointer ${assignmentsOpen || assignmentsColumnPinned
                    ? "bg-[var(--border-secondary)] text-[var(--text-primary)]"
                    : "bg-[var(--bg-hover)] text-[var(--text-secondary)] hover:bg-[var(--border-secondary)] hover:text-[var(--text-primary)]"
                  }`}
              >
                <span className="flex items-center justify-center w-[13px] h-[13px] shrink-0">
                  <ClipboardList className="w-[13px] h-[13px]" />
                </span>
                <span>Assignments</span>
                {assignmentPartition.totalUnreadCount > 0 && (
                  <span
                    data-testid="assignments-tile-unread-badge"
                    className="shrink-0 min-w-[16px] h-[16px] px-[4px] rounded-full
                             bg-[var(--unread-badge-bg,var(--accent))] text-white
                             text-[10px] font-semibold leading-none flex items-center justify-center"
                  >
                    {assignmentPartition.totalUnreadCount > 99 ? "99+" : assignmentPartition.totalUnreadCount}
                  </span>
                )}
              </button>
            )}

            <AssignmentsTilePanel
              open={assignmentsOpen}
              onClose={() => setAssignmentsOpen(false)}
              anchorRef={assignmentsButtonRef}
              assignmentGroups={assignmentPartition.assignmentGroups}
              activeThreadId={activeThreadId}
              onSelectThread={onSelectThread}
              onOpenRename={setRenameTarget}
              onArchiveThread={onArchiveThread}
              streamingByThreadId={assignmentStreamingByThreadId}
              pinned={assignmentsColumnPinned}
              onPin={() => {
                setAssignmentsColumnPinned(agentId, true);
                setAssignmentsOpen(false);
              }}
            />
          </div>
        )}

        {/* Compressible "other threads" group — the only non-pinned item
            between Main and the More pill (both `shrink-0`), so it's the
            one that absorbs all the shrink pressure as the window narrows.
            Each pill inside compresses down to its own MIN_PILL_WIDTH_CLASS
            floor (see `renderTab`) via ordinary flexbox shrink + CSS
            ellipsis truncation — Chrome-tab-style, no measuring involved.
            Once even the floor doesn't leave room for everyone,
            `overflow-hidden` here clips the trailing (oldest,
            furthest-from-Main) pills rather than wrapping them to a second
            row or shoving Main/More off-screen — they're still one click
            away via the More panel, which always renders the full,
            unclipped thread list regardless of what's visible here.
            `px-[9px] -mx-[9px]` is a "safe paint area" trick, not visible
            padding: it grows the clip box 9px past each edge while the
            matching negative margin cancels that growth for the OUTER
            row's own layout (so Main/More sit exactly where they always
            did) — the 9px just gives the ACTIVE tab's flared "feet" (see
            the SVGs in `renderTab`, which flare 8px past the pill's own
            box on each side) a 1px margin to render uncropped when that
            tab is first/last in this group, instead of the new
            `overflow-hidden` hard-clipping a curve that was never clipped
            before.
            `pt-[4px] -mt-[4px]` is the same trick on the vertical axis: the
            active tab's own box reaches 1px above this row (`-mt-[1px]` in
            `renderTab`, tucking it under the header so it reads as hanging
            from the navbar) and its feet reach a further 2px above that —
            3px of upward reach in total. Without this, whenever the ACTIVE
            tab is one of these (i.e. not Main, which sits outside this
            wrapper), that whole 3px got clipped at this div's own top edge,
            silently deleting the feet's curve before it ever painted — so
            the header's bottom border ran straight across the top of the
            tab uninterrupted instead of curving around it.
            `gap-[10px]` matches the outer row's gap above for the same
            reason: it has to clear the active tab's 8px foot reach so a
            neighbouring pill's own fill doesn't get painted over — this is
            the gap BETWEEN pills inside the group, not the padding/margin
            described above (which only widens the clip box, not the pills'
            actual spacing). */}
        <div className="flex items-stretch gap-[10px] min-w-0 overflow-hidden px-[9px] -mx-[9px] pt-[4px] -mt-[4px]">
          {orderedVisibleOtherThreads.map(renderTab)}
        </div>

        {/* Fixed "More" pill — always the last thing in the row, regardless
            of how many threads pile up in front of it, so there's always one
            reachable place to browse/manage the full list (see
            `ThreadOverflowPanel` below) instead of it scrolling out of
            reach. `relative` + the ref anchors the floating panel directly
            under this button rather than the strip's left edge, no matter
            how many pills precede it. */}
        <div
          ref={moreButtonRef}
          className="relative self-center shrink-0 pointer-events-auto"
          onWheel={forwardWheelToMessages}
        >
          <button
            type="button"
            onClick={() => setOverflowOpen((open) => !open)}
            data-testid="thread-tab-more"
            aria-label="Threads"
            aria-haspopup="true"
            aria-expanded={overflowOpen}
            className={`flex items-center justify-center w-[30px] h-[30px] rounded-full transition-colors cursor-pointer ${overflowOpen
              ? "bg-[var(--border-secondary)] text-[var(--text-primary)]"
              : "bg-[var(--bg-hover)] text-[var(--text-secondary)] hover:bg-[var(--border-secondary)] hover:text-[var(--text-primary)]"
              }`}
          >
            <MoreHorizontal className="w-[15px] h-[15px]" />
          </button>

          <ThreadOverflowPanel
            open={overflowOpen}
            onClose={() => setOverflowOpen(false)}
            anchorRef={moreButtonRef}
            threads={otherThreads}
            archivedThreads={archivedThreads}
            activeThreadId={activeThreadId}
            onSelectThread={onSelectThread}
            onDeleteThreads={handleDeleteThreads}
            onUnarchiveThread={onUnarchiveThread}
          />
        </div>
      </div>

      {/* Portaled to `document.body` (see RenameThreadModal) — safe to keep
          mounted here regardless of this component's own pointer-events
          quirks, and unmounting-on-close via `renameTarget &&` re-seeds a
          fresh `initialValue` from the current thread the next time it opens. */}
      {renameTarget && (
        <RenameThreadModal
          open
          initialValue={renameTarget.title ?? ""}
          placeholder={tabLabel(renameTarget)}
          onClose={() => setRenameTarget(null)}
          onSubmit={async (title) => { await onRenameThread(renameTarget.id, title); }}
        />
      )}
    </div>
  );
}
